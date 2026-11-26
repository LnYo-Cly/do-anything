//! Transport-neutral interactive session engine (`da session`).
//!
//! Commands arrive as [`SessionRequest`] and return [`SessionResponse`], so the
//! CLI, future stdio JSON-lines mode, and future `da mcp` share one handler.
//! Session state, step logs and saved variables live under `.da/sessions/<id>/`
//! without ever persisting resolved environment secrets.

pub mod export;
#[cfg(feature = "ai")]
pub(crate) mod model_ai;
pub mod model_hook;
pub mod store;

use std::borrow::Cow;
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
use self::model_hook::{
    ActStepAction, ActStepParams, ActStepStatus, BuiltinModelSpec, ExtractParams, HookError,
    JudgeOutcome, JudgeParams, LocateOutcome, LocateParams, ModelAudit, ModelHookSpec,
    STEP_ACTION_ACT, STEP_ACTION_EXTRACT, STEP_ACTION_JUDGE, STEP_ACTION_LOCATE,
    STEP_ACTION_SELECT_REF, SelectRefOutcome, SelectRefParams, validate_point_in_viewport,
    validate_reasoning_effort,
};
use self::store::{
    LockError, SessionLock, SessionState, StepRecord, StoredExpectation, StoredObservation,
};
use crate::core::session::{MaskedViewportCapture, SynthesizedTarget};

/// Default directory relative to the working directory for session storage.
pub const DEFAULT_SESSIONS_DIR: &str = ".da/sessions";

/// Default idle timeout before `start` or `list` reaps a session (30 minutes).
pub const DEFAULT_IDLE_TIMEOUT_MS: u64 = 1_800_000;

/// Default maximum steps executed by one [`SessionRequest::Act`] call.
pub const DEFAULT_ACT_MAX_STEPS: usize = 8;

/// Hard upper bound on `--max-steps` per [`SessionRequest::Act`] invocation.
pub const MAX_ACT_MAX_STEPS: usize = 50;

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

/// Optional immediate step executed with the `ref` chosen by `select_ref`.
#[derive(Debug, Clone)]
pub struct SelectRefAction {
    pub action: String,
    pub with_raw: Option<String>,
    pub args: Vec<(String, String)>,
    pub save: Vec<(String, String)>,
}

/// Request arguments for [`SessionRequest::SelectRef`].
#[derive(Debug, Clone)]
pub struct SelectRefRequest {
    pub sessions_dir: PathBuf,
    pub id: String,
    pub base_directory: PathBuf,
    pub query: String,
    pub scope_ref: Option<String>,
    pub max_depth: Option<usize>,
    pub max_nodes: Option<usize>,
    pub hook_override: Option<ModelHookSpec>,
    pub model_override: Option<BuiltinModelSpec>,
    pub reasoning_effort_override: Option<String>,
    pub timeout_ms: u64,
    pub act: Option<SelectRefAction>,
}

/// Request arguments for [`SessionRequest::Locate`].
#[derive(Debug, Clone)]
pub struct LocateRequest {
    pub sessions_dir: PathBuf,
    pub id: String,
    pub base_directory: PathBuf,
    pub query: String,
    pub hook_override: Option<ModelHookSpec>,
    pub model_override: Option<BuiltinModelSpec>,
    pub reasoning_effort_override: Option<String>,
    pub timeout_ms: u64,
    pub click: bool,
}

/// Request arguments for [`SessionRequest::Judge`].
#[derive(Debug, Clone)]
pub struct JudgeRequest {
    pub sessions_dir: PathBuf,
    pub id: String,
    pub base_directory: PathBuf,
    pub rubric: String,
    pub evidence: Option<Value>,
    pub screenshot: bool,
    pub hook_override: Option<ModelHookSpec>,
    pub model_override: Option<BuiltinModelSpec>,
    pub reasoning_effort_override: Option<String>,
    pub timeout_ms: u64,
}

/// Request arguments for [`SessionRequest::Extract`].
#[derive(Debug, Clone)]
pub struct ExtractRequest {
    pub sessions_dir: PathBuf,
    pub id: String,
    pub base_directory: PathBuf,
    pub query: String,
    pub schema: Option<Value>,
    pub evidence: Option<Value>,
    pub screenshot: bool,
    pub save: Option<String>,
    pub hook_override: Option<ModelHookSpec>,
    pub model_override: Option<BuiltinModelSpec>,
    pub reasoning_effort_override: Option<String>,
    pub timeout_ms: u64,
}

/// Request arguments for [`SessionRequest::Act`].
#[derive(Debug, Clone)]
pub struct ActRequest {
    pub sessions_dir: PathBuf,
    pub id: String,
    pub base_directory: PathBuf,
    pub goal: String,
    pub max_steps: usize,
    pub evidence: Option<Value>,
    pub screenshot: bool,
    pub hook_override: Option<ModelHookSpec>,
    pub model_override: Option<BuiltinModelSpec>,
    pub reasoning_effort_override: Option<String>,
    pub timeout_ms: u64,
}

/// A transport-neutral session command request.
#[derive(Debug, Clone)]
pub enum SessionRequest {
    Start {
        sessions_dir: PathBuf,
        idle_timeout_ms: u64,
        observe: Vec<StoredObservation>,
        model_hook: Option<ModelHookSpec>,
        model: Option<BuiltinModelSpec>,
        plugins: Vec<String>,
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
    SelectRef(SelectRefRequest),
    Locate(LocateRequest),
    Judge(JudgeRequest),
    Extract(ExtractRequest),
    Act(ActRequest),
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
    /// Optional human-readable text rendering when `--output text` is requested.
    pub raw_text: Option<String>,
}

/// Executes one [`SessionRequest`] against `registry`.
pub fn handle(request: SessionRequest, registry: &Registry) -> SessionResponse {
    match request {
        SessionRequest::Start {
            sessions_dir,
            idle_timeout_ms,
            observe,
            model_hook,
            model,
            plugins,
        } => handle_start(
            &sessions_dir,
            idle_timeout_ms,
            observe,
            model_hook,
            model,
            plugins,
            registry,
        ),
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
        SessionRequest::SelectRef(req) => handle_select_ref(req, registry),
        SessionRequest::Locate(req) => handle_locate(req, registry),
        SessionRequest::Judge(req) => handle_judge(req, registry),
        SessionRequest::Extract(req) => handle_extract(req, registry),
        SessionRequest::Act(req) => handle_act(req, registry),
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
        raw_text: None,
    }
}

fn handle_start(
    sessions_dir: &Path,
    idle_timeout_ms: u64,
    observe: Vec<StoredObservation>,
    model_hook: Option<ModelHookSpec>,
    mut model: Option<BuiltinModelSpec>,
    plugins: Vec<String>,
    registry: &Registry,
) -> SessionResponse {
    if model_hook.is_some() && model.is_some() {
        return error_response(
            ExitCode::Usage,
            "usage",
            "pass either `--model` or `--model-hook`, not both",
        );
    }
    if let Some(hook) = &model_hook
        && hook.command.trim().is_empty()
    {
        return error_response(
            ExitCode::Usage,
            "usage",
            "`--model-hook` executable path must not be empty",
        );
    }
    if let Some(spec) = &mut model {
        if let Some(raw_effort) = &spec.reasoning_effort {
            match validate_reasoning_effort(raw_effort) {
                Ok(canonical) => spec.reasoning_effort = Some(canonical),
                Err(msg) => return error_response(ExitCode::Usage, "usage", msg),
            }
        }
        if let Err(msg) = validate_builtin_model(spec) {
            return error_response(ExitCode::Usage, "usage", msg);
        }
    }

    let (observer_names, mut env_names) = match validate_start_observations(&observe, registry) {
        Ok(validated) => validated,
        Err(response) => return *response,
    };

    if let Some(hook) = &model_hook {
        env_names.extend(case::env_references(&json!([&hook.command, &hook.args])));
    }
    if let Some(spec) = &model {
        collect_builtin_model_env_names(spec, &mut env_names);
    }
    let mut secrets = store::secrets_from_env_names(&env_names);
    if let Some(hook) = &model_hook {
        if let Err(msg) = case::expand_env(&hook.command, &mut secrets) {
            return error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("`--model-hook`: {msg}")),
            );
        }
        for arg in &hook.args {
            if let Err(msg) = case::expand_env(arg, &mut secrets) {
                return error_response(
                    ExitCode::InvalidCase,
                    "invalid_input",
                    secrets.redact(&format!("`--model-hook-arg`: {msg}")),
                );
            }
        }
    }
    if let Some(spec) = &model
        && let Some(endpoint) = &spec.endpoint
        && let Err(msg) = case::expand_env(endpoint, &mut secrets)
    {
        return error_response(
            ExitCode::InvalidCase,
            "invalid_input",
            secrets.redact(&format!("`--model-endpoint`: {msg}")),
        );
    }

    let list_report = match store::list_and_reap(sessions_dir, None, registry, &secrets) {
        Ok(result) => result,
        Err(message) => {
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let id = store::generate_session_id();
    let session_dir = sessions_dir.join(&id);
    if let Err(error) = fs::create_dir_all(&session_dir) {
        return error_response(
            ExitCode::Environment,
            "environment",
            secrets.redact(&format!(
                "could not create session directory `{}`: {error}",
                paths::display(&session_dir)
            )),
        );
    }

    let now = store::now_ms();
    let hook_display = model_hook.as_ref().map(|h| secrets.redact(&h.command));
    let model_display = model.clone();
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
        plugins,
        model_hook,
        model,
    };
    if let Err(message) = store::save_state(&session_dir, &state, &secrets) {
        let _ = store::remove_session_dir(&session_dir);
        return error_response(
            ExitCode::Environment,
            "environment",
            secrets.redact(&message),
        );
    }

    let mut payload = json!({
        "ok": true,
        "session_id": id,
        "dir": paths::display(&session_dir),
        "idle_timeout_ms": idle_timeout_ms,
        "observers": observer_names,
        "plugins": state.plugins,
        "reaped": list_report.reaped,
        "reap_errors": list_report.errors,
    });
    if let Some(cmd) = hook_display {
        payload["model_hook"] = Value::String(cmd);
    }
    if let Some(spec) = model_display {
        payload["model"] = Value::String(secrets.redact(&spec.model));
        if let Some(adapter) = &spec.adapter {
            payload["model_adapter"] = Value::String(secrets.redact(adapter));
        }
        if let Some(endpoint) = &spec.endpoint {
            payload["model_endpoint"] = Value::String(secrets.redact(endpoint));
        }
        if let Some(api_key_env) = &spec.api_key_env {
            payload["model_api_key_env"] = Value::String(secrets.redact(api_key_env));
        }
        if let Some(effort) = &spec.reasoning_effort {
            payload["model_reasoning_effort"] = Value::String(secrets.redact(effort));
        }
    }
    SessionResponse {
        exit_code: ExitCode::Success,
        payload: secrets.redact_json(&payload),
        raw_yaml: None,
        raw_text: None,
    }
}

fn resolve_target_session_registry<'a>(
    plugins: &[String],
    base_registry: &'a Registry,
) -> Result<Cow<'a, Registry>, (ExitCode, &'static str, String)> {
    if plugins.is_empty() {
        return Ok(Cow::Borrowed(base_registry));
    }
    #[cfg(feature = "plugin")]
    {
        let mut reg = base_registry.clone();
        crate::plugin::register_persisted(&mut reg, plugins).map_err(|error| {
            let code = error.exit_code();
            let kind = match code {
                ExitCode::Timeout => "timeout",
                ExitCode::Environment => "environment",
                _ => "usage",
            };
            (code, kind, error.message().to_string())
        })?;
        Ok(Cow::Owned(reg))
    }
    #[cfg(not(feature = "plugin"))]
    {
        let _ = base_registry;
        Err((
            ExitCode::Usage,
            "usage",
            "this build was compiled without the `plugin` feature; rebuild with `--features plugin`".to_string(),
        ))
    }
}

fn acquire_existing(
    sessions_dir: &Path,
    id: &str,
) -> Result<(PathBuf, SessionLock, SessionState), Box<SessionResponse>> {
    if let Err(message) = store::validate_session_id(id) {
        return Err(Box::new(error_response(ExitCode::Usage, "usage", message)));
    }
    let session_dir = sessions_dir.join(id);
    if !session_dir.join(store::STATE_FILE).exists() {
        return Err(Box::new(error_response(
            ExitCode::Environment,
            "environment",
            format!(
                "session `{id}` does not exist or has already stopped; start one with `da session start`"
            ),
        )));
    }
    let lock = match SessionLock::try_acquire(&session_dir, id) {
        Ok(lock) => lock,
        Err(LockError::Busy(message)) | Err(LockError::Io(message)) => {
            return Err(Box::new(error_response(
                ExitCode::Environment,
                "environment",
                message,
            )));
        }
    };
    let state = match store::load_state(&session_dir) {
        Ok(state) => state,
        Err(message) => {
            return Err(Box::new(error_response(
                ExitCode::Environment,
                "environment",
                message,
            )));
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
        let session = def.reattach(handle).map_err(|err| {
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
        Err(resp) => return *resp,
    };

    let mut secrets = store::secrets_from_env_names(&state.env_names);
    let effective_registry = match resolve_target_session_registry(&state.plugins, registry) {
        Ok(reg) => reg,
        Err((code, kind, message)) => return error_response(code, kind, secrets.redact(&message)),
    };

    let step = match parse_step_input(step_input, effective_registry.as_ref()) {
        Ok(step) => step,
        Err(message) => {
            return error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&message),
            );
        }
    };

    state.env_names.extend(case::env_references(&step.args));
    secrets = store::secrets_from_env_names(&state.env_names);

    let mut sessions = match reattach_sessions(&state, base_directory, effective_registry.as_ref())
    {
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
        effective_registry.as_ref(),
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
        effective_registry.as_ref(),
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
        raw_text: None,
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
        Err(resp) => return *resp,
    };

    let secrets = store::secrets_from_env_names(&state.env_names);
    let effective_registry = match resolve_target_session_registry(&state.plugins, registry) {
        Ok(reg) => reg,
        Err((code, kind, message)) => return error_response(code, kind, secrets.redact(&message)),
    };
    let mut sessions = match reattach_sessions(&state, Path::new("."), effective_registry.as_ref())
    {
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
        raw_text: None,
    }
}

enum ModelBackend {
    Builtin(BuiltinModelSpec),
    Hook(ModelHookSpec),
}

/// Validates a built-in `genai` model specification without making network requests.
pub fn validate_builtin_model(spec: &BuiltinModelSpec) -> Result<(), String> {
    #[cfg(feature = "ai")]
    {
        model_ai::validate_spec(spec)
    }
    #[cfg(not(feature = "ai"))]
    {
        let _ = spec;
        Err(
            "this build was compiled without the `ai` feature; rebuild with `--features ai` or use `--model-hook` / `--hook`"
                .to_string(),
        )
    }
}

/// Collects environment variable names referenced by a [`BuiltinModelSpec`] for secret redaction.
pub fn collect_builtin_model_env_names(spec: &BuiltinModelSpec, env_names: &mut BTreeSet<String>) {
    #[cfg(feature = "ai")]
    {
        model_ai::collect_env_names(spec, env_names);
    }
    #[cfg(not(feature = "ai"))]
    {
        if let Some(endpoint) = &spec.endpoint {
            env_names.extend(case::env_references(&Value::String(endpoint.clone())));
        }
        if let Some(key_env) = &spec.api_key_env {
            let trimmed = key_env.trim();
            if !trimmed.is_empty() {
                env_names.insert(trimmed.to_string());
            }
        }
    }
}

/// Reads the `DA_MODEL` environment fallback configuration when present and non-empty.
pub fn builtin_model_from_env() -> Option<BuiltinModelSpec> {
    let model = std::env::var("DA_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let read_opt = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Some(BuiltinModelSpec {
        model,
        adapter: read_opt("DA_MODEL_ADAPTER"),
        endpoint: read_opt("DA_MODEL_ENDPOINT"),
        api_key_env: read_opt("DA_MODEL_API_KEY_ENV"),
        reasoning_effort: read_opt("DA_MODEL_REASONING_EFFORT"),
    })
}

fn resolve_model_backend(
    hook_override: Option<&ModelHookSpec>,
    model_override: Option<&BuiltinModelSpec>,
    reasoning_effort_override: Option<&str>,
    state: &mut SessionState,
    secrets: &mut Secrets,
) -> Result<ModelBackend, Box<SessionResponse>> {
    let normalized_effort = match reasoning_effort_override {
        Some(raw) => Some(
            validate_reasoning_effort(raw)
                .map_err(|msg| Box::new(error_response(ExitCode::Usage, "usage", msg)))?,
        ),
        None => None,
    };
    if hook_override.is_some() && model_override.is_some() {
        return Err(Box::new(error_response(
            ExitCode::Usage,
            "usage",
            "pass either `--hook` or `--model`, not both",
        )));
    }
    if hook_override.is_some() && normalized_effort.is_some() {
        return Err(Box::new(error_response(
            ExitCode::Usage,
            "usage",
            "`--reasoning-effort` applies only to the built-in `--model`, not `--hook`",
        )));
    }

    if let Some(hook) = hook_override {
        return expand_hook_spec(hook, state, secrets).map(ModelBackend::Hook);
    }
    if let Some(model) = model_override {
        let mut merged = model.clone();
        if let Some(effort) = normalized_effort {
            merged.reasoning_effort = Some(effort);
        } else if let Some(raw_effort) = &merged.reasoning_effort {
            merged.reasoning_effort = Some(
                validate_reasoning_effort(raw_effort)
                    .map_err(|msg| Box::new(error_response(ExitCode::Usage, "usage", msg)))?,
            );
        }
        return prepare_builtin_model(&merged, state, secrets).map(ModelBackend::Builtin);
    }
    if let Some(hook) = state.model_hook.clone() {
        if normalized_effort.is_some() {
            return Err(Box::new(error_response(
                ExitCode::Usage,
                "usage",
                "`--reasoning-effort` applies only to built-in `--model` backends, not sessions configured with `--model-hook`",
            )));
        }
        return expand_hook_spec(&hook, state, secrets).map(ModelBackend::Hook);
    }
    if let Some(mut model) = state.model.clone() {
        if let Some(effort) = normalized_effort {
            model.reasoning_effort = Some(effort);
        }
        return prepare_builtin_model(&model, state, secrets).map(ModelBackend::Builtin);
    }
    if let Some(mut env_model) = builtin_model_from_env() {
        if let Some(effort) = normalized_effort {
            env_model.reasoning_effort = Some(effort);
        } else if let Some(raw_effort) = &env_model.reasoning_effort {
            env_model.reasoning_effort = Some(
                validate_reasoning_effort(raw_effort)
                    .map_err(|msg| Box::new(error_response(ExitCode::Usage, "usage", msg)))?,
            );
        }
        return prepare_builtin_model(&env_model, state, secrets).map(ModelBackend::Builtin);
    }

    Err(Box::new(error_response(
        ExitCode::Usage,
        "usage",
        format!(
            "no model configured for session `{}`; pass `--model <MODEL>` or `--hook <EXECUTABLE>`, set `DA_MODEL`, or configure `--model` / `--model-hook` on `da session start`",
            state.id
        ),
    )))
}

fn expand_hook_spec(
    raw: &ModelHookSpec,
    state: &mut SessionState,
    secrets: &mut Secrets,
) -> Result<ModelHookSpec, Box<SessionResponse>> {
    if raw.command.trim().is_empty() {
        return Err(Box::new(error_response(
            ExitCode::Usage,
            "usage",
            "model hook executable path must not be empty",
        )));
    }
    state
        .env_names
        .extend(case::env_references(&json!([&raw.command, &raw.args])));
    *secrets = store::secrets_from_env_names(&state.env_names);

    let command = case::expand_env(&raw.command, secrets).map_err(|msg| {
        Box::new(error_response(
            ExitCode::InvalidCase,
            "invalid_input",
            secrets.redact(&format!("model hook command: {msg}")),
        ))
    })?;
    let mut args = Vec::with_capacity(raw.args.len());
    for arg in &raw.args {
        let expanded = case::expand_env(arg, secrets).map_err(|msg| {
            Box::new(error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("model hook argument: {msg}")),
            ))
        })?;
        args.push(expanded);
    }
    Ok(ModelHookSpec { command, args })
}

fn prepare_builtin_model(
    spec: &BuiltinModelSpec,
    state: &mut SessionState,
    secrets: &mut Secrets,
) -> Result<BuiltinModelSpec, Box<SessionResponse>> {
    if let Err(msg) = validate_builtin_model(spec) {
        return Err(Box::new(error_response(ExitCode::Usage, "usage", msg)));
    }
    collect_builtin_model_env_names(spec, &mut state.env_names);
    *secrets = store::secrets_from_env_names(&state.env_names);
    Ok(spec.clone())
}

#[cfg(not(feature = "ai"))]
fn ai_feature_disabled_error(spec: &BuiltinModelSpec) -> HookError {
    HookError::Usage(format!(
        "model `{}` requires a build with the `ai` feature; rebuild with `--features ai` or use `--hook`",
        spec.model
    ))
}

fn invoke_select_ref(
    backend: &ModelBackend,
    timeout_ms: u64,
    params: SelectRefParams<'_>,
    secrets: &mut Secrets,
) -> Result<SelectRefOutcome, HookError> {
    match backend {
        ModelBackend::Hook(hook) => model_hook::call_select_ref(hook, timeout_ms, params, secrets),
        #[cfg(feature = "ai")]
        ModelBackend::Builtin(spec) => model_ai::call_select_ref(spec, timeout_ms, params, secrets),
        #[cfg(not(feature = "ai"))]
        ModelBackend::Builtin(spec) => Err(ai_feature_disabled_error(spec)),
    }
}

fn invoke_locate(
    backend: &ModelBackend,
    timeout_ms: u64,
    params: LocateParams<'_>,
    secrets: &mut Secrets,
) -> Result<LocateOutcome, HookError> {
    match backend {
        ModelBackend::Hook(hook) => model_hook::call_locate(hook, timeout_ms, params, secrets),
        #[cfg(feature = "ai")]
        ModelBackend::Builtin(spec) => model_ai::call_locate(spec, timeout_ms, params, secrets),
        #[cfg(not(feature = "ai"))]
        ModelBackend::Builtin(spec) => Err(ai_feature_disabled_error(spec)),
    }
}

fn invoke_judge(
    backend: &ModelBackend,
    timeout_ms: u64,
    params: JudgeParams<'_>,
    secrets: &mut Secrets,
) -> Result<JudgeOutcome, HookError> {
    match backend {
        ModelBackend::Hook(hook) => model_hook::call_judge(hook, timeout_ms, params, secrets),
        #[cfg(feature = "ai")]
        ModelBackend::Builtin(spec) => model_ai::call_judge(spec, timeout_ms, params, secrets),
        #[cfg(not(feature = "ai"))]
        ModelBackend::Builtin(spec) => Err(ai_feature_disabled_error(spec)),
    }
}

fn invoke_extract(
    backend: &ModelBackend,
    timeout_ms: u64,
    params: ExtractParams<'_>,
    secrets: &mut Secrets,
) -> Result<model_hook::ExtractOutcome, HookError> {
    match backend {
        ModelBackend::Hook(hook) => model_hook::call_extract(hook, timeout_ms, params, secrets),
        #[cfg(feature = "ai")]
        ModelBackend::Builtin(spec) => model_ai::call_extract(spec, timeout_ms, params, secrets),
        #[cfg(not(feature = "ai"))]
        ModelBackend::Builtin(spec) => Err(ai_feature_disabled_error(spec)),
    }
}

fn invoke_act_step(
    backend: &ModelBackend,
    timeout_ms: u64,
    params: ActStepParams<'_>,
    secrets: &mut Secrets,
) -> Result<model_hook::ActStepOutcome, HookError> {
    match backend {
        ModelBackend::Hook(hook) => model_hook::call_act_step(hook, timeout_ms, params, secrets),
        #[cfg(feature = "ai")]
        ModelBackend::Builtin(spec) => model_ai::call_act_step(spec, timeout_ms, params, secrets),
        #[cfg(not(feature = "ai"))]
        ModelBackend::Builtin(spec) => Err(ai_feature_disabled_error(spec)),
    }
}

fn build_at_value(
    session_kind: &str,
    x: f64,
    y: f64,
    width: u32,
    height: u32,
) -> (Value, Value, Value, Value) {
    let viewport = json!({
        "width": width,
        "height": height,
    });
    let bounds_field = if session_kind == "desktop" {
        "window"
    } else {
        "viewport"
    };
    let x_val = if session_kind == "desktop" {
        json!(x.round() as i64)
    } else {
        coordinate_json_number(x)
    };
    let y_val = if session_kind == "desktop" {
        json!(y.round() as i64)
    } else {
        coordinate_json_number(y)
    };
    let mut at_map = Map::new();
    at_map.insert("x".to_string(), x_val.clone());
    at_map.insert("y".to_string(), y_val.clone());
    at_map.insert(bounds_field.to_string(), viewport.clone());
    (viewport, Value::Object(at_map), x_val, y_val)
}

fn detach_and_persist_handles(
    session_dir: &Path,
    state: &mut SessionState,
    mut sessions: Sessions,
    secrets: &Secrets,
) -> Vec<String> {
    let detached = sessions.detach_all();
    state.handles = detached.handles;
    state.last_active_ms = store::now_ms();
    let _ = store::save_state(session_dir, state, secrets);
    detached.errors
}

fn coordinate_json_number(value: f64) -> Value {
    let whole = value as i64;
    if value == whole as f64 {
        json!(whole)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

struct ModelDiagnosticSpec<'a> {
    index: usize,
    action: &'a str,
    raw_args: Value,
    status: &'a str,
    error: Option<String>,
    used_ref: Option<String>,
    synthesized_locator: Option<Value>,
    fragile: bool,
    observed_viewport: Option<Value>,
    started_at_ms: u64,
    duration_ms: u64,
    result: Value,
}

fn build_model_diagnostic_record(spec: ModelDiagnosticSpec<'_>, audit: &ModelAudit) -> StepRecord {
    StepRecord {
        index: spec.index,
        action: spec.action.to_string(),
        raw_args: spec.raw_args,
        redacted_args: false,
        expect: Vec::new(),
        save: BTreeMap::new(),
        optional: false,
        status: spec.status.to_string(),
        error: spec.error,
        used_ref: spec.used_ref,
        synthesized_locator: spec.synthesized_locator,
        fragile: spec.fragile,
        observed_viewport: spec.observed_viewport,
        started_at_ms: spec.started_at_ms,
        duration_ms: spec.duration_ms,
        result: Some(spec.result),
        model: Some(audit.model.clone()),
        latency_ms: Some(audit.latency_ms),
        usage: audit.usage.clone(),
        trace_id: audit.trace_id.clone(),
        confidence: audit.confidence,
    }
}

fn state_observations(state: &SessionState) -> Vec<Observation> {
    state
        .observe
        .iter()
        .map(|obs| Observation {
            name: obs.name.clone(),
            kind: obs.kind.clone(),
            args: obs.raw_args.clone(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
        })
        .collect()
}

fn handle_select_ref(req: SelectRefRequest, registry: &Registry) -> SessionResponse {
    if req.query.trim().is_empty() {
        return error_response(ExitCode::Usage, "usage", "`--query` must not be empty");
    }
    if req.timeout_ms == 0 {
        return error_response(
            ExitCode::Usage,
            "usage",
            "`--timeout-ms` must be at least 1",
        );
    }

    let (session_dir, _lock, mut state) = match acquire_existing(&req.sessions_dir, &req.id) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };

    state
        .env_names
        .extend(case::env_references(&Value::String(req.query.clone())));
    if let Some(act) = &req.act {
        if let Some(with_raw) = &act.with_raw
            && let Ok(parsed) = case::parse_json_value(with_raw)
        {
            state.env_names.extend(case::env_references(&parsed));
        }
        for (_, v) in &act.args {
            state
                .env_names
                .extend(case::env_references(&Value::String(v.clone())));
        }
    }

    let mut secrets = store::secrets_from_env_names(&state.env_names);
    let backend = match resolve_model_backend(
        req.hook_override.as_ref(),
        req.model_override.as_ref(),
        req.reasoning_effort_override.as_deref(),
        &mut state,
        &mut secrets,
    ) {
        Ok(backend) => backend,
        Err(resp) => return *resp,
    };
    let expanded_query = match case::expand_env(&req.query, &mut secrets) {
        Ok(q) => q,
        Err(msg) => {
            return error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("`--query`: {msg}")),
            );
        }
    };

    let effective_registry = match resolve_target_session_registry(&state.plugins, registry) {
        Ok(reg) => reg,
        Err((code, kind, message)) => return error_response(code, kind, secrets.redact(&message)),
    };

    let mut sessions =
        match reattach_sessions(&state, &req.base_directory, effective_registry.as_ref()) {
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
    let (session_kind, snapshot) = match sessions.collect_snapshot(
        req.scope_ref.as_deref(),
        req.max_depth,
        req.max_nodes,
    ) {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
            return error_response(
                ExitCode::Environment,
                "environment",
                "no active snapshot-capable session; run a browser or desktop step such as `browser.open` first",
            );
        }
        Err(message) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
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

    let candidates = model_hook::extract_snapshot_candidates(&snapshot);
    if candidates.is_empty() {
        let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
        return error_response(
            ExitCode::TestFailure,
            "execution",
            "active snapshot contains no addressable element refs; use `da session locate` for surfaces without accessibility nodes",
        );
    }

    let outcome = match invoke_select_ref(
        &backend,
        req.timeout_ms,
        SelectRefParams {
            session_id: &state.id,
            session_kind: &session_kind,
            query: &expanded_query,
            candidates: &candidates,
        },
        &mut secrets,
    ) {
        Ok(outcome) => outcome,
        Err(hook_err) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
            let (code, kind, msg) = hook_err.into_code_and_kind();
            return error_response(code, kind, secrets.redact(&msg));
        }
    };

    let synth_target = match sessions.synthesize_ref(Some(&session_kind), &outcome.selected_ref) {
        Ok(Some((_, target))) => Some(target),
        Ok(None) => None,
        Err(message) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let chosen_candidate = candidates
        .iter()
        .find(|c| c.get("ref").and_then(Value::as_str) == Some(&outcome.selected_ref))
        .cloned()
        .unwrap_or(Value::Null);

    let synth_locator = synth_target.as_ref().map(|t| t.locator.clone());
    let synth_fragile = synth_target.as_ref().is_some_and(|t| t.fragile);
    let diag_duration_ms = store::now_ms().saturating_sub(started_at_ms);

    let diag_result = json!({
        "ref": outcome.selected_ref,
        "candidate": chosen_candidate,
        "synthesized_locator": synth_locator,
        "fragile": synth_fragile,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });

    let mut existing_steps = store::load_steps(&session_dir).unwrap_or_default();
    let diag_record = build_model_diagnostic_record(
        ModelDiagnosticSpec {
            index: existing_steps.len() + 1,
            action: STEP_ACTION_SELECT_REF,
            raw_args: json!({ "query": secrets.redact(&req.query) }),
            status: "passed",
            error: None,
            used_ref: Some(outcome.selected_ref.clone()),
            synthesized_locator: synth_locator.clone(),
            fragile: synth_fragile,
            observed_viewport: None,
            started_at_ms,
            duration_ms: diag_duration_ms,
            result: diag_result,
        },
        &outcome.audit,
    );

    let mut act_report_opt: Option<(StepReport, Option<Value>, StepRecord)> = None;
    if let Some(act_spec) = req.act {
        let mut pairs = act_spec.args;
        if !pairs.iter().any(|(k, _)| k == "ref") {
            pairs.insert(0, ("ref".to_string(), outcome.selected_ref.clone()));
        }
        let step = match build_step_from_flags(
            effective_registry.as_ref(),
            &act_spec.action,
            act_spec.with_raw.as_deref(),
            &pairs,
            &act_spec.save,
        ) {
            Ok(step) => step,
            Err(message) => {
                let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                return error_response(
                    ExitCode::InvalidCase,
                    "invalid_input",
                    secrets.redact(&message),
                );
            }
        };
        state.env_names.extend(case::env_references(&step.args));
        let act_started_ms = store::now_ms();
        let observations = state_observations(&state);
        let report = runner::run_single_step_with_observations(
            &step,
            &observations,
            effective_registry.as_ref(),
            &mut state.vars,
            &mut secrets,
            &mut sessions,
        );
        let step_meta = sessions.take_step_metadata();
        let mut synthesis = extract_synthesis(&step, &step_meta);
        if synthesis.synthesized_locator.is_none() {
            synthesis.synthesized_locator = synth_locator.clone();
            synthesis.fragile = synth_fragile;
        }
        let post_snap = if !matches!(
            report.error.as_ref().map(|p| p.kind),
            Some(ProblemKind::Environment)
        ) {
            sessions
                .collect_step_snapshot()
                .ok()
                .flatten()
                .map(|(_, s)| s)
        } else {
            None
        };
        let mut step_rec = build_step_record(
            existing_steps.len() + 2,
            &step,
            &report,
            effective_registry.as_ref(),
            act_started_ms,
            synthesis,
        );
        step_rec.model = Some(outcome.audit.model.clone());
        step_rec.latency_ms = Some(outcome.audit.latency_ms);
        step_rec.usage = outcome.audit.usage.clone();
        step_rec.trace_id = outcome.audit.trace_id.clone();
        step_rec.confidence = outcome.audit.confidence;
        act_report_opt = Some((report, post_snap, step_rec));
    }

    let local_only_reason = sessions.local_only_reason();
    let detached = sessions.detach_all();
    state.handles = detached.handles;
    state.last_active_ms = store::now_ms();

    if let Err(err) = store::append_step(&session_dir, &diag_record, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }
    existing_steps.push(diag_record);

    let mut exit_code = if detached.errors.is_empty() {
        ExitCode::Success
    } else {
        ExitCode::Environment
    };
    let mut ok = detached.errors.is_empty();

    let mut payload = json!({
        "ok": ok,
        "session_id": state.id,
        "session_kind": session_kind,
        "ref": outcome.selected_ref,
        "candidate": chosen_candidate,
        "synthesized_locator": synth_locator,
        "fragile": synth_fragile,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });

    if let Some((report, post_snap, step_rec)) = act_report_opt {
        if let Err(err) = store::append_step(&session_dir, &step_rec, &secrets) {
            return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
        }
        let step_ok = matches!(report.status, StepStatus::Passed | StepStatus::Skipped);
        ok = ok && step_ok;
        if !step_ok {
            exit_code = exit_code_for_step(&report);
        }
        payload["ok"] = json!(ok);
        payload["step"] = output::step_json(&report);
        if let Some(snap) = post_snap {
            payload["snapshot"] = snap;
        }
    }

    if let Err(err) = store::save_state(&session_dir, &state, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
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
        raw_text: None,
    }
}

fn handle_locate(req: LocateRequest, registry: &Registry) -> SessionResponse {
    if req.query.trim().is_empty() {
        return error_response(ExitCode::Usage, "usage", "`--query` must not be empty");
    }
    if req.timeout_ms == 0 {
        return error_response(
            ExitCode::Usage,
            "usage",
            "`--timeout-ms` must be at least 1",
        );
    }

    let (session_dir, _lock, mut state) = match acquire_existing(&req.sessions_dir, &req.id) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };

    state
        .env_names
        .extend(case::env_references(&Value::String(req.query.clone())));
    let mut secrets = store::secrets_from_env_names(&state.env_names);
    let backend = match resolve_model_backend(
        req.hook_override.as_ref(),
        req.model_override.as_ref(),
        req.reasoning_effort_override.as_deref(),
        &mut state,
        &mut secrets,
    ) {
        Ok(backend) => backend,
        Err(resp) => return *resp,
    };
    let expanded_query = match case::expand_env(&req.query, &mut secrets) {
        Ok(q) => q,
        Err(msg) => {
            return error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("`--query`: {msg}")),
            );
        }
    };

    let effective_registry = match resolve_target_session_registry(&state.plugins, registry) {
        Ok(reg) => reg,
        Err((code, kind, message)) => return error_response(code, kind, secrets.redact(&message)),
    };

    let mut sessions =
        match reattach_sessions(&state, &req.base_directory, effective_registry.as_ref()) {
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
    let (session_kind, capture) = match sessions.collect_masked_viewport_capture() {
        Ok(Some(pair)) => pair,
        Ok(None) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
            return error_response(
                ExitCode::Environment,
                "environment",
                "no active visual session; run `browser.open` or `desktop.launch` before `da session locate`",
            );
        }
        Err(message) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let outcome = match invoke_locate(
        &backend,
        req.timeout_ms,
        LocateParams {
            session_id: &state.id,
            session_kind: &session_kind,
            query: &expanded_query,
            masked_png: &capture.png,
            width: capture.width,
            height: capture.height,
        },
        &mut secrets,
    ) {
        Ok(outcome) => outcome,
        Err(hook_err) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
            let (code, kind, msg) = hook_err.into_code_and_kind();
            return error_response(code, kind, secrets.redact(&msg));
        }
    };

    let synth_target = match sessions.synthesize_at_point(Some(&session_kind), outcome.x, outcome.y)
    {
        Ok(Some((_, target))) => Some(target),
        Ok(None) => None,
        Err(message) => {
            let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let (viewport, at_value, x_val, y_val) = build_at_value(
        &session_kind,
        outcome.x,
        outcome.y,
        capture.width,
        capture.height,
    );

    let synth_locator = synth_target.as_ref().map(|t| t.locator.clone());
    let fragile = match &synth_target {
        Some(t) => t.fragile,
        None => true,
    };
    let diag_duration_ms = store::now_ms().saturating_sub(started_at_ms);

    let diag_result = json!({
        "x": x_val,
        "y": y_val,
        "viewport": viewport,
        "at": at_value,
        "synthesized_locator": synth_locator,
        "fragile": fragile,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });

    let existing_steps = store::load_steps(&session_dir).unwrap_or_default();
    let diag_record = build_model_diagnostic_record(
        ModelDiagnosticSpec {
            index: existing_steps.len() + 1,
            action: STEP_ACTION_LOCATE,
            raw_args: json!({ "query": secrets.redact(&req.query) }),
            status: "passed",
            error: None,
            used_ref: None,
            synthesized_locator: synth_locator.clone(),
            fragile,
            observed_viewport: Some(viewport.clone()),
            started_at_ms,
            duration_ms: diag_duration_ms,
            result: diag_result,
        },
        &outcome.audit,
    );

    let mut click_outcome_opt: Option<(StepReport, Option<Value>, StepRecord)> = None;
    if req.click {
        let click_action = if session_kind == "desktop" {
            "desktop.click"
        } else {
            "browser.click"
        };
        let step_json = json!({
            click_action: {
                "at": at_value,
            }
        });
        let step_text = step_json.to_string();
        let step = match case::parse_single_step(&step_text) {
            Ok(step) => step,
            Err(message) => {
                let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                return error_response(
                    ExitCode::InvalidCase,
                    "invalid_input",
                    secrets.redact(&message),
                );
            }
        };
        click_outcome_opt = Some(execute_synthesized_step(
            &step,
            existing_steps.len() + 2,
            Some(&viewport),
            synth_target.as_ref(),
            Some(&outcome.audit),
            effective_registry.as_ref(),
            &mut state,
            &mut secrets,
            &mut sessions,
        ));
    }

    let local_only_reason = sessions.local_only_reason();
    let detached = sessions.detach_all();
    state.handles = detached.handles;
    state.last_active_ms = store::now_ms();

    if let Err(err) = store::append_step(&session_dir, &diag_record, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }

    let mut exit_code = if detached.errors.is_empty() {
        ExitCode::Success
    } else {
        ExitCode::Environment
    };
    let mut ok = detached.errors.is_empty();

    let mut payload = json!({
        "ok": ok,
        "session_id": state.id,
        "session_kind": session_kind,
        "x": x_val,
        "y": y_val,
        "viewport": viewport,
        "at": at_value,
        "synthesized_locator": synth_locator,
        "fragile": fragile,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });

    if let Some((report, post_snap, step_rec)) = click_outcome_opt {
        if let Err(err) = store::append_step(&session_dir, &step_rec, &secrets) {
            return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
        }
        let step_ok = matches!(report.status, StepStatus::Passed | StepStatus::Skipped);
        ok = ok && step_ok;
        if !step_ok {
            exit_code = exit_code_for_step(&report);
        }
        payload["ok"] = json!(ok);
        payload["step"] = output::step_json(&report);
        if let Some(snap) = post_snap {
            payload["snapshot"] = snap;
        }
    }

    if let Err(err) = store::save_state(&session_dir, &state, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
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
        raw_text: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_synthesized_step(
    step: &Step,
    step_index: usize,
    viewport_opt: Option<&Value>,
    fallback_synth: Option<&SynthesizedTarget>,
    audit: Option<&ModelAudit>,
    effective_registry: &Registry,
    state: &mut SessionState,
    secrets: &mut Secrets,
    sessions: &mut Sessions,
) -> (StepReport, Option<Value>, StepRecord) {
    let started_ms = store::now_ms();
    let observations = state_observations(state);
    let report = runner::run_single_step_with_observations(
        step,
        &observations,
        effective_registry,
        &mut state.vars,
        secrets,
        sessions,
    );
    let step_meta = sessions.take_step_metadata();
    let mut synthesis = extract_synthesis(step, &step_meta);
    if synthesis.synthesized_locator.is_none()
        && let Some(target) = fallback_synth
    {
        synthesis.synthesized_locator = Some(target.locator.clone());
        synthesis.fragile = target.fragile;
    }
    let post_snap = if !matches!(
        report.error.as_ref().map(|p| p.kind),
        Some(ProblemKind::Environment)
    ) {
        sessions
            .collect_step_snapshot()
            .ok()
            .flatten()
            .map(|(_, s)| s)
    } else {
        None
    };
    let mut step_rec = build_step_record(
        step_index,
        step,
        &report,
        effective_registry,
        started_ms,
        synthesis,
    );
    if step_rec.observed_viewport.is_none()
        && let Some(vp) = viewport_opt
    {
        step_rec.observed_viewport = Some(vp.clone());
    }
    if let Some(audit) = audit {
        step_rec.model = Some(audit.model.clone());
        step_rec.latency_ms = Some(audit.latency_ms);
        step_rec.usage = audit.usage.clone();
        step_rec.trace_id = audit.trace_id.clone();
        step_rec.confidence = audit.confidence;
    }
    (report, post_snap, step_rec)
}

struct VisualContext {
    session_kind: String,
    snapshot: Option<Value>,
    masked_capture: Option<MaskedViewportCapture>,
    detach_errors: Vec<String>,
    local_only_reason: Option<String>,
}

fn resolve_session_evidence(
    explicit: Option<&Value>,
    flag_name: &str,
    existing_steps: &[StepRecord],
    state: &SessionState,
    secrets: &mut Secrets,
) -> Result<Option<Value>, Box<SessionResponse>> {
    match explicit {
        Some(raw_ev) => match case::interpolate(raw_ev, &state.vars, secrets) {
            Ok(interpolated) => Ok(Some(secrets.redact_json(&interpolated))),
            Err(err) => Err(Box::new(error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("`{flag_name}`: {}", err.message)),
            ))),
        },
        None => {
            if existing_steps.is_empty() && state.vars.is_empty() {
                Ok(None)
            } else {
                let summarized: Vec<Value> = existing_steps
                    .iter()
                    .map(|rec| {
                        json!({
                            "index": rec.index,
                            "action": rec.action,
                            "status": rec.status,
                            "error": rec.error,
                            "result": rec.result,
                        })
                    })
                    .collect();
                Ok(Some(secrets.redact_json(&json!({
                    "steps": summarized,
                    "vars": state.vars,
                }))))
            }
        }
    }
}

fn capture_snapshot_and_screenshot(
    session_dir: &Path,
    state: &mut SessionState,
    base_directory: &Path,
    registry: &Registry,
    screenshot: bool,
    op_name: &str,
    secrets: &Secrets,
) -> Result<VisualContext, Box<SessionResponse>> {
    if screenshot && state.handles.is_empty() {
        return Err(Box::new(error_response(
            ExitCode::Environment,
            "environment",
            format!(
                "no active visual session; run `browser.open` or `desktop.launch` before calling `{op_name}` with `--screenshot`"
            ),
        )));
    }
    if state.handles.is_empty() {
        return Ok(VisualContext {
            session_kind: "session".to_string(),
            snapshot: None,
            masked_capture: None,
            detach_errors: Vec::new(),
            local_only_reason: None,
        });
    }

    let fallback_kind = state
        .handles
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "session".to_string());
    let effective_registry = resolve_target_session_registry(&state.plugins, registry).map_err(
        |(code, kind, message)| Box::new(error_response(code, kind, secrets.redact(&message))),
    )?;
    let mut sessions = reattach_sessions(state, base_directory, effective_registry.as_ref())
        .map_err(|message| {
            Box::new(error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            ))
        })?;

    let snap_res = sessions.collect_snapshot(None, None, None);
    let cap_res = if screenshot {
        Some(sessions.collect_masked_viewport_capture())
    } else {
        None
    };
    let local_only_reason = sessions.local_only_reason();
    let detached = sessions.detach_all();
    state.handles = detached.handles;

    let (session_kind, snapshot, masked_capture) = if let Some(cap_result) = cap_res {
        match cap_result {
            Ok(Some((cap_kind, cap))) => {
                let (kind, snap) = match snap_res {
                    Ok(Some((kind, snap))) => (kind, Some(snap)),
                    Ok(None) | Err(_) => (cap_kind, None),
                };
                (kind, snap, Some(cap))
            }
            Ok(None) => {
                state.last_active_ms = store::now_ms();
                let _ = store::save_state(session_dir, state, secrets);
                return Err(Box::new(error_response(
                    ExitCode::Environment,
                    "environment",
                    "active session has no visual surface for `--screenshot`",
                )));
            }
            Err(message) => {
                state.last_active_ms = store::now_ms();
                let _ = store::save_state(session_dir, state, secrets);
                return Err(Box::new(error_response(
                    ExitCode::Environment,
                    "environment",
                    secrets.redact(&message),
                )));
            }
        }
    } else {
        let (kind, snap) = match snap_res {
            Ok(Some((kind, snap))) => (kind, Some(snap)),
            Ok(None) | Err(_) => (fallback_kind, None),
        };
        (kind, snap, None)
    };

    Ok(VisualContext {
        session_kind,
        snapshot,
        masked_capture,
        detach_errors: detached.errors,
        local_only_reason,
    })
}

fn validate_var_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(char::is_whitespace)
        || trimmed.contains('=')
        || trimmed.contains('$')
        || trimmed.contains('{')
        || trimmed.contains('}')
    {
        return Err(format!("invalid `--save` variable name `{name}`"));
    }
    Ok(trimmed.to_string())
}

fn value_to_var_string(value: &Value, secrets: &Secrets) -> String {
    match value {
        Value::String(text) => secrets.redact(text),
        other => secrets.redact(&other.to_string()),
    }
}

fn handle_judge(req: JudgeRequest, registry: &Registry) -> SessionResponse {
    if req.rubric.trim().is_empty() {
        return error_response(ExitCode::Usage, "usage", "`--rubric` must not be empty");
    }
    if req.timeout_ms == 0 {
        return error_response(
            ExitCode::Usage,
            "usage",
            "`--timeout-ms` must be at least 1",
        );
    }

    let (session_dir, _lock, mut state) = match acquire_existing(&req.sessions_dir, &req.id) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };

    state
        .env_names
        .extend(case::env_references(&Value::String(req.rubric.clone())));
    if let Some(ev) = &req.evidence {
        state.env_names.extend(case::env_references(ev));
    }

    let mut secrets = store::secrets_from_env_names(&state.env_names);
    let backend = match resolve_model_backend(
        req.hook_override.as_ref(),
        req.model_override.as_ref(),
        req.reasoning_effort_override.as_deref(),
        &mut state,
        &mut secrets,
    ) {
        Ok(backend) => backend,
        Err(resp) => return *resp,
    };
    let expanded_rubric = match case::expand_env(&req.rubric, &mut secrets) {
        Ok(r) => r,
        Err(msg) => {
            return error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("`--rubric`: {msg}")),
            );
        }
    };

    let existing_steps = match store::load_steps(&session_dir) {
        Ok(steps) => steps,
        Err(message) => {
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let resolved_evidence = match resolve_session_evidence(
        req.evidence.as_ref(),
        "--evidence",
        &existing_steps,
        &state,
        &mut secrets,
    ) {
        Ok(ev) => ev,
        Err(resp) => return *resp,
    };

    let started_at_ms = store::now_ms();
    let visual = match capture_snapshot_and_screenshot(
        &session_dir,
        &mut state,
        &req.base_directory,
        registry,
        req.screenshot,
        "judge",
        &secrets,
    ) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    let outcome = match invoke_judge(
        &backend,
        req.timeout_ms,
        JudgeParams {
            session_id: &state.id,
            session_kind: &visual.session_kind,
            rubric: &expanded_rubric,
            snapshot: visual.snapshot.as_ref(),
            evidence: resolved_evidence.as_ref(),
            masked_capture: visual.masked_capture.as_ref(),
        },
        &mut secrets,
    ) {
        Ok(outcome) => outcome,
        Err(hook_err) => {
            state.last_active_ms = store::now_ms();
            let _ = store::save_state(&session_dir, &state, &secrets);
            let (code, kind, msg) = hook_err.into_code_and_kind();
            return error_response(code, kind, secrets.redact(&msg));
        }
    };

    let duration_ms = store::now_ms().saturating_sub(started_at_ms);
    let diag_result = json!({
        "passed": outcome.passed,
        "rationale": outcome.rationale,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });
    let raw_args = if req.screenshot {
        json!({ "rubric": secrets.redact(&req.rubric), "screenshot": true })
    } else {
        json!({ "rubric": secrets.redact(&req.rubric) })
    };
    let diag_record = build_model_diagnostic_record(
        ModelDiagnosticSpec {
            index: existing_steps.len() + 1,
            action: STEP_ACTION_JUDGE,
            raw_args,
            status: if outcome.passed { "passed" } else { "failed" },
            error: if outcome.passed {
                None
            } else {
                Some(outcome.rationale.clone())
            },
            used_ref: None,
            synthesized_locator: None,
            fragile: false,
            observed_viewport: visual.masked_capture.as_ref().map(|cap| {
                json!({
                    "width": cap.width,
                    "height": cap.height,
                })
            }),
            started_at_ms,
            duration_ms,
            result: diag_result,
        },
        &outcome.audit,
    );

    state.last_active_ms = store::now_ms();
    if let Err(err) = store::append_step(&session_dir, &diag_record, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }
    if let Err(err) = store::save_state(&session_dir, &state, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }

    let ok = outcome.passed && visual.detach_errors.is_empty();
    let exit_code = if !visual.detach_errors.is_empty() && outcome.passed {
        ExitCode::Environment
    } else if outcome.passed {
        ExitCode::Success
    } else {
        ExitCode::TestFailure
    };

    let mut payload = json!({
        "ok": ok,
        "session_id": state.id,
        "session_kind": visual.session_kind,
        "passed": outcome.passed,
        "rationale": outcome.rationale,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });
    if let Some(cap) = &visual.masked_capture {
        payload["screenshot"] = json!(true);
        payload["viewport"] = json!({ "width": cap.width, "height": cap.height });
    }
    if !visual.detach_errors.is_empty() {
        payload["detach_errors"] = json!(visual.detach_errors);
    }
    if let Some(reason) = visual.local_only_reason {
        payload["local_only"] = json!(true);
        payload["local_only_reason"] = json!(reason);
    }

    let verdict_label = if outcome.passed { "passed" } else { "failed" };
    let mut text = format!(
        "verdict: {verdict_label} (model: {}, latency_ms: {})\nrationale: {}\n",
        outcome.audit.model, outcome.audit.latency_ms, outcome.rationale
    );
    if let Some(conf) = outcome.audit.confidence {
        text.push_str(&format!("confidence: {conf}\n"));
    }
    if let Some(tid) = &outcome.audit.trace_id {
        text.push_str(&format!("trace_id: {tid}\n"));
    }

    SessionResponse {
        exit_code,
        payload: secrets.redact_json(&payload),
        raw_yaml: None,
        raw_text: Some(secrets.redact(&text)),
    }
}

fn handle_extract(req: ExtractRequest, registry: &Registry) -> SessionResponse {
    if req.query.trim().is_empty() {
        return error_response(ExitCode::Usage, "usage", "`--query` must not be empty");
    }
    if req.timeout_ms == 0 {
        return error_response(
            ExitCode::Usage,
            "usage",
            "`--timeout-ms` must be at least 1",
        );
    }
    if let Some(schema) = &req.schema
        && !schema.is_object()
    {
        return error_response(
            ExitCode::Usage,
            "usage",
            "`--schema` must be a JSON Schema object",
        );
    }
    let save_var = match req.save.as_deref() {
        Some(raw) => match validate_var_name(raw) {
            Ok(name) => Some(name),
            Err(msg) => return error_response(ExitCode::Usage, "usage", msg),
        },
        None => None,
    };

    let (session_dir, _lock, mut state) = match acquire_existing(&req.sessions_dir, &req.id) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };

    state
        .env_names
        .extend(case::env_references(&Value::String(req.query.clone())));
    if let Some(ev) = &req.evidence {
        state.env_names.extend(case::env_references(ev));
    }
    if let Some(schema) = &req.schema {
        state.env_names.extend(case::env_references(schema));
    }

    let mut secrets = store::secrets_from_env_names(&state.env_names);
    let backend = match resolve_model_backend(
        req.hook_override.as_ref(),
        req.model_override.as_ref(),
        req.reasoning_effort_override.as_deref(),
        &mut state,
        &mut secrets,
    ) {
        Ok(backend) => backend,
        Err(resp) => return *resp,
    };
    let expanded_query = match case::expand_env(&req.query, &mut secrets) {
        Ok(q) => q,
        Err(msg) => {
            return error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("`--query`: {msg}")),
            );
        }
    };

    let existing_steps = match store::load_steps(&session_dir) {
        Ok(steps) => steps,
        Err(message) => {
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };

    let resolved_evidence = match resolve_session_evidence(
        req.evidence.as_ref(),
        "--evidence",
        &existing_steps,
        &state,
        &mut secrets,
    ) {
        Ok(ev) => ev,
        Err(resp) => return *resp,
    };
    let redacted_schema = req.schema.as_ref().map(|s| secrets.redact_json(s));

    let started_at_ms = store::now_ms();
    let visual = match capture_snapshot_and_screenshot(
        &session_dir,
        &mut state,
        &req.base_directory,
        registry,
        req.screenshot,
        "extract",
        &secrets,
    ) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };

    let outcome = match invoke_extract(
        &backend,
        req.timeout_ms,
        ExtractParams {
            session_id: &state.id,
            session_kind: &visual.session_kind,
            query: &expanded_query,
            schema: redacted_schema.as_ref(),
            snapshot: visual.snapshot.as_ref(),
            evidence: resolved_evidence.as_ref(),
            masked_capture: visual.masked_capture.as_ref(),
        },
        &mut secrets,
    ) {
        Ok(outcome) => outcome,
        Err(hook_err) => {
            state.last_active_ms = store::now_ms();
            let _ = store::save_state(&session_dir, &state, &secrets);
            let (code, kind, msg) = hook_err.into_code_and_kind();
            return error_response(code, kind, secrets.redact(&msg));
        }
    };

    let mut saved_map = BTreeMap::new();
    if let Some(var_name) = save_var {
        let safe_var = secrets.redact(&var_name);
        let stored_val = value_to_var_string(&outcome.data, &secrets);
        state.vars.insert(safe_var.clone(), stored_val.clone());
        saved_map.insert(safe_var, stored_val);
    }

    let duration_ms = store::now_ms().saturating_sub(started_at_ms);
    let mut raw_args = Map::new();
    raw_args.insert(
        "query".to_string(),
        Value::String(secrets.redact(&req.query)),
    );
    if let Some(schema) = &redacted_schema {
        raw_args.insert("schema".to_string(), schema.clone());
    }
    if req.screenshot {
        raw_args.insert("screenshot".to_string(), Value::Bool(true));
    }

    let diag_result = json!({
        "data": outcome.data,
        "saved": saved_map,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });
    let diag_record = build_model_diagnostic_record(
        ModelDiagnosticSpec {
            index: existing_steps.len() + 1,
            action: STEP_ACTION_EXTRACT,
            raw_args: Value::Object(raw_args),
            status: "passed",
            error: None,
            used_ref: None,
            synthesized_locator: None,
            fragile: false,
            observed_viewport: visual.masked_capture.as_ref().map(|cap| {
                json!({
                    "width": cap.width,
                    "height": cap.height,
                })
            }),
            started_at_ms,
            duration_ms,
            result: diag_result,
        },
        &outcome.audit,
    );

    state.last_active_ms = store::now_ms();
    if let Err(err) = store::append_step(&session_dir, &diag_record, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }
    if let Err(err) = store::save_state(&session_dir, &state, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }

    let ok = visual.detach_errors.is_empty();
    let exit_code = if ok {
        ExitCode::Success
    } else {
        ExitCode::Environment
    };

    let mut payload = json!({
        "ok": ok,
        "session_id": state.id,
        "session_kind": visual.session_kind,
        "data": outcome.data,
        "saved": saved_map,
        "confidence": outcome.audit.confidence,
        "model": outcome.audit.model,
        "latency_ms": outcome.audit.latency_ms,
        "usage": outcome.audit.usage,
        "trace_id": outcome.audit.trace_id,
    });
    if let Some(cap) = &visual.masked_capture {
        payload["screenshot"] = json!(true);
        payload["viewport"] = json!({ "width": cap.width, "height": cap.height });
    }
    if !visual.detach_errors.is_empty() {
        payload["detach_errors"] = json!(visual.detach_errors);
    }
    if let Some(reason) = visual.local_only_reason {
        payload["local_only"] = json!(true);
        payload["local_only_reason"] = json!(reason);
    }

    let rendered_data =
        serde_json::to_string_pretty(&outcome.data).unwrap_or_else(|_| outcome.data.to_string());
    let mut text = format!(
        "extract (model: {}, latency_ms: {})\ndata: {}\n",
        outcome.audit.model, outcome.audit.latency_ms, rendered_data
    );
    if !saved_map.is_empty() {
        for (k, v) in &saved_map {
            text.push_str(&format!("saved: {k} = {v}\n"));
        }
    }

    SessionResponse {
        exit_code,
        payload: secrets.redact_json(&payload),
        raw_yaml: None,
        raw_text: Some(secrets.redact(&text)),
    }
}

fn act_available_actions(registry: &Registry, session_kind: &str) -> Vec<String> {
    registry
        .names()
        .into_iter()
        .filter(|name| !name.starts_with("model."))
        .filter(|name| match session_kind {
            "browser" => !name.starts_with("desktop."),
            "desktop" => !name.starts_with("browser."),
            _ => true,
        })
        .map(str::to_string)
        .collect()
}

fn act_probe_visual_point_step(
    kind: &str,
    x: f64,
    y: f64,
    text: Option<&str>,
    session_kind: &str,
    cached_capture: Option<&MaskedViewportCapture>,
    sessions: &mut Sessions,
) -> Result<(Step, Value, Option<SynthesizedTarget>), HookError> {
    if !matches!(session_kind, "browser" | "desktop") {
        return Err(HookError::Environment(format!(
            "visual coordinate action `{kind}` requires an active browser or desktop session"
        )));
    }
    let (width, height) = if let Some(cap) = cached_capture {
        (cap.width, cap.height)
    } else {
        let Some((_cap_kind, cap)) = sessions
            .collect_masked_viewport_capture()
            .map_err(HookError::Environment)?
        else {
            return Err(HookError::Environment(format!(
                "active `{session_kind}` session has no visual viewport for `{kind}`"
            )));
        };
        (cap.width, cap.height)
    };
    validate_point_in_viewport(x, y, width, height, model_hook::METHOD_ACT_STEP)?;
    let synth_target = sessions
        .synthesize_at_point(Some(session_kind), x, y)
        .map_err(HookError::Environment)?
        .map(|(_, target)| target);
    let (viewport, at_value, _, _) = build_at_value(session_kind, x, y, width, height);

    let step_name = match (session_kind, text.is_some()) {
        ("desktop", false) => "desktop.click",
        ("desktop", true) => "desktop.type",
        (_, false) => "browser.click",
        (_, true) => "browser.type",
    };
    let mut step_args = Map::new();
    step_args.insert("at".to_string(), at_value);
    if let Some(txt) = text {
        step_args.insert("text".to_string(), Value::String(txt.to_string()));
    }
    let mut outer = Map::new();
    outer.insert(step_name.to_string(), Value::Object(step_args));
    let step = case::parse_single_step(&Value::Object(outer).to_string())
        .map_err(HookError::Environment)?;
    Ok((step, viewport, synth_target))
}

#[allow(clippy::too_many_lines)]
fn handle_act(req: ActRequest, registry: &Registry) -> SessionResponse {
    if req.goal.trim().is_empty() {
        return error_response(ExitCode::Usage, "usage", "`--goal` must not be empty");
    }
    if req.max_steps == 0 || req.max_steps > MAX_ACT_MAX_STEPS {
        return error_response(
            ExitCode::Usage,
            "usage",
            format!("`--max-steps` must be between 1 and {MAX_ACT_MAX_STEPS}"),
        );
    }
    if req.timeout_ms == 0 {
        return error_response(
            ExitCode::Usage,
            "usage",
            "`--timeout-ms` must be at least 1",
        );
    }

    let (session_dir, _lock, mut state) = match acquire_existing(&req.sessions_dir, &req.id) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };

    state
        .env_names
        .extend(case::env_references(&Value::String(req.goal.clone())));
    if let Some(ev) = &req.evidence {
        state.env_names.extend(case::env_references(ev));
    }

    let mut secrets = store::secrets_from_env_names(&state.env_names);
    let backend = match resolve_model_backend(
        req.hook_override.as_ref(),
        req.model_override.as_ref(),
        req.reasoning_effort_override.as_deref(),
        &mut state,
        &mut secrets,
    ) {
        Ok(backend) => backend,
        Err(resp) => return *resp,
    };
    let expanded_goal = match case::expand_env(&req.goal, &mut secrets) {
        Ok(g) => g,
        Err(msg) => {
            return error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("`--goal`: {msg}")),
            );
        }
    };

    if req.screenshot && state.handles.is_empty() {
        return error_response(
            ExitCode::Environment,
            "environment",
            "no active visual session; run `browser.open` or `desktop.launch` before calling `act` with `--screenshot`",
        );
    }

    let effective_registry = match resolve_target_session_registry(&state.plugins, registry) {
        Ok(reg) => reg,
        Err((code, kind, message)) => return error_response(code, kind, secrets.redact(&message)),
    };

    let explicit_evidence = match &req.evidence {
        Some(raw_ev) => match case::interpolate(raw_ev, &state.vars, &mut secrets) {
            Ok(interpolated) => Some(secrets.redact_json(&interpolated)),
            Err(err) => {
                return error_response(
                    ExitCode::InvalidCase,
                    "invalid_input",
                    secrets.redact(&format!("`--evidence`: {}", err.message)),
                );
            }
        },
        None => None,
    };

    let existing_steps = match store::load_steps(&session_dir) {
        Ok(steps) => steps,
        Err(message) => {
            return error_response(
                ExitCode::Environment,
                "environment",
                secrets.redact(&message),
            );
        }
    };
    let mut next_step_index = existing_steps.len() + 1;

    let mut sessions =
        match reattach_sessions(&state, &req.base_directory, effective_registry.as_ref()) {
            Ok(s) => s,
            Err(message) => {
                return error_response(
                    ExitCode::Environment,
                    "environment",
                    secrets.redact(&message),
                );
            }
        };

    let act_started_ms = store::now_ms();
    let mut history: Vec<Value> = Vec::new();
    let mut executed_steps: Vec<Value> = Vec::new();
    let mut final_status = "max_steps_exceeded";
    let mut final_summary = String::new();
    let mut final_reason: Option<String> = None;
    let mut final_session_kind = state
        .handles
        .keys()
        .next()
        .cloned()
        .unwrap_or_else(|| "session".to_string());
    let mut last_viewport: Option<Value> = None;
    let mut last_audit: Option<ModelAudit> = None;
    let mut total_model_latency_ms: u64 = 0;

    for step_num in 1..=req.max_steps {
        let (mut session_kind, snapshot) = match sessions.collect_snapshot(None, None, None) {
            Ok(Some((kind, snap))) => (kind, Some(secrets.redact_json(&snap))),
            Ok(None) | Err(_) => (final_session_kind.clone(), None),
        };
        let masked_capture = if req.screenshot {
            match sessions.collect_masked_viewport_capture() {
                Ok(Some((cap_kind, cap))) => {
                    if session_kind == "session" {
                        session_kind = cap_kind;
                    }
                    last_viewport = Some(json!({ "width": cap.width, "height": cap.height }));
                    Some(cap)
                }
                Ok(None) => {
                    let _ =
                        detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                    return error_response(
                        ExitCode::Environment,
                        "environment",
                        "active session has no visual surface for `--screenshot`",
                    );
                }
                Err(message) => {
                    let _ =
                        detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                    return error_response(
                        ExitCode::Environment,
                        "environment",
                        secrets.redact(&message),
                    );
                }
            }
        } else {
            None
        };
        final_session_kind = session_kind.clone();

        let available_actions = act_available_actions(effective_registry.as_ref(), &session_kind);
        let dyn_evidence = explicit_evidence.clone().or_else(|| {
            if state.vars.is_empty() {
                None
            } else {
                Some(secrets.redact_json(&json!({ "vars": state.vars })))
            }
        });

        let outcome = match invoke_act_step(
            &backend,
            req.timeout_ms,
            ActStepParams {
                session_id: &state.id,
                session_kind: &session_kind,
                goal: &expanded_goal,
                step_index: step_num,
                max_steps: req.max_steps,
                available_actions: &available_actions,
                history: &history,
                snapshot: snapshot.as_ref(),
                evidence: dyn_evidence.as_ref(),
                masked_capture: masked_capture.as_ref(),
            },
            &mut secrets,
        ) {
            Ok(outcome) => outcome,
            Err(hook_err) => {
                let _ = detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                let (code, kind, msg) = hook_err.into_code_and_kind();
                return error_response(code, kind, secrets.redact(&msg));
            }
        };

        total_model_latency_ms = total_model_latency_ms.saturating_add(outcome.audit.latency_ms);
        final_summary = outcome.summary.clone();
        last_audit = Some(outcome.audit.clone());

        match outcome.status {
            ActStepStatus::Done => {
                final_status = "done";
                final_reason = outcome.reason;
                break;
            }
            ActStepStatus::Failed => {
                final_status = "failed";
                final_reason = outcome.reason.or(Some(outcome.summary));
                break;
            }
            ActStepStatus::Continue => {
                let Some(act_action) = outcome.action else {
                    let _ =
                        detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                    return error_response(
                        ExitCode::Environment,
                        "environment",
                        "model `model/act_step` returned `continue` without an `action`",
                    );
                };

                struct ActStepSetup {
                    step: Step,
                    viewport: Option<Value>,
                    synth: Option<SynthesizedTarget>,
                    kind_label: &'static str,
                }

                let step_setup: Result<ActStepSetup, HookError> = match act_action {
                    ActStepAction::Step { use_action, with } => {
                        if use_action.starts_with("model.")
                            || effective_registry.get(&use_action).is_none()
                        {
                            Err(HookError::Environment(secrets.redact(&format!(
                                "model `model/act_step` requested unavailable or disallowed step action `{use_action}`"
                            ))))
                        } else {
                            state.env_names.extend(case::env_references(&with));
                            secrets = store::secrets_from_env_names(&state.env_names);
                            let step_json = json!({ use_action: with });
                            case::parse_single_step(&step_json.to_string())
                                .map(|step| ActStepSetup {
                                    step,
                                    viewport: None,
                                    synth: None,
                                    kind_label: "step",
                                })
                                .map_err(|msg| HookError::Environment(secrets.redact(&msg)))
                        }
                    }
                    ActStepAction::LocateClick { x, y } => act_probe_visual_point_step(
                        "locate_click",
                        x,
                        y,
                        None,
                        &session_kind,
                        masked_capture.as_ref(),
                        &mut sessions,
                    )
                    .map(|(step, vp, synth)| ActStepSetup {
                        step,
                        viewport: Some(vp),
                        synth,
                        kind_label: "locate_click",
                    }),
                    ActStepAction::LocateType { x, y, text } => {
                        state
                            .env_names
                            .extend(case::env_references(&Value::String(text.clone())));
                        secrets = store::secrets_from_env_names(&state.env_names);
                        act_probe_visual_point_step(
                            "locate_type",
                            x,
                            y,
                            Some(&text),
                            &session_kind,
                            masked_capture.as_ref(),
                            &mut sessions,
                        )
                        .map(|(step, vp, synth)| ActStepSetup {
                            step,
                            viewport: Some(vp),
                            synth,
                            kind_label: "locate_type",
                        })
                    }
                };

                let ActStepSetup {
                    step,
                    viewport: step_vp,
                    synth: synth_target,
                    kind_label: action_kind_label,
                } = match step_setup {
                    Ok(setup) => setup,
                    Err(hook_err) => {
                        let _ = detach_and_persist_handles(
                            &session_dir,
                            &mut state,
                            sessions,
                            &secrets,
                        );
                        let (code, kind, msg) = hook_err.into_code_and_kind();
                        return error_response(code, kind, secrets.redact(&msg));
                    }
                };
                if let Some(vp) = &step_vp {
                    last_viewport = Some(vp.clone());
                }

                let (report, _post_snap, step_rec) = execute_synthesized_step(
                    &step,
                    next_step_index,
                    step_vp.as_ref(),
                    synth_target.as_ref(),
                    Some(&outcome.audit),
                    effective_registry.as_ref(),
                    &mut state,
                    &mut secrets,
                    &mut sessions,
                );
                if let Err(err) = store::append_step(&session_dir, &step_rec, &secrets) {
                    let _ =
                        detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                    return error_response(
                        ExitCode::Environment,
                        "environment",
                        secrets.redact(&err),
                    );
                }
                next_step_index += 1;

                let step_ok = report.status == StepStatus::Passed;
                let step_err = report.error.as_ref().map(|p| secrets.redact(&p.message));
                let exec_entry = json!({
                    "iteration": step_num,
                    "step_index": step_rec.index,
                    "action_kind": action_kind_label,
                    "use": step.action,
                    "status": step_rec.status,
                    "used_ref": step_rec.used_ref,
                    "synthesized_locator": step_rec.synthesized_locator,
                    "fragile": step_rec.fragile,
                    "summary": outcome.summary,
                    "error": step_err,
                });
                executed_steps.push(exec_entry);
                history.push(json!({
                    "iteration": step_num,
                    "action_kind": action_kind_label,
                    "use": step.action,
                    "status": if step_ok { "passed" } else { "failed" },
                    "summary": outcome.summary,
                    "error": step_err,
                }));

                if matches!(
                    report.error.as_ref().map(|p| p.kind),
                    Some(ProblemKind::Environment)
                ) {
                    let _ =
                        detach_and_persist_handles(&session_dir, &mut state, sessions, &secrets);
                    return error_response(
                        ExitCode::Environment,
                        "environment",
                        step_err.unwrap_or_else(|| {
                            "session step failed with environment error".to_string()
                        }),
                    );
                }
            }
        }
    }

    if final_status == "max_steps_exceeded" && final_reason.is_none() {
        final_reason = Some(format!(
            "reached `--max-steps` limit ({}) without completing goal",
            req.max_steps
        ));
    }

    let local_only_reason = sessions.local_only_reason();
    let detached = sessions.detach_all();
    state.handles = detached.handles;
    state.last_active_ms = store::now_ms();

    let Some(mut diag_audit) = last_audit else {
        let _ = store::save_state(&session_dir, &state, &secrets);
        return error_response(
            ExitCode::Environment,
            "environment",
            "model `model/act_step` did not produce an outcome",
        );
    };
    diag_audit.latency_ms = total_model_latency_ms;

    let act_passed = final_status == "done";
    let act_duration_ms = store::now_ms().saturating_sub(act_started_ms);
    let mut raw_args = json!({
        "goal": secrets.redact(&req.goal),
        "max_steps": req.max_steps,
    });
    if req.screenshot {
        raw_args["screenshot"] = json!(true);
    }
    let diag_result = json!({
        "status": final_status,
        "summary": final_summary,
        "reason": final_reason,
        "steps_taken": executed_steps.len(),
        "max_steps": req.max_steps,
        "executed_steps": executed_steps,
        "model": diag_audit.model,
        "latency_ms": total_model_latency_ms,
        "confidence": diag_audit.confidence,
        "trace_id": diag_audit.trace_id,
    });
    let diag_record = build_model_diagnostic_record(
        ModelDiagnosticSpec {
            index: next_step_index,
            action: STEP_ACTION_ACT,
            raw_args,
            status: if act_passed { "passed" } else { "failed" },
            error: if act_passed {
                None
            } else {
                final_reason.clone()
            },
            used_ref: None,
            synthesized_locator: None,
            fragile: false,
            observed_viewport: last_viewport.clone(),
            started_at_ms: act_started_ms,
            duration_ms: act_duration_ms,
            result: diag_result,
        },
        &diag_audit,
    );

    if let Err(err) = store::append_step(&session_dir, &diag_record, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }
    if let Err(err) = store::save_state(&session_dir, &state, &secrets) {
        return error_response(ExitCode::Environment, "environment", secrets.redact(&err));
    }

    let ok = act_passed && detached.errors.is_empty();
    let exit_code = if !detached.errors.is_empty() && act_passed {
        ExitCode::Environment
    } else if act_passed {
        ExitCode::Success
    } else {
        ExitCode::TestFailure
    };

    let mut payload = json!({
        "ok": ok,
        "session_id": state.id,
        "session_kind": final_session_kind,
        "status": final_status,
        "summary": final_summary,
        "reason": final_reason,
        "steps_taken": executed_steps.len(),
        "max_steps": req.max_steps,
        "executed_steps": executed_steps,
        "model": diag_audit.model,
        "latency_ms": total_model_latency_ms,
        "confidence": diag_audit.confidence,
        "trace_id": diag_audit.trace_id,
    });
    if req.screenshot {
        payload["screenshot"] = json!(true);
        if let Some(vp) = &last_viewport {
            payload["viewport"] = vp.clone();
        }
    }
    if !detached.errors.is_empty() {
        payload["detach_errors"] = json!(detached.errors);
    }
    if let Some(reason) = local_only_reason {
        payload["local_only"] = json!(true);
        payload["local_only_reason"] = json!(reason);
    }

    let mut text = format!(
        "act: {final_status} (steps: {}/{}, model: {}, latency_ms: {total_model_latency_ms})\n",
        executed_steps.len(),
        req.max_steps,
        diag_audit.model
    );
    if !final_summary.is_empty() {
        text.push_str(&format!("summary: {final_summary}\n"));
    }
    if let Some(reason) = &final_reason {
        text.push_str(&format!("reason: {reason}\n"));
    }

    SessionResponse {
        exit_code,
        payload: secrets.redact_json(&payload),
        raw_yaml: None,
        raw_text: Some(secrets.redact(&text)),
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
        Err(resp) => return *resp,
    };
    let records = match store::load_steps(&session_dir) {
        Ok(records) => records,
        Err(message) => return error_response(ExitCode::Environment, "environment", message),
    };
    let result = match export::export_session(&state.id, &state.observe, &records, options) {
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
        raw_text: None,
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
    let secrets = state
        .as_ref()
        .map(|s| store::secrets_from_env_names(&s.env_names))
        .unwrap_or_default();
    let mut errors =
        store::close_and_remove_session(&session_dir, state.as_ref(), lock, registry, &secrets);
    if let Some(err) = load_error {
        errors.insert(0, secrets.redact(&err));
    }

    let ok = errors.is_empty();
    let payload = json!({
        "ok": ok,
        "session_id": id,
        "errors": errors,
    });
    SessionResponse {
        exit_code: if ok {
            ExitCode::Success
        } else {
            ExitCode::Environment
        },
        payload: secrets.redact_json(&payload),
        raw_yaml: None,
        raw_text: None,
    }
}

fn handle_list(
    sessions_dir: &Path,
    idle_override_ms: Option<u64>,
    registry: &Registry,
) -> SessionResponse {
    let empty_secrets = Secrets::new();
    match store::list_and_reap(sessions_dir, idle_override_ms, registry, &empty_secrets) {
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
            raw_text: None,
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
    let raw_args = registry.redact_template_args(&step.action, &step.args);
    let redacted_args = raw_args != step.args;
    StepRecord {
        index,
        action: step.action.clone(),
        raw_args,
        redacted_args,
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
        model: None,
        latency_ms: None,
        usage: None,
        trace_id: None,
        confidence: None,
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
) -> Result<(Vec<String>, BTreeSet<String>), Box<SessionResponse>> {
    let mut seen = BTreeSet::new();
    let mut env_names = BTreeSet::new();
    for obs in observations {
        case::validate_observation_name(&obs.name)
            .map_err(|msg| Box::new(error_response(ExitCode::InvalidCase, "invalid_input", msg)))?;
        if !seen.insert(obs.name.as_str()) {
            return Err(Box::new(error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                format!("duplicate observation name `{}`", obs.name),
            )));
        }
        env_names.extend(case::env_references(&obs.raw_args));
    }

    let mut secrets = store::secrets_from_env_names(&env_names);
    let empty_vars = BTreeMap::new();
    let mut names = Vec::with_capacity(observations.len());
    for obs in observations {
        let args = case::interpolate(&obs.raw_args, &empty_vars, &mut secrets).map_err(|err| {
            Box::new(error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                secrets.redact(&format!("observation `{}`: {}", obs.name, err.message)),
            ))
        })?;
        let Some(def) = registry.observer(&obs.kind) else {
            return Err(Box::new(error_response(
                ExitCode::InvalidCase,
                "invalid_input",
                format!(
                    "observation `{}`: `{}` is not a registered observer",
                    obs.name, obs.kind
                ),
            )));
        };
        def.create(&args).map_err(|error| {
            let (exit_code, kind) = match &error {
                ObserverError::InvalidArguments(_) => (ExitCode::InvalidCase, "invalid_input"),
                ObserverError::Environment(_) => (ExitCode::Environment, "environment"),
            };
            Box::new(error_response(
                exit_code,
                kind,
                secrets.redact(&format!(
                    "observation `{}` (`{}`): {}",
                    obs.name,
                    def.name,
                    error.message()
                )),
            ))
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
                model_hook: None,
                model: None,
                plugins: Vec::new(),
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
                model_hook: None,
                model: None,
                plugins: Vec::new(),
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
