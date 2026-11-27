//! Fallback stub backend for `desktop.*` steps on unsupported target OSes.
//!
//! Keeps `--features desktop` building and schema-testing cleanly outside
//! Windows, Linux, and macOS.

use std::path::Path;

use serde_json::Value;

use crate::core::registry::StepError;
use crate::core::session::{Evidence, MaskedViewportCapture, SynthesizedTarget};

use super::{AttachArgs, ClickButton, DesktopLocator, DesktopPoint, LaunchArgs, WaitState};

pub const UNSUPPORTED_PLATFORM_ERROR: &str =
    "desktop.* is only supported on Windows, Linux, and macOS";

/// Placeholder desktop backend for targets other than Windows, Linux, and macOS.
pub struct DesktopBackend;

impl DesktopBackend {
    /// Rejects `desktop.launch` with an explicit environment error.
    pub fn launch(
        _args: &LaunchArgs,
        _base_dir: &Path,
        _interactive: bool,
    ) -> Result<(Self, Value), StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    /// Rejects `desktop.attach` with an explicit environment error.
    pub fn attach(_args: &AttachArgs) -> Result<(Self, Value), StepError> {
        Err(StepError::Environment(
            UNSUPPORTED_PLATFORM_ERROR.to_string(),
        ))
    }

    /// Rejects persisted desktop handle reattachment on unsupported hosts.
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

    pub fn take_target_bounds(&mut self) -> Option<Value> {
        None
    }

    pub fn masked_viewport_capture(&mut self) -> Result<Option<MaskedViewportCapture>, String> {
        Err(UNSUPPORTED_PLATFORM_ERROR.to_string())
    }

    pub fn synthesize_ref(&mut self, _ref_id: &str) -> Result<Option<SynthesizedTarget>, String> {
        Err(UNSUPPORTED_PLATFORM_ERROR.to_string())
    }

    pub fn synthesize_at_point(
        &mut self,
        _x: f64,
        _y: f64,
    ) -> Result<Option<SynthesizedTarget>, String> {
        Err(UNSUPPORTED_PLATFORM_ERROR.to_string())
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
