//! Synchronous stdin/stdout JSON-RPC 2.0 host for external model hooks.
//!
//! Model hooks assist interactive exploration (`da session` and `da mcp`) across
//! five operations — `model/select_ref`, `model/locate`, `model/judge`,
//! `model/extract`, and `model/act_step` — without bringing an async runtime or
//! model SDK into `da` and without running during `da run` case replay.
//!
//! Every outgoing prompt/evidence payload and every incoming rationale, model
//! name, usage block, trace identifier, or error message passes through
//! [`Secrets`] before crossing the boundary or being persisted.

use std::collections::BTreeSet;
use std::io::{ErrorKind, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::core::secrets::Secrets;
use crate::core::session::MaskedViewportCapture;
use crate::exit::ExitCode;
use crate::report::base64_encode;

/// Default per-invocation timeout for an external model hook, in milliseconds.
pub const DEFAULT_HOOK_TIMEOUT_MS: u64 = 30_000;

/// JSON-RPC 2.0 method name for candidate `ref` selection (e.g., Jev).
pub const METHOD_SELECT_REF: &str = "model/select_ref";

/// JSON-RPC 2.0 method name for visual coordinate grounding (e.g., UI-TARS / Qwen-VL).
pub const METHOD_LOCATE: &str = "model/locate";

/// JSON-RPC 2.0 method name for exploration-time semantic verdicts (e.g., vLLM / LLM).
pub const METHOD_JUDGE: &str = "model/judge";

/// JSON-RPC 2.0 method name for exploration-time structured data extraction.
pub const METHOD_EXTRACT: &str = "model/extract";

/// JSON-RPC 2.0 method name for one iteration of a bounded autonomous `act` loop.
pub const METHOD_ACT_STEP: &str = "model/act_step";

/// Action name recorded in `steps.jsonl` for `select-ref` diagnostics.
pub const STEP_ACTION_SELECT_REF: &str = "model.select_ref";

/// Action name recorded in `steps.jsonl` for `locate` diagnostics.
pub const STEP_ACTION_LOCATE: &str = "model.locate";

/// Action name recorded in `steps.jsonl` for `judge` diagnostics.
pub const STEP_ACTION_JUDGE: &str = "model.judge";

/// Action name recorded in `steps.jsonl` for `extract` diagnostics.
pub const STEP_ACTION_EXTRACT: &str = "model.extract";

/// Action name recorded in `steps.jsonl` for bounded `act` loop summaries.
pub const STEP_ACTION_ACT: &str = "model.act";

/// Accepted reasoning effort keywords forwarded to the built-in `genai` model layer.
pub const VALID_REASONING_EFFORTS: &[&str] = &[
    "none", "zero", "minimal", "low", "medium", "high", "xhigh", "max",
];

/// Normalizes and validates a `reasoning_effort` keyword (`none`/`zero`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`).
pub fn validate_reasoning_effort(raw: &str) -> Result<String, String> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Err("`--reasoning-effort` must not be empty".to_string());
    }
    if VALID_REASONING_EFFORTS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(format!(
            "unsupported reasoning effort `{raw}`; expected one of: {}",
            VALID_REASONING_EFFORTS.join(", ")
        ))
    }
}

const READ_CHUNK: usize = 8 * 1024;
const MAX_HOOK_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const POLL_SLEEP: Duration = Duration::from_millis(10);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// Executable command and optional arguments for an external model hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelHookSpec {
    /// Executable program name or path (started directly without a shell).
    pub command: String,
    /// Optional arguments passed directly to `command`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// Configuration for the built-in `genai` model access layer (`ai` feature).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuiltinModelSpec {
    /// Model name, optionally namespaced (`openai::gpt-4o-mini`, `deepseek::deepseek-chat`, `omlx::ui-tars-1.5-7b`, `genai_1::custom`).
    pub model: String,
    /// Optional explicit `genai` adapter override (`openai`, `openai_resp`, `anthropic`, `gemini`, `gemini_ix`, `deepseek`, `ollama`, `omlx`, `open_router`, `genai_<n>`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// Optional custom endpoint URL (supports `${env:NAME}`; persisted unexpanded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// Optional environment variable name storing the API key (never the literal secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Optional reasoning effort keyword (`none`, `zero`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Categorized failure from invoking or validating an external model hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookError {
    /// Invalid CLI/tool invocation (such as missing hook or zero timeout).
    Usage(String),
    /// The hook process exceeded `timeout_ms` and was terminated.
    Timeout(String),
    /// The hook failed to spawn, exited non-zero, or returned invalid JSON-RPC.
    Environment(String),
}

impl HookError {
    /// Maps this error to its process [`ExitCode`] and JSON error kind string.
    pub fn into_code_and_kind(self) -> (ExitCode, &'static str, String) {
        match self {
            Self::Usage(msg) => (ExitCode::Usage, "usage", msg),
            Self::Timeout(msg) => (ExitCode::Timeout, "timeout", msg),
            Self::Environment(msg) => (ExitCode::Environment, "environment", msg),
        }
    }
}

/// Redacted audit metadata returned by a model hook invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelAudit {
    /// Identifier of the model that produced the decision.
    pub model: String,
    /// Optional calibrated confidence in `0.0..=1.0`.
    pub confidence: Option<f64>,
    /// Hook-reported inference latency in ms, or measured RPC round-trip ms if omitted.
    pub latency_ms: u64,
    /// Optional token/compute usage object.
    pub usage: Option<Value>,
    /// Optional provider or adapter trace identifier.
    pub trace_id: Option<String>,
}

/// Input parameters for [`METHOD_SELECT_REF`].
pub struct SelectRefParams<'a> {
    pub session_id: &'a str,
    pub session_kind: &'a str,
    pub query: &'a str,
    pub candidates: &'a [Value],
}

/// Validated response from [`METHOD_SELECT_REF`].
#[derive(Debug, Clone)]
pub struct SelectRefOutcome {
    pub selected_ref: String,
    pub audit: ModelAudit,
}

/// Input parameters for [`METHOD_LOCATE`].
pub struct LocateParams<'a> {
    pub session_id: &'a str,
    pub session_kind: &'a str,
    pub query: &'a str,
    pub masked_png: &'a [u8],
    pub width: u32,
    pub height: u32,
}

/// Validated response from [`METHOD_LOCATE`].
#[derive(Debug, Clone)]
pub struct LocateOutcome {
    pub x: f64,
    pub y: f64,
    pub audit: ModelAudit,
}

/// Input parameters for [`METHOD_JUDGE`].
pub struct JudgeParams<'a> {
    pub session_id: &'a str,
    pub session_kind: &'a str,
    pub rubric: &'a str,
    pub snapshot: Option<&'a Value>,
    pub evidence: Option<&'a Value>,
    pub masked_capture: Option<&'a MaskedViewportCapture>,
}

/// Validated response from [`METHOD_JUDGE`].
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    pub passed: bool,
    pub rationale: String,
    pub audit: ModelAudit,
}

/// Input parameters for [`METHOD_EXTRACT`].
pub struct ExtractParams<'a> {
    pub session_id: &'a str,
    pub session_kind: &'a str,
    pub query: &'a str,
    pub schema: Option<&'a Value>,
    pub snapshot: Option<&'a Value>,
    pub evidence: Option<&'a Value>,
    pub masked_capture: Option<&'a MaskedViewportCapture>,
}

/// Validated response from [`METHOD_EXTRACT`].
#[derive(Debug, Clone)]
pub struct ExtractOutcome {
    pub data: Value,
    pub audit: ModelAudit,
}

/// Input parameters for [`METHOD_ACT_STEP`].
pub struct ActStepParams<'a> {
    pub session_id: &'a str,
    pub session_kind: &'a str,
    pub goal: &'a str,
    pub step_index: usize,
    pub max_steps: usize,
    pub available_actions: &'a [String],
    pub history: &'a [Value],
    pub snapshot: Option<&'a Value>,
    pub evidence: Option<&'a Value>,
    pub masked_capture: Option<&'a MaskedViewportCapture>,
}

/// Decision status returned by [`METHOD_ACT_STEP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActStepStatus {
    Continue,
    Done,
    Failed,
}

impl ActStepStatus {
    /// Returns the canonical lowercase status string (`continue`, `done`, or `failed`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// Concrete step or visual action returned by [`METHOD_ACT_STEP`] when `status` is `continue`.
#[derive(Debug, Clone, PartialEq)]
pub enum ActStepAction {
    /// Invoke a registered non-diagnostic `da` action (`use` + `with`).
    Step { use_action: String, with: Value },
    /// Click visual coordinates `(x, y)` and synthesize a deterministic locator.
    LocateClick { x: f64, y: f64 },
    /// Type text at visual coordinates `(x, y)` and synthesize a deterministic locator.
    LocateType { x: f64, y: f64, text: String },
}

/// Validated response from [`METHOD_ACT_STEP`].
#[derive(Debug, Clone)]
pub struct ActStepOutcome {
    pub status: ActStepStatus,
    pub summary: String,
    pub reason: Option<String>,
    pub action: Option<ActStepAction>,
    pub audit: ModelAudit,
}

/// Extracts flat candidate objects with a `ref` from a session snapshot JSON.
pub fn extract_snapshot_candidates(snapshot: &Value) -> Vec<Value> {
    if let Some(elements) = snapshot.get("elements").and_then(Value::as_array) {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for item in elements {
            if let Some(obj) = item.as_object()
                && let Some(r) = obj.get("ref").and_then(Value::as_str)
                && !r.is_empty()
                && seen.insert(r.to_string())
            {
                let mut clean = obj.clone();
                clean.remove("children");
                out.push(Value::Object(clean));
            }
        }
        if !out.is_empty() {
            return out;
        }
    }

    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    if let Some(tree) = snapshot.get("tree").and_then(Value::as_array) {
        collect_tree_candidates(tree, &mut seen, &mut out);
    }
    out
}

fn collect_tree_candidates(nodes: &[Value], seen: &mut BTreeSet<String>, out: &mut Vec<Value>) {
    for node in nodes {
        let Some(obj) = node.as_object() else {
            continue;
        };
        if let Some(r) = obj.get("ref").and_then(Value::as_str)
            && !r.is_empty()
            && seen.insert(r.to_string())
        {
            let mut clean = Map::new();
            for (k, v) in obj {
                if k != "children" {
                    clean.insert(k.clone(), v.clone());
                }
            }
            out.push(Value::Object(clean));
        }
        if let Some(children) = obj.get("children").and_then(Value::as_array) {
            collect_tree_candidates(children, seen, out);
        }
    }
}

/// Invokes `model/select_ref` and verifies that the chosen `ref` belongs to `candidates`.
pub fn call_select_ref(
    hook: &ModelHookSpec,
    timeout_ms: u64,
    params: SelectRefParams<'_>,
    secrets: &Secrets,
) -> Result<SelectRefOutcome, HookError> {
    let valid_refs: BTreeSet<&str> = params
        .candidates
        .iter()
        .filter_map(|c| c.get("ref").and_then(Value::as_str))
        .collect();

    let raw_params = json!({
        "session_id": params.session_id,
        "session_kind": params.session_kind,
        "query": params.query,
        "candidates": params.candidates,
    });
    let redacted_params = secrets.redact_json(&raw_params);

    let (result, elapsed_ms) = invoke_jsonrpc(
        hook,
        timeout_ms,
        METHOD_SELECT_REF,
        redacted_params,
        secrets,
    )?;
    let obj = expect_result_object(&result, METHOD_SELECT_REF, secrets)?;
    let selected_ref = validate_select_ref_choice(obj, &valid_refs, secrets)?;
    let audit = parse_audit(obj, METHOD_SELECT_REF, elapsed_ms, secrets)?;
    Ok(SelectRefOutcome {
        selected_ref,
        audit,
    })
}

/// Invokes `model/locate` with an input-masked PNG and validates `(x, y)` bounds.
pub fn call_locate(
    hook: &ModelHookSpec,
    timeout_ms: u64,
    params: LocateParams<'_>,
    secrets: &Secrets,
) -> Result<LocateOutcome, HookError> {
    if params.width == 0 || params.height == 0 {
        return Err(HookError::Environment(
            "cannot call `model/locate` with a zero-sized viewport".to_string(),
        ));
    }
    let image_base64 = base64_encode(params.masked_png);
    let rpc_params = json!({
        "session_id": secrets.redact(params.session_id),
        "session_kind": secrets.redact(params.session_kind),
        "query": secrets.redact(params.query),
        "image_base64": image_base64,
        "mime_type": "image/png",
        "viewport": {
            "width": params.width,
            "height": params.height,
        },
    });

    let (result, elapsed_ms) =
        invoke_jsonrpc(hook, timeout_ms, METHOD_LOCATE, rpc_params, secrets)?;
    let obj = expect_result_object(&result, METHOD_LOCATE, secrets)?;
    let (x, y) = validate_locate_coords(obj, params.width, params.height)?;
    let audit = parse_audit(obj, METHOD_LOCATE, elapsed_ms, secrets)?;
    Ok(LocateOutcome { x, y, audit })
}

fn insert_optional_json(map: &mut Map<String, Value>, key: &str, val: Option<&Value>) {
    if let Some(v) = val {
        map.insert(key.to_string(), v.clone());
    }
}

pub(crate) fn build_judge_payload(params: &JudgeParams<'_>, secrets: &Secrets) -> Value {
    let mut map = Map::new();
    map.insert(
        "session_id".to_string(),
        Value::String(params.session_id.to_string()),
    );
    map.insert(
        "session_kind".to_string(),
        Value::String(params.session_kind.to_string()),
    );
    map.insert(
        "rubric".to_string(),
        Value::String(params.rubric.to_string()),
    );
    insert_optional_json(&mut map, "snapshot", params.snapshot);
    insert_optional_json(&mut map, "evidence", params.evidence);
    secrets.redact_json(&Value::Object(map))
}

pub(crate) fn build_extract_payload(params: &ExtractParams<'_>, secrets: &Secrets) -> Value {
    let mut map = Map::new();
    map.insert(
        "session_id".to_string(),
        Value::String(params.session_id.to_string()),
    );
    map.insert(
        "session_kind".to_string(),
        Value::String(params.session_kind.to_string()),
    );
    map.insert("query".to_string(), Value::String(params.query.to_string()));
    insert_optional_json(&mut map, "schema", params.schema);
    insert_optional_json(&mut map, "snapshot", params.snapshot);
    insert_optional_json(&mut map, "evidence", params.evidence);
    secrets.redact_json(&Value::Object(map))
}

pub(crate) fn build_act_step_payload(params: &ActStepParams<'_>, secrets: &Secrets) -> Value {
    let mut map = Map::new();
    map.insert(
        "session_id".to_string(),
        Value::String(params.session_id.to_string()),
    );
    map.insert(
        "session_kind".to_string(),
        Value::String(params.session_kind.to_string()),
    );
    map.insert("goal".to_string(), Value::String(params.goal.to_string()));
    map.insert("step_index".to_string(), json!(params.step_index));
    map.insert("max_steps".to_string(), json!(params.max_steps));
    map.insert(
        "available_actions".to_string(),
        json!(params.available_actions),
    );
    map.insert("history".to_string(), Value::Array(params.history.to_vec()));
    insert_optional_json(&mut map, "snapshot", params.snapshot);
    insert_optional_json(&mut map, "evidence", params.evidence);
    secrets.redact_json(&Value::Object(map))
}

fn attach_masked_capture(
    payload: &mut Value,
    capture: Option<&MaskedViewportCapture>,
    method: &str,
) -> Result<(), HookError> {
    let Some(capture) = capture else {
        return Ok(());
    };
    if capture.width == 0 || capture.height == 0 {
        return Err(HookError::Environment(format!(
            "cannot call `{method}` with a zero-sized viewport"
        )));
    }
    if let Value::Object(obj) = payload {
        obj.insert(
            "image_base64".to_string(),
            Value::String(base64_encode(&capture.png)),
        );
        obj.insert(
            "mime_type".to_string(),
            Value::String("image/png".to_string()),
        );
        obj.insert(
            "viewport".to_string(),
            json!({ "width": capture.width, "height": capture.height }),
        );
    }
    Ok(())
}

/// Invokes `model/judge` over redacted session evidence and parses the verdict.
pub fn call_judge(
    hook: &ModelHookSpec,
    timeout_ms: u64,
    params: JudgeParams<'_>,
    secrets: &Secrets,
) -> Result<JudgeOutcome, HookError> {
    let mut redacted_params = build_judge_payload(&params, secrets);
    attach_masked_capture(&mut redacted_params, params.masked_capture, METHOD_JUDGE)?;

    let (result, elapsed_ms) =
        invoke_jsonrpc(hook, timeout_ms, METHOD_JUDGE, redacted_params, secrets)?;
    let obj = expect_result_object(&result, METHOD_JUDGE, secrets)?;
    let (passed, rationale) = validate_judge_fields(obj, secrets)?;
    let audit = parse_audit(obj, METHOD_JUDGE, elapsed_ms, secrets)?;
    Ok(JudgeOutcome {
        passed,
        rationale,
        audit,
    })
}

/// Invokes `model/extract` over redacted session evidence and validates extracted JSON `data`.
pub fn call_extract(
    hook: &ModelHookSpec,
    timeout_ms: u64,
    params: ExtractParams<'_>,
    secrets: &Secrets,
) -> Result<ExtractOutcome, HookError> {
    let mut redacted_params = build_extract_payload(&params, secrets);
    attach_masked_capture(&mut redacted_params, params.masked_capture, METHOD_EXTRACT)?;

    let (result, elapsed_ms) =
        invoke_jsonrpc(hook, timeout_ms, METHOD_EXTRACT, redacted_params, secrets)?;
    let obj = expect_result_object(&result, METHOD_EXTRACT, secrets)?;
    let data = validate_extract_fields(obj, secrets)?;
    let audit = parse_audit(obj, METHOD_EXTRACT, elapsed_ms, secrets)?;
    Ok(ExtractOutcome { data, audit })
}

/// Invokes `model/act_step` to choose the next step, visual target, or terminal status.
pub fn call_act_step(
    hook: &ModelHookSpec,
    timeout_ms: u64,
    params: ActStepParams<'_>,
    secrets: &Secrets,
) -> Result<ActStepOutcome, HookError> {
    let mut redacted_params = build_act_step_payload(&params, secrets);
    attach_masked_capture(&mut redacted_params, params.masked_capture, METHOD_ACT_STEP)?;

    let (result, elapsed_ms) =
        invoke_jsonrpc(hook, timeout_ms, METHOD_ACT_STEP, redacted_params, secrets)?;
    let obj = expect_result_object(&result, METHOD_ACT_STEP, secrets)?;
    let viewport = params.masked_capture.map(|c| (c.width, c.height));
    let (status, summary, reason, action) = validate_act_step_fields(obj, viewport, secrets)?;
    let audit = parse_audit(obj, METHOD_ACT_STEP, elapsed_ms, secrets)?;
    Ok(ActStepOutcome {
        status,
        summary,
        reason,
        action,
        audit,
    })
}

pub(crate) fn validate_select_ref_choice(
    obj: &Map<String, Value>,
    valid_refs: &BTreeSet<&str>,
    secrets: &Secrets,
) -> Result<String, HookError> {
    let selected_ref = required_non_empty_str(obj, "ref", METHOD_SELECT_REF, secrets)?;
    if !valid_refs.contains(selected_ref.as_str()) {
        return Err(HookError::Environment(secrets.redact(&format!(
            "model `{METHOD_SELECT_REF}` returned ref `{selected_ref}`, which is not in the active snapshot candidates"
        ))));
    }
    Ok(selected_ref)
}

pub(crate) fn validate_point_in_viewport(
    x: f64,
    y: f64,
    width: u32,
    height: u32,
    method: &str,
) -> Result<(), HookError> {
    if x < 0.0 || y < 0.0 || x >= f64::from(width) || y >= f64::from(height) {
        return Err(HookError::Environment(format!(
            "model `{method}` returned coordinates ({x}, {y}) outside the {width}x{height} viewport"
        )));
    }
    Ok(())
}

pub(crate) fn validate_locate_coords(
    obj: &Map<String, Value>,
    width: u32,
    height: u32,
) -> Result<(f64, f64), HookError> {
    let x = required_finite_f64(obj, "x", METHOD_LOCATE)?;
    let y = required_finite_f64(obj, "y", METHOD_LOCATE)?;
    validate_point_in_viewport(x, y, width, height, METHOD_LOCATE)?;
    Ok((x, y))
}

pub(crate) fn validate_judge_fields(
    obj: &Map<String, Value>,
    secrets: &Secrets,
) -> Result<(bool, String), HookError> {
    let passed = obj.get("passed").and_then(Value::as_bool).ok_or_else(|| {
        HookError::Environment(format!(
            "model `{METHOD_JUDGE}` response must carry boolean `passed`"
        ))
    })?;
    let rationale = obj
        .get("rationale")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HookError::Environment(format!(
                "model `{METHOD_JUDGE}` response must carry string `rationale`"
            ))
        })?;
    Ok((passed, secrets.redact(rationale)))
}

pub(crate) fn validate_extract_fields(
    obj: &Map<String, Value>,
    secrets: &Secrets,
) -> Result<Value, HookError> {
    let data = obj.get("data").ok_or_else(|| {
        HookError::Environment(format!(
            "model `{METHOD_EXTRACT}` response must carry field `data`"
        ))
    })?;
    Ok(secrets.redact_json(data))
}

pub(crate) fn validate_act_step_fields(
    obj: &Map<String, Value>,
    viewport: Option<(u32, u32)>,
    secrets: &Secrets,
) -> Result<(ActStepStatus, String, Option<String>, Option<ActStepAction>), HookError> {
    let status_raw = required_non_empty_str(obj, "status", METHOD_ACT_STEP, secrets)?;
    let status = match status_raw.trim() {
        "continue" => ActStepStatus::Continue,
        "done" => ActStepStatus::Done,
        "failed" => ActStepStatus::Failed,
        other => {
            return Err(HookError::Environment(secrets.redact(&format!(
                "model `{METHOD_ACT_STEP}` status `{other}` must be `continue`, `done`, or `failed`"
            ))));
        }
    };
    let summary = required_non_empty_str(obj, "summary", METHOD_ACT_STEP, secrets)?;
    let reason = match obj.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(secrets.redact(trimmed))
            }
        }
        Some(_) => {
            return Err(HookError::Environment(format!(
                "model `{METHOD_ACT_STEP}` field `reason` must be a string"
            )));
        }
    };

    let raw_action = obj.get("action").filter(|v| !v.is_null());
    match status {
        ActStepStatus::Done | ActStepStatus::Failed => {
            if raw_action.is_some() {
                return Err(HookError::Environment(format!(
                    "model `{METHOD_ACT_STEP}` must omit `action` when `status` is `{}`",
                    status.as_str()
                )));
            }
            Ok((status, summary, reason, None))
        }
        ActStepStatus::Continue => {
            let action_obj = raw_action.and_then(Value::as_object).ok_or_else(|| {
                HookError::Environment(format!(
                    "model `{METHOD_ACT_STEP}` must carry an `action` object when `status` is `continue`"
                ))
            })?;
            let action = parse_act_step_action(action_obj, viewport, secrets)?;
            Ok((status, summary, reason, Some(action)))
        }
    }
}

fn parse_act_step_action(
    action_obj: &Map<String, Value>,
    viewport: Option<(u32, u32)>,
    secrets: &Secrets,
) -> Result<ActStepAction, HookError> {
    let kind = required_non_empty_str(action_obj, "kind", METHOD_ACT_STEP, secrets)?;
    match kind.trim() {
        "step" => {
            let use_action = required_non_empty_str(action_obj, "use", METHOD_ACT_STEP, secrets)?;
            let with = match action_obj.get("with") {
                None | Some(Value::Null) => Value::Object(Map::new()),
                Some(val @ Value::Object(_)) => secrets.redact_json(val),
                Some(_) => {
                    return Err(HookError::Environment(format!(
                        "model `{METHOD_ACT_STEP}` `action.with` must be a JSON object"
                    )));
                }
            };
            Ok(ActStepAction::Step { use_action, with })
        }
        "locate_click" | "locate_type" => {
            let x = required_finite_f64(action_obj, "x", METHOD_ACT_STEP)?;
            let y = required_finite_f64(action_obj, "y", METHOD_ACT_STEP)?;
            if let Some((w, h)) = viewport {
                validate_point_in_viewport(x, y, w, h, METHOD_ACT_STEP)?;
            } else if x < 0.0 || y < 0.0 {
                return Err(HookError::Environment(format!(
                    "model `{METHOD_ACT_STEP}` coordinates ({x}, {y}) must be non-negative"
                )));
            }
            if kind.trim() == "locate_click" {
                Ok(ActStepAction::LocateClick { x, y })
            } else {
                let text = action_obj
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        HookError::Environment(format!(
                            "model `{METHOD_ACT_STEP}` `locate_type` action must carry string `text`"
                        ))
                    })?;
                Ok(ActStepAction::LocateType {
                    x,
                    y,
                    text: secrets.redact(text),
                })
            }
        }
        other => Err(HookError::Environment(secrets.redact(&format!(
            "model `{METHOD_ACT_STEP}` action kind `{other}` must be `step`, `locate_click`, or `locate_type`"
        )))),
    }
}

pub(crate) fn extract_confidence(
    obj: &Map<String, Value>,
    method: &str,
) -> Result<Option<f64>, HookError> {
    match obj.get("confidence") {
        None | Some(Value::Null) => Ok(None),
        Some(val) => {
            let num = val.as_f64().filter(|n| n.is_finite()).ok_or_else(|| {
                HookError::Environment(format!(
                    "model `{method}` field `confidence` must be a number in 0.0..=1.0"
                ))
            })?;
            if !(0.0..=1.0).contains(&num) {
                return Err(HookError::Environment(format!(
                    "model `{method}` field `confidence` ({num}) must be in 0.0..=1.0"
                )));
            }
            Ok(Some(num))
        }
    }
}

fn expect_result_object<'a>(
    result: &'a Value,
    method: &str,
    secrets: &Secrets,
) -> Result<&'a Map<String, Value>, HookError> {
    result.as_object().ok_or_else(|| {
        HookError::Environment(secrets.redact(&format!(
            "model hook `{method}` returned a non-object `result`"
        )))
    })
}

fn required_non_empty_str(
    obj: &Map<String, Value>,
    field: &str,
    method: &str,
    secrets: &Secrets,
) -> Result<String, HookError> {
    match obj.get(field).and_then(Value::as_str) {
        Some(val) if !val.trim().is_empty() => Ok(secrets.redact(val)),
        _ => Err(HookError::Environment(format!(
            "model hook `{method}` response must carry non-empty string `{field}`"
        ))),
    }
}

fn required_finite_f64(
    obj: &Map<String, Value>,
    field: &str,
    method: &str,
) -> Result<f64, HookError> {
    obj.get(field)
        .and_then(Value::as_f64)
        .filter(|num| num.is_finite())
        .ok_or_else(|| {
            HookError::Environment(format!(
                "model hook `{method}` response must carry finite number `{field}`"
            ))
        })
}

fn parse_audit(
    obj: &Map<String, Value>,
    method: &str,
    fallback_latency_ms: u64,
    secrets: &Secrets,
) -> Result<ModelAudit, HookError> {
    let model = required_non_empty_str(obj, "model", method, secrets)?;
    let confidence = extract_confidence(obj, method)?;

    let latency_ms = match obj.get("latency_ms") {
        None | Some(Value::Null) => fallback_latency_ms,
        Some(val) => val
            .as_u64()
            .or_else(|| {
                val.as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .map(|n| n.round() as u64)
            })
            .ok_or_else(|| {
                HookError::Environment(format!(
                    "model hook `{method}` field `latency_ms` must be a non-negative number"
                ))
            })?,
    };

    let usage = match obj.get("usage") {
        None | Some(Value::Null) => None,
        Some(val @ Value::Object(_)) => Some(secrets.redact_json(val)),
        Some(_) => {
            return Err(HookError::Environment(format!(
                "model hook `{method}` field `usage` must be a JSON object"
            )));
        }
    };

    let trace_id = match obj.get("trace_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(secrets.redact(s)),
        Some(Value::String(_)) => None,
        Some(_) => {
            return Err(HookError::Environment(format!(
                "model hook `{method}` field `trace_id` must be a string"
            )));
        }
    };

    Ok(ModelAudit {
        model,
        confidence,
        latency_ms,
        usage,
        trace_id,
    })
}

/// Ensures a spawned hook process is terminated and reaped on every exit path.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match &mut self.child {
            Some(child) => child.try_wait(),
            None => Ok(None),
        }
    }

    fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn disarm(&mut self) {
        self.child = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// Reads a child pipe on a helper thread up to `MAX_HOOK_OUTPUT_BYTES`.
struct BoundedPipeReader {
    buffer: Arc<Mutex<Vec<u8>>>,
    overflowed: Arc<Mutex<bool>>,
    done: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl BoundedPipeReader {
    fn spawn<R: Read + Send + 'static>(mut pipe: R) -> Self {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let overflowed = Arc::new(Mutex::new(false));
        let shared_buf = Arc::clone(&buffer);
        let shared_ovf = Arc::clone(&overflowed);
        let (sender, done) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut chunk = [0_u8; READ_CHUNK];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        if let Ok(mut buf) = shared_buf.lock() {
                            let remaining = MAX_HOOK_OUTPUT_BYTES.saturating_sub(buf.len());
                            if read <= remaining {
                                buf.extend_from_slice(&chunk[..read]);
                            } else {
                                if remaining > 0 {
                                    buf.extend_from_slice(&chunk[..remaining]);
                                }
                                if let Ok(mut ovf) = shared_ovf.lock() {
                                    *ovf = true;
                                }
                            }
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = sender.send(());
        });
        Self {
            buffer,
            overflowed,
            done,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> (Vec<u8>, bool) {
        let closed = self.done.recv_timeout(DRAIN_TIMEOUT).is_ok();
        if closed && let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let bytes = self.buffer.lock().map(|b| b.clone()).unwrap_or_default();
        let overflowed = self.overflowed.lock().map(|f| *f).unwrap_or(false);
        (bytes, overflowed)
    }
}

fn invoke_jsonrpc(
    hook: &ModelHookSpec,
    timeout_ms: u64,
    method: &str,
    params: Value,
    secrets: &Secrets,
) -> Result<(Value, u64), HookError> {
    if hook.command.trim().is_empty() {
        return Err(HookError::Usage(
            "model hook executable path must not be empty".to_string(),
        ));
    }
    if timeout_ms == 0 {
        return Err(HookError::Usage(
            "`--timeout-ms` must be at least 1".to_string(),
        ));
    }

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let mut request_line = serde_json::to_vec(&request).map_err(|err| {
        HookError::Environment(format!(
            "could not serialize JSON-RPC `{method}` request: {err}"
        ))
    })?;
    request_line.push(b'\n');

    let display_cmd = secrets.redact(&hook.command);
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms);

    let mut child = Command::new(&hook.command)
        .args(&hook.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| {
            let hint = match err.kind() {
                ErrorKind::NotFound => {
                    "check that the hook executable exists and is on `PATH`, or pass an absolute path"
                }
                ErrorKind::PermissionDenied => "check that the hook file is executable",
                _ => "check `--model-hook` / `--hook`",
            };
            HookError::Environment(secrets.redact(&format!(
                "could not start model hook `{display_cmd}`: {err}; {hint}"
            )))
        })?;

    let stdout_reader = child.stdout.take().map(BoundedPipeReader::spawn);
    let stderr_reader = child.stderr.take().map(BoundedPipeReader::spawn);

    if let Some(mut stdin) = child.stdin.take() {
        thread::spawn(move || {
            let _ = stdin.write_all(&request_line);
            let _ = stdin.flush();
        });
    }

    let mut guard = ChildGuard::new(child);
    let status = loop {
        match guard.try_wait() {
            Ok(Some(status)) => {
                guard.disarm();
                break status;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    guard.kill_and_reap();
                    let _ = stdout_reader.map(BoundedPipeReader::finish);
                    let _ = stderr_reader.map(BoundedPipeReader::finish);
                    return Err(HookError::Timeout(format!(
                        "model hook `{display_cmd}` (`{method}`) did not finish within {timeout_ms} ms and was terminated"
                    )));
                }
                thread::sleep(POLL_SLEEP);
            }
            Err(err) => {
                guard.kill_and_reap();
                let _ = stdout_reader.map(BoundedPipeReader::finish);
                let _ = stderr_reader.map(BoundedPipeReader::finish);
                return Err(HookError::Environment(secrets.redact(&format!(
                    "could not wait for model hook `{display_cmd}`: {err}"
                ))));
            }
        }
    };

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (stdout_bytes, stdout_overflowed) = stdout_reader
        .map(BoundedPipeReader::finish)
        .unwrap_or_default();
    let (stderr_bytes, _) = stderr_reader
        .map(BoundedPipeReader::finish)
        .unwrap_or_default();

    let stderr_text = secrets.redact(String::from_utf8_lossy(&stderr_bytes).trim());

    if !status.success() {
        let code_desc = status.code().map_or_else(
            || "terminated by signal".to_string(),
            |c| format!("exit code {c}"),
        );
        let detail = if stderr_text.is_empty() {
            String::new()
        } else {
            format!(": {stderr_text}")
        };
        return Err(HookError::Environment(format!(
            "model hook `{display_cmd}` (`{method}`) failed with {code_desc}{detail}"
        )));
    }

    if stdout_overflowed {
        return Err(HookError::Environment(format!(
            "model hook `{display_cmd}` (`{method}`) exceeded the {MAX_HOOK_OUTPUT_BYTES}-byte stdout limit"
        )));
    }

    let stdout_text = String::from_utf8(stdout_bytes).map_err(|_| {
        HookError::Environment(format!(
            "model hook `{display_cmd}` (`{method}`) wrote non-UTF-8 bytes to stdout"
        ))
    })?;

    let response = parse_jsonrpc_stdout(&stdout_text).map_err(|reason| {
        let extra = if stderr_text.is_empty() {
            String::new()
        } else {
            format!(" (stderr: {stderr_text})")
        };
        HookError::Environment(secrets.redact(&format!(
            "model hook `{display_cmd}` (`{method}`) returned invalid JSON-RPC 2.0 output: {reason}{extra}"
        )))
    })?;

    let Some(envelope) = response.as_object() else {
        return Err(HookError::Environment(format!(
            "model hook `{display_cmd}` (`{method}`) response must be a JSON object"
        )));
    };

    if envelope.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(HookError::Environment(format!(
            "model hook `{display_cmd}` (`{method}`) response is missing `\"jsonrpc\": \"2.0\"`"
        )));
    }

    if envelope.get("id") != Some(&json!(1)) {
        return Err(HookError::Environment(format!(
            "model hook `{display_cmd}` (`{method}`) response `id` did not match request id `1`"
        )));
    }

    if let Some(err_val) = envelope.get("error")
        && !err_val.is_null()
    {
        let code = err_val
            .get("code")
            .and_then(Value::as_i64)
            .map_or_else(|| "unknown".to_string(), |c| c.to_string());
        let msg = err_val
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unspecified JSON-RPC error");
        return Err(HookError::Environment(secrets.redact(&format!(
            "model hook `{display_cmd}` (`{method}`) returned JSON-RPC error {code}: {msg}"
        ))));
    }

    let Some(result) = envelope.get("result") else {
        return Err(HookError::Environment(format!(
            "model hook `{display_cmd}` (`{method}`) response is missing `result`"
        )));
    };

    Ok((result.clone(), elapsed_ms))
}

fn parse_jsonrpc_stdout(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("stdout was empty".to_string());
    }
    if let Ok(parsed) = serde_json::from_str::<Value>(trimmed) {
        return Ok(parsed);
    }
    // Support Content-Length framed output as well for compatibility with #82 / MCP adapters.
    if trimmed
        .as_bytes()
        .get(..15)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"content-length:"))
        && let Some((_, body)) = raw
            .split_once("\r\n\r\n")
            .or_else(|| raw.split_once("\n\n"))
    {
        return serde_json::from_str::<Value>(body.trim())
            .map_err(|err| format!("could not parse framed JSON body: {err}"));
    }
    for line in trimmed.lines().rev() {
        let line = line.trim();
        if line.starts_with('{')
            && let Ok(parsed) = serde_json::from_str::<Value>(line)
        {
            return Ok(parsed);
        }
    }
    serde_json::from_str::<Value>(trimmed).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_snapshot_candidates_flattens_nested_tree_without_children() {
        let snap = json!({
            "tree": [
                {
                    "ref": "e1",
                    "role": "form",
                    "name": "Login",
                    "children": [
                        { "ref": "e2", "role": "textbox", "name": "Username", "value": "alice" },
                        { "ref": "e3", "role": "button", "name": "Sign In" }
                    ]
                }
            ]
        });
        let candidates = extract_snapshot_candidates(&snap);
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates[0],
            json!({ "ref": "e1", "role": "form", "name": "Login" })
        );
        assert_eq!(
            candidates[1],
            json!({ "ref": "e2", "role": "textbox", "name": "Username", "value": "alice" })
        );
        assert_eq!(
            candidates[2],
            json!({ "ref": "e3", "role": "button", "name": "Sign In" })
        );
    }

    #[test]
    fn parse_audit_validates_confidence_and_redacts_secrets() {
        let mut secrets = Secrets::new();
        secrets.record("secret-trace-777");
        let raw = json!({
            "model": "jev-small",
            "confidence": 0.85,
            "latency_ms": 19,
            "usage": { "prompt_tokens": 50, "tag": "secret-trace-777" },
            "trace_id": "tr-secret-trace-777"
        });
        let audit = parse_audit(
            raw.as_object().expect("object"),
            METHOD_SELECT_REF,
            100,
            &secrets,
        )
        .expect("valid audit");
        assert_eq!(audit.model, "jev-small");
        assert_eq!(audit.confidence, Some(0.85));
        assert_eq!(audit.latency_ms, 19);
        assert_eq!(
            audit.usage,
            Some(json!({ "prompt_tokens": 50, "tag": "***" }))
        );
        assert_eq!(audit.trace_id.as_deref(), Some("tr-***"));

        let bad_conf = json!({ "model": "jev-small", "confidence": 1.5 });
        assert!(
            parse_audit(
                bad_conf.as_object().expect("obj"),
                METHOD_SELECT_REF,
                5,
                &secrets
            )
            .is_err()
        );
    }

    #[test]
    fn parse_jsonrpc_stdout_accepts_line_and_content_length_frames() {
        let line = r#"{"jsonrpc":"2.0","id":1,"result":{"ref":"e1","model":"m"}}"#;
        assert_eq!(
            parse_jsonrpc_stdout(line).expect("line")["result"]["ref"],
            "e1"
        );

        let framed = format!("Content-Length: {}\r\n\r\n{line}", line.len());
        assert_eq!(
            parse_jsonrpc_stdout(&framed).expect("framed")["result"]["ref"],
            "e1"
        );
    }

    #[test]
    fn validate_reasoning_effort_normalizes_keywords_and_rejects_unknown() {
        for raw in [
            "none", "Zero", "minimal", "LOW", "medium", "high", "xhigh", "max",
        ] {
            assert_eq!(
                validate_reasoning_effort(raw).expect("valid effort"),
                raw.to_ascii_lowercase()
            );
        }
        assert!(validate_reasoning_effort("").is_err());
        assert!(validate_reasoning_effort("ultra").is_err());
    }

    #[test]
    fn validate_extract_fields_requires_data_and_redacts_secrets() {
        let mut secrets = Secrets::new();
        secrets.record("tok-secret-999");
        let missing = json!({ "model": "m" });
        assert!(validate_extract_fields(missing.as_object().expect("obj"), &secrets).is_err());

        let valid = json!({ "data": { "token": "tok-secret-999", "count": 2 } });
        let data =
            validate_extract_fields(valid.as_object().expect("obj"), &secrets).expect("data");
        assert_eq!(data, json!({ "token": "***", "count": 2 }));
    }

    #[test]
    fn validate_act_step_fields_validates_statuses_actions_and_bounds() {
        let mut secrets = Secrets::new();
        secrets.record("pw-secret-123");

        let step_resp = json!({
            "status": "continue",
            "summary": "Click button with pw-secret-123",
            "action": { "kind": "step", "use": "browser.click", "with": { "ref": "e1" } }
        });
        let (status, summary, reason, action) = validate_act_step_fields(
            step_resp.as_object().expect("obj"),
            Some((800, 600)),
            &secrets,
        )
        .expect("valid continue");
        assert_eq!(status, ActStepStatus::Continue);
        assert_eq!(summary, "Click button with ***");
        assert_eq!(reason, None);
        assert_eq!(
            action,
            Some(ActStepAction::Step {
                use_action: "browser.click".to_string(),
                with: json!({ "ref": "e1" }),
            })
        );

        let locate_type_resp = json!({
            "status": "continue",
            "summary": "Type query",
            "action": { "kind": "locate_type", "x": 120.0, "y": 40.0, "text": "hello pw-secret-123" }
        });
        let (_, _, _, action) = validate_act_step_fields(
            locate_type_resp.as_object().expect("obj"),
            Some((800, 600)),
            &secrets,
        )
        .expect("valid locate_type");
        assert_eq!(
            action,
            Some(ActStepAction::LocateType {
                x: 120.0,
                y: 40.0,
                text: "hello ***".to_string(),
            })
        );

        let out_of_bounds = json!({
            "status": "continue",
            "summary": "OOB",
            "action": { "kind": "locate_click", "x": 900.0, "y": 40.0 }
        });
        assert!(
            validate_act_step_fields(
                out_of_bounds.as_object().expect("obj"),
                Some((800, 600)),
                &secrets,
            )
            .is_err()
        );

        let done_resp = json!({
            "status": "done",
            "summary": "All complete",
            "reason": "Order confirmed"
        });
        let (status, _, reason, action) =
            validate_act_step_fields(done_resp.as_object().expect("obj"), None, &secrets)
                .expect("valid done");
        assert_eq!(status, ActStepStatus::Done);
        assert_eq!(reason.as_deref(), Some("Order confirmed"));
        assert_eq!(action, None);
    }
}
