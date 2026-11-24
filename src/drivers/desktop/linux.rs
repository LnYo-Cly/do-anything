//! Linux AT-SPI2 + pure-Rust X11/XTEST backend (`#[cfg(target_os = "linux")]`) for `desktop.*`.
//!
//! Drives semantic accessibility tree queries and actions over D-Bus (`zbus`
//! blocking client with `async-io`, zero `tokio`) so `launch`, `attach`,
//! `snapshot`, `click`, `type`, `wait`, and `read` work on both X11/Xvfb and
//! pure Wayland. Uses pure-Rust `x11rb` (`GetImage` + `XTEST`) under an
//! exclusive desktop input lock for window screenshots, `desktop.press`, and
//! `at:` coordinate fallbacks, returning an actionable `StepError::Environment`
//! when `DISPLAY` / X11 is unavailable under pure Wayland.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::process::Child;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use x11rb::connection::Connection as X11Connection;
use x11rb::image::{Image as X11Image, PixelLayout};
use x11rb::protocol::xproto::{
    self, AtomEnum, ConfigureWindowAux, ConnectionExt as XProtoExt, InputFocus, MapState,
    StackMode, Window as X11Window,
};
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as XWrapperExt;
use zbus::blocking::Connection;
use zbus::blocking::connection::Builder as ConnectionBuilder;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};

use crate::core::paths;
use crate::core::registry::StepError;
use crate::core::session::Evidence;

use super::shared::{
    BoundsRect, DEFAULT_SNAPSHOT_MAX_DEPTH, DEFAULT_SNAPSHOT_MAX_NODES, KeyModifier,
    MAX_COLLECTED_NODES, MainKey, POLL_INTERVAL, PersistedHandle, SharedNodeData, SharedTreeData,
    StoredRef, acquire_desktop_input_lock, build_evidence_items, build_snapshot_payload,
    check_window_point_bounds, click_button_name, encode_rgba_png, is_unix_pid_alive,
    mask_tree_inputs, missing_target_timeout, node_summary, parse_uia_key_expr,
    resolve_command_and_cwd, resolve_node_in_tree, reverse_lookup_in_tree,
    spawn_unix_desktop_child, step_error_message, synthesize_locator, terminate_unix_child,
    terminate_unix_detached_pid, verify_unix_reattach_identity, wait_state_name, write_png_private,
};
use super::{
    AttachArgs, ClickButton, DesktopLocator, DesktopPoint, DesktopRole, LaunchArgs, WaitState,
};

const ATSPI_REGISTRY_DEST: &str = "org.a11y.atspi.Registry";
const ATSPI_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const ATSPI_ACCESSIBLE_IFACE: &str = "org.a11y.atspi.Accessible";
const ATSPI_COMPONENT_IFACE: &str = "org.a11y.atspi.Component";
const ATSPI_ACTION_IFACE: &str = "org.a11y.atspi.Action";
const ATSPI_TEXT_IFACE: &str = "org.a11y.atspi.Text";
const ATSPI_EDITABLE_TEXT_IFACE: &str = "org.a11y.atspi.EditableText";
const ATSPI_VALUE_IFACE: &str = "org.a11y.atspi.Value";
const ATSPI_SELECTION_IFACE: &str = "org.a11y.atspi.Selection";

const ATSPI_COORD_SCREEN: u32 = 0;

const STATE_DEFUNCT: u32 = 6;
const STATE_EDITABLE: u32 = 7;
const STATE_ENABLED: u32 = 8;
const STATE_SENSITIVE: u32 = 24;
const STATE_SHOWING: u32 = 25;
const STATE_VISIBLE: u32 = 30;

const LINUX_DEFAULT_A11Y_ENV: [(&str, &str); 4] = [
    ("GTK_MODULES", "gail:atk-bridge"),
    ("QT_ACCESSIBILITY", "1"),
    ("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1"),
    ("GNOME_ACCESSIBILITY", "1"),
];

type AtspiRef = (String, OwnedObjectPath);

#[derive(Debug, Clone)]
struct LiveNodeRef {
    bus: String,
    path: String,
    interfaces: Vec<String>,
    child_index_in_parent: Option<i32>,
}

struct LiveLinuxTree {
    data: SharedTreeData,
    refs: Vec<LiveNodeRef>,
}

struct FoundLinuxWindow {
    pid: u32,
    hwnd: i64,
    win_bus: String,
    win_path: String,
    title: String,
    width: u32,
    height: u32,
}

/// Native Linux AT-SPI2 + X11/XTEST backend managing one target desktop window.
pub struct DesktopBackend {
    conn: Connection,
    child: Option<Child>,
    pid: u32,
    hwnd: i64,
    win_bus: String,
    win_path: String,
    owned_launch: bool,
    process_start_token: Option<String>,
    local_only_reason: Option<String>,
    title: String,
    refs: BTreeMap<String, StoredRef>,
    last_step_meta: Option<Value>,
}

impl DesktopBackend {
    /// Launches an executable with AT-SPI2 bridge variables enabled and waits for its window.
    pub fn launch(
        args: &LaunchArgs,
        base_dir: &Path,
        interactive: bool,
    ) -> Result<(Self, Value), StepError> {
        // Connect and activate the AT-SPI2 bus before spawning the child so
        // GTK/Qt `atk-bridge` sees a ready registry on cold `dbus-run-session`.
        let conn = connect_atspi_bus()?;
        let (command_path, resolved_cwd) = resolve_command_and_cwd(args, base_dir);
        let mut child = spawn_unix_desktop_child(
            &command_path,
            &args.args,
            resolved_cwd.as_deref(),
            &args.env,
            &LINUX_DEFAULT_A11Y_ENV,
            interactive,
            &args.command,
        )?;
        let spawned_pid = child.id();
        let start_token = linux_proc_start_token(spawned_pid);
        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

        loop {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(StepError::Environment(format!(
                    "desktop application `{}` (PID {spawned_pid}) exited with {status} before opening an AT-SPI2 window",
                    args.command
                )));
            }

            match find_atspi_window(
                &conn,
                Some(spawned_pid),
                None,
                args.window_title_contains.as_deref(),
            ) {
                Ok(Some(win)) => {
                    let actual_token = if win.pid == spawned_pid {
                        start_token
                    } else {
                        linux_proc_start_token(win.pid)
                    };
                    let result = json!({
                        "pid": win.pid,
                        "hwnd": win.hwnd,
                        "title": win.title,
                        "window": { "width": win.width, "height": win.height },
                        "viewport": { "width": win.width, "height": win.height },
                        "owned": true,
                    });
                    let backend = Self {
                        conn,
                        child: Some(child),
                        pid: win.pid,
                        hwnd: win.hwnd,
                        win_bus: win.win_bus,
                        win_path: win.win_path,
                        owned_launch: true,
                        process_start_token: actual_token,
                        local_only_reason: None,
                        title: win.title,
                        refs: BTreeMap::new(),
                        last_step_meta: None,
                    };
                    return Ok((backend, result));
                }
                Ok(None) => {}
                Err(err) => {
                    let _ = terminate_unix_child(&mut child, spawned_pid);
                    return Err(err);
                }
            }

            if Instant::now() >= deadline {
                let _ = terminate_unix_child(&mut child, spawned_pid);
                return Err(StepError::Timeout(format!(
                    "timed out after {}ms waiting for AT-SPI2 window of `{}` (PID {spawned_pid})",
                    args.timeout_ms, args.command
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Attaches to an already-running desktop window over AT-SPI2 without owning its lifetime.
    pub fn attach(args: &AttachArgs) -> Result<(Self, Value), StepError> {
        let conn = connect_atspi_bus()?;
        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

        loop {
            if let Some(win) = find_atspi_window(
                &conn,
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
                    conn,
                    child: None,
                    pid: win.pid,
                    hwnd: win.hwnd,
                    win_bus: win.win_bus,
                    win_path: win.win_path,
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
                    "timed out after {}ms waiting for AT-SPI2 window matching attach criteria",
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
            linux_proc_start_token,
        )?;

        let conn = connect_atspi_bus().map_err(|e| step_error_message(&e).to_string())?;
        let win = find_atspi_window(&conn, Some(parsed.pid), None, None)
            .map_err(|e| step_error_message(&e).to_string())?
            .ok_or_else(|| {
                format!(
                    "desktop window closed: no active AT-SPI2 window for PID {}",
                    parsed.pid
                )
            })?;

        Ok(Self {
            conn,
            child: None,
            pid: parsed.pid,
            hwnd: if win.hwnd != 0 { win.hwnd } else { parsed.hwnd },
            win_bus: win.win_bus,
            win_path: win.win_path,
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
                linux_proc_start_token,
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
                    Some(linux_proc_start_token(self.pid).ok_or_else(|| {
                        format!("failed to read /proc/{}/stat start token", self.pid)
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
                    linux_proc_start_token,
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
                            && try_atspi_semantic_click(&self.conn, &live, idx);

                        if !clicked_semantically {
                            let _lock = acquire_desktop_input_lock(timeout_ms)?;
                            let cx =
                                node.screen_rect.0 + (node.screen_rect.2 - node.screen_rect.0) / 2;
                            let cy =
                                node.screen_rect.1 + (node.screen_rect.3 - node.screen_rect.1) / 2;
                            x11_focus_and_click(self.pid, self.hwnd, cx, cy, button)?;
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

        let sx = live.data.win_left + point.x;
        let sy = live.data.win_top + point.y;
        let reverse_meta = if interactive {
            reverse_lookup_in_tree(&live.data, sx, sy)
        } else {
            None
        };

        x11_focus_and_click(self.pid, self.hwnd, sx, sy, button)?;

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
            let sx = live.data.win_left + point.x;
            let sy = live.data.win_top + point.y;
            let reverse_meta = if interactive {
                reverse_lookup_in_tree(&live.data, sx, sy)
            } else {
                None
            };

            x11_focus_and_click(self.pid, self.hwnd, sx, sy, ClickButton::Left)?;
            x11_type_string(self.pid, self.hwnd, text, false)?;

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

                        let live_ref = &live.refs[idx];
                        let typed_semantically = try_atspi_set_text(&self.conn, live_ref, text);

                        if !typed_semantically {
                            let _lock = acquire_desktop_input_lock(timeout_ms)?;
                            let _ = dbus_call::<_, bool>(
                                &self.conn,
                                &live_ref.bus,
                                &live_ref.path,
                                ATSPI_COMPONENT_IFACE,
                                "GrabFocus",
                                &(),
                            );
                            let cx =
                                node.screen_rect.0 + (node.screen_rect.2 - node.screen_rect.0) / 2;
                            let cy =
                                node.screen_rect.1 + (node.screen_rect.3 - node.screen_rect.1) / 2;
                            x11_focus_and_click(self.pid, self.hwnd, cx, cy, ClickButton::Left)?;
                            x11_type_string(self.pid, self.hwnd, text, true)?;
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
        let _ = self.ensure_window_alive()?;
        x11_send_key_chord(self.pid, self.hwnd, &mods, &main_key)?;
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
        let (png_bytes, width, height, masked_count) =
            capture_x11_window_png(self.pid, self.hwnd, &live.data, mask_inputs)?;
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
        let tree_res = self.ensure_and_collect_tree().map(|live| live.data);
        let png_res = match &tree_res {
            Ok(tree) => Some(
                capture_x11_window_png(self.pid, self.hwnd, tree, true)
                    .map(|(bytes, _, _, _)| bytes),
            ),
            Err(_) => None,
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

    fn ensure_window_alive(&mut self) -> Result<(String, String), StepError> {
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

        if is_atspi_window_alive(&self.conn, &self.win_bus, &self.win_path) {
            return Ok((self.win_bus.clone(), self.win_path.clone()));
        }

        if let Some(win) = find_atspi_window(&self.conn, Some(self.pid), None, None)? {
            self.win_bus = win.win_bus.clone();
            self.win_path = win.win_path.clone();
            self.title = win.title;
            if win.hwnd != 0 {
                self.hwnd = win.hwnd;
            }
            return Ok((win.win_bus, win.win_path));
        }

        Err(StepError::Environment(format!(
            "desktop window closed: no active AT-SPI2 window for PID {}",
            self.pid
        )))
    }

    fn ensure_and_collect_tree(&mut self) -> Result<LiveLinuxTree, StepError> {
        let (win_bus, win_path) = self.ensure_window_alive()?;
        let live = collect_atspi_tree(&self.conn, &win_bus, &win_path)?;
        self.title.clone_from(&live.data.title);
        Ok(live)
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

fn connect_atspi_bus() -> Result<Connection, StepError> {
    if let Ok(addr) = std::env::var("AT_SPI_BUS_ADDRESS") {
        let trimmed = addr.trim();
        if !trimmed.is_empty() {
            let builder = ConnectionBuilder::address(trimmed).map_err(|err| {
                StepError::Environment(format!("invalid `AT_SPI_BUS_ADDRESS` `{trimmed}`: {err}"))
            })?;
            return builder
                .method_timeout(Duration::from_secs(5))
                .build()
                .map_err(|err| {
                    StepError::Environment(format!(
                        "failed to connect to AT-SPI2 bus at `{trimmed}`: {err}"
                    ))
                });
        }
    }

    let session_builder = ConnectionBuilder::session().map_err(|err| {
        StepError::Environment(format!(
            "failed to open Linux session D-Bus for AT-SPI2 discovery: {err} — ensure `dbus-run-session` and `at-spi2-core` are running"
        ))
    })?;
    let session = session_builder
        .method_timeout(Duration::from_secs(5))
        .build()
        .map_err(|err| {
            StepError::Environment(format!(
                "failed to connect to Linux session D-Bus: {err} — ensure `dbus-run-session` and `at-spi2-core` are running"
            ))
        })?;

    let _ = dbus_set_bool_prop(
        &session,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Status",
        "IsEnabled",
        true,
    );
    let _ = dbus_set_bool_prop(
        &session,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Status",
        "ScreenReaderEnabled",
        true,
    );

    let a11y_addr: String = dbus_call(
        &session,
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus",
        "GetAddress",
        &(),
    )
    .map_err(|err| {
        StepError::Environment(format!(
            "failed to query `org.a11y.Bus.GetAddress`: {err} — install `at-spi2-core` and run inside an active D-Bus session"
        ))
    })?;

    let a11y_trimmed = a11y_addr.trim();
    if a11y_trimmed.is_empty() {
        return Err(StepError::Environment(
            "`org.a11y.Bus.GetAddress` returned an empty AT-SPI2 bus address".to_string(),
        ));
    }

    let a11y_builder = ConnectionBuilder::address(a11y_trimmed).map_err(|err| {
        StepError::Environment(format!(
            "invalid AT-SPI2 bus address `{a11y_trimmed}` returned by `org.a11y.Bus`: {err}"
        ))
    })?;
    a11y_builder
        .method_timeout(Duration::from_secs(5))
        .build()
        .map_err(|err| {
            StepError::Environment(format!(
                "failed to connect to AT-SPI2 bus `{a11y_trimmed}`: {err}"
            ))
        })
}

fn dbus_call<B, R>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<R>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    let reply = conn.call_method(Some(dest), path, Some(iface), method, body)?;
    reply.body().deserialize::<R>()
}

fn dbus_get_prop<T>(conn: &Connection, dest: &str, path: &str, iface: &str, prop: &str) -> Option<T>
where
    T: TryFrom<OwnedValue>,
{
    let val: OwnedValue = dbus_call(
        conn,
        dest,
        path,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(iface, prop),
    )
    .ok()?;
    T::try_from(val).ok()
}

fn dbus_set_bool_prop(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    prop: &str,
    val: bool,
) -> zbus::Result<()> {
    let variant = ZValue::from(val);
    let _reply = conn.call_method(
        Some(dest),
        path,
        Some("org.freedesktop.DBus.Properties"),
        "Set",
        &(iface, prop, variant),
    )?;
    Ok(())
}

fn get_children(conn: &Connection, bus: &str, path: &str) -> Vec<AtspiRef> {
    if let Ok(children) =
        dbus_call::<_, Vec<AtspiRef>>(conn, bus, path, ATSPI_ACCESSIBLE_IFACE, "GetChildren", &())
    {
        return children
            .into_iter()
            .filter(|(b, p)| !b.is_empty() && p.as_str() != "/org/a11y/atspi/null")
            .collect();
    }

    let count = dbus_get_prop::<i32>(conn, bus, path, ATSPI_ACCESSIBLE_IFACE, "ChildCount")
        .unwrap_or(0)
        .clamp(0, 512);
    let mut out = Vec::new();
    for idx in 0..count {
        if let Ok((b, p)) = dbus_call::<_, AtspiRef>(
            conn,
            bus,
            path,
            ATSPI_ACCESSIBLE_IFACE,
            "GetChildAtIndex",
            &(idx,),
        ) && !b.is_empty()
            && p.as_str() != "/org/a11y/atspi/null"
        {
            out.push((b, p));
        }
    }
    out
}

fn bus_owner_pid(conn: &Connection, bus_name: &str) -> Option<u32> {
    dbus_call::<_, u32>(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetConnectionUnixProcessID",
        &(bus_name,),
    )
    .ok()
    .filter(|&pid| pid > 0)
}

fn linux_proc_ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(')')?;
    let mut fields = after_comm.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse::<u32>().ok()
}

fn is_same_or_descendant_pid(candidate: u32, root_pid: u32) -> bool {
    let mut cur = candidate;
    for _ in 0..6 {
        if cur == root_pid {
            return true;
        }
        let Some(ppid) = linux_proc_ppid(cur) else {
            break;
        };
        if ppid <= 1 || ppid == cur {
            break;
        }
        cur = ppid;
    }
    false
}

fn linux_proc_start_token(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, after_comm) = stat.rsplit_once(')')?;
    // Field 3 (`state`) is index 0 after `(comm)`; field 22 (`starttime`) is index 19.
    after_comm
        .split_whitespace()
        .nth(19)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn pid_matches_executable(pid: u32, needle_lower: &str) -> bool {
    if let Ok(exe_link) = std::fs::read_link(format!("/proc/{pid}/exe"))
        && let Some(name) = exe_link.file_name().and_then(|s| s.to_str())
        && name.to_ascii_lowercase() == needle_lower
    {
        return true;
    }
    if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        && comm.trim().to_ascii_lowercase() == needle_lower
    {
        return true;
    }
    if let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        for part in cmdline.split(|&b| b == 0) {
            if let Ok(arg) = std::str::from_utf8(part)
                && let Some(base) = Path::new(arg).file_name().and_then(|s| s.to_str())
                && base.to_ascii_lowercase() == needle_lower
            {
                return true;
            }
        }
    }
    false
}

fn has_state(states: &[u32], bit: u32) -> bool {
    let idx = (bit / 32) as usize;
    let rem = bit % 32;
    states
        .get(idx)
        .is_some_and(|word| (word & (1u32 << rem)) != 0)
}

fn is_atspi_window_alive(conn: &Connection, bus: &str, path: &str) -> bool {
    let Ok(states) =
        dbus_call::<_, Vec<u32>>(conn, bus, path, ATSPI_ACCESSIBLE_IFACE, "GetState", &())
    else {
        return false;
    };
    if has_state(&states, STATE_DEFUNCT) {
        return false;
    }
    has_state(&states, STATE_SHOWING) || has_state(&states, STATE_VISIBLE)
}

fn find_atspi_window(
    conn: &Connection,
    pid_filter: Option<u32>,
    exe_filter: Option<&str>,
    title_contains: Option<&str>,
) -> Result<Option<FoundLinuxWindow>, StepError> {
    let apps = get_children(conn, ATSPI_REGISTRY_DEST, ATSPI_ROOT_PATH);
    let exe_lower = exe_filter.map(|s| {
        Path::new(s)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(s)
            .to_ascii_lowercase()
    });
    let title_lower = title_contains.map(str::to_ascii_lowercase);

    for (app_bus, app_path) in apps {
        let Some(app_pid) = bus_owner_pid(conn, &app_bus) else {
            continue;
        };
        if let Some(expected_pid) = pid_filter
            && !is_same_or_descendant_pid(app_pid, expected_pid)
        {
            continue;
        }
        if let Some(needle_exe) = &exe_lower
            && !pid_matches_executable(app_pid, needle_exe)
        {
            continue;
        }

        let windows = get_children(conn, &app_bus, app_path.as_str());
        for (win_bus, win_path) in windows {
            let win_path_str = win_path.as_str();
            let states = dbus_call::<_, Vec<u32>>(
                conn,
                &win_bus,
                win_path_str,
                ATSPI_ACCESSIBLE_IFACE,
                "GetState",
                &(),
            )
            .unwrap_or_default();
            if has_state(&states, STATE_DEFUNCT) {
                continue;
            }
            if !states.is_empty()
                && !has_state(&states, STATE_SHOWING)
                && !has_state(&states, STATE_VISIBLE)
            {
                continue;
            }

            let (_, _, ww, wh) = dbus_call::<_, (i32, i32, i32, i32)>(
                conn,
                &win_bus,
                win_path_str,
                ATSPI_COMPONENT_IFACE,
                "GetExtents",
                &(ATSPI_COORD_SCREEN,),
            )
            .unwrap_or((0, 0, 0, 0));
            if ww <= 0 || wh <= 0 {
                continue;
            }

            let title = dbus_get_prop::<String>(
                conn,
                &win_bus,
                win_path_str,
                ATSPI_ACCESSIBLE_IFACE,
                "Name",
            )
            .unwrap_or_default();
            if let Some(needle) = &title_lower
                && !title.to_ascii_lowercase().contains(needle)
            {
                continue;
            }

            let hwnd = find_x11_window_for_pid(app_pid, Some(&title)).unwrap_or(0) as i64;
            return Ok(Some(FoundLinuxWindow {
                pid: app_pid,
                hwnd,
                win_bus,
                win_path: win_path_str.to_string(),
                title,
                width: ww as u32,
                height: wh as u32,
            }));
        }
    }
    Ok(None)
}

fn map_atspi_role(role_name: &str) -> (DesktopRole, bool) {
    let lower = role_name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "password text" => (DesktopRole::Edit, true),
        "text" | "entry" | "paragraph" | "terminal" => (DesktopRole::Edit, false),
        "push button" | "button" => (DesktopRole::Button, false),
        "split pane" => (DesktopRole::Pane, false),
        "check box" => (DesktopRole::CheckBox, false),
        "radio button" | "toggle button" => (DesktopRole::RadioButton, false),
        "combo box" => (DesktopRole::ComboBox, false),
        "label" | "static" | "caption" | "heading" => (DesktopRole::Text, false),
        "frame" | "dialog" | "window" | "alert" | "file chooser" | "color chooser"
        | "font chooser" => (DesktopRole::Window, false),
        "panel" | "filler" | "drawing area" | "canvas" | "viewport" | "scroll pane"
        | "layered pane" | "root pane" | "glass pane" | "section" | "form" => {
            (DesktopRole::Pane, false)
        }
        "grouping" => (DesktopRole::Group, false),
        "list" | "description list" => (DesktopRole::List, false),
        "list item" | "description value" => (DesktopRole::ListItem, false),
        "menu" | "popup menu" => (DesktopRole::Menu, false),
        "menu bar" => (DesktopRole::MenuBar, false),
        "menu item" | "check menu item" | "radio menu item" | "tearoff menu item" => {
            (DesktopRole::MenuItem, false)
        }
        "progress bar" | "level bar" => (DesktopRole::ProgressBar, false),
        "scroll bar" => (DesktopRole::ScrollBar, false),
        "slider" => (DesktopRole::Slider, false),
        "spin button" => (DesktopRole::Spinner, false),
        "status bar" | "statusbar" => (DesktopRole::StatusBar, false),
        "page tab list" => (DesktopRole::Tab, false),
        "page tab" => (DesktopRole::TabItem, false),
        "tool bar" | "toolbar" => (DesktopRole::ToolBar, false),
        "tool tip" | "tooltip" => (DesktopRole::ToolTip, false),
        "tree" => (DesktopRole::Tree, false),
        "tree item" => (DesktopRole::TreeItem, false),
        "tree table" => (DesktopRole::DataGrid, false),
        "table" => (DesktopRole::Table, false),
        "table row" | "table cell" => (DesktopRole::DataItem, false),
        "column header" | "row header" | "table column header" | "table row header" => {
            (DesktopRole::HeaderItem, false)
        }
        "header" => (DesktopRole::Header, false),
        "separator" => (DesktopRole::Separator, false),
        "link" => (DesktopRole::Hyperlink, false),
        "image" | "icon" | "desktop icon" => (DesktopRole::Image, false),
        "calendar" | "date editor" => (DesktopRole::Calendar, false),
        "document frame"
        | "document web"
        | "document text"
        | "document spreadsheet"
        | "document presentation"
        | "document email" => (DesktopRole::Document, false),
        "title bar" => (DesktopRole::TitleBar, false),
        _ => (DesktopRole::Custom, false),
    }
}

fn collect_atspi_tree(
    conn: &Connection,
    win_bus: &str,
    win_path: &str,
) -> Result<LiveLinuxTree, StepError> {
    let (win_left, win_top, win_w_i32, win_h_i32) = dbus_call::<_, (i32, i32, i32, i32)>(
        conn,
        win_bus,
        win_path,
        ATSPI_COMPONENT_IFACE,
        "GetExtents",
        &(ATSPI_COORD_SCREEN,),
    )
    .map_err(|err| {
        StepError::Environment(format!(
            "desktop window closed: failed to query AT-SPI2 window extents: {err}"
        ))
    })?;
    if win_w_i32 <= 0 || win_h_i32 <= 0 {
        return Err(StepError::Environment(
            "desktop window closed: AT-SPI2 window has empty bounds".to_string(),
        ));
    }

    let win_width = win_w_i32 as u32;
    let win_height = win_h_i32 as u32;
    let win_title =
        dbus_get_prop::<String>(conn, win_bus, win_path, ATSPI_ACCESSIBLE_IFACE, "Name")
            .unwrap_or_default();

    let mut nodes = Vec::new();
    let mut live_refs = Vec::new();
    let mut truncated = false;
    let mut visited = HashSet::new();

    struct WalkCtx<'a> {
        conn: &'a Connection,
        win_left: i32,
        win_top: i32,
        nodes: &'a mut Vec<SharedNodeData>,
        live_refs: &'a mut Vec<LiveNodeRef>,
        truncated: &'a mut bool,
        visited: &'a mut HashSet<String>,
    }

    fn walk_node(
        ctx: &mut WalkCtx<'_>,
        bus: &str,
        path: &str,
        parent: Option<usize>,
        depth: usize,
        child_idx_in_parent: Option<i32>,
    ) {
        if ctx.nodes.len() >= MAX_COLLECTED_NODES {
            *ctx.truncated = true;
            return;
        }
        let runtime_key = format!("{bus}|{path}");
        if !ctx.visited.insert(runtime_key.clone()) {
            return;
        }

        let states =
            dbus_call::<_, Vec<u32>>(ctx.conn, bus, path, ATSPI_ACCESSIBLE_IFACE, "GetState", &())
                .unwrap_or_default();
        if has_state(&states, STATE_DEFUNCT) {
            return;
        }

        let role_name = dbus_call::<_, String>(
            ctx.conn,
            bus,
            path,
            ATSPI_ACCESSIBLE_IFACE,
            "GetRoleName",
            &(),
        )
        .unwrap_or_default();
        let (mut role, is_password) = map_atspi_role(&role_name);
        if depth == 0 {
            role = DesktopRole::Window;
        } else if role == DesktopRole::Text && has_state(&states, STATE_EDITABLE) {
            role = DesktopRole::Edit;
        }

        let interfaces = dbus_call::<_, Vec<String>>(
            ctx.conn,
            bus,
            path,
            ATSPI_ACCESSIBLE_IFACE,
            "GetInterfaces",
            &(),
        )
        .unwrap_or_default();

        let attrs = dbus_call::<_, HashMap<String, String>>(
            ctx.conn,
            bus,
            path,
            ATSPI_ACCESSIBLE_IFACE,
            "GetAttributes",
            &(),
        )
        .unwrap_or_default();

        let mut name = dbus_get_prop::<String>(ctx.conn, bus, path, ATSPI_ACCESSIBLE_IFACE, "Name")
            .unwrap_or_default();

        let automation_id =
            dbus_get_prop::<String>(ctx.conn, bus, path, ATSPI_ACCESSIBLE_IFACE, "AccessibleId")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    attrs
                        .get("id")
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                });

        let class_name = attrs
            .get("class")
            .cloned()
            .filter(|s| !s.is_empty())
            .unwrap_or(role_name);

        let (sx, sy, sw, sh) =
            if interfaces.iter().any(|i| i == ATSPI_COMPONENT_IFACE) || depth == 0 {
                dbus_call::<_, (i32, i32, i32, i32)>(
                    ctx.conn,
                    bus,
                    path,
                    ATSPI_COMPONENT_IFACE,
                    "GetExtents",
                    &(ATSPI_COORD_SCREEN,),
                )
                .unwrap_or((ctx.win_left, ctx.win_top, 0, 0))
            } else {
                (ctx.win_left, ctx.win_top, 0, 0)
            };

        let value = if is_password {
            None
        } else if interfaces.iter().any(|i| i == ATSPI_TEXT_IFACE) {
            read_atspi_text(ctx.conn, bus, path)
        } else if interfaces.iter().any(|i| i == ATSPI_VALUE_IFACE) {
            dbus_get_prop::<f64>(ctx.conn, bus, path, ATSPI_VALUE_IFACE, "CurrentValue")
                .map(|v| v.to_string())
        } else {
            None
        };

        if role == DesktopRole::Text
            && name.is_empty()
            && let Some(txt) = &value
            && !txt.is_empty()
        {
            name.clone_from(txt);
        }

        let enabled = if states.is_empty() {
            true
        } else {
            has_state(&states, STATE_ENABLED) && has_state(&states, STATE_SENSITIVE)
        };
        let showing = states.is_empty() || has_state(&states, STATE_SHOWING);
        let offscreen = !showing || sw <= 0 || sh <= 0;

        let idx = ctx.nodes.len();
        ctx.nodes.push(SharedNodeData {
            index: idx,
            parent,
            depth,
            subtree_end: idx + 1,
            runtime_key,
            role,
            name,
            automation_id,
            class_name,
            value,
            enabled,
            offscreen,
            is_password,
            screen_rect: (
                sx,
                sy,
                sx.saturating_add(sw.max(0)),
                sy.saturating_add(sh.max(0)),
            ),
            bounds: BoundsRect {
                x: sx.saturating_sub(ctx.win_left),
                y: sy.saturating_sub(ctx.win_top),
                width: sw.max(0),
                height: sh.max(0),
            },
        });
        ctx.live_refs.push(LiveNodeRef {
            bus: bus.to_string(),
            path: path.to_string(),
            interfaces,
            child_index_in_parent: child_idx_in_parent,
        });

        let children = get_children(ctx.conn, bus, path);
        for (c_pos, (c_bus, c_path)) in children.into_iter().enumerate() {
            if ctx.nodes.len() >= MAX_COLLECTED_NODES {
                *ctx.truncated = true;
                break;
            }
            walk_node(
                ctx,
                &c_bus,
                c_path.as_str(),
                Some(idx),
                depth + 1,
                i32::try_from(c_pos).ok(),
            );
        }
        ctx.nodes[idx].subtree_end = ctx.nodes.len();
    }

    let mut ctx = WalkCtx {
        conn,
        win_left,
        win_top,
        nodes: &mut nodes,
        live_refs: &mut live_refs,
        truncated: &mut truncated,
        visited: &mut visited,
    };
    walk_node(&mut ctx, win_bus, win_path, None, 0, None);

    if nodes.is_empty() {
        return Err(StepError::Environment(
            "desktop window closed: AT-SPI2 window disappeared during tree walk".to_string(),
        ));
    }

    Ok(LiveLinuxTree {
        data: SharedTreeData {
            win_left,
            win_top,
            win_width,
            win_height,
            title: win_title,
            nodes,
            truncated,
        },
        refs: live_refs,
    })
}

fn read_atspi_text(conn: &Connection, bus: &str, path: &str) -> Option<String> {
    if let Ok(txt) =
        dbus_call::<_, String>(conn, bus, path, ATSPI_TEXT_IFACE, "GetText", &(0i32, -1i32))
    {
        return Some(txt);
    }
    let count = dbus_get_prop::<i32>(conn, bus, path, ATSPI_TEXT_IFACE, "CharacterCount")?;
    if count <= 0 {
        return Some(String::new());
    }
    dbus_call::<_, String>(conn, bus, path, ATSPI_TEXT_IFACE, "GetText", &(0i32, count)).ok()
}

fn try_atspi_semantic_click(conn: &Connection, live: &LiveLinuxTree, idx: usize) -> bool {
    let node = &live.data.nodes[idx];
    let live_ref = &live.refs[idx];
    if matches!(
        node.role,
        DesktopRole::Edit | DesktopRole::Pane | DesktopRole::Window | DesktopRole::Document
    ) {
        return false;
    }

    if live_ref.interfaces.iter().any(|i| i == ATSPI_ACTION_IFACE) {
        let n_actions = dbus_get_prop::<i32>(
            conn,
            &live_ref.bus,
            &live_ref.path,
            ATSPI_ACTION_IFACE,
            "NActions",
        )
        .or_else(|| {
            dbus_call::<_, Vec<(String, String, String)>>(
                conn,
                &live_ref.bus,
                &live_ref.path,
                ATSPI_ACTION_IFACE,
                "GetActions",
                &(),
            )
            .ok()
            .and_then(|actions| i32::try_from(actions.len()).ok())
        })
        .unwrap_or(0);
        if n_actions > 0 {
            let count = n_actions.min(16);
            let mut chosen = 0i32;
            for action_idx in 0..count {
                if let Ok(name) = dbus_call::<_, String>(
                    conn,
                    &live_ref.bus,
                    &live_ref.path,
                    ATSPI_ACTION_IFACE,
                    "GetName",
                    &(action_idx,),
                ) {
                    let lower = name.trim().to_ascii_lowercase();
                    if matches!(
                        lower.as_str(),
                        "click" | "activate" | "press" | "toggle" | "jump" | "open"
                    ) {
                        chosen = action_idx;
                        break;
                    }
                }
            }
            if dbus_call::<_, bool>(
                conn,
                &live_ref.bus,
                &live_ref.path,
                ATSPI_ACTION_IFACE,
                "DoAction",
                &(chosen,),
            )
            .unwrap_or(false)
            {
                return true;
            }
        }
    }

    if matches!(
        node.role,
        DesktopRole::ListItem | DesktopRole::TabItem | DesktopRole::RadioButton
    ) && let Some(parent_idx) = node.parent
        && let Some(child_pos) = live_ref.child_index_in_parent
    {
        let parent_ref = &live.refs[parent_idx];
        if parent_ref
            .interfaces
            .iter()
            .any(|i| i == ATSPI_SELECTION_IFACE)
            && dbus_call::<_, bool>(
                conn,
                &parent_ref.bus,
                &parent_ref.path,
                ATSPI_SELECTION_IFACE,
                "SelectChild",
                &(child_pos,),
            )
            .unwrap_or(false)
        {
            return true;
        }
    }

    false
}

fn try_atspi_set_text(conn: &Connection, live_ref: &LiveNodeRef, text: &str) -> bool {
    if !live_ref
        .interfaces
        .iter()
        .any(|i| i == ATSPI_EDITABLE_TEXT_IFACE)
    {
        return false;
    }
    let _ = dbus_call::<_, bool>(
        conn,
        &live_ref.bus,
        &live_ref.path,
        ATSPI_COMPONENT_IFACE,
        "GrabFocus",
        &(),
    );
    dbus_call::<_, bool>(
        conn,
        &live_ref.bus,
        &live_ref.path,
        ATSPI_EDITABLE_TEXT_IFACE,
        "SetTextContents",
        &(text,),
    )
    .unwrap_or(false)
}

fn connect_x11() -> Result<(RustConnection, usize), StepError> {
    let display = std::env::var("DISPLAY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(dpy) = display else {
        return Err(StepError::Environment(
            "X11 display is unavailable (`DISPLAY` is unset; pure Wayland restricts global window capture and synthetic XTEST input) — run under X11, XWayland, or `xvfb-run -a` for screenshots, `desktop.press`, and `at:` coordinate actions"
                .to_string(),
        ));
    };

    x11rb::connect(Some(&dpy)).map_err(|err| {
        StepError::Environment(format!(
            "failed to connect to X11 display `{dpy}`: {err} — pure Wayland without XWayland restricts X11 window capture and XTEST input; use X11, XWayland, or `xvfb-run -a`"
        ))
    })
}

fn find_x11_window_for_pid(pid: u32, title_hint: Option<&str>) -> Option<X11Window> {
    let (conn, screen_num) = connect_x11().ok()?;
    let root = conn.setup().roots.get(screen_num)?.root;
    find_x11_window_on_conn(&conn, root, pid, title_hint)
}

fn find_x11_window_on_conn(
    conn: &RustConnection,
    root: X11Window,
    pid: u32,
    title_hint: Option<&str>,
) -> Option<X11Window> {
    let pid_atom = conn
        .intern_atom(false, b"_NET_WM_PID")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let net_wm_name_atom = conn
        .intern_atom(false, b"_NET_WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let utf8_atom = conn
        .intern_atom(false, b"UTF8_STRING")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let hint_lower = title_hint.map(str::to_ascii_lowercase);

    let mut stack = vec![root];
    let mut fallback_match = None;

    while let Some(win) = stack.pop() {
        if win != root {
            let viewable = conn
                .get_window_attributes(win)
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|a| a.map_state == MapState::VIEWABLE);
            if viewable {
                let win_pid = conn
                    .get_property(false, win, pid_atom, AtomEnum::CARDINAL, 0, 1)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32()?.next());
                if win_pid == Some(pid) {
                    if let Some(needle) = &hint_lower {
                        let name = read_x11_window_title(conn, win, net_wm_name_atom, utf8_atom);
                        if name.to_ascii_lowercase().contains(needle) {
                            return Some(win);
                        }
                        if fallback_match.is_none() {
                            fallback_match = Some(win);
                        }
                    } else {
                        return Some(win);
                    }
                }
            }
        }
        if let Ok(cookie) = conn.query_tree(win)
            && let Ok(reply) = cookie.reply()
        {
            for child in reply.children {
                stack.push(child);
            }
        }
    }
    fallback_match
}

fn read_x11_window_title(
    conn: &RustConnection,
    win: X11Window,
    net_wm_name_atom: u32,
    utf8_atom: u32,
) -> String {
    if let Ok(cookie) = conn.get_property(false, win, net_wm_name_atom, utf8_atom, 0, 256)
        && let Ok(reply) = cookie.reply()
        && !reply.value.is_empty()
    {
        return String::from_utf8_lossy(&reply.value).into_owned();
    }
    if let Ok(cookie) = conn.get_property(false, win, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 256)
        && let Ok(reply) = cookie.reply()
        && !reply.value.is_empty()
    {
        return String::from_utf8_lossy(&reply.value).into_owned();
    }
    String::new()
}

fn raise_and_focus_x11(conn: &RustConnection, root: X11Window, pid: u32, hwnd: i64) {
    let target = if hwnd > 0 {
        u32::try_from(hwnd).ok()
    } else {
        None
    }
    .or_else(|| find_x11_window_on_conn(conn, root, pid, None));

    if let Some(win) = target {
        let _ = conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        let _ = conn.set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME);
        let _ = conn.sync();
    }
}

fn capture_x11_window_png(
    pid: u32,
    hwnd: i64,
    tree: &SharedTreeData,
    mask_inputs: bool,
) -> Result<(Vec<u8>, u32, u32, usize), StepError> {
    let (conn, screen_num) = connect_x11()?;
    let screen = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| StepError::Environment("X11 connection has no root screen".to_string()))?
        .clone();

    raise_and_focus_x11(&conn, screen.root, pid, hwnd);

    let width = tree.win_width.max(1);
    let height = tree.win_height.max(1);
    let root_w = i32::from(screen.width_in_pixels);
    let root_h = i32::from(screen.height_in_pixels);

    let vis_x0 = tree.win_left.clamp(0, root_w);
    let vis_y0 = tree.win_top.clamp(0, root_h);
    let vis_x1 = tree.win_left.saturating_add(width as i32).clamp(0, root_w);
    let vis_y1 = tree.win_top.saturating_add(height as i32).clamp(0, root_h);

    if vis_x1 <= vis_x0 || vis_y1 <= vis_y0 {
        return Err(StepError::Environment(format!(
            "target X11 window at ({}, {}) {}x{} lies completely outside root screen {}x{}",
            tree.win_left, tree.win_top, width, height, root_w, root_h
        )));
    }

    let crop_w = (vis_x1 - vis_x0) as u16;
    let crop_h = (vis_y1 - vis_y0) as u16;
    let (img, visual_id) = X11Image::get(
        &conn,
        screen.root,
        vis_x0 as i16,
        vis_y0 as i16,
        crop_w,
        crop_h,
    )
    .map_err(|err| StepError::Environment(format!("X11 GetImage capture failed: {err}")))?;

    let visual_type = screen
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == visual_id)
        .copied()
        .ok_or_else(|| {
            StepError::Environment(format!(
                "X11 screen visual `{visual_id}` not found in allowed_depths"
            ))
        })?;
    let layout = PixelLayout::from_visual_type(visual_type).map_err(|err| {
        StepError::Environment(format!("unsupported X11 TrueColor visual layout: {err}"))
    })?;

    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];
    for px in rgba.as_chunks_mut::<4>().0 {
        px[3] = 255;
    }

    let dst_off_x = (vis_x0 - tree.win_left) as u32;
    let dst_off_y = (vis_y0 - tree.win_top) as u32;
    for cy in 0..crop_h {
        for cx in 0..crop_w {
            let pixel = img.get_pixel(cx, cy);
            let (r16, g16, b16) = layout.decode(pixel);
            let dx = dst_off_x + u32::from(cx);
            let dy = dst_off_y + u32::from(cy);
            if dx < width && dy < height {
                let offset = ((dy * width + dx) * 4) as usize;
                rgba[offset] = (r16 >> 8) as u8;
                rgba[offset + 1] = (g16 >> 8) as u8;
                rgba[offset + 2] = (b16 >> 8) as u8;
                rgba[offset + 3] = 255;
            }
        }
    }

    let masked_count = if mask_inputs {
        mask_tree_inputs(&mut rgba, width, height, tree)
    } else {
        0
    };

    let png_bytes = encode_rgba_png(&rgba, width, height)?;
    Ok((png_bytes, width, height, masked_count))
}

fn x11_focus_and_click(
    pid: u32,
    hwnd: i64,
    sx: i32,
    sy: i32,
    button: ClickButton,
) -> Result<(), StepError> {
    let (conn, screen_num) = connect_x11()?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| StepError::Environment("X11 connection has no root screen".to_string()))?
        .root;

    raise_and_focus_x11(&conn, root, pid, hwnd);

    let rx = i16::try_from(sx).map_err(|_| {
        StepError::Execution(format!("X11 click X coordinate `{sx}` overflows i16"))
    })?;
    let ry = i16::try_from(sy).map_err(|_| {
        StepError::Execution(format!("X11 click Y coordinate `{sy}` overflows i16"))
    })?;

    let _ = conn
        .xtest_fake_input(
            xproto::MOTION_NOTIFY_EVENT,
            0,
            x11rb::CURRENT_TIME,
            root,
            rx,
            ry,
            0,
        )
        .map_err(|err| StepError::Execution(format!("XTEST pointer move failed: {err}")))?;

    let (btn_detail, clicks) = match button {
        ClickButton::Left => (1u8, 1usize),
        ClickButton::Right => (3u8, 1usize),
        ClickButton::Double => (1u8, 2usize),
    };

    for idx in 0..clicks {
        if idx > 0 {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = conn
            .xtest_fake_input(
                xproto::BUTTON_PRESS_EVENT,
                btn_detail,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .map_err(|err| StepError::Execution(format!("XTEST button press failed: {err}")))?;
        let _ = conn
            .xtest_fake_input(
                xproto::BUTTON_RELEASE_EVENT,
                btn_detail,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .map_err(|err| StepError::Execution(format!("XTEST button release failed: {err}")))?;
        conn.sync()
            .map_err(|err| StepError::Execution(format!("X11 sync after click failed: {err}")))?;
    }
    Ok(())
}

fn modifier_keysym(modifier: KeyModifier) -> u32 {
    match modifier {
        KeyModifier::Ctrl => 0xffe3,
        KeyModifier::RCtrl => 0xffe4,
        KeyModifier::Alt => 0xffe9,
        KeyModifier::RAlt => 0xffea,
        KeyModifier::Shift => 0xffe1,
        KeyModifier::RShift => 0xffe2,
        KeyModifier::Meta => 0xffeb,
        KeyModifier::RMeta => 0xffec,
    }
}

fn main_key_keysym(key: &MainKey) -> u32 {
    match key {
        MainKey::Enter => 0xff0d,
        MainKey::Tab => 0xff09,
        MainKey::Escape => 0xff1b,
        MainKey::Backspace => 0xff08,
        MainKey::Delete => 0xffff,
        MainKey::Insert => 0xff63,
        MainKey::Space => 0x0020,
        MainKey::Home => 0xff50,
        MainKey::Left => 0xff51,
        MainKey::Up => 0xff52,
        MainKey::Right => 0xff53,
        MainKey::Down => 0xff54,
        MainKey::PageUp => 0xff55,
        MainKey::PageDown => 0xff56,
        MainKey::End => 0xff57,
        MainKey::PrintScreen => 0xff61,
        MainKey::CapsLock => 0xffe5,
        MainKey::Pause => 0xff13,
        MainKey::F(n) => 0xffbe + u32::from(n.saturating_sub(1)),
        MainKey::Char(ch) => char_to_x11_keysym(*ch),
    }
}

fn char_to_x11_keysym(ch: char) -> u32 {
    match ch {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        c if ('\u{0020}'..='\u{007e}').contains(&c) || ('\u{00a0}'..='\u{00ff}').contains(&c) => {
            c as u32
        }
        c => 0x0100_0000 | (c as u32),
    }
}

fn resolve_x11_keycode(
    conn: &RustConnection,
    keysym: u32,
    scratch_keycode: &mut Option<u8>,
) -> Result<(u8, bool), StepError> {
    let setup = conn.setup();
    let min_kc = setup.min_keycode;
    let max_kc = setup.max_keycode;
    let count = max_kc.saturating_sub(min_kc).saturating_add(1);
    let reply = conn
        .get_keyboard_mapping(min_kc, count)
        .map_err(|e| StepError::Execution(format!("X11 GetKeyboardMapping failed: {e}")))?
        .reply()
        .map_err(|e| StepError::Execution(format!("X11 GetKeyboardMapping reply failed: {e}")))?;

    let per = usize::from(reply.keysyms_per_keycode);
    if per > 0 {
        let mut empty_kc = None;
        for idx in 0..usize::from(count) {
            let kc = min_kc.saturating_add(idx as u8);
            let slice_start = idx * per;
            let slice_end = (slice_start + per).min(reply.keysyms.len());
            let syms = &reply.keysyms[slice_start..slice_end];
            if syms.first().copied() == Some(keysym) {
                return Ok((kc, false));
            }
            if syms.get(1).copied() == Some(keysym) {
                return Ok((kc, true));
            }
            if empty_kc.is_none() && syms.iter().all(|&s| s == 0) {
                empty_kc = Some(kc);
            }
        }

        let target_kc = scratch_keycode.or(empty_kc).ok_or_else(|| {
            StepError::Execution(format!(
                "no unused X11 keycode available to map keysym `0x{keysym:x}`"
            ))
        })?;
        *scratch_keycode = Some(target_kc);
        let _ = conn
            .change_keyboard_mapping(1, target_kc, 2, &[keysym, keysym])
            .map_err(|e| StepError::Execution(format!("X11 ChangeKeyboardMapping failed: {e}")))?;
        conn.sync()
            .map_err(|e| StepError::Execution(format!("X11 sync after keymap bind failed: {e}")))?;
        return Ok((target_kc, false));
    }

    Err(StepError::Execution(
        "X11 server reported zero keysyms_per_keycode".to_string(),
    ))
}

fn x11_send_key_chord(
    pid: u32,
    hwnd: i64,
    mods: &[KeyModifier],
    main_key: &MainKey,
) -> Result<(), StepError> {
    let (conn, screen_num) = connect_x11()?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| StepError::Environment("X11 connection has no root screen".to_string()))?
        .root;

    raise_and_focus_x11(&conn, root, pid, hwnd);

    let mut scratch_kc = None;
    let mut mod_keycodes = Vec::with_capacity(mods.len() + 1);
    for &m in mods {
        let (kc, _) = resolve_x11_keycode(&conn, modifier_keysym(m), &mut scratch_kc)?;
        mod_keycodes.push(kc);
    }

    let (main_kc, needs_shift) =
        resolve_x11_keycode(&conn, main_key_keysym(main_key), &mut scratch_kc)?;
    if needs_shift && !mods.contains(&KeyModifier::Shift) && !mods.contains(&KeyModifier::RShift) {
        let (shift_kc, _) =
            resolve_x11_keycode(&conn, modifier_keysym(KeyModifier::Shift), &mut scratch_kc)?;
        mod_keycodes.push(shift_kc);
    }

    for &kc in &mod_keycodes {
        let _ = conn
            .xtest_fake_input(
                xproto::KEY_PRESS_EVENT,
                kc,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .map_err(|e| StepError::Execution(format!("XTEST modifier press failed: {e}")))?;
    }
    let _ = conn
        .xtest_fake_input(
            xproto::KEY_PRESS_EVENT,
            main_kc,
            x11rb::CURRENT_TIME,
            root,
            0,
            0,
            0,
        )
        .map_err(|e| StepError::Execution(format!("XTEST key press failed: {e}")))?;
    let _ = conn
        .xtest_fake_input(
            xproto::KEY_RELEASE_EVENT,
            main_kc,
            x11rb::CURRENT_TIME,
            root,
            0,
            0,
            0,
        )
        .map_err(|e| StepError::Execution(format!("XTEST key release failed: {e}")))?;
    for &kc in mod_keycodes.iter().rev() {
        let _ = conn
            .xtest_fake_input(
                xproto::KEY_RELEASE_EVENT,
                kc,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .map_err(|e| StepError::Execution(format!("XTEST modifier release failed: {e}")))?;
    }
    conn.sync()
        .map_err(|e| StepError::Execution(format!("X11 sync after key chord failed: {e}")))
}

fn x11_type_string(pid: u32, hwnd: i64, text: &str, clear_existing: bool) -> Result<(), StepError> {
    let (conn, screen_num) = connect_x11()?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or_else(|| StepError::Environment("X11 connection has no root screen".to_string()))?
        .root;

    raise_and_focus_x11(&conn, root, pid, hwnd);

    let mut scratch_kc = None;
    let (shift_kc, _) =
        resolve_x11_keycode(&conn, modifier_keysym(KeyModifier::Shift), &mut scratch_kc)?;

    if clear_existing {
        let (ctrl_kc, _) =
            resolve_x11_keycode(&conn, modifier_keysym(KeyModifier::Ctrl), &mut scratch_kc)?;
        let (a_kc, _) = resolve_x11_keycode(&conn, u32::from(b'a'), &mut scratch_kc)?;
        let (bs_kc, _) = resolve_x11_keycode(&conn, 0xff08, &mut scratch_kc)?;
        for (ev, kc) in [
            (xproto::KEY_PRESS_EVENT, ctrl_kc),
            (xproto::KEY_PRESS_EVENT, a_kc),
            (xproto::KEY_RELEASE_EVENT, a_kc),
            (xproto::KEY_RELEASE_EVENT, ctrl_kc),
            (xproto::KEY_PRESS_EVENT, bs_kc),
            (xproto::KEY_RELEASE_EVENT, bs_kc),
        ] {
            let _ = conn
                .xtest_fake_input(ev, kc, x11rb::CURRENT_TIME, root, 0, 0, 0)
                .map_err(|e| StepError::Execution(format!("XTEST clear field failed: {e}")))?;
        }
        conn.sync()
            .map_err(|e| StepError::Execution(format!("X11 sync after clear failed: {e}")))?;
    }

    for ch in text.chars() {
        let keysym = char_to_x11_keysym(ch);
        let (kc, needs_shift) = resolve_x11_keycode(&conn, keysym, &mut scratch_kc)?;
        if needs_shift {
            let _ = conn
                .xtest_fake_input(
                    xproto::KEY_PRESS_EVENT,
                    shift_kc,
                    x11rb::CURRENT_TIME,
                    root,
                    0,
                    0,
                    0,
                )
                .map_err(|e| StepError::Execution(format!("XTEST shift press failed: {e}")))?;
        }
        let _ = conn
            .xtest_fake_input(
                xproto::KEY_PRESS_EVENT,
                kc,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .map_err(|e| StepError::Execution(format!("XTEST char press failed: {e}")))?;
        let _ = conn
            .xtest_fake_input(
                xproto::KEY_RELEASE_EVENT,
                kc,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .map_err(|e| StepError::Execution(format!("XTEST char release failed: {e}")))?;
        if needs_shift {
            let _ = conn
                .xtest_fake_input(
                    xproto::KEY_RELEASE_EVENT,
                    shift_kc,
                    x11rb::CURRENT_TIME,
                    root,
                    0,
                    0,
                    0,
                )
                .map_err(|e| StepError::Execution(format!("XTEST shift release failed: {e}")))?;
        }
        conn.sync()
            .map_err(|e| StepError::Execution(format!("X11 sync after char failed: {e}")))?;
    }
    Ok(())
}
