//! External plugins over synchronous newline-delimited JSON-RPC 2.0.
//!
//! A plugin is an executable named explicitly with `--plugin <path>`. `da`
//! starts it, calls `plugin/manifest`, and registers the declared steps,
//! observers and detachable sessions in the same [`Registry`] built-in drivers
//! use. Nothing is discovered from the working directory or the environment.
//!
//! The transport and the child-process lifecycle live here; `src/core` only
//! learns that a definition may carry owned names and closures.

mod client;
mod runtime;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::paths;
use crate::core::registry::{
    DynamicObserverDefinition, DynamicSessionDefinition, DynamicStepDefinition, Registry,
    StepError, allow_retry, disallow_retry, no_lint_warnings,
};
use crate::core::session::Evidence;
use crate::exit::ExitCode;

use self::client::{CallError, PluginClient};
use self::runtime::{PluginObserver, PluginSession, run_step};

/// Why one explicitly named plugin could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The plugin did not answer `plugin/manifest` before the deadline.
    Timeout(String),
    /// The plugin could not start, exited, or broke the protocol.
    Environment(String),
    /// The manifest is invalid, or a declared name collides.
    Invalid(String),
}

impl RegisterError {
    /// The message shown to the user.
    pub fn message(&self) -> &str {
        match self {
            Self::Timeout(message) | Self::Environment(message) | Self::Invalid(message) => message,
        }
    }

    /// The process exit code that matches this failure.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Timeout(_) => ExitCode::Timeout,
            Self::Environment(_) => ExitCode::Environment,
            Self::Invalid(_) => ExitCode::Usage,
        }
    }
}

/// One plugin declaration from `plugin/manifest`.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginManifest {
    /// Human-readable plugin name, used in diagnostics.
    #[serde(default)]
    pub name: Option<String>,
    /// Plugin version, reported for diagnostics only.
    #[serde(default)]
    pub version: Option<String>,
    /// Steps the plugin adds to the registry.
    #[serde(default)]
    pub steps: Vec<PluginStep>,
    /// Observers the plugin adds to the registry.
    #[serde(default)]
    pub observers: Vec<PluginObserverDecl>,
    /// Detachable session kinds the plugin owns.
    #[serde(default)]
    pub sessions: Vec<PluginSessionDecl>,
}

/// A step declared by a plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginStep {
    /// Name used in case files.
    pub name: String,
    /// One sentence describing the step.
    #[serde(default)]
    pub summary: String,
    /// JSON Schema for the step's arguments.
    #[serde(default = "default_args_schema")]
    pub args_schema: Value,
    /// Whether the step is read-only and may use `retry`.
    #[serde(default)]
    pub allow_retry: bool,
    /// Session kind this step binds to, when it keeps state across steps.
    #[serde(default)]
    pub session_kind: Option<String>,
}

/// An observer declared by a plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginObserverDecl {
    /// Name used in `observe:`.
    pub name: String,
    /// One sentence describing what the observer reports.
    #[serde(default)]
    pub summary: String,
    /// JSON Schema for the observer's arguments.
    #[serde(default = "default_args_schema")]
    pub args_schema: Value,
}

/// A session kind declared by a plugin.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginSessionDecl {
    /// Session kind, matching `session_kind` of a plugin step.
    pub kind: String,
    /// Whether the session can be detached and reattached by a later command.
    #[serde(default)]
    pub detachable: bool,
}

fn default_args_schema() -> Value {
    json!({"type": "object"})
}

/// Starts every explicit `--plugin` executable and registers its declarations.
///
/// Returns the canonical absolute paths of the loaded plugins in order.
/// `da session start` stores them so later `da session` commands for that
/// session can reattach without repeating `--plugin`.
pub fn register(registry: &mut Registry, paths: &[PathBuf]) -> Result<Vec<String>, RegisterError> {
    let mut loaded = Vec::new();
    for path in paths {
        let absolute = absolute_path(path)?;
        let stored = stored_path(&absolute);
        if !registry.has_dynamic_source(&stored) {
            register_resolved(registry, &absolute, &stored)?;
        }
        if !loaded.contains(&stored) {
            loaded.push(stored);
        }
    }
    Ok(loaded)
}

/// Validates and registers persisted plugin paths from one target session's
/// `state.json`, skipping paths already loaded via `--plugin`.
pub(crate) fn register_persisted(
    registry: &mut Registry,
    raw_paths: &[String],
) -> Result<(), RegisterError> {
    for raw in raw_paths {
        let canonical = validate_persisted_path(raw)?;
        let stored = stored_path(&canonical);
        if registry.has_dynamic_source(&stored) {
            continue;
        }
        register_resolved(registry, &canonical, &stored)?;
    }
    Ok(())
}

/// Builds an isolated per-session registry for handle disposal when a session
/// holds non-built-in handles and persisted plugin paths.
///
/// Returns `None` when `base_registry` already contains every validated
/// persisted plugin path, avoiding spawning plugins twice. Errors from invalid
/// or broken persisted plugins are collected per-plugin so working plugins on
/// the same session can still close their handles.
pub(crate) fn resolve_persisted_for_dispose(
    base_registry: &Registry,
    raw_paths: &[String],
) -> (Option<Registry>, Vec<String>) {
    let mut all_present = true;
    for raw in raw_paths {
        match validate_persisted_path(raw) {
            Ok(canonical) => {
                let stored = stored_path(&canonical);
                if !base_registry.has_dynamic_source(&stored) {
                    all_present = false;
                }
            }
            Err(_) => {
                all_present = false;
                break;
            }
        }
    }
    if all_present {
        return (None, Vec::new());
    }

    let mut per_session = base_registry.builtins_clone();
    let mut errors = Vec::new();
    for raw in raw_paths {
        let canonical = match validate_persisted_path(raw) {
            Ok(path) => path,
            Err(error) => {
                errors.push(error.message().to_string());
                continue;
            }
        };
        let stored = stored_path(&canonical);
        if per_session.has_dynamic_source(&stored) {
            continue;
        }
        if let Err(error) = register_resolved(&mut per_session, &canonical, &stored) {
            errors.push(error.message().to_string());
        }
    }
    (Some(per_session), errors)
}

fn register_resolved(
    registry: &mut Registry,
    absolute: &Path,
    stored: &str,
) -> Result<(), RegisterError> {
    let label = absolute
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| stored.to_string());
    let host = Arc::new(PluginHost::connect(absolute, &label)?);
    let manifest = host.manifest.clone();
    let prefix = manifest.name.clone().unwrap_or_else(|| label.clone());
    let detachable_sessions: BTreeMap<&str, bool> = manifest
        .sessions
        .iter()
        .map(|session| (session.kind.as_str(), session.detachable))
        .collect();

    for step in &manifest.steps {
        let host = Arc::clone(&host);
        let name = step.name.clone();
        let session_kind = step.session_kind.clone();
        let detachable = session_kind
            .as_deref()
            .and_then(|kind| detachable_sessions.get(kind).copied())
            .unwrap_or(false);
        let check_retry = if step.allow_retry {
            Arc::new(allow_retry) as Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>
        } else {
            Arc::new(disallow_retry) as Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>
        };
        registry
            .register_dynamic(DynamicStepDefinition {
                name: step.name.clone(),
                description: description(&step.summary, &step.name),
                args_schema: step.args_schema.clone(),
                validate: {
                    let host = Arc::clone(&host);
                    let name = name.clone();
                    Arc::new(move |args| host.validate_step(&name, args))
                },
                check_retry,
                lint_warnings: Arc::new(no_lint_warnings),
                run: {
                    let host = Arc::clone(&host);
                    let name = name.clone();
                    let session_kind = session_kind.clone();
                    Arc::new(move |args, sessions| {
                        run_step(
                            &host,
                            &name,
                            session_kind.as_deref(),
                            detachable,
                            args,
                            sessions,
                        )
                    })
                },
            })
            .map_err(|error| RegisterError::Invalid(format!("plugin `{prefix}`: {error}")))?;
    }

    for observer in &manifest.observers {
        let host = Arc::clone(&host);
        let name = observer.name.clone();
        registry
            .register_observer_dynamic(DynamicObserverDefinition {
                name: observer.name.clone(),
                description: description(&observer.summary, &observer.name),
                args_schema: observer.args_schema.clone(),
                validate: Arc::new(|_| Ok(())),
                create: Arc::new(move |args| {
                    Ok(Box::new(PluginObserver::create(
                        Arc::clone(&host),
                        name.clone(),
                        args.clone(),
                    ))
                        as Box<dyn crate::core::observer::Observer>)
                }),
            })
            .map_err(|error| RegisterError::Invalid(format!("plugin `{prefix}`: {error}")))?;
    }

    for session in &manifest.sessions {
        let host = Arc::clone(&host);
        let kind = session.kind.clone();
        let detachable = session.detachable;
        registry
            .register_session_dynamic(DynamicSessionDefinition {
                kind: session.kind.clone(),
                reattach: {
                    let host = Arc::clone(&host);
                    let kind = kind.clone();
                    Arc::new(move |handle| {
                        PluginSession::reattach(Arc::clone(&host), kind.clone(), handle, detachable)
                            .map(|session| {
                                Box::new(session) as Box<dyn crate::core::session::Session>
                            })
                    })
                },
                dispose_handle: {
                    let host = Arc::clone(&host);
                    let kind = kind.clone();
                    Arc::new(move |handle| host.close_session(&kind, handle))
                },
            })
            .map_err(|error| RegisterError::Invalid(format!("plugin `{prefix}`: {error}")))?;
    }

    registry.record_dynamic_source(stored.to_string());
    Ok(())
}

/// Resolves explicit `--plugin` paths to the canonical form stored in session
/// state, without starting them.
pub fn stored_paths(paths: &[PathBuf]) -> Result<Vec<String>, RegisterError> {
    paths
        .iter()
        .map(|path| absolute_path(path).map(|absolute| stored_path(&absolute)))
        .collect()
}

/// Validates a persisted plugin path from `state.json`.
///
/// Unlike explicit CLI `--plugin` flags, persisted paths are never joined with
/// `current_dir` and must already be absolute and canonical.
pub(crate) fn validate_persisted_path(raw: &str) -> Result<PathBuf, RegisterError> {
    if raw.trim().is_empty() {
        return Err(RegisterError::Invalid(
            "persisted plugin path must not be empty".to_string(),
        ));
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(RegisterError::Invalid(format!(
            "persisted plugin path `{raw}` must be absolute"
        )));
    }
    if raw.starts_with(r"\\?\") || raw.starts_with("//?/") {
        return Err(RegisterError::Invalid(format!(
            "persisted plugin path `{raw}` must not use a verbatim prefix"
        )));
    }
    if has_non_canonical_segments(path) {
        return Err(RegisterError::Invalid(format!(
            "persisted plugin path `{raw}` must be canonical"
        )));
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| {
        RegisterError::Environment(format!(
            "could not open plugin `{}`: {error}",
            paths::display(path)
        ))
    })?;
    let expected = stored_path(&canonical);
    if expected != stored_path(path) {
        return Err(RegisterError::Invalid(format!(
            "persisted plugin path `{raw}` is not canonical (expected `{expected}`)"
        )));
    }
    Ok(canonical)
}

fn has_non_canonical_segments(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::CurDir | Component::ParentDir)
            || matches!(component, Component::Normal(part) if part == "." || part == "..")
    })
}

/// Resolves an explicit CLI `--plugin` path against the working directory and
/// makes it canonical.
fn absolute_path(path: &Path) -> Result<PathBuf, RegisterError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                RegisterError::Invalid(format!("could not read the working directory: {error}"))
            })?
            .join(path)
    };
    std::fs::canonicalize(&absolute).map_err(|error| {
        RegisterError::Invalid(format!(
            "could not open plugin `{}`: {error}",
            paths::display(&absolute)
        ))
    })
}

/// The path written to session state: absolute, forward slashes, no Windows
/// verbatim prefix.
fn stored_path(path: &Path) -> String {
    let text = paths::display(path);
    if let Some(unc) = text.strip_prefix("//?/UNC/") {
        format!("//{unc}")
    } else {
        text.strip_prefix("//?/").unwrap_or(&text).to_string()
    }
}

fn description(summary: &str, name: &str) -> String {
    if summary.trim().is_empty() {
        format!("Plugin step `{name}`.")
    } else {
        summary.to_string()
    }
}

/// One plugin process and its manifest.
pub struct PluginHost {
    name: String,
    manifest: PluginManifest,
    client: Mutex<PluginClient>,
}

impl PluginHost {
    fn connect(path: &Path, label: &str) -> Result<Self, RegisterError> {
        let mut client = PluginClient::spawn(path, label).map_err(RegisterError::Environment)?;
        let result = client
            .request("plugin/manifest", json!({}))
            .map_err(|error| match error {
                CallError::Timeout(message) => RegisterError::Timeout(message),
                CallError::Environment(message) => RegisterError::Environment(message),
                CallError::Remote { message, .. } => {
                    RegisterError::Invalid(format!("plugin `{label}`: {message}"))
                }
            })?;
        let manifest: PluginManifest = serde_json::from_value(result).map_err(|error| {
            RegisterError::Invalid(format!(
                "plugin `{label}` returned an invalid manifest: {error}"
            ))
        })?;
        validate_manifest(&manifest, label).map_err(RegisterError::Invalid)?;
        Ok(Self {
            name: manifest.name.clone().unwrap_or_else(|| label.to_string()),
            manifest,
            client: Mutex::new(client),
        })
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let mut client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        client.request(method, params).map_err(|error| match error {
            CallError::Remote { code, message } => CallError::Remote {
                code,
                message: format!("plugin `{}`: {message}", self.name),
            },
            other => other,
        })
    }

    fn validate_step(&self, step: &str, args: &Value) -> Result<(), StepError> {
        self.call("step/validate", json!({"step": step, "args": args}))
            .map(|_| ())
            .map_err(|error| match error {
                CallError::Timeout(message) => StepError::Timeout(message),
                CallError::Environment(message) => StepError::Environment(message),
                CallError::Remote { message, .. } => StepError::InvalidArguments(message),
            })
    }

    pub(crate) fn start_observer(&self, observer: &str, args: &Value) -> Result<Value, CallError> {
        Ok(self
            .call(
                "observer/start",
                json!({"observer": observer, "args": args}),
            )?
            .get("handle")
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub(crate) fn collect_observer(
        &self,
        observer: &str,
        handle: &Value,
    ) -> Result<Value, CallError> {
        let response = self.call(
            "observer/collect",
            json!({"observer": observer, "handle": handle}),
        )?;
        response.get("value").cloned().ok_or_else(|| {
            CallError::Environment(format!(
                "plugin `{}` returned no value for observer `{observer}`",
                self.name
            ))
        })
    }

    pub(crate) fn run_step(
        &self,
        step: &str,
        args: &Value,
        session: Option<(&str, &Value)>,
    ) -> Result<Value, CallError> {
        let mut params = json!({"step": step, "args": args});
        if let Some((kind, handle)) = session {
            params["session"] = json!({"kind": kind, "handle": handle});
        }
        self.call("step/run", params)
    }

    pub(crate) fn snapshot_session(
        &self,
        kind: &str,
        handle: &Value,
        scope_ref: Option<&str>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        let response = self
            .call(
                "session/snapshot",
                json!({
                    "kind": kind,
                    "handle": handle,
                    "scope_ref": scope_ref,
                    "max_depth": max_depth,
                    "max_nodes": max_nodes,
                }),
            )
            .map_err(|error| error.message().to_string())?;
        match response.get("snapshot") {
            Some(Value::Null) | None => Ok(None),
            Some(value) => Ok(Some(value.clone())),
        }
    }

    pub(crate) fn session_evidence(
        &self,
        kind: &str,
        handle: &Value,
    ) -> Result<Vec<Evidence>, String> {
        let response = self
            .call("session/evidence", json!({"kind": kind, "handle": handle}))
            .map_err(|error| error.message().to_string())?;
        evidence_from_response(&response)
    }

    pub(crate) fn detach_session(
        &self,
        kind: &str,
        handle: &Value,
    ) -> Result<Option<Value>, String> {
        let response = self
            .call("session/detach", json!({"kind": kind, "handle": handle}))
            .map_err(|error| error.message().to_string())?;
        match response.get("handle") {
            Some(Value::Null) | None => Ok(None),
            Some(value) => Ok(Some(value.clone())),
        }
    }

    pub(crate) fn reattach_session(&self, kind: &str, handle: &Value) -> Result<Value, String> {
        let response = self
            .call("session/reattach", json!({"kind": kind, "handle": handle}))
            .map_err(|error| error.message().to_string())?;
        Ok(response
            .get("handle")
            .cloned()
            .unwrap_or_else(|| handle.clone()))
    }

    pub(crate) fn close_session(&self, kind: &str, handle: &Value) -> Result<(), String> {
        self.call("session/close", json!({"kind": kind, "handle": handle}))
            .map(|_| ())
            .map_err(|error| error.message().to_string())
    }
}

fn validate_manifest(manifest: &PluginManifest, label: &str) -> Result<(), String> {
    let mut sessions = BTreeSet::new();
    for session in &manifest.sessions {
        if !sessions.insert(session.kind.as_str()) {
            return Err(format!(
                "plugin `{label}` declares session kind `{}` more than once",
                session.kind
            ));
        }
    }
    for step in &manifest.steps {
        if let Some(kind) = &step.session_kind
            && !sessions.contains(kind.as_str())
        {
            return Err(format!(
                "plugin `{label}` step `{}` binds session kind `{kind}`, which the manifest does not declare",
                step.name
            ));
        }
    }
    Ok(())
}

fn evidence_from_response(response: &Value) -> Result<Vec<Evidence>, String> {
    let files = response
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| "session/evidence returned no `files` array".to_string())?;
    let mut evidence = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let name = file
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("evidence file {} has no `name` string", index + 1))?;
        if let Some(text) = file.get("text").and_then(Value::as_str) {
            evidence.push(Evidence::text(name, text)?);
            continue;
        }
        if let Some(bytes) = file.get("bytes").and_then(Value::as_array) {
            let mut data = Vec::with_capacity(bytes.len());
            for byte in bytes {
                let value = byte.as_u64().ok_or_else(|| {
                    format!("evidence file `{name}` has a byte that is not an integer")
                })?;
                data.push(
                    u8::try_from(value).map_err(|_| {
                        format!("evidence file `{name}` has a byte outside 0..=255")
                    })?,
                );
            }
            evidence.push(Evidence::bytes(name, data)?);
            continue;
        }
        return Err(format!(
            "evidence file `{name}` needs a `text` string or a `bytes` array"
        ));
    }
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "da-plugin-unit-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::canonicalize(&dir).expect("canonicalize temp dir")
    }

    #[test]
    fn persisted_paths_reject_relative_dot_and_verbatim_forms() {
        let dir = temp_dir("validate");
        let file = dir.join("fake-plugin.bin");
        std::fs::write(&file, b"stub").expect("write stub");
        let canonical_stored = stored_path(&file);

        assert!(
            validate_persisted_path(&canonical_stored).is_ok(),
            "canonical absolute path must succeed: {canonical_stored}"
        );

        for bad in [
            "",
            "   ",
            "fake-plugin.bin",
            "./fake-plugin.bin",
            "../fake-plugin.bin",
            "sub/fake-plugin.bin",
        ] {
            assert!(
                matches!(validate_persisted_path(bad), Err(RegisterError::Invalid(_))),
                "relative path `{bad}` must be rejected as invalid"
            );
        }

        let parent_traversal = format!("{}/../{}", dir.display(), "fake-plugin.bin");
        assert!(
            matches!(
                validate_persisted_path(&parent_traversal),
                Err(RegisterError::Invalid(_))
            ),
            "dot traversal `{parent_traversal}` must be rejected"
        );

        let cur_dir = format!("{}/./fake-plugin.bin", dir.display());
        assert!(
            matches!(
                validate_persisted_path(&cur_dir),
                Err(RegisterError::Invalid(_))
            ),
            "current-dir segment `{cur_dir}` must be rejected"
        );

        let verbatim = format!("//?/{canonical_stored}");
        assert!(
            matches!(
                validate_persisted_path(&verbatim),
                Err(RegisterError::Invalid(_))
            ),
            "verbatim prefix `{verbatim}` must be rejected"
        );

        let missing = stored_path(&dir.join("missing.bin"));
        assert!(
            matches!(
                validate_persisted_path(&missing),
                Err(RegisterError::Environment(_))
            ),
            "missing absolute path `{missing}` must return Environment"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
