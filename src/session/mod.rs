//! Transport-neutral interactive session engine (`da session`).
//!
//! Commands arrive as [`SessionRequest`] and return [`SessionResponse`], so the
//! CLI, future stdio JSON-lines mode, and future `da mcp` share one handler.
//! Session state, step logs and saved variables live under `.da/sessions/<id>/`
//! without ever persisting resolved environment secrets.

pub mod export;
pub mod store;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use serde_saphyr::Location;

use crate::core::case::{self, Observation, SavedValue, Step};
use crate::core::observer::ObserverError;
use crate::core::output;
use crate::core::paths;
use crate::core::registry::Registry;
use crate::core::report::{ProblemKind, StepReport, StepStatus};
use crate::core::runner;
use crate::core::secrets::Secrets;
use crate::core::session::Sessions;
use crate::exit::ExitCode;

use self::export::ExportOptions;
use self::store::{
    LockError, SessionLock, SessionState, StepRecord, StoredExpectation, StoredObservation,
};

/// Default directory relative to the working directory for session storage.
pub const DEFAULT_SESSIONS_DIR: &str = ".da/sessions";

/// Default idle timeout before `start` or `list` reaps a session (30 minutes).
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 1_800_000;

/// How a caller supplies one step to [`SessionRequest::Step`].
#[derive(Debug, Clone)]
pub enum StepInput {
    /// Raw YAML or JSON text in either `<action>: {...}` or `{use, with, expect, save}` form.
    Text(String),
    /// Structured action name, optional raw YAML/JSON `with`, `--arg` pairs and `--save` pairs.
    Flags {
        action: String,
        with_raw: Option<String>,
        args: Vec<(String, String)>,
        save: Vec<(String, String)>,
    },
}

/// A transport-neutral session command request.
#[derive(Debug, Clone)]
pub enum SessionRequest {
    Start {
        sessions_dir: PathBuf,
        idle_timeout_ms: u64,
        observe: Vec<StoredObservation>,
    },
    Step {
        sessions_dir: PathBuf,
        id: String,
        base_directory: PathBuf,
        step: StepInput,
    },
    Snapshot {
        sessions_dir: PathBuf,
        id: String,
        scope_ref: Option<String>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    },
    Export {
        sessions_dir: PathBuf,
        id: String,
        options: ExportOptions,
        out_path: Option<PathBuf>,
    },
    Stop {
        sessions_dir: PathBuf,
        id: String,
    },
    List {
        sessions_dir: PathBuf,
        idle_override_ms: Option<u64>,
    },
}

/// A transport-neutral session command response.
#[derive(Debug, Clone)]
pub struct SessionResponse {
    pub exit_code: ExitCode,
    pub payload: Value,
    /// Optional raw YAML text when exporting with `--yaml` / text mode.
    pub raw_yaml: Option<String>,
}

/// Executes one [`SessionRequest`] against `registry`.
pub fn handle(request: SessionRequest, registry: &Registry) -> SessionResponse {
    match request {
        SessionRequest::Start {
            sessions_dir,
            idle_timeout_ms,
            observe,
        } => handle_start(&sessions_dir, idle_timeout_ms, observe, registry),
        SessionRequest::Step {
            sessions_dir,
            id,
            base_directory,
            step,
        } => handle_step(&sessions_dir, &id, &base_directory, step, registry),
        SessionRequest::Snapshot {
            sessions_dir,
            id,
            scope_ref,
            max_depth,
            max_nodes,
        } => handle_snapshot(
            &sessions_dir,
            &id,
            scope_ref.as_deref(),
            max_depth,
            max_nodes,
            registry,
        ),
        SessionRequest::Export {
            sessions_dir,
            id,
            options,
            out_path,
        } => handle_export(&sessions_dir, &id, &options, out_path.as_deref()),
        SessionRequest::Stop { sessions_dir, id } => handle_stop(&sessions_dir, &id, registry),
        SessionRequest::List {
            sessions_dir,
            idle_override_ms,
        } => handle_list(&sessions_dir, idle_override_ms, registry),
    }
}

fn error_response(code: ExitCode, kind: &str, message: impl Into<String>) -> SessionResponse {
    SessionResponse {
        exit_code: code,
        payload: json!({
            "ok": false,
            "error": {
                "kind": kind,
                "message": message.into(),
            }
        }),
        raw_yaml: None,
    }
}

fn handle_start(
    sessions_dir: &Path,
    idle_timeout_ms: u64,
    observe: Vec<StoredObservation>,
    registry: &Registry,
) -> SessionResponse {
    let (observer_names, env_names) = match validate_start_observations(&observe, registry) {
        Ok(validated) => validated,
        Err(response) => return response,
    };

    let list_report = match store::list_and_reap(sessions_dir, None, registry) {
        Ok(result) => result,
        Err(message) => return error_response(ExitCode::Environment, "environment", message),
    };

    let id = store::generate_session_id();
    let session_dir = sessions_dir.join(&id);
    if let Err(error) = fs::create_dir_all(&session_dir) {
        return error_response(
            ExitCode::Environment,
            "environment",
            format!(
                "could not create session directory `{}`: {error}",
                paths::display(&session_dir)
            ),
        );
    }

    let now = store::now_ms();
    let state = SessionState {
        schema_version: 1,
        id: id.clone(),
        created_at_ms: now,
        last_active_ms: now,
        idle_timeout_ms,
        handles: BTreeMap::new(),
        vars: BTreeMap::new(),
        env_names,
        observe,
    };
    let secrets = Secrets::new();
    if let Err(message) = store::save_state(&session_dir, &state, &secrets) {
        let _ = store::remove_session_dir(&session_dir);
        return error_response(ExitCode::Environment, "environment", message);
    }

    SessionResponse {
        exit_code: ExitCode::Success,
        payload: json!({
            "ok": true,
            "session_id": id,
            "dir": paths::display(&session_dir),
            "idle_timeout_ms": idle_timeout_ms,
            "observers": observer_names,
            "reaped": list_report.reaped,
            "reap_errors": list_report.errors,
        }),
        raw_yaml: None,
    }
}

fn acquire_existing(
    sessions_dir: &Path,
    id: &str,
) -> Result<(PathBuf, SessionLock, SessionState), SessionResponse> {
    if let Err(message) = store::validate_session_id(id) {
        return Err(error_response(ExitCode::Usage, "usage", message));
    }
    let session_dir = sessions_dir.join(id);
    if !session_dir.join(store::STATE_FILE).exists() {
        return Err(error_response(
            ExitCode::Environment,
            "environment",
            format!(
                "session `{id}` does not exist or has already stopped; start one with `da session start`"
            ),
        ));
    }
    let lock = match SessionLock::try_acquire(&session_dir, id) {
        Ok(lock) => lock,
        Err(LockError::Busy(message)) | Err(LockError::Io(message)) => {
            return Err(error_response(
                ExitCode::Environment,
                "environment",
                message,
            ));
        }
    };
    let state = match store::load_state(&session_dir) {
        Ok(state) => state,
        Err(message) => {
            return Err(error_response(
                ExitCode::Environment,
                "environment",
                message,
            ));
        }
    };
    Ok((session_dir, lock, state))
}

fn reattach_sessions(
    state: &SessionState,
    base_directory: &Path,
    registry: &Registry,
) -> Result<Sessions, String> {
    let mut sessions = Sessions::new();
    sessions.set_interactive(true);
    sessions.set_base_directory(base_directory);
    for (kind, handle) in &state.handles {
        let Some(def) = registry.session(kind) else {
            return Err(format!(
                "session `{}` requires driver `{kind}`, which is not enabled in this build",
                state.id
            ));
        };
        let session = (def.reattach)(handle).map_err(|err| {
            format!(
                "session `{}` has expired or could not reconnect to `{kind}`: {err}",
                state.id
            )
        })?;
        sessions.insert_reattached(def.kind, session)?;
    }
    Ok(sessions)
}

fn handle_step(
    sessions_dir: &Path,
    id: &str,
    base_directory: &Path,
    step_input: StepInput,
    registry: &Registry,
) -> SessionResponse {
    let (session_dir, _lock, mut state) = match acquire_existing(sessions_dir, id) {
        Ok(found) => found,
        Err(resp) => return resp,
    };

    let step = match parse_step_input(step_input, registry) {
        Ok(step) => step,
        Err(message) => {
            return error_response(ExitCode::InvalidCase, "invalid_input", message);
        }
    };

    state.env_names.extend(case::env_references(&step.args));
    let mut secrets = store::secrets_from_env_names(&state.env_names);

    let mut sessions = match reattach_sessions(&state, base_directory, registry) {
        Ok(sessions) => sessions,
        Err(message) => {
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let started_at_ms = store::now_ms();
    let observations: Vec<Observation> = state
        .observe
        .iter()
        .map(|obs| Observation {
            name: obs.name.clone(),
            kind: obs.kind.clone(),
            args: obs.raw_args.clone(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
        })
        .collect();
    let report = runner::run_single_step_with_observations(
        &step,
        &observations,
        registry,
        &mut state.vars,
        &mut secrets,
        &mut sessions,
    );
    let step_meta = sessions.take_step_metadata();

    let synthesis = extract_synthesis(&step, &step_meta);

    let mut snapshot_error: Option<String> = None;
    let snapshot = if !matches!(
        report.error.as_ref().map(|problem| problem.kind),
        Some(ProblemKind::Environment)
    ) {
        match sessions.collect_step_snapshot() {
            Ok(Some((_kind, snap))) => Some(secrets.redact_json(&snap)),
            Ok(None) => None,
            Err(err) => {
                snapshot_error = Some(secrets.redact(&err));
                None
            }
        }
    } else {
        None
    };

    let local_only_reason = sessions.local_only_reason();
    let detached = sessions.detach_all();
    state.handles = detached.handles;
    state.last_active_ms = store::now_ms();

    let existing_steps = store::load_steps(&session_dir).unwrap_or_default();
    let record = build_step_record(
        existing_steps.len() + 1,
        &step,
        &report,
        registry,
        started_at_ms,
        synthesis,
    );
    if let Err(err) = store::append_step(&session_dir, &record, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }
    if let Err(err) = store::save_state(&session_dir, &state, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }

    let ok = matches!(report.status, StepStatus::Passed | StepStatus::Skipped)
        && detached.errors.is_empty();
    let exit_code = if !detached.errors.is_empty()
        && matches!(report.status, StepStatus::Passed | StepStatus::Skipped)
    {
        ExitCode::Environment
    } else {
        exit_code_for_step(&report)
    };

    let mut payload = json!({
        "ok": ok,
        "session_id": state.id,
        "step": output::step_json(&report),
    });
    if let Some(snap) = snapshot {
        payload["snapshot"] = snap;
    }
    if let Some(err) = snapshot_error {
        payload["snapshot_error"] = Value::String(err);
    }
    if !detached.errors.is_empty() {
        payload["detach_errors"] = json!(detached.errors);
    }
    if let Some(reason) = local_only_reason {
        payload["local_only"] = json!(true);
        payload["local_only_reason"] = json!(reason);
    }

    SessionResponse {
        exit_code,
        payload: secrets.redact_json(&payload),
        raw_yaml: None,
    }
}

fn handle_snapshot(
    sessions_dir: &Path,
    id: &str,
    scope_ref: Option<&str>,
    max_depth: Option<usize>,
    max_nodes: Option<usize>,
    registry: &Registry,
) -> SessionResponse {
    let (session_dir, _lock, mut state) = match acquire_existing(sessions_dir, id) {
        Ok(found) => found,
        Err(resp) => return resp,
    };

    let secrets = store::secrets_from_env_names(&state.env_names);
    let mut sessions = match reattach_sessions(&state, Path::new("."), registry) {
        Ok(sessions) => sessions,
        Err(message) => {
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let snapshot = match sessions.collect_snapshot(scope_ref, max_depth, max_nodes) {
        Ok(Some((_kind, snap))) => snap,
        Ok(None) => {
            let _ = sessions.detach_all();
            return error_response(
                ExitCode::Environment,
                "environment",
                "no active snapshot-capable session; run a browser step such as `browser.open` first",
            );
        }
        Err(message) => {
            let _ = sessions.detach_all();
            let code = if message.contains("ref `") || message.contains("stale") {
                ExitCode::TestFailure
            } else {
                ExitCode::Environment
            };
            let kind = if code == ExitCode::TestFailure {
                "execution"
            } else {
                "environment"
            };
            return error_response(code, kind, secrets.redact(&message));
        }
    };

    let detached = sessions.detach_all();
    state.handles = detached.handles;
    state.last_active_ms = store::now_ms();
    if let Err(err) = store::save_state(&session_dir, &state, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }

    let payload = json!({
        "ok": detached.errors.is_empty(),
        "session_id": state.id,
        "snapshot": snapshot,
    });
    SessionResponse {
        exit_code: if detached.errors.is_empty() {
            ExitCode::Success
        } else {
            ExitCode::Environment
        },
        payload: secrets.redact_json(&payload),
        raw_yaml: None,
    }
}

fn handle_export(
    sessions_dir: &Path,
    id: &str,
    options: &ExportOptions,
    out_path: Option<&Path>,
) -> SessionResponse {
    let (session_dir, _lock, state) = match acquire_existing(sessions_dir, id) {
        Ok(found) => found,
        Err(resp) => return resp,
    };
    let records = match store::load_steps(&session_dir) {
        Ok(records) => records,
        Err(message) => return error_response(ExitCode::Environment, "environment", message),
    };
    let result = match export::export_session(&state.id, &records, options) {
        Ok(result) => result,
        Err(message) => return error_response(ExitCode::InvalidCase, "invalid_input", message),
    };

    if let Some(path) = out_path {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(error) = fs::create_dir_all(parent)
        {
            return error_response(
                ExitCode::Environment,
                "environment",
                format!(
                    "could not create output directory `{}`: {error}",
                    paths::display(parent)
                ),
            );
        }
        if let Err(error) = fs::write(path, &result.yaml) {
            return error_response(
                ExitCode::Environment,
                "environment",
                format!(
                    "could not write exported case `{}`: {error}",
                    paths::display(path)
                ),
            );
        }
    }

    let mut payload = json!({
        "ok": true,
        "session_id": state.id,
        "case_name": result.case_name,
        "exported_steps": result.exported_steps,
        "skipped_steps": result.skipped_steps,
        "fragile": result.fragile,
        "yaml": result.yaml,
    });
    if let Some(path) = out_path {
        payload["path"] = Value::String(paths::display(path));
    }

    SessionResponse {
        exit_code: ExitCode::Success,
        payload,
        raw_yaml: Some(result.yaml),
    }
}

fn handle_stop(sessions_dir: &Path, id: &str, registry: &Registry) -> SessionResponse {
    if let Err(message) = store::validate_session_id(id) {
        return error_response(ExitCode::Usage, "usage", message);
    }
    let session_dir = sessions_dir.join(id);
    if !session_dir.exists() {
        return error_response(
            ExitCode::Environment,
            "environment",
            format!("session `{id}` does not exist or has already stopped"),
        );
    }
    let lock = match SessionLock::try_acquire(&session_dir, id) {
        Ok(lock) => lock,
        Err(LockError::Busy(message)) | Err(LockError::Io(message)) => {
            return error_response(ExitCode::Environment, "environment", message);
        }
    };
    let (state, load_error) = if session_dir.join(store::STATE_FILE).exists() {
        match store::load_state(&session_dir) {
            Ok(state) => (Some(state), None),
            Err(err) => (None, Some(err)),
        }
    } else {
        (None, None)
    };
    let mut errors = store::close_and_remove_session(&session_dir, state.as_ref(), lock, registry);
    if let Some(err) = load_error {
        errors.insert(0, err);
    }

    let ok = errors.is_empty();
    SessionResponse {
        exit_code: if ok {
            ExitCode::Success
        } else {
            ExitCode::Environment
        },
        payload: json!({
            "ok": ok,
            "session_id": id,
            "errors": errors,
        }),
        raw_yaml: None,
    }
}

fn handle_list(
    sessions_dir: &Path,
    idle_override_ms: Option<u64>,
    registry: &Registry,
) -> SessionResponse {
    match store::list_and_reap(sessions_dir, idle_override_ms, registry) {
        Ok(report) => SessionResponse {
            exit_code: if report.errors.is_empty() {
                ExitCode::Success
            } else {
                ExitCode::Environment
            },
            payload: json!({
                "ok": report.errors.is_empty(),
                "sessions": report.sessions,
                "reaped": report.reaped,
                "errors": report.errors,
            }),
            raw_yaml: None,
        },
        Err(message) => error_response(ExitCode::Environment, "environment", message),
    }
}

fn parse_step_input(input: StepInput, registry: &Registry) -> Result<Step, String> {
    match input {
        StepInput::Text(text) => case::parse_single_step(&text),
        StepInput::Flags {
            action,
            with_raw,
            args,
            save,
        } => build_step_from_flags(registry, &action, with_raw.as_deref(), &args, &save),
    }
}

fn build_step_from_flags(
    registry: &Registry,
    action: &str,
    with_raw: Option<&str>,
    pairs: &[(String, String)],
    saves: &[(String, String)],
) -> Result<Step, String> {
    let mut map = match with_raw {
        Some(text) => {
            let parsed = case::parse_json_value(text)
                .map_err(|error| format!("could not parse `--with`: {error}"))?;
            match parsed {
                Value::Object(entries) => entries,
                _ => return Err("`--with` must be a mapping of step arguments".to_string()),
            }
        }
        None => Map::new(),
    };

    let properties = registry
        .get(action)
        .and_then(|def| def.args_schema.get("properties"))
        .and_then(Value::as_object);

    for (key, raw_value) in pairs {
        let segments: Vec<&str> = key.split('.').collect();
        if segments.iter().any(|s| s.is_empty()) {
            return Err(format!("invalid argument path `{key}`"));
        }
        let coerced = if segments.len() == 1 {
            coerce_arg_value(properties.and_then(|p| p.get(segments[0])), raw_value)?
        } else {
            coerce_arg_value(None, raw_value)?
        };
        insert_nested(&mut map, &segments, coerced)?;
    }

    let mut save_items = Vec::with_capacity(saves.len());
    for (name, path) in saves {
        if name.trim().is_empty() {
            return Err("`--save` variable name must not be empty".to_string());
        }
        // Validate JSONPath syntax with jsonpath-rust via case::parse_single_step?
        // Construct minimal YAML or validate directly by letting parse_single_step check save:
        save_items.push(SavedValue {
            name: name.clone(),
            path: path.clone(),
        });
    }

    let step_obj = {
        let mut outer = Map::new();
        outer.insert(action.to_string(), Value::Object(map));
        if !save_items.is_empty() {
            let mut save_map = Map::new();
            for item in &save_items {
                save_map.insert(item.name.clone(), Value::String(item.path.clone()));
            }
            outer.insert("save".to_string(), Value::Object(save_map));
        }
        Value::Object(outer)
    };
    let text = serde_json::to_string(&step_obj)
        .map_err(|error| format!("could not serialize step arguments: {error}"))?;
    case::parse_single_step(&text)
}

fn coerce_arg_value(schema: Option<&Value>, raw: &str) -> Result<Value, String> {
    let kind = schema.and_then(|s| s.get("type"));
    let expected = match kind {
        Some(Value::String(s)) => Some(s.as_str()),
        Some(Value::Array(arr)) => arr.iter().find_map(Value::as_str),
        _ => None,
    };
    match expected {
        Some("integer") => raw
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| format!("expected an integer, got `{raw}`")),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| format!("expected a number, got `{raw}`")),
        Some("boolean") => match raw {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(format!("expected `true` or `false`, got `{raw}`")),
        },
        Some("array") | Some("object") => case::parse_json_value(raw)
            .map_err(|error| format!("expected YAML/JSON value, got `{raw}`: {error}")),
        _ => Ok(Value::String(raw.to_string())),
    }
}

fn insert_nested(root: &mut Map<String, Value>, path: &[&str], value: Value) -> Result<(), String> {
    let Some((head, tail)) = path.split_first() else {
        return Ok(());
    };
    if tail.is_empty() {
        root.insert((*head).to_string(), value);
        return Ok(());
    }
    let entry = root
        .entry((*head).to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(child) = entry else {
        return Err(format!("argument `{head}` is not a mapping"));
    };
    insert_nested(child, tail, value)
}

struct StepSynthesis {
    used_ref: Option<String>,
    synthesized_locator: Option<Value>,
    fragile: bool,
}

fn extract_synthesis(step: &Step, metadata: &BTreeMap<String, Value>) -> StepSynthesis {
    let used_ref = step
        .args
        .get("ref")
        .and_then(Value::as_str)
        .map(str::to_string);
    for meta in metadata.values() {
        if let Some(locator) = meta.get("synthesized_locator").cloned() {
            let fragile = meta
                .get("fragile")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let meta_ref = meta
                .get("ref")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or(used_ref);
            return StepSynthesis {
                used_ref: meta_ref,
                synthesized_locator: Some(locator),
                fragile,
            };
        }
    }
    StepSynthesis {
        used_ref,
        synthesized_locator: None,
        fragile: false,
    }
}

fn build_step_record(
    index: usize,
    step: &Step,
    report: &StepReport,
    registry: &Registry,
    started_at_ms: u64,
    synthesis: StepSynthesis,
) -> StepRecord {
    let expect = step
        .expect
        .iter()
        .map(|item| StoredExpectation {
            path: item.path.clone(),
            comparison: item.matcher.comparison().to_string(),
            expected: match &item.matcher {
                crate::core::case::Matcher::Equals(v)
                | crate::core::case::Matcher::NotEquals(v)
                | crate::core::case::Matcher::Contains(v) => v.clone(),
                crate::core::case::Matcher::Exists(flag) => Value::Bool(*flag),
            },
        })
        .collect();
    let save = step
        .save
        .iter()
        .map(|s| (s.name.clone(), s.path.clone()))
        .collect();
    // A step that clicked explicit coordinates reports the viewport it ran at,
    // so export can pin the replayed step to the same size.
    let observed_viewport = step
        .args
        .get("at")
        .and(report.result.as_ref())
        .and_then(|result| result.get("viewport"))
        .cloned();
    StepRecord {
        index,
        action: step.action.clone(),
        raw_args: registry.redact_step_args(&step.action, &step.args),
        expect,
        save,
        optional: step.optional,
        status: report.status.as_str().to_string(),
        error: report.error.as_ref().map(|problem| problem.message.clone()),
        used_ref: synthesis.used_ref,
        synthesized_locator: synthesis.synthesized_locator,
        fragile: synthesis.fragile,
        observed_viewport,
        started_at_ms,
        duration_ms: u64::try_from(report.duration.as_millis()).unwrap_or(u64::MAX),
        result: report.result.clone(),
    }
}

/// Parses repeatable CLI `--observe` flags (`NAME=KIND`, `NAME=KIND:key=val`,
/// `NAME=KIND:{...}`, or YAML/JSON `NAME: { KIND: {...} }`).
///
/// Syntax errors return `Err` for CLI `Usage` reporting; semantic checks
/// (unknown observer, invalid arguments, unset secrets) happen in
/// `handle_start` so they report `ExitCode::InvalidCase`.
pub fn parse_observe_specs(
    specs: &[String],
    registry: &Registry,
) -> Result<Vec<StoredObservation>, String> {
    let mut out = Vec::with_capacity(specs.len());
    for raw in specs {
        parse_one_observe_spec(raw, registry, &mut out)?;
    }
    Ok(out)
}

fn parse_one_observe_spec(
    raw: &str,
    registry: &Registry,
    out: &mut Vec<StoredObservation>,
) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("`--observe` specification must not be empty".to_string());
    }

    let colon_before_eq = trimmed
        .find(':')
        .is_some_and(|colon| trimmed.find('=').is_none_or(|eq| colon < eq));
    if trimmed.starts_with('{') || colon_before_eq {
        return parse_observe_mapping(trimmed, out);
    }

    let Some((raw_name, rest)) = trimmed.split_once('=') else {
        return Err(format!(
            "invalid `--observe` specification `{raw}`; expected `NAME=KIND` or `NAME=KIND:ARGS`"
        ));
    };
    let name = raw_name.trim();
    let rest = rest.trim();
    if name.is_empty() || rest.is_empty() {
        return Err(format!(
            "invalid `--observe` specification `{raw}`; expected `NAME=KIND` or `NAME=KIND:ARGS`"
        ));
    }

    let (kind, raw_args) = match rest.split_once(':') {
        None => (rest.to_string(), Value::Object(Map::new())),
        Some((raw_kind, raw_arg_spec)) => {
            let kind = raw_kind.trim();
            let arg_spec = raw_arg_spec.trim();
            if kind.is_empty() || arg_spec.is_empty() {
                return Err(format!(
                    "invalid `--observe` specification `{raw}`; expected `NAME=KIND:ARGS`"
                ));
            }
            let parsed_args = if arg_spec.starts_with('{') {
                let parsed = case::parse_json_value(arg_spec)
                    .map_err(|error| format!("invalid `--observe` JSON/YAML arguments: {error}"))?;
                if !parsed.is_object() {
                    return Err("`--observe` arguments must be an object".to_string());
                }
                parsed
            } else if arg_spec.contains('=') {
                parse_observe_kv_args(arg_spec, kind, registry)?
            } else {
                return Err(format!(
                    "invalid `--observe` argument `{arg_spec}`; expected `key=value` or `{{...}}`"
                ));
            };
            (kind.to_string(), parsed_args)
        }
    };

    out.push(StoredObservation {
        name: name.to_string(),
        kind,
        raw_args,
    });
    Ok(())
}

fn parse_observe_mapping(raw: &str, out: &mut Vec<StoredObservation>) -> Result<(), String> {
    let parsed = case::parse_json_value(raw)
        .map_err(|error| format!("invalid `--observe` mapping: {error}"))?;
    let Value::Object(entries) = parsed else {
        return Err("`--observe` mapping must be `{ NAME: { KIND: ARGS } }`".to_string());
    };
    if entries.is_empty() {
        return Err("`--observe` mapping must declare at least one observer".to_string());
    }
    for (name, spec) in entries {
        let Value::Object(kinds) = spec else {
            return Err(format!(
                "observation `{name}` must map to `{{ KIND: ARGS }}`"
            ));
        };
        if kinds.len() != 1 {
            return Err(format!(
                "observation `{name}` must declare exactly one observer kind"
            ));
        }
        let (kind, args) = kinds.into_iter().next().expect("checked len == 1");
        let args = match args {
            Value::Object(_) => args,
            Value::Null => Value::Object(Map::new()),
            _ => {
                return Err(format!(
                    "observation `{name}` (`{kind}`) arguments must be an object"
                ));
            }
        };
        out.push(StoredObservation {
            name,
            kind,
            raw_args: args,
        });
    }
    Ok(())
}

fn parse_observe_kv_args(arg_spec: &str, kind: &str, registry: &Registry) -> Result<Value, String> {
    let pairs: Vec<&str> = if arg_spec.contains(',') && arg_spec.split(',').all(|p| p.contains('='))
    {
        arg_spec.split(',').collect()
    } else {
        vec![arg_spec]
    };

    let properties = registry
        .observer(kind)
        .and_then(|def| def.args_schema.get("properties"))
        .and_then(Value::as_object);

    let mut map = Map::new();
    for pair in pairs {
        let Some((raw_key, raw_value)) = pair.split_once('=') else {
            return Err(format!(
                "invalid `--observe` argument `{pair}`; expected `key=value`"
            ));
        };
        let key = raw_key.trim();
        let segments: Vec<&str> = key.split('.').collect();
        if key.is_empty() || segments.iter().any(|s| s.is_empty()) {
            return Err(format!("invalid `--observe` argument path `{key}`"));
        }
        let coerced = if segments.len() == 1 {
            coerce_arg_value(properties.and_then(|p| p.get(segments[0])), raw_value)?
        } else {
            coerce_arg_value(None, raw_value)?
        };
        insert_nested(&mut map, &segments, coerced)?;
    }
    Ok(Value::Object(map))
}

fn validate_start_observations(
    observations: &[StoredObservation],
    registry: &Registry,
) -> Result<(Vec<String>, BTreeSet<String>), SessionResponse> {
    let mut seen = BTreeSet::new();
    let mut env_names = BTreeSet::new();
    for obs in observations {
        case::validate_observation_name(&obs.name)
            .map_err(|msg| error_response(ExitCode::InvalidCase, "invalid_input", msg))?;
        if !seen.insert(obs.name.as_str()) {
            return Err(error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                format!("duplicate observation name `{}`", obs.name),
            ));
        }
        env_names.extend(case::env_references(&obs.raw_args));
    }

    let mut secrets = store::secrets_from_env_names(&env_names);
    let empty_vars = BTreeMap::new();
    let mut names = Vec::with_capacity(observations.len());
    for obs in observations {
        let args = case::interpolate(&obs.raw_args, &empty_vars, &mut secrets).map_err(|err| {
            error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("observation `{}`: {}", obs.name, err.message)),
            )
        })?;
        let Some(def) = registry.observer(&obs.kind) else {
            return Err(error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                format!(
                    "observation `{}`: `{}` is not a registered observer",
                    obs.name, obs.kind
                ),
            ));
        };
        (def.create)(&args).map_err(|error| {
            let (exit_code, kind) = match &error {
                ObserverError::InvalidArguments(_) => (ExitCode::InvalidCase, "invalid_input"),
                ObserverError::Environment(_) => (ExitCode::Environment, "environment"),
            };
            error_response(
                exit_code,
                kind,
                secrets.redact(&format!(
                    "observation `{}` (`{}`): {}",
                    obs.name,
                    def.name,
                    error.message()
                )),
            )
        })?;
        names.push(obs.name.clone());
    }
    Ok((names, env_names))
}

fn exit_code_for_step(report: &StepReport) -> ExitCode {
    if report.status == StepStatus::Skipped {
        return ExitCode::Success;
    }
    match report.error.as_ref().map(|problem| problem.kind) {
        None => ExitCode::Success,
        Some(ProblemKind::InvalidInput) => ExitCode::InvalidCase,
        Some(ProblemKind::Environment) => ExitCode::Environment,
        Some(ProblemKind::Timeout) => ExitCode::Timeout,
        Some(ProblemKind::Assertion | ProblemKind::Execution) => ExitCode::TestFailure,
    }
}

/// Helper used by `da session step` to build a dummy `Location` when needed.
#[allow(dead_code, reason = "kept for future transport adapters")]
fn _unused_location() -> Location {
    Location::UNKNOWN
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::observer::{ObserverDefinition, ObserverError};
    use crate::core::session::Sessions;

    fn test_registry() -> Registry {
        struct NullObserver;
        impl crate::core::observer::Observer for NullObserver {
            fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
                Ok(())
            }

            fn collect(&mut self, _sessions: &mut Sessions) -> Result<Value, ObserverError> {
                Ok(json!({}))
            }
        }

        let mut registry = Registry::new();
        registry
            .register_observer(ObserverDefinition {
                name: "test.obs",
                description: "stub observer",
                args_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "max_entries": { "type": "integer" }
                    }
                }),
                validate: |_| Ok(()),
                create: |args| {
                    if let Some(p) = args.get("path").and_then(Value::as_str)
                        && p.is_empty()
                    {
                        return Err(ObserverError::InvalidArguments("empty path".to_string()));
                    }
                    Ok(Box::new(NullObserver))
                },
            })
            .expect("register test.obs");
        registry
    }

    #[test]
    fn parse_observe_specs_supports_shorthand_kv_windows_paths_json_and_yaml_mapping() {
        let registry = test_registry();
        let specs = vec![
            "net=test.obs".to_string(),
            r"app=test.obs:path=C:\logs\app.log,max_entries=10".to_string(),
            r#"json_obs=test.obs:{"path":"/tmp/x.log"}"#.to_string(),
            r#"{"yml_obs":{"test.obs":{"path":"/var/log/a.log"}}}"#.to_string(),
        ];

        let parsed = parse_observe_specs(&specs, &registry).expect("valid observe specs");
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].name, "net");
        assert_eq!(parsed[0].kind, "test.obs");
        assert_eq!(parsed[0].raw_args, json!({}));

        assert_eq!(parsed[1].name, "app");
        assert_eq!(
            parsed[1].raw_args,
            json!({ "path": r"C:\logs\app.log", "max_entries": 10 })
        );

        assert_eq!(parsed[2].name, "json_obs");
        assert_eq!(parsed[2].raw_args, json!({ "path": "/tmp/x.log" }));

        assert_eq!(parsed[3].name, "yml_obs");
        assert_eq!(parsed[3].raw_args, json!({ "path": "/var/log/a.log" }));
    }

    #[test]
    fn parse_observe_specs_rejects_malformed_inputs() {
        let registry = test_registry();
        for bad in [
            "bad-spec",
            "=test.obs",
            "a=",
            "a=test.obs:",
            "a=test.obs:no_eq",
        ] {
            assert!(
                parse_observe_specs(&[bad.to_string()], &registry).is_err(),
                "expected syntax error for `{bad}`"
            );
        }
    }

    #[test]
    fn handle_start_rejects_bad_observers_without_leaving_session_dir() {
        let registry = test_registry();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("da-sess-bad-obs-{nanos}"));

        // Invalid args
        let resp = handle(
            SessionRequest::Start {
                sessions_dir: dir.clone(),
                idle_timeout_ms: 60_000,
                observe: vec![StoredObservation {
                    name: "app".to_string(),
                    kind: "test.obs".to_string(),
                    raw_args: json!({ "path": "" }),
                }],
            },
            &registry,
        );
        assert_eq!(resp.exit_code, ExitCode::InvalidCase);
        assert!(!dir.exists(), "failed start must not create sessions dir");

        // Duplicate name
        let dup = handle(
            SessionRequest::Start {
                sessions_dir: dir.clone(),
                idle_timeout_ms: 60_000,
                observe: vec![
                    StoredObservation {
                        name: "app".to_string(),
                        kind: "test.obs".to_string(),
                        raw_args: json!({}),
                    },
                    StoredObservation {
                        name: "app".to_string(),
                        kind: "test.obs".to_string(),
                        raw_args: json!({}),
                    },
                ],
            },
            &registry,
        );
        assert_eq!(dup.exit_code, ExitCode::InvalidCase);
        assert!(
            !dir.exists(),
            "duplicate start must not create sessions dir"
        );
    }
}
