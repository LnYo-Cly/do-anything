//! Non-Windows stub backend for `desktop.*` steps.
//!
//! Keeps `--features desktop` building and schema-testing cleanly on macOS and
//! Linux until native AT-SPI / Accessibility backends land in `#88`.

use std::path::Path;

use serde_json::Value;

use crate::core::registry::StepError;
use crate::core::session::Evidence;

use super::{AttachArgs, ClickButton, DesktopLocator, DesktopPoint, LaunchArgs, WaitState};

pub const UNSUPPORTED_PLATFORM_ERROR: &str = "desktop.* is currently implemented on Windows (#84); macOS and Linux backends are tracked in #88";

/// Placeholder desktop backend for non-Windows targets.
pub struct DesktopBackend;

impl DesktopBackend {
    /// Rejects `desktop.launch` with an explicit environment error naming `#88`.
    pub fn launch(
        _args: &LaunchArgs,
        _base_dir: &Path,
        _interactive: bool,
    ) -> Result<(Self, Value), StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    /// Rejects `desktop.attach` with an explicit environment error naming `#88`.
    pub fn attach(_args: &AttachArgs) -> Result<(Self, Value), StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    /// Rejects persisted desktop handle reattachment on non-Windows hosts.
    pub fn reattach(_handle: &Value) -> Result<Self, String> {
        Err(UNSUPPORTED_PLATFORM_ERROR.to_string())
    }

    /// Disposes a persisted desktop handle cleanly if non-owned, otherwise errors.
    pub fn dispose_handle(_handle: &Value) -> Result<(), String> {
        Ok(())
    }

    pub fn local_only_reason(&self) -> Option<String> {
        None
    }

    pub fn click(
        &mut self,
        _target: Option<&DesktopLocator>,
        _at: Option<&DesktopPoint>,
        _ref_id: Option<&str>,
        _button: ClickButton,
        _timeout_ms: u64,
        _interactive: bool,
    ) -> Result<Value, StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    pub fn type_text(
        &mut self,
        _target: Option<&DesktopLocator>,
        _at: Option<&DesktopPoint>,
        _ref_id: Option<&str>,
        _text: &str,
        _timeout_ms: u64,
        _interactive: bool,
    ) -> Result<Value, StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    pub fn press(&mut self, _uia_key_expr: &str, _timeout_ms: u64) -> Result<Value, StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    pub fn wait(
        &mut self,
        _target: Option<&DesktopLocator>,
        _ref_id: Option<&str>,
        _state: WaitState,
        _timeout_ms: u64,
        _interactive: bool,
    ) -> Result<Value, StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    pub fn read(
        &mut self,
        _target: Option<&DesktopLocator>,
        _ref_id: Option<&str>,
        _timeout_ms: u64,
        _interactive: bool,
    ) -> Result<Value, StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    pub fn screenshot(
        &mut self,
        _path: &Path,
        _mask_inputs: bool,
        _timeout_ms: u64,
    ) -> Result<Value, StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    pub fn snapshot(
        &mut self,
        _scope_ref: Option<&str>,
        _max_depth: Option<usize>,
        _max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        Err(UNSUPPORTED_PLATFORM_ERROR.to_string())
    }

    pub fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        Ok(None)
    }

    pub fn take_step_metadata(&mut self) -> Option<Value> {
        None
    }

    pub fn detach(&mut self) -> Result<Option<Value>, String> {
        Ok(None)
    }

    pub fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        Ok(Vec::new())
    }

    pub fn close(&mut self) -> Result<(), String> {
        Ok(())
    }
}
