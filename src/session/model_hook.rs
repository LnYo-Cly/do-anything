//! Synchronous stdin/stdout JSON-RPC 2.0 host for external model hooks.
//!
//! Model hooks assist interactive exploration (`da session` and `da mcp`) across
//! three atomic operations — `model/select_ref`, `model/locate`, and
//! `model/judge` — without bringing an async runtime or model SDK into `da` and
//! without running during `da run` case replay.
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

/// Action name recorded in `steps.jsonl` for `select-ref` diagnostics.
pub const STEP_ACTION_SELECT_REF: &str = "model.select_ref";

/// Action name recorded in `steps.jsonl` for `locate` diagnostics.
pub const STEP_ACTION_LOCATE: &str = "model.locate";

/// Action name recorded in `steps.jsonl` for `judge` diagnostics.
pub const STEP_ACTION_JUDGE: &str = "model.judge";

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
}

/// Validated response from [`METHOD_JUDGE`].
#[derive(Debug, Clone)]
pub struct JudgeOutcome {
    pub passed: bool,
    pub rationale: String,
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
    let selected_ref = required_non_empty_str(obj, "ref", METHOD_SELECT_REF, secrets)?;
    if !valid_refs.contains(selected_ref.as_str()) {
        return Err(HookError::Environment(secrets.redact(&format!(
            "model hook `{METHOD_SELECT_REF}` returned ref `{selected_ref}`, which is not in the active snapshot candidates"
        ))));
    }
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
    let x = required_finite_f64(obj, "x", METHOD_LOCATE)?;
    let y = required_finite_f64(obj, "y", METHOD_LOCATE)?;
    if x < 0.0 || y < 0.0 || x >= f64::from(params.width) || y >= f64::from(params.height) {
        return Err(HookError::Environment(format!(
            "model hook `{METHOD_LOCATE}` returned coordinates ({x}, {y}) outside the {}x{} viewport",
            params.width, params.height
        )));
    }
    let audit = parse_audit(obj, METHOD_LOCATE, elapsed_ms, secrets)?;
    Ok(LocateOutcome { x, y, audit })
}

/// Invokes `model/judge` over redacted session evidence and parses the verdict.
pub fn call_judge(
    hook: &ModelHookSpec,
    timeout_ms: u64,
    params: JudgeParams<'_>,
    secrets: &Secrets,
) -> Result<JudgeOutcome, HookError> {
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
    if let Some(snap) = params.snapshot {
        map.insert("snapshot".to_string(), snap.clone());
    }
    if let Some(ev) = params.evidence {
        map.insert("evidence".to_string(), ev.clone());
    }
    let redacted_params = secrets.redact_json(&Value::Object(map));

    let (result, elapsed_ms) =
        invoke_jsonrpc(hook, timeout_ms, METHOD_JUDGE, redacted_params, secrets)?;
    let obj = expect_result_object(&result, METHOD_JUDGE, secrets)?;
    let passed = obj.get("passed").and_then(Value::as_bool).ok_or_else(|| {
        HookError::Environment(format!(
            "model hook `{METHOD_JUDGE}` response must carry boolean `passed`"
        ))
    })?;
    let rationale = obj
        .get("rationale")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            HookError::Environment(format!(
                "model hook `{METHOD_JUDGE}` response must carry string `rationale`"
            ))
        })?;
    let audit = parse_audit(obj, METHOD_JUDGE, elapsed_ms, secrets)?;
    Ok(JudgeOutcome {
        passed,
        rationale: secrets.redact(rationale),
        audit,
    })
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

    let confidence = match obj.get("confidence") {
        None | Some(Value::Null) => None,
        Some(val) => {
            let num = val.as_f64().filter(|n| n.is_finite()).ok_or_else(|| {
                HookError::Environment(format!(
                    "model hook `{method}` field `confidence` must be a number in 0.0..=1.0"
                ))
            })?;
            if !(0.0..=1.0).contains(&num) {
                return Err(HookError::Environment(format!(
                    "model hook `{method}` field `confidence` ({num}) must be in 0.0..=1.0"
                )));
            }
            Some(num)
        }
    };

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
}
