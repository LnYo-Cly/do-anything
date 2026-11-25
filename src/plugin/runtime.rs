//! Runtime values that connect a plugin's steps, observers and sessions to the
//! engine's [`Sessions`] and [`Observer`] contracts.

use std::sync::Arc;

use serde_json::Value;

use crate::core::observer::{Observer, ObserverError};
use crate::core::registry::StepError;
use crate::core::session::{Evidence, Session, Sessions};

use super::PluginHost;
use super::client::CallError;

/// Runs one plugin step, creating or reusing its bound session.
pub(crate) fn run_step(
    host: &Arc<PluginHost>,
    step: &str,
    session_kind: Option<&str>,
    detachable: bool,
    args: &Value,
    sessions: &mut Sessions,
) -> Result<Value, StepError> {
    let bound = match session_kind {
        Some(kind) => {
            let session = sessions
                .get_or_create(kind, || {
                    Ok(PluginSession::new(
                        Arc::clone(host),
                        kind.to_string(),
                        detachable,
                    ))
                })
                .map_err(StepError::Environment)?;
            Some((kind.to_string(), session.handle().clone()))
        }
        None => None,
    };

    let response = host
        .run_step(
            step,
            args,
            bound.as_ref().map(|(kind, handle)| (kind.as_str(), handle)),
        )
        .map_err(step_error)?;

    let result = response.get("result").cloned().ok_or_else(|| {
        StepError::Environment(format!(
            "plugin step `{step}` returned no `result`; the protocol requires one"
        ))
    })?;
    if let Some((kind, _)) = bound
        && let Some(handle) = response.get("session").filter(|value| !value.is_null())
    {
        let session = sessions
            .get::<PluginSession>(&kind)
            .map_err(StepError::Environment)?
            .ok_or_else(|| {
                StepError::Environment(format!(
                    "session `{kind}` disappeared while step `{step}` ran"
                ))
            })?;
        session.set_handle(handle.clone());
    }
    Ok(result)
}

fn step_error(error: CallError) -> StepError {
    match error {
        CallError::Timeout(message) => StepError::Timeout(message),
        CallError::Environment(message) => StepError::Environment(message),
        CallError::Remote {
            code: -32602,
            message,
        } => StepError::InvalidArguments(message),
        CallError::Remote { message, .. } => StepError::Execution(message),
    }
}

fn observer_error(error: CallError) -> ObserverError {
    match error {
        CallError::Remote {
            code: -32602,
            message,
        } => ObserverError::InvalidArguments(message),
        other => ObserverError::Environment(other.message().to_string()),
    }
}

/// A plugin-owned session bound to one case or one interactive session.
pub(crate) struct PluginSession {
    host: Arc<PluginHost>,
    kind: String,
    handle: Value,
    detachable: bool,
    detached: bool,
    participated: bool,
}

impl PluginSession {
    pub(crate) fn new(host: Arc<PluginHost>, kind: String, detachable: bool) -> Self {
        Self {
            host,
            kind,
            handle: Value::Null,
            detachable,
            detached: false,
            participated: false,
        }
    }

    pub(crate) fn reattach(
        host: Arc<PluginHost>,
        kind: String,
        handle: &Value,
        detachable: bool,
    ) -> Result<Self, String> {
        let handle = host.reattach_session(&kind, handle)?;
        Ok(Self {
            host,
            kind,
            handle,
            detachable,
            detached: false,
            participated: false,
        })
    }

    pub(crate) fn handle(&self) -> &Value {
        &self.handle
    }

    fn set_handle(&mut self, handle: Value) {
        self.handle = handle;
        self.participated = true;
    }
}

impl Session for PluginSession {
    fn close(&mut self) -> Result<(), String> {
        if self.detached {
            return Ok(());
        }
        self.host.close_session(&self.kind, &self.handle)
    }

    fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        if self.detached {
            return Ok(Vec::new());
        }
        self.host.session_evidence(&self.kind, &self.handle)
    }

    fn step_evidence(&mut self) -> Result<Vec<Evidence>, String> {
        self.evidence()
    }

    fn detach(&mut self) -> Result<Option<Value>, String> {
        if !self.detachable {
            return Ok(None);
        }
        if self.detached {
            return Ok(Some(self.handle.clone()));
        }
        match self.host.detach_session(&self.kind, &self.handle)? {
            Some(handle) => {
                self.handle = handle.clone();
                self.detached = true;
                Ok(Some(handle))
            }
            None => Ok(None),
        }
    }

    fn snapshot(
        &mut self,
        scope_ref: Option<&str>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        self.host
            .snapshot_session(&self.kind, &self.handle, scope_ref, max_depth, max_nodes)
    }

    fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        if !self.participated {
            return Ok(None);
        }
        self.participated = false;
        self.snapshot(None, None, None)
    }
}

/// A read-only observation implemented by a plugin.
pub(crate) struct PluginObserver {
    host: Arc<PluginHost>,
    name: String,
    args: Value,
    handle: Option<Value>,
}

impl PluginObserver {
    pub(crate) fn create(host: Arc<PluginHost>, name: String, args: Value) -> Self {
        Self {
            host,
            name,
            args,
            handle: None,
        }
    }
}

impl Observer for PluginObserver {
    fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
        let handle = self
            .host
            .start_observer(&self.name, &self.args)
            .map_err(observer_error)?;
        self.handle = Some(handle);
        Ok(())
    }

    fn collect(&mut self, _sessions: &mut Sessions) -> Result<Value, ObserverError> {
        let handle = self.handle.as_ref().cloned().unwrap_or(Value::Null);
        self.host
            .collect_observer(&self.name, &handle)
            .map_err(observer_error)
    }
}
