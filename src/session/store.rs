//! On-disk session directory layout, locking, and secret-free state storage.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::paths;
use crate::core::registry::Registry;
use crate::core::secrets::Secrets;
use crate::session::model_hook::ModelHookSpec;

pub const STATE_FILE: &str = "state.json";
pub const STEPS_FILE: &str = "steps.jsonl";
pub const LOCK_FILE: &str = "lock";

/// Persistent metadata of one interactive session stored in `state.json`.
/// One declared observer persisted in `state.json` for `da session step`.
///
/// `raw_args` keeps unexpanded `${env:NAME}` references so secret values never
/// enter the session directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StoredObservation {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub raw_args: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub schema_version: u32,
    pub id: String,
    pub created_at_ms: u64,
    pub last_active_ms: u64,
    pub idle_timeout_ms: u64,
    #[serde(default)]
    pub handles: BTreeMap<String, Value>,
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub env_names: BTreeSet<String>,
    #[serde(default)]
    pub observe: Vec<StoredObservation>,
    /// Absolute paths of the explicit plugins this session was started with.
    #[serde(default)]
    pub plugins: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_hook: Option<ModelHookSpec>,
}

/// One expectation stored in the step log for deterministic YAML export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredExpectation {
    pub path: String,
    pub comparison: String,
    pub expected: Value,
}

/// One executed step recorded in `steps.jsonl`.
///
/// `raw_args` preserves unexpanded `${env:NAME}` references so secret values
/// never enter the session directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub index: usize,
    pub action: String,
    pub raw_args: Value,
    /// Set when an inline sensitive value in `raw_args` was masked to `***`
    /// before persisting to `steps.jsonl`, making `raw_args` non-replayable.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted_args: bool,
    #[serde(default)]
    pub expect: Vec<StoredExpectation>,
    #[serde(default)]
    pub save: BTreeMap<String, String>,
    #[serde(default)]
    pub optional: bool,
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub used_ref: Option<String>,
    #[serde(default)]
    pub synthesized_locator: Option<Value>,
    #[serde(default)]
    pub fragile: bool,
    /// The viewport a step that used explicit coordinates ran at, as
    /// `{ "width": ..., "height": ... }`.
    ///
    /// Export writes it into the step's `at.viewport`, so a replayed case fails
    /// on a different window size instead of clicking the wrong place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_viewport: Option<Value>,
    /// Unix epoch milliseconds when the step started executing.
    #[serde(default)]
    pub started_at_ms: u64,
    /// Step execution duration in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
    /// Redacted step output JSON (or `null` when the step produced no result).
    #[serde(default)]
    pub result: Option<Value>,
    /// Model identifier recorded when an external model hook assisted exploration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Model inference or hook latency in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Redacted token or compute usage object returned by the model hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    /// Redacted trace identifier returned by the model hook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Optional calibrated model confidence in `0.0..=1.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// Exclusive non-blocking lock on one session directory.
pub struct SessionLock {
    file: Option<File>,
}

/// Why acquiring a session lock failed.
pub enum LockError {
    Busy(String),
    Io(String),
}

impl SessionLock {
    /// Tries to acquire an exclusive lock without blocking.
    pub fn try_acquire(session_dir: &Path, id: &str) -> Result<Self, LockError> {
        let path = session_dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                LockError::Io(format!(
                    "could not open session lock `{}`: {error}",
                    paths::display(&path)
                ))
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { file: Some(file) }),
            Err(TryLockError::WouldBlock) => Err(LockError::Busy(format!(
                "session `{id}` is busy: another command is currently running a step; wait for it to finish"
            ))),
            Err(TryLockError::Error(error)) => Err(LockError::Io(format!(
                "could not lock session `{id}`: {error}"
            ))),
        }
    }

    /// Explicitly unlocks and closes the lock file handle.
    pub fn release(mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

/// Current epoch timestamp in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Generates a unique, path-safe session identifier.
pub fn generate_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("s-{}-{}-{seq}", now_ms(), std::process::id())
}

/// Validates that `id` is a safe single path segment.
pub fn validate_session_id(id: &str) -> Result<(), String> {
    if id.is_empty() || matches!(id, "." | "..") {
        return Err(format!("invalid session id `{id}`"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "invalid session id `{id}`; use ASCII letters, digits, `.`, `_` and `-`"
        ));
    }
    Ok(())
}

/// Reads `state.json` from `session_dir`.
pub fn load_state(session_dir: &Path) -> Result<SessionState, String> {
    let path = session_dir.join(STATE_FILE);
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "could not read session state `{}`: {error}",
            paths::display(&path)
        )
    })?;
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "could not parse session state `{}`: {error}",
            paths::display(&path)
        )
    })
}

/// Writes `state.json` atomically-ish via write + flush, ensuring any known
/// secrets are redacted from `vars` before touching disk.
pub fn save_state(
    session_dir: &Path,
    state: &SessionState,
    secrets: &Secrets,
) -> Result<(), String> {
    let mut sanitized = state.clone();
    for value in sanitized.vars.values_mut() {
        *value = secrets.redact(value);
    }
    for handle in sanitized.handles.values_mut() {
        *handle = secrets.redact_json(handle);
    }
    if let Some(hook) = &mut sanitized.model_hook {
        hook.command = secrets.redact(&hook.command);
        for arg in &mut hook.args {
            *arg = secrets.redact(arg);
        }
    }
    let path = session_dir.join(STATE_FILE);
    let json = serde_json::to_string_pretty(&sanitized)
        .map_err(|error| format!("could not serialize session state: {error}"))?;
    fs::write(&path, format!("{json}\n")).map_err(|error| {
        format!(
            "could not write session state `{}`: {error}",
            paths::display(&path)
        )
    })
}

/// Reads all recorded steps from `steps.jsonl`.
pub fn load_steps(session_dir: &Path) -> Result<Vec<StepRecord>, String> {
    let path = session_dir.join(STEPS_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "could not read step log `{}`: {error}",
            paths::display(&path)
        )
    })?;
    let mut records = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let record: StepRecord = serde_json::from_str(trimmed).map_err(|error| {
            format!(
                "could not parse step log `{}` line {}: {error}",
                paths::display(&path),
                idx + 1
            )
        })?;
        records.push(record);
    }
    Ok(records)
}

/// Appends one step record to `steps.jsonl`, redacting any non-raw fields.
pub fn append_step(
    session_dir: &Path,
    record: &StepRecord,
    secrets: &Secrets,
) -> Result<(), String> {
    let mut sanitized = record.clone();
    sanitized.error = sanitized.error.as_deref().map(|err| secrets.redact(err));
    sanitized.synthesized_locator = sanitized
        .synthesized_locator
        .as_ref()
        .map(|loc| secrets.redact_json(loc));
    sanitized.result = sanitized
        .result
        .as_ref()
        .map(|value| secrets.redact_json(value));
    sanitized.model = sanitized.model.as_deref().map(|m| secrets.redact(m));
    sanitized.usage = sanitized.usage.as_ref().map(|u| secrets.redact_json(u));
    sanitized.trace_id = sanitized.trace_id.as_deref().map(|t| secrets.redact(t));

    let path = session_dir.join(STEPS_FILE);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| {
            format!(
                "could not open step log `{}`: {error}",
                paths::display(&path)
            )
        })?;
    let line = serde_json::to_string(&sanitized)
        .map_err(|error| format!("could not serialize step record: {error}"))?;
    writeln!(file, "{line}").map_err(|error| {
        format!(
            "could not append to step log `{}`: {error}",
            paths::display(&path)
        )
    })
}

/// Populates a [`Secrets`] instance in memory from recorded `${env:NAME}` names
/// without storing any secret value in the session directory.
pub fn secrets_from_env_names(env_names: &BTreeSet<String>) -> Secrets {
    let mut secrets = Secrets::new();
    for name in env_names {
        if let Ok(value) = std::env::var(name) {
            secrets.record(&value);
        }
    }
    secrets
}

/// Result of attempting to close a session's detached handles.
#[derive(Debug, Clone, Default)]
pub struct DisposeReport {
    /// Handles that failed to close and must remain persisted for retry.
    pub remaining: BTreeMap<String, Value>,
    /// Redacted error messages for every handle that could not be closed.
    pub errors: Vec<String>,
}

/// Closes all detached handles stored in `handles`, continuing through failures
/// and returning redacted errors alongside any unclosed handles.
pub fn dispose_handles(
    handles: &BTreeMap<String, Value>,
    registry: &Registry,
    secrets: &Secrets,
) -> DisposeReport {
    dispose_handles_inner(handles, registry, secrets, false)
}

fn dispose_handles_inner(
    handles: &BTreeMap<String, Value>,
    registry: &Registry,
    secrets: &Secrets,
    has_plugin_load_errors: bool,
) -> DisposeReport {
    let mut report = DisposeReport::default();
    for (kind, handle) in handles {
        let Some(def) = registry.session(kind) else {
            report.remaining.insert(kind.clone(), handle.clone());
            let message = if has_plugin_load_errors {
                format!("cannot close session `{kind}`: persisted plugin failed to load")
            } else {
                format!("cannot close session `{kind}`: driver is not enabled in this build")
            };
            report.errors.push(secrets.redact(&message));
            continue;
        };
        if let Err(dispose_err) = def.dispose_handle(handle) {
            report.remaining.insert(kind.clone(), handle.clone());
            report
                .errors
                .push(secrets.redact(&format!("could not close `{kind}`: {dispose_err}")));
        }
    }
    report
}

#[cfg(feature = "plugin")]
fn resolve_persisted_for_dispose(
    plugins: &[String],
    base_registry: &Registry,
    secrets: &Secrets,
) -> (Option<Registry>, Vec<String>) {
    let (per_session, raw_errors) =
        crate::plugin::resolve_persisted_for_dispose(base_registry, plugins);
    let errors = raw_errors
        .into_iter()
        .map(|err| secrets.redact(&err))
        .collect();
    (per_session, errors)
}

#[cfg(not(feature = "plugin"))]
fn resolve_persisted_for_dispose(
    plugins: &[String],
    _base_registry: &Registry,
    secrets: &Secrets,
) -> (Option<Registry>, Vec<String>) {
    if plugins.is_empty() {
        return (None, Vec::new());
    }
    (
        None,
        vec![secrets.redact(
            "this build was compiled without the `plugin` feature; rebuild with `--features plugin`",
        )],
    )
}

/// Removes a session directory, retrying briefly on Windows handle release.
pub fn remove_session_dir(session_dir: &Path) -> Result<(), String> {
    let mut last = None;
    for _ in 0..20 {
        match fs::remove_dir_all(session_dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
    let detail = last.map_or_else(|| "unknown error".to_string(), |error| error.to_string());
    Err(format!(
        "could not remove session directory `{}`: {detail}",
        paths::display(session_dir)
    ))
}

/// Closes a session's detached resources and deletes its directory only when
/// every handle closes cleanly.
///
/// When any handle fails to close (or its persisted plugin fails to load),
/// `state.json` is retained and updated in place so successfully closed handles
/// are cleared while unclosed handles remain recoverable.
pub fn close_and_remove_session(
    session_dir: &Path,
    state: Option<&SessionState>,
    lock: SessionLock,
    registry: &Registry,
    secrets: &Secrets,
) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(state) = state
        && !state.handles.is_empty()
    {
        let needs_plugins = state
            .handles
            .keys()
            .any(|kind| !registry.is_builtin_session(kind))
            && !state.plugins.is_empty();
        let (per_session_registry, plugin_errors) = if needs_plugins {
            resolve_persisted_for_dispose(&state.plugins, registry, secrets)
        } else {
            (None, Vec::new())
        };
        let has_plugin_load_errors = !plugin_errors.is_empty();
        errors.extend(plugin_errors);

        let effective_registry = per_session_registry.as_ref().unwrap_or(registry);
        let dispose = dispose_handles_inner(
            &state.handles,
            effective_registry,
            secrets,
            has_plugin_load_errors,
        );
        errors.extend(dispose.errors);

        if !errors.is_empty() {
            let mut retained = state.clone();
            retained.handles = dispose.remaining;
            if let Err(save_err) = save_state(session_dir, &retained, secrets) {
                errors.push(secrets.redact(&save_err));
            }
            lock.release();
            return errors;
        }
    }

    // Remove state.json while still holding the lock so any concurrent starter
    // sees the session is gone, then release the lock handle before removing
    // the directory on Windows.
    let _ = fs::remove_file(session_dir.join(STATE_FILE));
    lock.release();
    if let Err(error) = remove_session_dir(session_dir) {
        errors.push(secrets.redact(&error));
    }
    errors
}

/// Information about one active session returned by `list`.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub dir: String,
    pub status: String,
    pub created_at_ms: u64,
    pub last_active_ms: u64,
    pub idle_timeout_ms: u64,
    pub handles: Vec<String>,
}

/// Result of listing sessions and reaping any expired ones.
#[derive(Debug, Clone, Default)]
pub struct ListReport {
    pub sessions: Vec<SessionSummary>,
    pub reaped: Vec<String>,
    pub errors: Vec<String>,
}

/// Scans `sessions_dir`, reaps expired sessions, and returns active summaries.
pub fn list_and_reap(
    sessions_dir: &Path,
    idle_override_ms: Option<u64>,
    registry: &Registry,
    extra_secrets: &Secrets,
) -> Result<ListReport, String> {
    if !sessions_dir.exists() {
        return Ok(ListReport::default());
    }
    let entries = fs::read_dir(sessions_dir).map_err(|error| {
        format!(
            "could not list sessions directory `{}`: {error}",
            paths::display(sessions_dir)
        )
    })?;

    let now = now_ms();
    let mut active = Vec::new();
    let mut reaped = Vec::new();
    let mut errors = Vec::new();

    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();

    for dir in dirs {
        let Some(id) = dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let id = id.to_string();
        let lock = match SessionLock::try_acquire(&dir, &id) {
            Ok(lock) => lock,
            Err(LockError::Busy(_)) => {
                if let Ok(state) = load_state(&dir) {
                    active.push(SessionSummary {
                        id: state.id,
                        dir: paths::display(&dir),
                        status: "busy".to_string(),
                        created_at_ms: state.created_at_ms,
                        last_active_ms: state.last_active_ms,
                        idle_timeout_ms: state.idle_timeout_ms,
                        handles: state.handles.keys().cloned().collect(),
                    });
                }
                continue;
            }
            Err(LockError::Io(message)) => {
                errors.push(extra_secrets.redact(&message));
                continue;
            }
        };

        let state = match load_state(&dir) {
            Ok(state) => state,
            Err(err) => {
                if dir.join(STATE_FILE).exists() {
                    errors.push(extra_secrets.redact(&err));
                }
                lock.release();
                continue;
            }
        };

        let threshold = idle_override_ms.unwrap_or(state.idle_timeout_ms);
        let elapsed = now.saturating_sub(state.last_active_ms);
        if threshold == 0 || elapsed >= threshold {
            let mut session_secrets = secrets_from_env_names(&state.env_names);
            session_secrets.merge(extra_secrets);
            let reap_errs =
                close_and_remove_session(&dir, Some(&state), lock, registry, &session_secrets);
            if reap_errs.is_empty() {
                reaped.push(state.id);
            } else {
                errors.extend(reap_errs);
            }
        } else {
            active.push(SessionSummary {
                id: state.id,
                dir: paths::display(&dir),
                status: "active".to_string(),
                created_at_ms: state.created_at_ms,
                last_active_ms: state.last_active_ms,
                idle_timeout_ms: state.idle_timeout_ms,
                handles: state.handles.keys().cloned().collect(),
            });
            lock.release();
        }
    }

    Ok(ListReport {
        sessions: active,
        reaped,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_session_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "da-store-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_step_record_and_session_state_deserialize_with_defaults() {
        let state: SessionState = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "id": "sess-legacy",
            "created_at_ms": 100,
            "last_active_ms": 200,
            "idle_timeout_ms": 60000
        }))
        .expect("legacy SessionState");
        assert!(state.observe.is_empty());

        let dir = temp_session_dir("legacy");
        let legacy_step = serde_json::json!({
            "index": 1,
            "action": "http.request",
            "raw_args": { "url": "http://localhost/health" },
            "status": "passed"
        });
        fs::write(dir.join(STEPS_FILE), format!("{legacy_step}\n")).unwrap();

        let steps = load_steps(&dir).expect("load legacy steps");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].started_at_ms, 0);
        assert_eq!(steps[0].duration_ms, 0);
        assert_eq!(steps[0].result, None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_step_redacts_secrets_in_result() {
        let dir = temp_session_dir("redact-result");
        let mut secrets = Secrets::new();
        secrets.record("tok-secret-999");

        let record = StepRecord {
            index: 1,
            action: "http.request".to_string(),
            raw_args: serde_json::json!({ "header": "${env:TOKEN}" }),
            redacted_args: false,
            expect: Vec::new(),
            save: BTreeMap::new(),
            optional: false,
            status: "passed".to_string(),
            error: None,
            used_ref: None,
            synthesized_locator: None,
            fragile: false,
            observed_viewport: None,
            started_at_ms: 1_700_000_000_000,
            duration_ms: 42,
            result: Some(serde_json::json!({
                "body": { "echo": "tok-secret-999" }
            })),
            model: None,
            latency_ms: None,
            usage: None,
            trace_id: None,
            confidence: None,
        };

        append_step(&dir, &record, &secrets).expect("append_step");
        let loaded = load_steps(&dir).expect("load_steps");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].started_at_ms, 1_700_000_000_000);
        assert_eq!(loaded[0].duration_ms, 42);
        assert_eq!(
            loaded[0].result,
            Some(serde_json::json!({ "body": { "echo": "***" } }))
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn close_and_remove_session_retains_only_unclosed_handles_and_redacts_errors() {
        use crate::core::registry::SessionDefinition;
        use crate::core::session::Session;

        struct StubSession;
        impl Session for StubSession {
            fn close(&mut self) -> Result<(), String> {
                Ok(())
            }
        }

        let dir = temp_session_dir("retain-unclosed");
        let sess_dir = dir.join("s-1");
        fs::create_dir_all(&sess_dir).unwrap();

        let mut registry = Registry::new();
        registry
            .register_session(SessionDefinition {
                kind: "test.ok",
                reattach: |_| Ok(Box::new(StubSession)),
                dispose_handle: |_| Ok(()),
            })
            .unwrap();
        registry
            .register_session(SessionDefinition {
                kind: "test.fail",
                reattach: |_| Ok(Box::new(StubSession)),
                dispose_handle: |handle| {
                    let fail = handle.get("fail").and_then(Value::as_bool).unwrap_or(false);
                    if fail {
                        Err("close failed with secret tok-secret-999".to_string())
                    } else {
                        Ok(())
                    }
                },
            })
            .unwrap();

        let mut handles = BTreeMap::new();
        handles.insert("test.ok".to_string(), serde_json::json!({"id": 1}));
        handles.insert(
            "test.fail".to_string(),
            serde_json::json!({"id": 2, "fail": true}),
        );
        let state = SessionState {
            schema_version: 1,
            id: "s-1".to_string(),
            created_at_ms: 1,
            last_active_ms: 1,
            idle_timeout_ms: 1,
            handles,
            vars: BTreeMap::new(),
            env_names: BTreeSet::new(),
            observe: Vec::new(),
            plugins: Vec::new(),
            model_hook: None,
        };
        let mut secrets = Secrets::new();
        secrets.record("tok-secret-999");
        save_state(&sess_dir, &state, &secrets).unwrap();

        // First reap attempt: `test.fail` fails -> state retained with only `test.fail`,
        // `reaped` excludes `s-1`, and secret is scrubbed.
        let first = list_and_reap(&dir, Some(0), &registry, &secrets).expect("list_and_reap");
        assert!(
            first.reaped.is_empty(),
            "failed reap must not be in reaped: {:?}",
            first.reaped
        );
        assert_eq!(first.errors.len(), 1);
        assert!(
            !first.errors[0].contains("tok-secret-999"),
            "{:?}",
            first.errors
        );
        assert!(first.errors[0].contains("***"), "{:?}", first.errors);
        assert!(sess_dir.exists(), "session dir must be retained");
        let retained = load_state(&sess_dir).expect("state.json must be retained");
        assert!(
            !retained.handles.contains_key("test.ok"),
            "closed handle must be removed from retained state"
        );
        assert_eq!(
            retained.handles.get("test.fail"),
            Some(&serde_json::json!({"id": 2, "fail": true}))
        );

        // Simulate recovery by flipping the handle's `fail` flag, then retry reap.
        let mut recovered = retained;
        recovered.handles.insert(
            "test.fail".to_string(),
            serde_json::json!({"id": 2, "fail": false}),
        );
        save_state(&sess_dir, &recovered, &secrets).unwrap();

        let second = list_and_reap(&dir, Some(0), &registry, &secrets).expect("retry reap");
        assert_eq!(second.reaped, vec!["s-1".to_string()]);
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        assert!(
            !sess_dir.exists(),
            "session dir must be deleted after recovery"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
