//! The six tools `da mcp` exposes and how each one reaches the session engine.
//!
//! The list is fixed on purpose: one tool per registered step would grow with
//! every driver and would differ between builds, which spends a model's context
//! on tool descriptions it cannot use. Instead `step` takes the same
//! `use`/`with`/`expect`/`save` shape a case file holds and validates it with
//! the case parser, so a new driver needs no protocol change.

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
use crate::session::{self, SessionRequest, StepInput};

/// Names of the tools, in the order `tools/list` reports them.
pub const NAMES: [&str; 6] = [
    "session_start",
    "step",
    "snapshot",
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

/// Describes the six tools for `tools/list`.
///
/// The schemas are written by hand because they describe this protocol, not a
/// step: a step's own arguments schema comes from the registry and is echoed
/// back when a `step` call is refused.
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "session_start",
            "description": "Starts an interactive session and returns its `session_id`. The session keeps live resources such as a browser open across calls and stores its state under `.da/sessions`.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {},
            },
        }),
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
        }),
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
        }),
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
        }),
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
        }),
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
        }),
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
        "export" => export(context, arguments),
        "run" => run(context, arguments),
        "session_stop" => session_stop(context, arguments),
        _ => return None,
    };
    Some(outcome)
}

/// Starts one session and reports the identifier to use from now on.
fn session_start(context: &Context, arguments: &Map<String, Value>) -> ToolOutcome {
    if let Err(message) = only(arguments, &[]) {
        return ToolOutcome::failed(refusal(message));
    }
    from_response(session::handle(
        SessionRequest::Start {
            sessions_dir: context.sessions_dir.to_path_buf(),
            idle_timeout_ms: context.idle_timeout_ms,
            observe: Vec::new(),
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
            vec![CaseError {
                file: paths.first().cloned().unwrap_or_default(),
                message: "no `.yaml` or `.yml` case files found; pass a case file or a directory that contains one".to_string(),
            }],
        );
    }

    let mut expanded = Vec::with_capacity(overrides.len());
    let mut overridden_names = std::collections::BTreeSet::new();
    let mut errors = Vec::new();
    for (name, value) in overrides {
        overridden_names.insert(name.clone());
        match case::expand_env(value, secrets) {
            Ok(value) => expanded.push((name.clone(), value)),
            Err(message) => errors.push(CaseError {
                file: PathBuf::from("<vars>"),
                message: format!("variable `{name}`: {message}"),
            }),
        }
    }
    let overridden_vars: Vec<String> = overridden_names.into_iter().collect();

    let mut cases = Vec::with_capacity(files.len());
    for file in &files {
        match case::load_file(file) {
            Ok(mut loaded) => {
                if let Err(message) = case::expand_vars(&mut loaded.case.vars, secrets) {
                    errors.push(CaseError {
                        file: loaded.path.clone(),
                        message,
                    });
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

        for name in NAMES {
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
}
