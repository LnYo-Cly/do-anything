//! The base tools `da mcp` exposes (and optional model tools when configured)
//! and how each one reaches the session engine.
//!
//! The base list is fixed at six on purpose: one tool per registered step would
//! grow with every driver and would differ between builds, which spends a
//! model's context on tool descriptions it cannot use. Instead `step` takes the
//! same `use`/`with`/`expect`/`save` shape a case file holds and validates it
//! with the case parser, so a new driver needs no protocol change. When model
//! access is enabled, `definitions_for` appends `select_ref`, `locate`,
//! `judge`, `extract`, and `act`.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::core::case::{self, CaseError, LoadedCase};
use crate::core::events::EventBus;
use crate::core::filter::Filters;
use crate::core::output::{self, Format};
use crate::core::registry::Registry;
use crate::core::report::RunOutcome;
use crate::core::secrets::Secrets;
use crate::session::export::ExportOptions;
use crate::session::model_hook::{
    BuiltinModelSpec, DEFAULT_HOOK_TIMEOUT_MS, ModelHookSpec, validate_reasoning_effort,
};
use crate::session::{
    self, ActRequest, DEFAULT_ACT_MAX_STEPS, ExtractRequest, JudgeRequest, LocateRequest,
    MAX_ACT_MAX_STEPS, SelectRefAction, SelectRefRequest, SessionRequest, StepInput,
};

/// Names of the base tools, in the order `tools/list` reports them without server model config.
pub const NAMES: [&str; 6] = [
    "session_start",
    "step",
    "snapshot",
    "export",
    "run",
    "session_stop",
];

/// Names of all tools exposed when `--model-hook`, `--model`, or `DA_MODEL` is configured on `da mcp`.
pub const ALL_NAMES: [&str; 11] = [
    "session_start",
    "step",
    "snapshot",
    "select_ref",
    "locate",
    "judge",
    "extract",
    "act",
    "export",
    "run",
    "session_stop",
];

/// What one `tools/call` needs from the server that owns it.
pub struct Context<'a> {
    /// Steps, observers and detachable sessions this build registered.
    pub registry: &'a Registry,
    /// Directory that stores session state.
    pub sessions_dir: &'a Path,
    /// Idle timeout `session_start` applies, in milliseconds.
    pub idle_timeout_ms: u64,
    /// Optional server-default model hook configured on `da mcp`.
    pub model_hook: Option<&'a ModelHookSpec>,
    /// Optional server-default built-in model access configuration on `da mcp`.
    pub model: Option<&'a BuiltinModelSpec>,
    /// Absolute paths of the explicit plugins this server was started with.
    pub plugins: &'a [String],
}

/// The result of one tool call, before redaction and framing.
pub struct ToolOutcome {
    /// The JSON document the caller reads.
    pub payload: Value,
    /// Whether the call failed, which MCP reports as `isError`.
    pub is_error: bool,
}

impl ToolOutcome {
    /// A call that could not finish, with a payload that says why.
    fn failed(payload: Value) -> Self {
        Self {
            payload,
            is_error: true,
        }
    }
}

/// Describes the six base tools for `tools/list`.
///
/// Use [`definitions_for`] to include the optional model tools (`select_ref`,
/// `locate`, `judge`, `extract`, `act`). The schemas are written by hand
/// because they describe this protocol, not a step: a step's own arguments
/// schema comes from the registry and is echoed back when a `step` call is
/// refused.
pub fn definitions() -> Vec<Value> {
    vec![
        session_start_definition(),
        step_definition(),
        snapshot_definition(),
        export_definition(),
        run_definition(),
        session_stop_definition(),
    ]
}

fn session_start_definition() -> Value {
    json!({
        "name": "session_start",
        "description": "Starts an interactive session and returns its `session_id`. The session keeps live resources such as a browser open across calls and stores its state under `.da/sessions`.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "model_hook": {
                    "type": "string",
                    "description": "Optional external JSON-RPC 2.0 model hook executable persisted for `select_ref`, `locate`, and `judge`.",
                },
                "model_hook_args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional arguments passed to `model_hook`.",
                },
                "model": {
                    "type": "string",
                    "description": "Optional built-in `genai` model identifier (`openai::gpt-4o-mini`, `deepseek::deepseek-chat`, `omlx::ui-tars-1.5-7b`, ...).",
                },
                "model_adapter": {
                    "type": "string",
                    "description": "Optional `genai` adapter override (`openai`, `openai_resp`, `anthropic`, `gemini`, `deepseek`, `ollama`, `omlx`, `genai_<n>`, ...).",
                },
                "model_endpoint": {
                    "type": "string",
                    "description": "Optional custom HTTP endpoint URL for `model` (supports `${env:NAME}`).",
                },
                "model_api_key_env": {
                    "type": "string",
                    "description": "Optional environment variable name holding the API key for `model`.",
                },
                "model_reasoning_effort": {
                    "type": "string",
                    "enum": ["none", "zero", "minimal", "low", "medium", "high", "xhigh", "max"],
                    "description": "Optional reasoning effort level forwarded to the built-in `genai` model (`none`/`zero`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`).",
                },
            },
        },
    })
}

fn step_definition() -> Value {
    json!({
        "name": "step",
        "description": "Runs one step in an existing session. `use` names a registered step and `with` holds its arguments, exactly as a case file nests them under the step name; `expect` and `save` behave as in a case file. When the arguments do not satisfy the step's schema, the error result carries that schema so the call can be corrected.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["session_id", "use"],
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Identifier returned by `session_start`.",
                },
                "use": {
                    "type": "string",
                    "description": "Registered step name, for example `browser.open` or `cli.run`.",
                },
                "with": {
                    "type": "object",
                    "description": "Arguments of the step, as the mapping a case file nests under the step name.",
                },
                "expect": {
                    "type": "array",
                    "description": "Assertions evaluated against the step result, as in a case file.",
                    "items": {"type": "object"},
                },
                "save": {
                    "type": "object",
                    "description": "Variables filled from the step result as `NAME` to JSONPath, as in a case file.",
                    "additionalProperties": {"type": "string"},
                },
            },
        },
    })
}

fn snapshot_definition() -> Value {
    json!({
        "name": "snapshot",
        "description": "Captures an accessibility snapshot of the session's browser without running a step. Only a session with an open browser page can answer; the result is the same compact tree `da session snapshot` prints.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
                "ref": {
                    "type": "string",
                    "description": "Element reference from a previous snapshot to expand within.",
                },
                "max_depth": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum traversal depth, relative to the root or `ref`.",
                },
                "max_nodes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum nodes included before the tree is truncated.",
                },
            },
        },
    })
}

fn export_definition() -> Value {
    json!({
        "name": "export",
        "description": "Exports the session's passed steps as one deterministic, replayable YAML case. The YAML is byte for byte what `da session export --yaml` writes for the same steps: transient browser `ref` values become stable locators and `${env:...}` references stay references.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
                "name": {
                    "type": "string",
                    "description": "Case `name:` written into the exported YAML. Defaults to a name derived from the session id.",
                },
                "include_reads": {
                    "type": "boolean",
                    "description": "Include read-only `browser.read` steps that have neither `expect` nor `save`.",
                },
                "out": {
                    "type": "string",
                    "description": "Also write the YAML to this file path.",
                },
            },
        },
    })
}

fn run_definition() -> Value {
    json!({
        "name": "run",
        "description": "Runs case files or directories and returns the same JSON report `da --output json run` prints: one object with every case, step, assertion and the summary.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["paths"],
            "properties": {
                "paths": {
                    "type": "array",
                    "minItems": 1,
                    "items": {"type": "string"},
                    "description": "Case files or directories that contain `.yaml` or `.yml` cases.",
                },
                "vars": {
                    "type": "object",
                    "description": "Case variables that override the ones declared in the case files.",
                    "additionalProperties": {"type": ["string", "number", "boolean"]},
                },
                "jobs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Runs up to N cases at the same time. Defaults to 1.",
                },
            },
        },
    })
}

fn session_stop_definition() -> Value {
    json!({
        "name": "session_stop",
        "description": "Closes every resource the session holds and removes its state directory, the same cleanup `da session stop` performs.",
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["session_id"],
            "properties": {
                "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
            },
        },
    })
}

/// Describes `tools/list`, including `select_ref`, `locate`, `judge`, `extract`,
/// and `act` when `--model-hook`, `--model`, or `DA_MODEL` is configured on `da mcp`.
pub fn definitions_for(include_model_hooks: bool) -> Vec<Value> {
    let mut base = definitions();
    if !include_model_hooks {
        return base;
    }
    let hook_defs = model_hook_definitions();
    // Insert right after `snapshot` (index 3) so exploration tools stay grouped.
    base.splice(3..3, hook_defs);
    base
}

fn insert_model_call_properties(properties: &mut Map<String, Value>, timeout_description: &str) {
    properties.insert(
        "hook".to_string(),
        json!({"type": "string", "description": "Per-call model hook executable override."}),
    );
    properties.insert(
        "hook_args".to_string(),
        json!({"type": "array", "items": {"type": "string"}, "description": "Arguments passed to `hook`."}),
    );
    properties.insert(
        "model".to_string(),
        json!({"type": "string", "description": "Per-call built-in `genai` model override."}),
    );
    properties.insert(
        "adapter".to_string(),
        json!({"type": "string", "description": "Optional `genai` adapter override for `model`."}),
    );
    properties.insert(
        "endpoint".to_string(),
        json!({"type": "string", "description": "Optional custom endpoint URL override for `model` (supports `${env:NAME}`)."}),
    );
    properties.insert(
        "api_key_env".to_string(),
        json!({"type": "string", "description": "Optional environment variable name holding the API key for `model`."}),
    );
    properties.insert(
        "reasoning_effort".to_string(),
        json!({"type": "string", "enum": ["none", "zero", "minimal", "low", "medium", "high", "xhigh", "max"], "description": "Optional reasoning effort level for the built-in `genai` model."}),
    );
    properties.insert(
        "timeout_ms".to_string(),
        json!({"type": "integer", "minimum": 1, "description": timeout_description}),
    );
}

fn model_tool_definition(
    name: &str,
    description: &str,
    required: &[&str],
    extra_properties: Value,
    timeout_description: &str,
) -> Value {
    let mut properties = match extra_properties {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    insert_model_call_properties(&mut properties, timeout_description);
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "additionalProperties": false,
            "required": required,
            "properties": properties,
        },
    })
}

fn select_ref_definition() -> Value {
    let mut def = model_tool_definition(
        "select_ref",
        "Invokes `model/select_ref` (built-in `genai` model or external JSON-RPC 2.0 hook) over the session's active snapshot candidates, verifies the chosen `ref`, and returns its deterministic `synthesized_locator`.",
        &["session_id", "query"],
        json!({
            "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
            "query": {"type": "string", "description": "Natural-language target description (redacted via `Secrets`)."},
            "ref": {"type": "string", "description": "Optional snapshot `ref` to scope candidates within."},
            "max_depth": {"type": "integer", "minimum": 1, "description": "Maximum snapshot traversal depth."},
            "max_nodes": {"type": "integer", "minimum": 1, "description": "Maximum snapshot nodes."},
        }),
        "Hook timeout in milliseconds (default 30000).",
    );
    if let Some(props) = def
        .pointer_mut("/inputSchema/properties")
        .and_then(Value::as_object_mut)
    {
        props.insert(
            "act".to_string(),
            json!({"type": "string", "description": "Optional step name (such as `browser.click`) to execute immediately with the selected `ref`."}),
        );
        props.insert(
            "with".to_string(),
            json!({"type": "object", "description": "Optional step arguments merged when `act` is set."}),
        );
    }
    def
}

fn locate_definition() -> Value {
    let mut def = model_tool_definition(
        "locate",
        "Captures an input-masked PNG screenshot, invokes `model/locate` for `(x, y)` coordinates, and reverse-queries the live DOM/UI tree to return a deterministic `synthesized_locator` (falling back to `at` on `<canvas>`).",
        &["session_id", "query"],
        json!({
            "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
            "query": {"type": "string", "description": "Visual grounding query (redacted via `Secrets`)."},
        }),
        "Hook timeout in milliseconds (default 30000).",
    );
    if let Some(props) = def
        .pointer_mut("/inputSchema/properties")
        .and_then(Value::as_object_mut)
    {
        props.insert(
            "click".to_string(),
            json!({"type": "boolean", "description": "Immediately click the located target and record the step for export."}),
        );
    }
    def
}

fn judge_definition() -> Value {
    model_tool_definition(
        "judge",
        "Invokes `model/judge` over redacted session evidence (and an optional masked screenshot when `screenshot` is `true`) and returns `{ passed, rationale, model, ... }`; recorded in `steps.jsonl` and omitted from `export`.",
        &["session_id", "rubric"],
        json!({
            "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
            "rubric": {"type": "string", "description": "Evaluation rubric (redacted via `Secrets`)."},
            "evidence": {"description": "Optional custom JSON evidence; defaults to redacted session steps and variables."},
            "screenshot": {"type": "boolean", "description": "Attach an input-masked PNG viewport/window screenshot from the active visual session (default false)."},
        }),
        "Hook timeout in milliseconds (default 30000).",
    )
}

fn extract_definition() -> Value {
    model_tool_definition(
        "extract",
        "Invokes `model/extract` over redacted session evidence (and an optional masked screenshot when `screenshot` is `true`), returns structured `data` (optionally constrained by `schema`), and optionally saves to session variable `save`; omitted from `export`.",
        &["session_id", "query"],
        json!({
            "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
            "query": {"type": "string", "description": "What structured information to extract (redacted via `Secrets`)."},
            "schema": {"type": "object", "description": "Optional JSON Schema object describing the expected `data` shape."},
            "evidence": {"description": "Optional custom JSON evidence; defaults to redacted session steps and variables."},
            "screenshot": {"type": "boolean", "description": "Attach an input-masked PNG viewport/window screenshot from the active visual session (default false)."},
            "save": {"type": "string", "description": "Optional session variable name in which to store the extracted value for subsequent `${var}` steps."},
        }),
        "Hook timeout in milliseconds (default 30000).",
    )
}

fn act_definition() -> Value {
    model_tool_definition(
        "act",
        "Runs a bounded observe → decide (`model/act_step`) → execute loop up to `max_steps` (default 8) toward `goal`, recording real deterministic UI steps (plus synthesized locators for visual actions) that export cleanly to YAML.",
        &["session_id", "goal"],
        json!({
            "session_id": {"type": "string", "description": "Identifier returned by `session_start`."},
            "goal": {"type": "string", "description": "Autonomous goal description (redacted via `Secrets`)."},
            "max_steps": {"type": "integer", "minimum": 1, "maximum": MAX_ACT_MAX_STEPS, "description": "Maximum autonomous steps before stopping (default 8)."},
            "evidence": {"description": "Optional extra JSON context made available to every iteration."},
            "screenshot": {"type": "boolean", "description": "Capture and attach an input-masked PNG screenshot on every iteration (default false)."},
        }),
        "Per-iteration model/hook timeout in milliseconds (default 30000).",
    )
}

fn model_hook_definitions() -> Vec<Value> {
    vec![
        select_ref_definition(),
        locate_definition(),
        judge_definition(),
        extract_definition(),
        act_definition(),
    ]
}

/// Runs the tool called `name`.
///
/// `None` means this server has no such tool; an argument problem is a tool
/// result with `isError`, because the protocol error codes describe the
/// transport, not the arguments a model can fix.
pub fn call(context: &Context, name: &str, arguments: &Map<String, Value>) -> Option<ToolOutcome> {
    let outcome = match name {
        "session_start" => session_start(context, arguments),
        "step" => step(context, arguments),
        "snapshot" => snapshot(context, arguments),
        "select_ref" => select_ref(context, arguments),
        "locate" => locate(context, arguments),
        "judge" => judge(context, arguments),
        "extract" => extract(context, arguments),
        "act" => act(context, arguments),
        "export" => export(context, arguments),
        "run" => run(context, arguments),
        "session_stop" => session_stop(context, arguments),
        _ => return None,
    };
    Some(outcome)
}

/// Starts one session and reports the identifier to use from now on.
fn session_start(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(
        arguments,
        &[
            "model_hook",
            "model_hook_args",
            "model",
            "model_adapter",
            "model_endpoint",
            "model_api_key_env",
            "model_reasoning_effort",
        ],
    ) {
        return ToolOutcome::failed(refusal(message));
    }
    let resolved_models = match resolve_start_models(context, arguments, &START_MODEL_KEYS) {
        Ok(models) => models,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    from_response(session::handle(
        SessionRequest::Start {
            sessions_dir: context.sessions_dir.to_path_buf(),
            idle_timeout_ms: context.idle_timeout_ms,
            observe: Vec::new(),
            model_hook: resolved_models.hook,
            model: resolved_models.model,
            plugins: context.plugins.to_vec(),
        },
        context.registry,
    ))
}

/// Runs one step, reporting the step's own schema when its arguments are wrong.
fn step(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(arguments, &["session_id", "use", "with", "expect", "save"]) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let use_name = match text_argument(arguments, "use") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let Some(schema) = context
        .registry
        .get(&use_name)
        .map(|definition| definition.args_schema.clone())
    else {
        let mut payload = refusal(format!(
            "`{use_name}` is not a step this build registered; registered steps are {}",
            names_or_none(&context.registry.names())
        ));
        payload["available_steps"] = json!(context.registry.names());
        payload["hint"] = json!("Send one of `available_steps` as `use`.");
        return ToolOutcome::failed(payload);
    };

    // The step the model sends is parsed by the case parser, so a session step
    // and a case step cannot accept different shapes.
    let text = step_envelope(&use_name, arguments).to_string();
    if let Err(message) = case::parse_single_step(&text) {
        return ToolOutcome::failed(invalid_step(&session_id, &message, &use_name, &schema));
    }

    let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let response = session::handle(
        SessionRequest::Step {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id.clone(),
            base_directory,
            step: StepInput::Text(text),
        },
        context.registry,
    );

    let payload = response.payload;
    let invalid_input = payload
        .pointer("/step/error/kind")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == "invalid_input");
    if invalid_input {
        let message = payload
            .pointer("/step/error/message")
            .and_then(Value::as_str)
            .unwrap_or("the step arguments are invalid")
            .to_string();
        return ToolOutcome::failed(invalid_step(&session_id, &message, &use_name, &schema));
    }
    let is_error = payload.get("ok").and_then(Value::as_bool) != Some(true);
    ToolOutcome { payload, is_error }
}

/// Captures an accessibility snapshot of the session's browser.
fn snapshot(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(arguments, &["session_id", "ref", "max_depth", "max_nodes"]) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let scope_ref = match optional_text(arguments, "ref") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let max_depth = match optional_count(arguments, "max_depth") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let max_nodes = match optional_count(arguments, "max_nodes") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    from_response(session::handle(
        SessionRequest::Snapshot {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
            scope_ref,
            max_depth,
            max_nodes,
        },
        context.registry,
    ))
}

/// Invokes `model/select_ref` against the session's active snapshot candidates.
fn select_ref(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(
        arguments,
        &[
            "session_id",
            "query",
            "ref",
            "max_depth",
            "max_nodes",
            "hook",
            "hook_args",
            "model",
            "adapter",
            "endpoint",
            "api_key_env",
            "reasoning_effort",
            "timeout_ms",
            "act",
            "with",
        ],
    ) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let query = match text_argument(arguments, "query") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let scope_ref = match optional_text(arguments, "ref") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let max_depth = match optional_count(arguments, "max_depth") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let max_nodes = match optional_count(arguments, "max_nodes") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let resolved_models = match resolve_call_models(arguments, &CALL_MODEL_KEYS) {
        Ok(models) => models,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let timeout_ms = match optional_timeout_ms(arguments) {
        Ok(ms) => ms,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let act_name = match optional_text(arguments, "act") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let with_raw = match arguments.get("with") {
        None | Some(Value::Null) => None,
        Some(obj @ Value::Object(_)) => Some(obj.to_string()),
        Some(_) => return ToolOutcome::failed(refusal("`with` must be an object".to_string())),
    };
    if with_raw.is_some() && act_name.is_none() {
        return ToolOutcome::failed(refusal("`with` requires `act`".to_string()));
    }
    let act = act_name.map(|action| SelectRefAction {
        action,
        with_raw,
        args: Vec::new(),
        save: Vec::new(),
    });
    let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    from_response(session::handle(
        SessionRequest::SelectRef(SelectRefRequest {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
            base_directory,
            query,
            scope_ref,
            max_depth,
            max_nodes,
            hook_override: resolved_models.hook,
            model_override: resolved_models.model,
            reasoning_effort_override: resolved_models.reasoning_effort,
            timeout_ms,
            act,
        }),
        context.registry,
    ))
}

/// Captures an input-masked screenshot and invokes `model/locate`.
fn locate(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(
        arguments,
        &[
            "session_id",
            "query",
            "hook",
            "hook_args",
            "model",
            "adapter",
            "endpoint",
            "api_key_env",
            "reasoning_effort",
            "timeout_ms",
            "click",
        ],
    ) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let query = match text_argument(arguments, "query") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let resolved_models = match resolve_call_models(arguments, &CALL_MODEL_KEYS) {
        Ok(models) => models,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let timeout_ms = match optional_timeout_ms(arguments) {
        Ok(ms) => ms,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let click = match optional_flag(arguments, "click") {
        Ok(flag) => flag.unwrap_or(false),
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    from_response(session::handle(
        SessionRequest::Locate(LocateRequest {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
            base_directory,
            query,
            hook_override: resolved_models.hook,
            model_override: resolved_models.model,
            reasoning_effort_override: resolved_models.reasoning_effort,
            timeout_ms,
            click,
        }),
        context.registry,
    ))
}

/// Invokes `model/judge` over redacted session evidence and optional screenshot.
fn judge(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(
        arguments,
        &[
            "session_id",
            "rubric",
            "evidence",
            "screenshot",
            "hook",
            "hook_args",
            "model",
            "adapter",
            "endpoint",
            "api_key_env",
            "reasoning_effort",
            "timeout_ms",
        ],
    ) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let rubric = match text_argument(arguments, "rubric") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let evidence = arguments.get("evidence").filter(|v| !v.is_null()).cloned();
    let screenshot = match optional_flag(arguments, "screenshot") {
        Ok(flag) => flag.unwrap_or(false),
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let resolved_models = match resolve_call_models(arguments, &CALL_MODEL_KEYS) {
        Ok(models) => models,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let timeout_ms = match optional_timeout_ms(arguments) {
        Ok(ms) => ms,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    from_response(session::handle(
        SessionRequest::Judge(JudgeRequest {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
            base_directory,
            rubric,
            evidence,
            screenshot,
            hook_override: resolved_models.hook,
            model_override: resolved_models.model,
            reasoning_effort_override: resolved_models.reasoning_effort,
            timeout_ms,
        }),
        context.registry,
    ))
}

/// Invokes `model/extract` over redacted session evidence and optional schema/screenshot.
fn extract(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(
        arguments,
        &[
            "session_id",
            "query",
            "schema",
            "evidence",
            "screenshot",
            "save",
            "hook",
            "hook_args",
            "model",
            "adapter",
            "endpoint",
            "api_key_env",
            "reasoning_effort",
            "timeout_ms",
        ],
    ) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let query = match text_argument(arguments, "query") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let schema = match arguments.get("schema") {
        None | Some(Value::Null) => None,
        Some(val @ Value::Object(_)) => Some(val.clone()),
        Some(_) => {
            return ToolOutcome::failed(refusal(
                "`schema` must be a JSON Schema object".to_string(),
            ));
        }
    };
    let evidence = arguments.get("evidence").filter(|v| !v.is_null()).cloned();
    let screenshot = match optional_flag(arguments, "screenshot") {
        Ok(flag) => flag.unwrap_or(false),
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let save = match optional_text(arguments, "save") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let resolved_models = match resolve_call_models(arguments, &CALL_MODEL_KEYS) {
        Ok(models) => models,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let timeout_ms = match optional_timeout_ms(arguments) {
        Ok(ms) => ms,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    from_response(session::handle(
        SessionRequest::Extract(ExtractRequest {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
            base_directory,
            query,
            schema,
            evidence,
            screenshot,
            save,
            hook_override: resolved_models.hook,
            model_override: resolved_models.model,
            reasoning_effort_override: resolved_models.reasoning_effort,
            timeout_ms,
        }),
        context.registry,
    ))
}

/// Executes a bounded autonomous loop toward `goal` via `model/act_step`.
fn act(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(
        arguments,
        &[
            "session_id",
            "goal",
            "max_steps",
            "evidence",
            "screenshot",
            "hook",
            "hook_args",
            "model",
            "adapter",
            "endpoint",
            "api_key_env",
            "reasoning_effort",
            "timeout_ms",
        ],
    ) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let goal = match text_argument(arguments, "goal") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let max_steps = match optional_count(arguments, "max_steps") {
        Ok(Some(n)) if n <= MAX_ACT_MAX_STEPS => n,
        Ok(Some(_)) => {
            return ToolOutcome::failed(refusal(format!(
                "`max_steps` must be between 1 and {MAX_ACT_MAX_STEPS}"
            )));
        }
        Ok(None) => DEFAULT_ACT_MAX_STEPS,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let evidence = arguments.get("evidence").filter(|v| !v.is_null()).cloned();
    let screenshot = match optional_flag(arguments, "screenshot") {
        Ok(flag) => flag.unwrap_or(false),
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let resolved_models = match resolve_call_models(arguments, &CALL_MODEL_KEYS) {
        Ok(models) => models,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let timeout_ms = match optional_timeout_ms(arguments) {
        Ok(ms) => ms,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let base_directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    from_response(session::handle(
        SessionRequest::Act(ActRequest {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
            base_directory,
            goal,
            max_steps,
            evidence,
            screenshot,
            hook_override: resolved_models.hook,
            model_override: resolved_models.model,
            reasoning_effort_override: resolved_models.reasoning_effort,
            timeout_ms,
        }),
        context.registry,
    ))
}

/// Exports the session's passed steps as a replayable case.
fn export(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(arguments, &["session_id", "name", "include_reads", "out"]) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let case_name = match optional_text(arguments, "name") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let include_reads = match optional_flag(arguments, "include_reads") {
        Ok(value) => value.unwrap_or(false),
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let out_path = match optional_text(arguments, "out") {
        Ok(value) => value.map(PathBuf::from),
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    from_response(session::handle(
        SessionRequest::Export {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
            options: ExportOptions {
                case_name,
                include_reads,
            },
            out_path,
        },
        context.registry,
    ))
}

/// Runs case files or directories and returns the JSON report.
fn run(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(arguments, &["paths", "vars", "jobs"]) {
        return ToolOutcome::failed(refusal(message));
    }
    let paths = match paths_argument(arguments) {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let overrides = match vars_argument(arguments) {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    let jobs = match optional_count(arguments, "jobs") {
        Ok(Some(jobs)) => jobs,
        Ok(None) => 1,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };

    let mut secrets = Secrets::new();
    let (cases, errors) = load_cases(&paths, &overrides, &mut secrets);
    let outcome = if errors.is_empty() {
        let bus = EventBus::new();
        let filters = Filters::default();
        crate::core::runner::run_with_record(
            &cases,
            context.registry,
            &bus,
            None,
            None,
            &filters,
            &mut secrets,
            jobs,
        )
    } else {
        RunOutcome::Rejected { errors }
    };
    let is_error = !outcome.is_ok();
    let rendered = output::render(&outcome, Format::Json, &secrets);
    let payload = serde_json::from_str::<Value>(rendered.stdout.trim())
        .unwrap_or_else(|_| json!({"ok": !is_error, "output": rendered.stdout.trim().to_string()}));
    ToolOutcome { payload, is_error }
}

/// Closes the session's resources and removes its state directory.
fn session_stop(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(arguments, &["session_id"]) {
        return ToolOutcome::failed(refusal(message));
    }
    let session_id = match text_argument(arguments, "session_id") {
        Ok(value) => value,
        Err(message) => return ToolOutcome::failed(refusal(message)),
    };
    from_response(session::handle(
        SessionRequest::Stop {
            sessions_dir: context.sessions_dir.to_path_buf(),
            id: session_id,
        },
        context.registry,
    ))
}

/// Reports a session response, which already carries `ok` and a redacted payload.
fn from_response(response: session::SessionResponse) -> ToolOutcome {
    let is_error = response.payload.get("ok").and_then(Value::as_bool) != Some(true);
    ToolOutcome {
        payload: response.payload,
        is_error,
    }
}

/// The step envelope the case parser reads: `use`, `with`, `expect`, `save`.
fn step_envelope(use_name: &str, arguments: &Map<String, Value>) -> Value {
    let mut envelope = Map::new();
    envelope.insert("use".to_string(), Value::String(use_name.to_string()));
    for key in ["with", "expect", "save"] {
        if let Some(value) = arguments.get(key) {
            envelope.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(envelope)
}

/// The error result of a step whose arguments the schema refuses.
///
/// The schema travels with the message because the caller is usually a model:
/// it cannot read the case format, but it can follow the schema it is handed.
fn invalid_step(session_id: &str, message: &str, use_name: &str, schema: &Value) -> Value {
    json!({
        "ok": false,
        "session_id": session_id,
        "error": {"kind": "invalid_input", "message": message},
        "arguments_schema": schema,
        "hint": format!(
            "`with` must satisfy `arguments_schema` for `{use_name}`; fix the arguments and call `step` again with the same `session_id`."
        ),
    })
}

/// The payload of a tool call refused before it reached the engine.
fn refusal(message: String) -> Value {
    json!({"ok": false, "error": {"kind": "invalid_arguments", "message": message}})
}

/// Refuses an argument the tool does not accept.
fn only(arguments: &Map<String, Value>, accepted: &[&str]) -> Result<(), String> {
    let Some(unknown) = arguments
        .keys()
        .find(|key| !accepted.contains(&key.as_str()))
    else {
        return Ok(());
    };
    Err(format!(
        "`{unknown}` is not an argument of this tool; the accepted arguments are {}",
        names_or_none(accepted)
    ))
}

/// A required non-empty text argument.
fn text_argument(arguments: &Map<String, Value>, name: &str) -> Result<String, String> {
    match arguments.get(name) {
        None => Err(format!("`{name}` is required")),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.clone()),
        Some(Value::String(_)) => Err(format!("`{name}` must not be empty")),
        Some(_) => Err(format!("`{name}` must be a string")),
    }
}

/// An optional non-empty text argument.
fn optional_text(arguments: &Map<String, Value>, name: &str) -> Result<Option<String>, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.clone())),
        Some(Value::String(_)) => Err(format!("`{name}` must not be empty")),
        Some(_) => Err(format!("`{name}` must be a string")),
    }
}

/// An optional count argument of at least one.
fn optional_count(arguments: &Map<String, Value>, name: &str) -> Result<Option<usize>, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_u64() {
            Some(count) if count >= 1 => usize::try_from(count)
                .map(Some)
                .map_err(|_| format!("`{name}` is too large for this platform")),
            Some(_) => Err(format!("`{name}` must be at least 1")),
            None => Err(format!("`{name}` must be a whole number")),
        },
    }
}

/// An optional boolean argument.
fn optional_flag(arguments: &Map<String, Value>, name: &str) -> Result<Option<bool>, String> {
    match arguments.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(format!("`{name}` must be `true` or `false`")),
    }
}

fn optional_timeout_ms(arguments: &Map<String, Value>) -> Result<u64, String> {
    match arguments.get("timeout_ms") {
        None | Some(Value::Null) => Ok(DEFAULT_HOOK_TIMEOUT_MS),
        Some(value) => match value.as_u64() {
            Some(ms) if ms >= 1 => Ok(ms),
            Some(_) => Err("`timeout_ms` must be at least 1".to_string()),
            None => Err("`timeout_ms` must be a whole number".to_string()),
        },
    }
}

struct ModelOptionKeys<'a> {
    hook: &'a str,
    hook_args: &'a str,
    model: &'a str,
    adapter: &'a str,
    endpoint: &'a str,
    api_key_env: &'a str,
    reasoning_effort: &'a str,
}

const START_MODEL_KEYS: ModelOptionKeys<'static> = ModelOptionKeys {
    hook: "model_hook",
    hook_args: "model_hook_args",
    model: "model",
    adapter: "model_adapter",
    endpoint: "model_endpoint",
    api_key_env: "model_api_key_env",
    reasoning_effort: "model_reasoning_effort",
};

const CALL_MODEL_KEYS: ModelOptionKeys<'static> = ModelOptionKeys {
    hook: "hook",
    hook_args: "hook_args",
    model: "model",
    adapter: "adapter",
    endpoint: "endpoint",
    api_key_env: "api_key_env",
    reasoning_effort: "reasoning_effort",
};

fn optional_hook_spec(
    arguments: &Map<String, Value>,
    cmd_key: &str,
    args_key: &str,
) -> Result<Option<ModelHookSpec>, String> {
    let command = optional_text(arguments, cmd_key)?;
    let args = match arguments.get(args_key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(format!("every entry of `{args_key}` must be a string"));
                };
                out.push(s.to_string());
            }
            out
        }
        Some(_) => return Err(format!("`{args_key}` must be an array of strings")),
    };
    match command {
        Some(command) => Ok(Some(ModelHookSpec { command, args })),
        None if args.is_empty() => Ok(None),
        None => Err(format!("`{args_key}` requires `{cmd_key}`")),
    }
}

fn optional_model_spec(
    arguments: &Map<String, Value>,
    keys: &ModelOptionKeys<'_>,
    require_model_for_effort: bool,
) -> Result<(Option<BuiltinModelSpec>, Option<String>), String> {
    let model = optional_text(arguments, keys.model)?;
    let adapter = optional_text(arguments, keys.adapter)?;
    let endpoint = optional_text(arguments, keys.endpoint)?;
    let api_key_env = optional_text(arguments, keys.api_key_env)?;
    let reasoning_effort = match optional_text(arguments, keys.reasoning_effort)? {
        Some(raw) => Some(validate_reasoning_effort(&raw)?),
        None => None,
    };
    match model {
        Some(model) => Ok((
            Some(BuiltinModelSpec {
                model,
                adapter,
                endpoint,
                api_key_env,
                reasoning_effort,
            }),
            None,
        )),
        None => {
            for (present, name) in [
                (adapter.is_some(), keys.adapter),
                (endpoint.is_some(), keys.endpoint),
                (api_key_env.is_some(), keys.api_key_env),
                (
                    require_model_for_effort && reasoning_effort.is_some(),
                    keys.reasoning_effort,
                ),
            ] {
                if present {
                    return Err(format!("`{name}` requires `{}`", keys.model));
                }
            }
            Ok((None, reasoning_effort))
        }
    }
}

#[derive(Debug)]
struct ResolvedToolModels {
    hook: Option<ModelHookSpec>,
    model: Option<BuiltinModelSpec>,
    reasoning_effort: Option<String>,
}

/// Parses per-call model/hook/reasoning-effort overrides without substituting
/// server defaults, so session-level `model` / `model_hook` persisted at
/// `session_start` remain authoritative unless explicitly overridden on the
/// call.
fn resolve_call_models(
    arguments: &Map<String, Value>,
    keys: &ModelOptionKeys<'_>,
) -> Result<ResolvedToolModels, String> {
    let hook = optional_hook_spec(arguments, keys.hook, keys.hook_args)?;
    let (model, reasoning_effort_override) = optional_model_spec(arguments, keys, false)?;
    if hook.is_some() && model.is_some() {
        return Err(format!(
            "pass either `{}` or `{}`, not both",
            keys.hook, keys.model
        ));
    }
    if hook.is_some() && reasoning_effort_override.is_some() {
        return Err(format!(
            "`{}` applies only to built-in `{}`, not `{}`",
            keys.reasoning_effort, keys.model, keys.hook
        ));
    }
    Ok(ResolvedToolModels {
        hook,
        model,
        reasoning_effort: reasoning_effort_override,
    })
}

/// Resolves session-start model/hook config, falling back to the MCP server's
/// startup defaults only when the caller omits both `model_hook` and `model`.
fn resolve_start_models(
    context: &Context,
    arguments: &Map<String, Value>,
    keys: &ModelOptionKeys<'_>,
) -> Result<ResolvedToolModels, String> {
    let hook = optional_hook_spec(arguments, keys.hook, keys.hook_args)?;
    let (model, _) = optional_model_spec(arguments, keys, true)?;
    if hook.is_some() && model.is_some() {
        return Err(format!(
            "pass either `{}` or `{}`, not both",
            keys.hook, keys.model
        ));
    }
    if hook.is_some() || model.is_some() {
        Ok(ResolvedToolModels {
            hook,
            model,
            reasoning_effort: None,
        })
    } else {
        Ok(ResolvedToolModels {
            hook: context.model_hook.cloned(),
            model: context.model.cloned(),
            reasoning_effort: None,
        })
    }
}

/// The `paths` argument of `run`.
fn paths_argument(arguments: &Map<String, Value>) -> Result<Vec<PathBuf>, String> {
    let Some(value) = arguments.get("paths") else {
        return Err("`paths` is required: pass the case files or directories to run".to_string());
    };
    let Some(items) = value.as_array() else {
        return Err("`paths` must be an array of case files or directories".to_string());
    };
    if items.is_empty() {
        return Err("`paths` must name at least one case file or directory".to_string());
    }
    let mut paths = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err("every entry of `paths` must be a string".to_string());
        };
        if text.trim().is_empty() {
            return Err("an entry of `paths` is empty".to_string());
        }
        paths.push(PathBuf::from(text));
    }
    Ok(paths)
}

/// The `vars` argument of `run`, as `--var NAME=VALUE` pairs.
fn vars_argument(arguments: &Map<String, Value>) -> Result<Vec<(String, String)>, String> {
    let Some(value) = arguments.get("vars") else {
        return Ok(Vec::new());
    };
    let Some(entries) = value.as_object() else {
        return Err("`vars` must be an object that maps names to values".to_string());
    };
    let mut pairs = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            Value::Null | Value::Array(_) | Value::Object(_) => {
                return Err(format!(
                    "variable `{name}` must be a string, number or boolean"
                ));
            }
        };
        pairs.push((name.clone(), text));
    }
    Ok(pairs)
}

/// Discovers, loads and parses the cases of a `run` call.
///
/// It mirrors what `da run` does so the two cannot disagree about which cases
/// are valid: every file that fails to parse becomes an error and the rest
/// still load, which is what `da lint` reports the same way.
fn load_cases(
    paths: &[PathBuf],
    overrides: &[(String, String)],
    secrets: &mut Secrets,
) -> (Vec<LoadedCase>, Vec<CaseError>) {
    let files = match case::discover(paths) {
        Ok(files) => files,
        Err(error) => return (Vec::new(), vec![error]),
    };
    if files.is_empty() {
        return (
            Vec::new(),
            vec![CaseError::invalid(
                paths.first().cloned().unwrap_or_default(),
                "no `.yaml` or `.yml` case files found; pass a case file or a directory that contains one",
            )],
        );
    }

    let mut expanded = Vec::with_capacity(overrides.len());
    let mut overridden_names = std::collections::BTreeSet::new();
    let mut errors = Vec::new();
    for (name, value) in overrides {
        overridden_names.insert(name.clone());
        match case::expand_env(value, secrets) {
            Ok(value) => expanded.push((name.clone(), value)),
            Err(message) => errors.push(CaseError::invalid(
                PathBuf::from("<vars>"),
                format!("variable `{name}`: {message}"),
            )),
        }
    }
    let overridden_vars: Vec<String> = overridden_names.into_iter().collect();

    let mut cases = Vec::with_capacity(files.len());
    for file in &files {
        match case::load_file(file) {
            Ok(mut loaded) => {
                if let Err(message) = case::expand_vars(&mut loaded.case.vars, secrets) {
                    errors.push(CaseError::invalid(loaded.path.clone(), message));
                    continue;
                }
                // A case that does not use the name ignores it.
                for (name, value) in &expanded {
                    loaded.case.vars.insert(name.clone(), value.clone());
                }
                loaded.overridden_vars = overridden_vars.clone();
                cases.push(loaded);
            }
            Err(error) => errors.push(error),
        }
    }
    (cases, errors)
}

/// Joins names for a message, or says there are none.
fn names_or_none(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::session::Sessions;

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry
            .register(crate::core::registry::StepDefinition {
                name: "test.echo",
                description: "Returns its arguments.",
                args_schema: json!({
                    "$schema": "dialect-placeholder",
                    "title": "Arguments",
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["value"],
                    "properties": {"value": {"type": "string"}}
                }),
                validate: |args| match args.get("value").and_then(Value::as_str) {
                    Some(_) => Ok(()),
                    None => Err("invalid arguments: `value` is required".to_string()),
                },
                check_retry: crate::core::registry::disallow_retry,
                lint_warnings: crate::core::registry::no_lint_warnings,
                run: |_, _: &mut Sessions| Ok(json!({"echo": "ok"})),
            })
            .expect("the test step must register");
        registry
    }

    fn context(registry: &Registry) -> Context<'_> {
        Context {
            registry,
            sessions_dir: Path::new(".da/sessions"),
            idle_timeout_ms: 60_000,
            model_hook: None,
            model: None,
            plugins: &[],
        }
    }

    #[test]
    fn the_tool_list_is_fixed_and_complete() {
        let definitions = definitions();
        let names: Vec<&str> = definitions
            .iter()
            .filter_map(|definition| definition["name"].as_str())
            .collect();

        assert_eq!(names, NAMES);
        for definition in &definitions {
            assert!(
                definition["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "every tool needs a description: {definition}"
            );
            assert_eq!(definition["inputSchema"]["type"], "object");
        }

        let with_hooks = definitions_for(true);
        let hook_names: Vec<&str> = with_hooks
            .iter()
            .filter_map(|definition| definition["name"].as_str())
            .collect();
        assert_eq!(hook_names, ALL_NAMES);
        for definition in &with_hooks {
            assert!(
                definition["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty()),
                "every tool needs a description: {definition}"
            );
            assert_eq!(definition["inputSchema"]["type"], "object");
        }
    }

    #[test]
    fn an_unknown_tool_is_not_routed() {
        let registry = registry();
        let context = context(&registry);

        assert!(call(&context, "session_reset", &Map::new()).is_none());
    }

    #[test]
    fn every_tool_refuses_an_argument_it_does_not_accept() {
        let registry = registry();
        let context = context(&registry);

        for name in ALL_NAMES {
            let mut arguments = Map::new();
            arguments.insert("surprise".to_string(), json!(1));
            let outcome = call(&context, name, &arguments).expect("the tool must be routed");

            assert!(outcome.is_error, "{name} must refuse an unknown argument");
            assert_eq!(outcome.payload["error"]["kind"], "invalid_arguments");
            assert!(
                outcome.payload["error"]["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("surprise")),
                "{name}: {}",
                outcome.payload
            );
        }
    }

    #[test]
    fn a_step_without_a_registered_use_lists_the_steps_to_choose_from() {
        let registry = registry();
        let context = context(&registry);
        let mut arguments = Map::new();
        arguments.insert("session_id".to_string(), json!("s-1"));
        arguments.insert("use".to_string(), json!("test.absent"));

        let outcome = call(&context, "step", &arguments).expect("the tool must be routed");

        assert!(outcome.is_error);
        assert_eq!(
            outcome.payload["available_steps"],
            json!(["test.echo"]),
            "{}",
            outcome.payload
        );
        assert!(
            outcome.payload["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("`use`")),
            "{}",
            outcome.payload
        );
    }

    #[test]
    fn a_step_the_case_parser_refuses_carries_the_step_schema_back() {
        let registry = registry();
        let context = context(&registry);
        let mut arguments = Map::new();
        arguments.insert("session_id".to_string(), json!("s-1"));
        arguments.insert("use".to_string(), json!("test.echo"));
        // `expect` is not a list, so the case parser refuses the step before a
        // session is touched; the schema has to come back anyway.
        arguments.insert("expect".to_string(), json!("not a list"));

        let outcome = call(&context, "step", &arguments).expect("the tool must be routed");

        assert!(outcome.is_error, "{}", outcome.payload);
        assert_eq!(outcome.payload["error"]["kind"], "invalid_input");
        assert_eq!(
            outcome.payload["arguments_schema"]["properties"]["value"]["type"], "string",
            "{}",
            outcome.payload
        );
        assert!(
            outcome.payload["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("`with`")),
            "{}",
            outcome.payload
        );
    }

    #[test]
    fn the_run_tool_refuses_a_paths_argument_that_is_not_a_list() {
        let registry = registry();
        let context = context(&registry);
        let mut arguments = Map::new();
        arguments.insert("paths".to_string(), json!("cases/"));

        let outcome = call(&context, "run", &arguments).expect("the tool must be routed");

        assert!(outcome.is_error);
        assert!(
            outcome.payload["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("array")),
            "{}",
            outcome.payload
        );
    }

    #[test]
    fn a_run_with_no_matching_file_reports_every_problem() {
        let registry = registry();
        let context = context(&registry);
        let mut arguments = Map::new();
        arguments.insert("paths".to_string(), json!(["definitely-absent.yaml"]));

        let outcome = call(&context, "run", &arguments).expect("the tool must be routed");

        assert!(outcome.is_error);
        assert_eq!(outcome.payload["ok"], false);
        assert!(
            outcome.payload["errors"]
                .as_array()
                .is_some_and(|errors| !errors.is_empty()),
            "{}",
            outcome.payload
        );
    }

    #[test]
    fn variables_are_read_as_text_and_refuse_containers() {
        let mut arguments = Map::new();
        arguments.insert(
            "vars".to_string(),
            json!({"host": "127.0.0.1", "port": 8080, "secure": false}),
        );
        let pairs = vars_argument(&arguments).expect("scalars are valid variables");
        assert_eq!(
            pairs,
            vec![
                ("host".to_string(), "127.0.0.1".to_string()),
                ("port".to_string(), "8080".to_string()),
                ("secure".to_string(), "false".to_string()),
            ]
        );

        arguments.insert("vars".to_string(), json!({"list": [1, 2]}));
        let error = vars_argument(&arguments).expect_err("a list is not a variable");
        assert!(error.contains("list"), "got: {error}");
    }

    #[test]
    fn a_count_argument_must_be_a_whole_number_of_at_least_one() {
        let mut arguments = Map::new();
        assert_eq!(optional_count(&arguments, "jobs"), Ok(None));

        arguments.insert("jobs".to_string(), json!(2));
        assert_eq!(optional_count(&arguments, "jobs"), Ok(Some(2)));

        arguments.insert("jobs".to_string(), json!(0));
        assert!(optional_count(&arguments, "jobs").is_err());

        arguments.insert("jobs".to_string(), json!(1.5));
        assert!(optional_count(&arguments, "jobs").is_err());

        arguments.insert("jobs".to_string(), json!("three"));
        assert!(optional_count(&arguments, "jobs").is_err());
    }

    #[test]
    fn model_and_hook_overrides_are_mutually_exclusive_and_preserve_session_precedence() {
        let registry = registry();
        let default_model = BuiltinModelSpec {
            model: "openai::gpt-4o-mini".to_string(),
            adapter: None,
            endpoint: None,
            api_key_env: None,
            reasoning_effort: None,
        };
        let mut ctx = context(&registry);
        ctx.model = Some(&default_model);

        let mut both = Map::new();
        both.insert("hook".to_string(), json!("./hook"));
        both.insert("model".to_string(), json!("deepseek::deepseek-chat"));
        let err = resolve_call_models(&both, &CALL_MODEL_KEYS)
            .expect_err("hook and model together must fail");
        assert!(err.contains("not both"), "got: {err}");

        // Omitting per-call args must leave overrides `None` so session-level
        // `model` / `model_hook` persisted by `session_start` are not shadowed.
        let empty = Map::new();
        let resolved_empty =
            resolve_call_models(&empty, &CALL_MODEL_KEYS).expect("empty call args must not fail");
        assert!(resolved_empty.hook.is_none());
        assert!(resolved_empty.model.is_none());
        assert!(resolved_empty.reasoning_effort.is_none());

        // `session_start` inherits server defaults when both args are omitted,
        // and overrides server defaults when the caller specifies `model_hook`.
        let start_default = resolve_start_models(&ctx, &empty, &START_MODEL_KEYS)
            .expect("empty session_start inherits server default");
        assert!(start_default.hook.is_none());
        assert_eq!(
            start_default.model.as_ref().map(|m| m.model.as_str()),
            Some("openai::gpt-4o-mini")
        );

        let mut start_hook_only = Map::new();
        start_hook_only.insert("model_hook".to_string(), json!("./hook"));
        let start_hook = resolve_start_models(&ctx, &start_hook_only, &START_MODEL_KEYS)
            .expect("session_start hook suppresses server default model");
        assert!(start_hook.hook.is_some());
        assert!(start_hook.model.is_none());

        let mut call_hook_only = Map::new();
        call_hook_only.insert("hook".to_string(), json!("./hook"));
        let resolved_hook = resolve_call_models(&call_hook_only, &CALL_MODEL_KEYS)
            .expect("per-call hook override succeeds");
        assert!(resolved_hook.hook.is_some());
        assert!(resolved_hook.model.is_none());
        assert!(resolved_hook.reasoning_effort.is_none());

        // Passing `reasoning_effort` alone must pass through as a per-call
        // override without substituting the server default model, so it applies
        // to the session's own persisted model.
        let mut effort_only = Map::new();
        effort_only.insert("reasoning_effort".to_string(), json!("low"));
        let resolved_effort = resolve_call_models(&effort_only, &CALL_MODEL_KEYS)
            .expect("effort alone passes through to session model");
        assert!(resolved_effort.hook.is_none());
        assert!(resolved_effort.model.is_none());
        assert_eq!(resolved_effort.reasoning_effort.as_deref(), Some("low"));

        let mut orphan_endpoint = Map::new();
        orphan_endpoint.insert("endpoint".to_string(), json!("http://127.0.0.1:8000/v1"));
        let err = resolve_call_models(&orphan_endpoint, &CALL_MODEL_KEYS)
            .expect_err("endpoint without model must fail");
        assert!(err.contains("requires `model`"), "got: {err}");
    }
}
