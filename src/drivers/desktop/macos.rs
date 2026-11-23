//! macOS Accessibility (`AXUIElement`) + `CGEvent` (`enigo`) + `screencapture`
//! backend (`#[cfg(target_os = "macos")]`) for `desktop.*`.
//!
//! Drives semantic `AXUIElement` attributes and actions first (`AXPress`,
//! settable `AXValue`, `AXConfirm`, `AXPick`), falling back under an exclusive
//! desktop input lock to `enigo` (`CGEvent`) for `desktop.press`, right/double
//! clicks, non-settable `NSSecureTextField` typing, and window-relative `at:`
//! coordinates. Captures windows via `/usr/sbin/screencapture -x -o -l <id>`
//! and masks `Edit` / `AXSecureTextField` rectangles in RGBA (accounting for
//! Retina backing scale) before encoding `0600` PNG files.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use axuielement::{AXError, AXUIElement, is_process_trusted};
use enigo::{Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, NewConError, Settings};
use serde_json::{Value, json};

use crate::core::paths;
use crate::core::registry::StepError;
use crate::core::session::Evidence;

use super::shared::{
    BoundsRect, DEFAULT_SNAPSHOT_MAX_DEPTH, DEFAULT_SNAPSHOT_MAX_NODES, KeyModifier,
    MAX_COLLECTED_NODES, MainKey, POLL_INTERVAL, PersistedHandle, SharedNodeData, SharedTreeData,
    StoredRef, acquire_desktop_input_lock, build_evidence_items, build_snapshot_payload,
    check_window_point_bounds, click_button_name, decode_png_to_rgba8, encode_rgba_png,
    is_unix_pid_alive, mask_tree_inputs, missing_target_timeout, node_summary, parse_uia_key_expr,
    resolve_command_and_cwd, resolve_node_in_tree, reverse_lookup_in_tree,
    spawn_unix_desktop_child, step_error_message, synthesize_locator, terminate_unix_child,
    terminate_unix_detached_pid, verify_unix_reattach_identity, wait_state_name, write_png_private,
};
use super::{
    AttachArgs, ClickButton, DesktopLocator, DesktopPoint, DesktopRole, LaunchArgs, WaitState,
};

const MAX_AX_WALK_DEPTH: usize = 32;
const TCC_ACCESSIBILITY_HINT: &str = "macOS Accessibility permission is disabled for this process — grant Accessibility access to your terminal or CI runner in System Settings -> Privacy & Security -> Accessibility";
const TCC_SCREEN_RECORDING_HINT: &str = "grant Screen Recording access to your terminal or CI runner in System Settings -> Privacy & Security -> Screen & System Audio Recording";

static TEMP_CAPTURE_SEQ: AtomicU64 = AtomicU64::new(1);

struct LiveMacTree {
    data: SharedTreeData,
    app_el: AXUIElement,
    win_el: AXUIElement,
    elements: Vec<AXUIElement>,
}

struct FoundMacWindow {
    pid: u32,
    hwnd: i64,
    win_el: AXUIElement,
    title: String,
    width: u32,
    height: u32,
}

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Native macOS `AXUIElement` + `CGEvent` backend managing one target desktop window.
pub struct DesktopBackend {
    child: Option<Child>,
    pid: u32,
    hwnd: i64,
    owned_launch: bool,
    process_start_token: Option<String>,
    local_only_reason: Option<String>,
    title: String,
    refs: BTreeMap<String, StoredRef>,
    last_step_meta: Option<Value>,
}

impl DesktopBackend {
    /// Launches a macOS application and waits for its main accessible window.
    pub fn launch(
        args: &LaunchArgs,
        base_dir: &Path,
        interactive: bool,
    ) -> Result<(Self, Value), StepError> {
        ensure_accessibility_trusted()?;

        let (command_path, resolved_cwd) = resolve_command_and_cwd(args, base_dir);
        let mut child = spawn_unix_desktop_child(
            &command_path,
            &args.args,
            resolved_cwd.as_deref(),
            &args.env,
            &[],
            interactive,
            &args.command,
        )?;
        let pid = child.id();
        let start_token = macos_proc_start_token(pid);
        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(StepError::Environment(format!(
                    "desktop application `{}` (PID {pid}) exited with {status} before opening an accessible window",
                    args.command
                )));
            }

            match find_macos_window(Some(pid), None, args.window_title_contains.as_deref()) {
                Ok(Some(win)) => {
                    let result = json!({
                        "pid": win.pid,
                        "hwnd": win.hwnd,
                        "title": win.title,
                        "window": { "width": win.width, "height": win.height },
                        "viewport": { "width": win.width, "height": win.height },
                        "owned": true,
                    });
                    let backend = Self {
                        child: Some(child),
                        pid: win.pid,
                        hwnd: win.hwnd,
                        owned_launch: true,
                        process_start_token: start_token,
                        local_only_reason: None,
                        title: win.title,
                        refs: BTreeMap::new(),
                        last_step_meta: None,
                    };
                    return Ok((backend, result));
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = terminate_unix_child(&mut child, pid);
                    return Err(err);
                }
            }

            if Instant::now() >= deadline {
                let _ = terminate_unix_child(&mut child, pid);
                return Err(StepError::Timeout(format!(
                    "timed out after {}ms waiting for macOS AXWindow of `{}` (PID {pid})",
                    args.timeout_ms, args.command
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Attaches to an already-running macOS desktop window without owning its lifetime.
    pub fn attach(args: &AttachArgs) -> Result<(Self, Value), StepError> {
        ensure_accessibility_trusted()?;
        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

        loop {
            if let Some(win) = find_macos_window(
                args.pid,
                args.executable.as_deref(),
                args.title_contains.as_deref(),
            )? {
                let reason = format!(
                    "desktop.attach connected to existing window \"{}\" (PID {}); pre-existing local desktop state is not managed by `da`",
                    win.title, win.pid
                );
                let result = json!({
                    "pid": win.pid,
                    "hwnd": win.hwnd,
                    "title": win.title,
                    "window": { "width": win.width, "height": win.height },
                    "viewport": { "width": win.width, "height": win.height },
                    "owned": false,
                    "local_only": true,
                });
                let backend = Self {
                    child: None,
                    pid: win.pid,
                    hwnd: win.hwnd,
                    owned_launch: false,
                    process_start_token: None,
                    local_only_reason: Some(reason),
                    title: win.title,
                    refs: BTreeMap::new(),
                    last_step_meta: None,
                };
                return Ok((backend, result));
            }

            if Instant::now() >= deadline {
                return Err(StepError::Timeout(format!(
                    "timed out after {}ms waiting for macOS AXWindow matching attach criteria",
                    args.timeout_ms
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Reconstructs a `DesktopBackend` from persisted session state.
    pub fn reattach(handle: &Value) -> Result<Self, String> {
        let parsed: PersistedHandle = serde_json::from_value(handle.clone())
            .map_err(|err| format!("invalid persisted desktop session handle: {err}"))?;
        verify_unix_reattach_identity(
            parsed.pid,
            parsed.owned_launch,
            parsed.process_start_token.as_deref(),
            macos_proc_start_token,
        )?;
        ensure_accessibility_trusted().map_err(|e| step_error_message(&e).to_string())?;

        let win = find_macos_window_by_pid_and_hwnd(parsed.pid, parsed.hwnd)
            .map_err(|e| step_error_message(&e).to_string())?
            .ok_or_else(|| {
                format!(
                    "desktop window closed: no active AXWindow for PID {}",
                    parsed.pid
                )
            })?;

        Ok(Self {
            child: None,
            pid: parsed.pid,
            hwnd: if win.hwnd > 0 { win.hwnd } else { parsed.hwnd },
            owned_launch: parsed.owned_launch,
            process_start_token: parsed.process_start_token,
            local_only_reason: parsed.local_only_reason,
            title: win.title,
            refs: parsed.refs,
            last_step_meta: None,
        })
    }

    /// Disposes a persisted desktop handle, terminating only verified owned processes.
    pub fn dispose_handle(handle: &Value) -> Result<(), String> {
        let parsed: PersistedHandle = serde_json::from_value(handle.clone())
            .map_err(|err| format!("invalid persisted desktop session handle: {err}"))?;
        if parsed.owned_launch {
            terminate_unix_detached_pid(
                parsed.pid,
                parsed.process_start_token.as_deref(),
                macos_proc_start_token,
            )?;
        }
        Ok(())
    }

    pub fn local_only_reason(&self) -> Option<String> {
        self.local_only_reason.clone()
    }

    pub fn detach(&mut self) -> Result<Option<Value>, String> {
        let mut owned_launch = self.owned_launch;
        if owned_launch && self.process_start_token.is_none() {
            if let Some(child) = &mut self.child
                && child.try_wait().map_err(|e| e.to_string())?.is_some()
            {
                owned_launch = false;
                self.owned_launch = false;
            } else {
                self.process_start_token =
                    Some(macos_proc_start_token(self.pid).ok_or_else(|| {
                        format!("failed to query process start time for PID {}", self.pid)
                    })?);
            }
        }
        let handle = PersistedHandle {
            pid: self.pid,
            hwnd: self.hwnd,
            owned_launch,
            process_start_token: self.process_start_token.clone(),
            local_only_reason: self.local_only_reason.clone(),
            title: self.title.clone(),
            refs: self.refs.clone(),
        };
        self.child.take();
        serde_json::to_value(handle)
            .map(Some)
            .map_err(|err| format!("failed to serialize desktop session handle: {err}"))
    }

    pub fn close(&mut self) -> Result<(), String> {
        if self.owned_launch {
            if let Some(mut child) = self.child.take() {
                terminate_unix_child(&mut child, self.pid)?;
            } else {
                terminate_unix_detached_pid(
                    self.pid,
                    self.process_start_token.as_deref(),
                    macos_proc_start_token,
                )?;
            }
        }
        Ok(())
    }

    pub fn take_step_metadata(&mut self) -> Option<Value> {
        self.last_step_meta.take()
    }

    pub fn click(
        &mut self,
        target: Option<&DesktopLocator>,
        at: Option<&DesktopPoint>,
        ref_id: Option<&str>,
        button: ClickButton,
        timeout_ms: u64,
        interactive: bool,
    ) -> Result<Value, StepError> {
        self.last_step_meta = None;
        if let Some(point) = at {
            return self.click_at(point, button, timeout_ms, interactive);
        }

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let live = self.ensure_and_collect_tree()?;
            match resolve_node_in_tree(&live.data, target, ref_id, &self.refs)? {
                Some(idx) => {
                    let node = &live.data.nodes[idx];
                    if !node.enabled {
                        if Instant::now() >= deadline {
                            return Err(StepError::Execution(format!(
                                "desktop element (`{}`) is disabled",
                                node.role.as_str()
                            )));
                        }
                    } else {
                        let (locator, fragile) = synthesize_locator(idx, &live.data.nodes);
                        if interactive {
                            self.last_step_meta = Some(json!({
                                "ref": ref_id,
                                "synthesized_locator": locator,
                                "fragile": fragile,
                            }));
                        }

                        let clicked_semantically = button == ClickButton::Left
                            && try_ax_semantic_click(&live.elements[idx], node.role);

                        if !clicked_semantically {
                            let _lock = acquire_desktop_input_lock(timeout_ms)?;
                            focus_ax_window(&live.app_el, &live.win_el);
                            let cx =
                                node.screen_rect.0 + (node.screen_rect.2 - node.screen_rect.0) / 2;
                            let cy =
                                node.screen_rect.1 + (node.screen_rect.3 - node.screen_rect.1) / 2;
                            enigo_mouse_click(cx, cy, button)?;
                        }

                        return Ok(json!({
                            "clicked": true,
                            "button": click_button_name(button),
                            "role": node.role.as_str(),
                            "name": node.name,
                            "automation_id": node.automation_id,
                            "window": {
                                "width": live.data.win_width,
                                "height": live.data.win_height,
                            },
                        }));
                    }
                }
                None => {
                    if Instant::now() >= deadline {
                        return Err(missing_target_timeout(ref_id, timeout_ms));
                    }
                }
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn click_at(
        &mut self,
        point: &DesktopPoint,
        button: ClickButton,
        timeout_ms: u64,
        interactive: bool,
    ) -> Result<Value, StepError> {
        let _lock = acquire_desktop_input_lock(timeout_ms)?;
        let live = self.ensure_and_collect_tree()?;
        check_window_point_bounds(&live.data, point)?;

        focus_ax_window(&live.app_el, &live.win_el);
        let sx = live.data.win_left + point.x;
        let sy = live.data.win_top + point.y;
        let reverse_meta = if interactive {
            reverse_lookup_in_tree(&live.data, sx, sy)
        } else {
            None
        };

        enigo_mouse_click(sx, sy, button)?;

        if let Some((locator, fragile)) = reverse_meta {
            self.last_step_meta = Some(json!({
                "synthesized_locator": locator,
                "fragile": fragile,
            }));
        }

        Ok(json!({
            "clicked": true,
            "button": click_button_name(button),
            "at": { "x": point.x, "y": point.y },
            "window": { "width": live.data.win_width, "height": live.data.win_height },
            "viewport": { "width": live.data.win_width, "height": live.data.win_height },
        }))
    }

    pub fn type_text(
        &mut self,
        target: Option<&DesktopLocator>,
        at: Option<&DesktopPoint>,
        ref_id: Option<&str>,
        text: &str,
        timeout_ms: u64,
        interactive: bool,
    ) -> Result<Value, StepError> {
        self.last_step_meta = None;
        if let Some(point) = at {
            let _lock = acquire_desktop_input_lock(timeout_ms)?;
            let live = self.ensure_and_collect_tree()?;
            check_window_point_bounds(&live.data, point)?;

            focus_ax_window(&live.app_el, &live.win_el);
            let sx = live.data.win_left + point.x;
            let sy = live.data.win_top + point.y;
            let reverse_meta = if interactive {
                reverse_lookup_in_tree(&live.data, sx, sy)
            } else {
                None
            };

            enigo_mouse_click(sx, sy, ClickButton::Left)?;
            enigo_type_text(text, false)?;

            if let Some((locator, fragile)) = reverse_meta {
                self.last_step_meta = Some(json!({
                    "synthesized_locator": locator,
                    "fragile": fragile,
                }));
            }

            return Ok(json!({
                "typed_chars": text.chars().count(),
                "window": { "width": live.data.win_width, "height": live.data.win_height },
                "viewport": { "width": live.data.win_width, "height": live.data.win_height },
            }));
        }

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let live = self.ensure_and_collect_tree()?;
            match resolve_node_in_tree(&live.data, target, ref_id, &self.refs)? {
                Some(idx) => {
                    let node = &live.data.nodes[idx];
                    if !node.enabled {
                        if Instant::now() >= deadline {
                            return Err(StepError::Execution(format!(
                                "desktop element (`{}`) is disabled",
                                node.role.as_str()
                            )));
                        }
                    } else {
                        let (locator, fragile) = synthesize_locator(idx, &live.data.nodes);
                        if interactive {
                            self.last_step_meta = Some(json!({
                                "ref": ref_id,
                                "synthesized_locator": locator,
                                "fragile": fragile,
                            }));
                        }

                        let el = &live.elements[idx];
                        let typed_semantically =
                            !node.is_password && try_ax_set_value(&live.win_el, el, text);

                        if !typed_semantically {
                            let _lock = acquire_desktop_input_lock(timeout_ms)?;
                            focus_ax_window(&live.app_el, &live.win_el);
                            let _ = el.set_bool_attribute("AXFocused", true);
                            if el.bool_attribute("AXFocused").ok().flatten() != Some(true) {
                                let cx = node.screen_rect.0
                                    + (node.screen_rect.2 - node.screen_rect.0) / 2;
                                let cy = node.screen_rect.1
                                    + (node.screen_rect.3 - node.screen_rect.1) / 2;
                                enigo_mouse_click(cx, cy, ClickButton::Left)?;
                            }
                            enigo_type_text(text, true)?;
                        }

                        return Ok(json!({
                            "typed_chars": text.chars().count(),
                            "role": node.role.as_str(),
                            "window": {
                                "width": live.data.win_width,
                                "height": live.data.win_height,
                            },
                        }));
                    }
                }
                None => {
                    if Instant::now() >= deadline {
                        return Err(missing_target_timeout(ref_id, timeout_ms));
                    }
                }
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn press(&mut self, uia_key_expr: &str, timeout_ms: u64) -> Result<Value, StepError> {
        self.last_step_meta = None;
        let (mods, main_key) = parse_uia_key_expr(uia_key_expr)?;
        let _lock = acquire_desktop_input_lock(timeout_ms)?;
        let live = self.ensure_and_collect_tree()?;
        focus_ax_window(&live.app_el, &live.win_el);
        enigo_send_key_chord(&mods, &main_key)?;
        Ok(json!({ "pressed": true }))
    }

    pub fn wait(
        &mut self,
        target: Option<&DesktopLocator>,
        ref_id: Option<&str>,
        state: WaitState,
        timeout_ms: u64,
        interactive: bool,
    ) -> Result<Value, StepError> {
        self.last_step_meta = None;
        let start = Instant::now();
        let deadline = start + Duration::from_millis(timeout_ms);

        loop {
            match self.ensure_and_collect_tree() {
                Ok(live) => {
                    let matched_idx = resolve_node_in_tree(&live.data, target, ref_id, &self.refs)?;
                    let satisfied = match (state, matched_idx) {
                        (WaitState::Gone, None) => true,
                        (WaitState::Gone, Some(_)) => false,
                        (WaitState::Present, Some(_)) => true,
                        (WaitState::Enabled, Some(idx)) => live.data.nodes[idx].enabled,
                        (WaitState::Visible, Some(idx)) => !live.data.nodes[idx].offscreen,
                        (_, None) => false,
                    };

                    if satisfied {
                        if let Some(idx) = matched_idx
                            && interactive
                        {
                            let (locator, fragile) = synthesize_locator(idx, &live.data.nodes);
                            self.last_step_meta = Some(json!({
                                "ref": ref_id,
                                "synthesized_locator": locator,
                                "fragile": fragile,
                            }));
                        }
                        let element = matched_idx.map(|idx| node_summary(&live.data.nodes[idx]));
                        return Ok(json!({
                            "matched": true,
                            "state": wait_state_name(state),
                            "elapsed_ms": start.elapsed().as_millis() as u64,
                            "element": element,
                        }));
                    }
                }
                Err(err) => {
                    let msg = step_error_message(&err);
                    if state == WaitState::Gone
                        && (msg.contains("closed") || msg.contains("exited"))
                    {
                        return Ok(json!({
                            "matched": true,
                            "state": "gone",
                            "window_closed": true,
                            "elapsed_ms": start.elapsed().as_millis() as u64,
                        }));
                    }
                    return Err(err);
                }
            }

            if Instant::now() >= deadline {
                return Err(StepError::Timeout(format!(
                    "timed out after {timeout_ms}ms waiting for desktop element to reach state `{}`",
                    wait_state_name(state)
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn read(
        &mut self,
        target: Option<&DesktopLocator>,
        ref_id: Option<&str>,
        timeout_ms: u64,
        interactive: bool,
    ) -> Result<Value, StepError> {
        self.last_step_meta = None;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let live = self.ensure_and_collect_tree()?;
            if let Some(idx) = resolve_node_in_tree(&live.data, target, ref_id, &self.refs)? {
                if interactive {
                    let (locator, fragile) = synthesize_locator(idx, &live.data.nodes);
                    self.last_step_meta = Some(json!({
                        "ref": ref_id,
                        "synthesized_locator": locator,
                        "fragile": fragile,
                    }));
                }
                return Ok(node_summary(&live.data.nodes[idx]));
            }
            if Instant::now() >= deadline {
                return Err(missing_target_timeout(ref_id, timeout_ms));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    pub fn screenshot(
        &mut self,
        path: &Path,
        mask_inputs: bool,
        timeout_ms: u64,
    ) -> Result<Value, StepError> {
        self.last_step_meta = None;
        let _lock = acquire_desktop_input_lock(timeout_ms)?;
        let live = self.ensure_and_collect_tree()?;
        if self.hwnd <= 0
            && let Some(num) = live.win_el.i64_attribute("AXWindowNumber").ok().flatten()
            && num > 0
        {
            self.hwnd = num;
        }
        let (png_bytes, width, height, masked_count) = capture_macos_window_png(
            &live.app_el,
            &live.win_el,
            self.hwnd,
            &live.data,
            mask_inputs,
        )?;
        write_png_private(path, &png_bytes)?;

        Ok(json!({
            "path": paths::display(path),
            "width": width,
            "height": height,
            "masked_inputs": mask_inputs,
            "masked_count": masked_count,
        }))
    }

    pub fn snapshot(
        &mut self,
        scope_ref: Option<&str>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        let live = self
            .ensure_and_collect_tree()
            .map_err(|e| step_error_message(&e).to_string())?;
        self.title.clone_from(&live.data.title);
        build_snapshot_payload(
            &live.data,
            self.pid,
            self.hwnd,
            &mut self.refs,
            scope_ref,
            max_depth.unwrap_or(DEFAULT_SNAPSHOT_MAX_DEPTH),
            max_nodes.unwrap_or(DEFAULT_SNAPSHOT_MAX_NODES),
            true,
        )
        .map(Some)
    }

    pub fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        let Ok(live) = self.ensure_and_collect_tree() else {
            return Ok(None);
        };
        self.title.clone_from(&live.data.title);
        build_snapshot_payload(
            &live.data,
            self.pid,
            self.hwnd,
            &mut self.refs,
            None,
            DEFAULT_SNAPSHOT_MAX_DEPTH,
            DEFAULT_SNAPSHOT_MAX_NODES,
            true,
        )
        .map(Some)
    }

    pub fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        let live_res = self.ensure_and_collect_tree();
        let (tree_res, png_res) = match live_res {
            Ok(live) => {
                let win_id = if self.hwnd > 0 {
                    self.hwnd
                } else {
                    live.win_el
                        .i64_attribute("AXWindowNumber")
                        .ok()
                        .flatten()
                        .unwrap_or(0)
                };
                let shot =
                    capture_macos_window_png(&live.app_el, &live.win_el, win_id, &live.data, true)
                        .map(|(bytes, _, _, _)| bytes);
                (Ok(live.data), Some(shot))
            }
            Err(err) => (Err(err), None),
        };
        build_evidence_items(
            self.pid,
            self.hwnd,
            &self.title,
            tree_res,
            png_res,
            &mut self.refs,
        )
    }

    fn ensure_and_collect_tree(&mut self) -> Result<LiveMacTree, StepError> {
        if let Some(child) = &mut self.child
            && let Ok(Some(status)) = child.try_wait()
        {
            return Err(StepError::Environment(format!(
                "desktop window closed: target process (PID {}) exited with {status}",
                self.pid
            )));
        }
        if !is_unix_pid_alive(self.pid) {
            return Err(StepError::Environment(format!(
                "desktop window closed: target process (PID {}) exited",
                self.pid
            )));
        }
        ensure_accessibility_trusted()?;

        let win = find_macos_window_by_pid_and_hwnd(self.pid, self.hwnd)?.ok_or_else(|| {
            StepError::Environment(format!(
                "desktop window closed: no active AXWindow for PID {}",
                self.pid
            ))
        })?;
        if win.hwnd > 0 {
            self.hwnd = win.hwnd;
        }
        self.title.clone_from(&win.title);

        let pid_i32 = i32::try_from(self.pid).map_err(|_| {
            StepError::Environment(format!("PID {} exceeds macOS pid_t range", self.pid))
        })?;
        let app_el = AXUIElement::from_pid(pid_i32).ok_or_else(|| {
            StepError::Environment(format!(
                "desktop window closed: AXUIElementCreateApplication failed for PID {}",
                self.pid
            ))
        })?;
        let _ = app_el.set_timeout(5.0);

        collect_macos_tree(self.pid, app_el, win.win_el)
    }
}

impl Drop for DesktopBackend {
    fn drop(&mut self) {
        if self.owned_launch
            && let Some(child) = &mut self.child
        {
            let _ = terminate_unix_child(child, self.pid);
        }
    }
}

fn ensure_accessibility_trusted() -> Result<(), StepError> {
    if !is_process_trusted() {
        return Err(StepError::Environment(TCC_ACCESSIBILITY_HINT.to_string()));
    }
    Ok(())
}

fn classify_ax_error(pid: u32, err: &AXError) -> StepError {
    if !is_unix_pid_alive(pid) {
        return StepError::Environment(format!(
            "desktop window closed: target process (PID {pid}) exited"
        ));
    }
    match err {
        AXError::APIDisabled => StepError::Environment(format!(
            "macOS Accessibility API disabled (kAXErrorAPIDisabled / -25211) — {TCC_ACCESSIBILITY_HINT}"
        )),
        AXError::CannotComplete | AXError::InvalidUIElement => StepError::Environment(format!(
            "desktop window closed: target AXWindow for PID {pid} is no longer available ({err})"
        )),
        _ => StepError::Environment(format!("macOS AXUIElement error for PID {pid}: {err}")),
    }
}

fn macos_proc_start_token(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let output = Command::new("/bin/ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn list_macos_candidate_pids(exe_filter: Option<&str>) -> Vec<u32> {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-axo", "pid=,comm="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let needle_lower = exe_filter.map(|s| {
        Path::new(s)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(s)
            .to_ascii_lowercase()
    });

    let mut pids = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        let Some((pid_part, comm_part)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid_part.trim().parse::<u32>() else {
            continue;
        };
        if pid <= 1 {
            continue;
        }
        let comm = comm_part.trim();
        if let Some(needle) = &needle_lower {
            let base = Path::new(comm)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(comm)
                .to_ascii_lowercase();
            if base != *needle && !comm.to_ascii_lowercase().contains(needle) {
                continue;
            }
        }
        pids.push(pid);
    }
    pids
}

fn candidate_windows_for_app(app: &AXUIElement) -> Result<Vec<AXUIElement>, AXError> {
    let mut out = app.element_array_attribute("AXWindows")?;
    if out.is_empty() {
        if let Ok(Some(main_win)) = app.element_attribute("AXMainWindow") {
            out.push(main_win);
        } else if let Ok(Some(focused_win)) = app.element_attribute("AXFocusedWindow") {
            out.push(focused_win);
        }
    }
    Ok(out)
}

fn find_macos_window_by_pid_and_hwnd(
    pid: u32,
    preferred_hwnd: i64,
) -> Result<Option<FoundMacWindow>, StepError> {
    let pid_i32 = i32::try_from(pid)
        .map_err(|_| StepError::Environment(format!("PID {pid} exceeds macOS pid_t range")))?;
    let Some(app) = AXUIElement::from_pid(pid_i32) else {
        return Ok(None);
    };
    let _ = app.set_timeout(5.0);

    let windows = match candidate_windows_for_app(&app) {
        Ok(list) => list,
        Err(AXError::CannotComplete | AXError::AttributeUnsupported(_) | AXError::NoValue) => {
            return Ok(None);
        }
        Err(err) => return Err(classify_ax_error(pid, &err)),
    };

    let mut fallback: Option<FoundMacWindow> = None;
    for win_el in windows {
        let _ = win_el.set_timeout(5.0);
        if win_el.bool_attribute("AXMinimized").ok().flatten() == Some(true) {
            continue;
        }
        let Some(size) = win_el.size_attribute("AXSize").ok().flatten() else {
            continue;
        };
        let w = size.width.round() as i32;
        let h = size.height.round() as i32;
        if w <= 0 || h <= 0 {
            continue;
        }
        let hwnd = win_el
            .i64_attribute("AXWindowNumber")
            .ok()
            .flatten()
            .unwrap_or(0);
        let title = win_el
            .string_attribute("AXTitle")
            .ok()
            .flatten()
            .unwrap_or_default();
        let candidate = FoundMacWindow {
            pid,
            hwnd,
            win_el,
            title,
            width: w as u32,
            height: h as u32,
        };
        if preferred_hwnd > 0 && hwnd == preferred_hwnd {
            return Ok(Some(candidate));
        }
        if fallback.is_none() {
            fallback = Some(candidate);
        }
    }
    Ok(fallback)
}

fn find_macos_window(
    pid_filter: Option<u32>,
    exe_filter: Option<&str>,
    title_contains: Option<&str>,
) -> Result<Option<FoundMacWindow>, StepError> {
    let pids = match pid_filter {
        Some(pid) => vec![pid],
        None => list_macos_candidate_pids(exe_filter),
    };
    let title_lower = title_contains.map(str::to_ascii_lowercase);

    for pid in pids {
        let Ok(pid_i32) = i32::try_from(pid) else {
            continue;
        };
        let Some(app) = AXUIElement::from_pid(pid_i32) else {
            continue;
        };
        let _ = app.set_timeout(2.5);

        let windows = match candidate_windows_for_app(&app) {
            Ok(list) => list,
            Err(AXError::APIDisabled) => {
                return Err(classify_ax_error(pid, &AXError::APIDisabled));
            }
            Err(_) => continue,
        };

        for win_el in windows {
            let _ = win_el.set_timeout(5.0);
            if win_el.bool_attribute("AXMinimized").ok().flatten() == Some(true) {
                continue;
            }
            let Some(size) = win_el.size_attribute("AXSize").ok().flatten() else {
                continue;
            };
            let w = size.width.round() as i32;
            let h = size.height.round() as i32;
            if w <= 0 || h <= 0 {
                continue;
            }
            let title = win_el
                .string_attribute("AXTitle")
                .ok()
                .flatten()
                .unwrap_or_default();
            if let Some(needle) = &title_lower
                && !title.to_ascii_lowercase().contains(needle)
            {
                continue;
            }
            let hwnd = win_el
                .i64_attribute("AXWindowNumber")
                .ok()
                .flatten()
                .unwrap_or(0);
            return Ok(Some(FoundMacWindow {
                pid,
                hwnd,
                win_el,
                title,
                width: w as u32,
                height: h as u32,
            }));
        }
    }
    Ok(None)
}

fn map_ax_role(ax_role: &str, ax_subrole: &str) -> (DesktopRole, bool) {
    let is_password = ax_subrole == "AXSecureTextField";
    if is_password {
        return (DesktopRole::Edit, true);
    }
    let role = match ax_role {
        "AXButton" | "AXDisclosureTriangle" => DesktopRole::Button,
        "AXMenuButton" => DesktopRole::SplitButton,
        "AXTextField" | "AXTextArea" => DesktopRole::Edit,
        "AXPopUpButton" | "AXComboBox" => DesktopRole::ComboBox,
        "AXCheckBox" => DesktopRole::CheckBox,
        "AXRadioButton" if ax_subrole == "AXTabButton" => DesktopRole::TabItem,
        "AXRadioButton" => DesktopRole::RadioButton,
        "AXStaticText" | "AXHeading" => DesktopRole::Text,
        "AXSlider" => DesktopRole::Slider,
        "AXIncrementor" => DesktopRole::Spinner,
        "AXProgressIndicator" | "AXBusyIndicator" | "AXLevelIndicator" => DesktopRole::ProgressBar,
        "AXWindow" => DesktopRole::Window,
        "AXScrollArea" | "AXSplitGroup" | "AXLayoutArea" | "AXPopover" | "AXSheet" | "AXDrawer" => {
            DesktopRole::Pane
        }
        "AXGroup" | "AXRadioGroup" => DesktopRole::Group,
        "AXTabGroup" => DesktopRole::Tab,
        "AXToolbar" => DesktopRole::ToolBar,
        "AXScrollBar" => DesktopRole::ScrollBar,
        "AXValueIndicator" => DesktopRole::Thumb,
        "AXMenuBar" => DesktopRole::MenuBar,
        "AXMenu" => DesktopRole::Menu,
        "AXMenuItem" | "AXMenuBarItem" => DesktopRole::MenuItem,
        "AXList" => DesktopRole::List,
        "AXOutline" => DesktopRole::Tree,
        "AXRow" if ax_subrole == "AXOutlineRow" => DesktopRole::TreeItem,
        "AXRow" if ax_subrole == "AXTableRow" => DesktopRole::DataItem,
        "AXRow" => DesktopRole::ListItem,
        "AXCell" => DesktopRole::DataItem,
        "AXTable" => DesktopRole::Table,
        "AXGrid" => DesktopRole::DataGrid,
        "AXColumn" => DesktopRole::Header,
        "AXSplitter" => DesktopRole::Separator,
        "AXImage" => DesktopRole::Image,
        "AXLink" => DesktopRole::Hyperlink,
        "AXDateField" | "AXTimeField" => DesktopRole::Calendar,
        "AXHelpTag" => DesktopRole::ToolTip,
        _ => DesktopRole::Custom,
    };
    (role, false)
}

fn read_ax_scalar_value(el: &AXUIElement) -> Option<String> {
    let val = el.attribute("AXValue").ok()??;
    if let Some(s) = val.as_string() {
        return Some(s);
    }
    if let Some(i) = val.as_i64() {
        return Some(i.to_string());
    }
    if let Some(f) = val.as_f64() {
        return Some(f.to_string());
    }
    if let Some(b) = val.as_bool() {
        return Some(b.to_string());
    }
    None
}

fn collect_macos_tree(
    pid: u32,
    app_el: AXUIElement,
    win_el: AXUIElement,
) -> Result<LiveMacTree, StepError> {
    let win_pos = win_el
        .point_attribute("AXPosition")
        .map_err(|err| classify_ax_error(pid, &err))?
        .ok_or_else(|| {
            StepError::Environment(format!(
                "desktop window closed: AXPosition unavailable for PID {pid}"
            ))
        })?;
    let win_size = win_el
        .size_attribute("AXSize")
        .map_err(|err| classify_ax_error(pid, &err))?
        .ok_or_else(|| {
            StepError::Environment(format!(
                "desktop window closed: AXSize unavailable for PID {pid}"
            ))
        })?;

    let win_left = win_pos.x.round() as i32;
    let win_top = win_pos.y.round() as i32;
    let win_w_i32 = win_size.width.round() as i32;
    let win_h_i32 = win_size.height.round() as i32;
    if win_w_i32 <= 0 || win_h_i32 <= 0 {
        return Err(StepError::Environment(format!(
            "desktop window closed: AXWindow has empty bounds for PID {pid}"
        )));
    }
    let win_width = win_w_i32 as u32;
    let win_height = win_h_i32 as u32;
    let win_title = win_el
        .string_attribute("AXTitle")
        .ok()
        .flatten()
        .unwrap_or_default();

    let mut nodes = Vec::new();
    let mut elements = Vec::new();
    let mut truncated = false;

    struct MacWalkCtx<'a> {
        win_left: i32,
        win_top: i32,
        nodes: &'a mut Vec<SharedNodeData>,
        elements: &'a mut Vec<AXUIElement>,
        truncated: &'a mut bool,
    }

    fn walk_ax(ctx: &mut MacWalkCtx<'_>, el: AXUIElement, parent: Option<usize>, depth: usize) {
        if ctx.nodes.len() >= MAX_COLLECTED_NODES {
            *ctx.truncated = true;
            return;
        }
        if depth > MAX_AX_WALK_DEPTH {
            *ctx.truncated = true;
            return;
        }
        let _ = el.set_timeout(2.5);

        let ax_role = el
            .string_attribute("AXRole")
            .ok()
            .flatten()
            .unwrap_or_default();
        let ax_subrole = el
            .string_attribute("AXSubrole")
            .ok()
            .flatten()
            .unwrap_or_default();
        let (mut role, is_password) = map_ax_role(&ax_role, &ax_subrole);
        if depth == 0 {
            role = DesktopRole::Window;
        }

        let automation_id = el
            .string_attribute("AXIdentifier")
            .ok()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && !s.starts_with("_NS:"));

        let value = if is_password {
            None
        } else {
            read_ax_scalar_value(&el)
        };

        let title = el
            .string_attribute("AXTitle")
            .ok()
            .flatten()
            .unwrap_or_default();
        let desc = el
            .string_attribute("AXDescription")
            .ok()
            .flatten()
            .unwrap_or_default();

        let name = if !title.is_empty() {
            title
        } else if !desc.is_empty() {
            desc
        } else if role == DesktopRole::Text {
            value.clone().unwrap_or_default()
        } else if role == DesktopRole::Edit {
            el.string_attribute("AXPlaceholderValue")
                .ok()
                .flatten()
                .unwrap_or_default()
        } else {
            String::new()
        };

        let class_name = if !ax_subrole.is_empty() {
            format!("{ax_role}:{ax_subrole}")
        } else {
            ax_role
        };

        let pos = el.point_attribute("AXPosition").ok().flatten();
        let size = el.size_attribute("AXSize").ok().flatten();
        let (sx, sy, sw, sh) = match (pos, size) {
            (Some(p), Some(s)) => (
                p.x.round() as i32,
                p.y.round() as i32,
                s.width.round().max(0.0) as i32,
                s.height.round().max(0.0) as i32,
            ),
            _ => (ctx.win_left, ctx.win_top, 0, 0),
        };

        let enabled = el
            .bool_attribute("AXEnabled")
            .ok()
            .flatten()
            .unwrap_or(true);
        let hidden = el
            .bool_attribute("AXHidden")
            .ok()
            .flatten()
            .unwrap_or(false);
        let offscreen = hidden || sw <= 0 || sh <= 0;

        let idx = ctx.nodes.len();
        ctx.nodes.push(SharedNodeData {
            index: idx,
            parent,
            depth,
            subtree_end: idx + 1,
            runtime_key: String::new(),
            role,
            name,
            automation_id,
            class_name,
            value,
            enabled,
            offscreen,
            is_password,
            screen_rect: (sx, sy, sx.saturating_add(sw), sy.saturating_add(sh)),
            bounds: BoundsRect {
                x: sx.saturating_sub(ctx.win_left),
                y: sy.saturating_sub(ctx.win_top),
                width: sw,
                height: sh,
            },
        });
        let children = el.children().unwrap_or_default();
        ctx.elements.push(el);

        for child in children {
            if ctx.nodes.len() >= MAX_COLLECTED_NODES {
                *ctx.truncated = true;
                break;
            }
            walk_ax(ctx, child, Some(idx), depth + 1);
        }
        ctx.nodes[idx].subtree_end = ctx.nodes.len();
    }

    let root_win_clone = win_el.clone();
    let mut ctx = MacWalkCtx {
        win_left,
        win_top,
        nodes: &mut nodes,
        elements: &mut elements,
        truncated: &mut truncated,
    };
    walk_ax(&mut ctx, root_win_clone, None, 0);

    if nodes.is_empty() {
        return Err(StepError::Environment(format!(
            "desktop window closed: AXWindow disappeared during tree walk for PID {pid}"
        )));
    }

    Ok(LiveMacTree {
        data: SharedTreeData {
            win_left,
            win_top,
            win_width,
            win_height,
            title: win_title,
            nodes,
            truncated,
        },
        app_el,
        win_el,
        elements,
    })
}

fn focus_ax_window(app_el: &AXUIElement, win_el: &AXUIElement) {
    let _ = app_el.set_bool_attribute("AXFrontmost", true);
    let _ = win_el.perform_action("AXRaise");
    let _ = win_el.set_bool_attribute("AXMain", true);
}

fn try_ax_semantic_click(el: &AXUIElement, role: DesktopRole) -> bool {
    if matches!(
        role,
        DesktopRole::Edit
            | DesktopRole::Pane
            | DesktopRole::Group
            | DesktopRole::Custom
            | DesktopRole::Window
            | DesktopRole::Document
    ) {
        return false;
    }

    let actions = el.action_names().unwrap_or_default();
    for preferred in ["AXPress", "AXPick", "AXConfirm"] {
        if (actions.is_empty() || actions.iter().any(|a| a == preferred))
            && el.perform_action(preferred).is_ok()
        {
            return true;
        }
    }
    false
}

fn try_ax_set_value(win_el: &AXUIElement, el: &AXUIElement, text: &str) -> bool {
    if !el.is_attribute_settable("AXValue").unwrap_or(false) {
        return false;
    }
    let _ = win_el.set_bool_attribute("AXMain", true);
    let _ = el.set_bool_attribute("AXFocused", true);
    el.set_string_attribute("AXValue", text).is_ok()
}

fn new_enigo() -> Result<Enigo, StepError> {
    let settings = Settings {
        open_prompt_to_get_permissions: false,
        ..Settings::default()
    };
    Enigo::new(&settings).map_err(|err| match err {
        NewConError::NoPermission => StepError::Environment(TCC_ACCESSIBILITY_HINT.to_string()),
        other => StepError::Environment(format!(
            "failed to initialize macOS CGEvent input source: {other}"
        )),
    })
}

fn enigo_mouse_click(sx: i32, sy: i32, button: ClickButton) -> Result<(), StepError> {
    let mut enigo = new_enigo()?;
    enigo
        .move_mouse(sx, sy, Coordinate::Abs)
        .map_err(|e| StepError::Execution(format!("macOS mouse move failed: {e}")))?;

    match button {
        ClickButton::Left => enigo
            .button(Button::Left, Direction::Click)
            .map_err(|e| StepError::Execution(format!("macOS left click failed: {e}")))?,
        ClickButton::Right => enigo
            .button(Button::Right, Direction::Click)
            .map_err(|e| StepError::Execution(format!("macOS right click failed: {e}")))?,
        ClickButton::Double => {
            enigo
                .button(Button::Left, Direction::Click)
                .map_err(|e| StepError::Execution(format!("macOS double click (1) failed: {e}")))?;
            thread::sleep(Duration::from_millis(20));
            enigo
                .button(Button::Left, Direction::Click)
                .map_err(|e| StepError::Execution(format!("macOS double click (2) failed: {e}")))?;
        }
    }
    Ok(())
}

fn enigo_type_text(text: &str, clear_existing: bool) -> Result<(), StepError> {
    let mut enigo = new_enigo()?;
    if clear_existing {
        enigo
            .key(Key::Meta, Direction::Press)
            .map_err(|e| StepError::Execution(format!("macOS Cmd press failed: {e}")))?;
        enigo
            .key(Key::Unicode('a'), Direction::Click)
            .map_err(|e| StepError::Execution(format!("macOS Cmd+A failed: {e}")))?;
        enigo
            .key(Key::Meta, Direction::Release)
            .map_err(|e| StepError::Execution(format!("macOS Cmd release failed: {e}")))?;
        enigo
            .key(Key::Backspace, Direction::Click)
            .map_err(|e| StepError::Execution(format!("macOS Backspace clear failed: {e}")))?;
    }
    enigo
        .text(text)
        .map_err(|e| StepError::Execution(format!("macOS keyboard text entry failed: {e}")))
}

fn map_macos_modifier(modifier: KeyModifier) -> Key {
    match modifier {
        KeyModifier::Ctrl => Key::Control,
        KeyModifier::RCtrl => Key::RControl,
        KeyModifier::Alt => Key::Option,
        KeyModifier::RAlt => Key::ROption,
        KeyModifier::Shift => Key::Shift,
        KeyModifier::RShift => Key::RShift,
        KeyModifier::Meta => Key::Meta,
        KeyModifier::RMeta => Key::RCommand,
    }
}

fn map_macos_main_key(key: &MainKey) -> Result<Key, StepError> {
    let mapped = match key {
        MainKey::Enter => Key::Return,
        MainKey::Tab => Key::Tab,
        MainKey::Escape => Key::Escape,
        MainKey::Backspace => Key::Backspace,
        MainKey::Delete => Key::Delete,
        MainKey::Insert => Key::Help,
        MainKey::Space => Key::Space,
        MainKey::Home => Key::Home,
        MainKey::End => Key::End,
        MainKey::PageUp => Key::PageUp,
        MainKey::PageDown => Key::PageDown,
        MainKey::Up => Key::UpArrow,
        MainKey::Down => Key::DownArrow,
        MainKey::Left => Key::LeftArrow,
        MainKey::Right => Key::RightArrow,
        MainKey::PrintScreen => Key::F13,
        MainKey::CapsLock => Key::CapsLock,
        MainKey::Pause => Key::F15,
        MainKey::F(1) => Key::F1,
        MainKey::F(2) => Key::F2,
        MainKey::F(3) => Key::F3,
        MainKey::F(4) => Key::F4,
        MainKey::F(5) => Key::F5,
        MainKey::F(6) => Key::F6,
        MainKey::F(7) => Key::F7,
        MainKey::F(8) => Key::F8,
        MainKey::F(9) => Key::F9,
        MainKey::F(10) => Key::F10,
        MainKey::F(11) => Key::F11,
        MainKey::F(12) => Key::F12,
        MainKey::F(13) => Key::F13,
        MainKey::F(14) => Key::F14,
        MainKey::F(15) => Key::F15,
        MainKey::F(16) => Key::F16,
        MainKey::F(17) => Key::F17,
        MainKey::F(18) => Key::F18,
        MainKey::F(19) => Key::F19,
        MainKey::F(20) => Key::F20,
        MainKey::F(n) => {
            return Err(StepError::InvalidArguments(format!(
                "macOS CGEvent supports function keys F1..F20, got F{n}"
            )));
        }
        MainKey::Char(ch) => Key::Unicode(*ch),
    };
    Ok(mapped)
}

fn enigo_send_key_chord(mods: &[KeyModifier], main_key: &MainKey) -> Result<(), StepError> {
    let target_key = map_macos_main_key(main_key)?;
    let mod_keys: Vec<Key> = mods.iter().copied().map(map_macos_modifier).collect();
    let mut enigo = new_enigo()?;

    for &mk in &mod_keys {
        enigo
            .key(mk, Direction::Press)
            .map_err(|e| StepError::Execution(format!("macOS modifier press failed: {e}")))?;
    }
    enigo
        .key(target_key, Direction::Click)
        .map_err(|e| StepError::Execution(format!("macOS key click failed: {e}")))?;
    for &mk in mod_keys.iter().rev() {
        enigo
            .key(mk, Direction::Release)
            .map_err(|e| StepError::Execution(format!("macOS modifier release failed: {e}")))?;
    }
    Ok(())
}

fn capture_macos_window_png(
    app_el: &AXUIElement,
    win_el: &AXUIElement,
    window_id: i64,
    tree: &SharedTreeData,
    mask_inputs: bool,
) -> Result<(Vec<u8>, u32, u32, usize), StepError> {
    focus_ax_window(app_el, win_el);

    let seq = TEMP_CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp_path =
        std::env::temp_dir().join(format!("da-macos-winshot-{}-{seq}.png", std::process::id()));
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temp_path);
    let _cleanup = TempFileGuard(temp_path.clone());

    let mut cmd = Command::new("/usr/sbin/screencapture");
    cmd.arg("-x");
    if window_id > 0 {
        cmd.args(["-o", "-l", &window_id.to_string()]);
    } else {
        cmd.args([
            "-R",
            &format!(
                "{},{},{},{}",
                tree.win_left,
                tree.win_top,
                tree.win_width.max(1),
                tree.win_height.max(1)
            ),
        ]);
    }
    cmd.arg(&temp_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let output = cmd.output().map_err(|err| {
        StepError::Environment(format!("failed to invoke `/usr/sbin/screencapture`: {err}"))
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(StepError::Environment(format!(
            "macOS window screenshot failed (`screencapture` exited with {}): {} — {TCC_SCREEN_RECORDING_HINT}",
            output.status,
            if stderr.is_empty() {
                "no diagnostic output"
            } else {
                &stderr
            }
        )));
    }

    let raw_png = std::fs::read(&temp_path).map_err(|err| {
        StepError::Environment(format!(
            "macOS `screencapture` did not write output PNG ({err}) — {TCC_SCREEN_RECORDING_HINT}"
        ))
    })?;
    if raw_png.is_empty() {
        return Err(StepError::Environment(format!(
            "macOS `screencapture` produced an empty PNG — {TCC_SCREEN_RECORDING_HINT}"
        )));
    }

    let (width, height, mut rgba) = decode_png_to_rgba8(&raw_png)?;
    let masked_count = if mask_inputs {
        mask_tree_inputs(&mut rgba, width, height, tree)
    } else {
        0
    };
    let final_png = encode_rgba_png(&rgba, width, height)?;
    Ok((final_png, width, height, masked_count))
}
