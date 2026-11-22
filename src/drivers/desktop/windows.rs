//! Windows UI Automation backend (`#[cfg(windows)]`) for `desktop.*`.
//!
//! Runs all COM/UIA calls inside a dedicated `COINIT_MULTITHREADED` worker
//! thread (`std::thread::spawn` + `mpsc::sync_channel`) so [`DesktopBackend`]
//! stays safely `Send` with zero `unsafe` in this crate and no `UIElement`
//! crosses thread boundaries.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::BufWriter;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, Once};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uiautomation::UIAutomation;
use uiautomation::core::{UICacheRequest, UIElement};
use uiautomation::inputs::{Keyboard, Mouse};
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UILegacyIAccessiblePattern, UIPatternType,
    UISelectionItemPattern, UITogglePattern, UIValuePattern, UIWindowPattern,
};
use uiautomation::types::{
    ControlType, ElementMode, Handle, Point, Rect, TreeScope, UIProperty, WindowVisualState,
};
use uiautomation::variants::Value as UiaValue;

use crate::core::paths;
use crate::core::registry::StepError;
use crate::core::session::Evidence;

use super::{
    AttachArgs, ClickButton, DesktopLocator, DesktopPoint, DesktopRole, LaunchArgs, LocatorFilter,
    WaitState,
};

const E_ACCESSDENIED_HRESULT: i32 = 0x8007_0005_u32 as i32;
const UIA_E_ELEMENTNOTAVAILABLE_HRESULT: i32 = 0x8004_0201_u32 as i32;
const ERROR_INVALID_WINDOW_HANDLE_HRESULT: i32 = 0x8007_0578_u32 as i32;
const E_INVALIDARG_HRESULT: i32 = 0x8007_0057_u32 as i32;

const MAX_COLLECTED_NODES: usize = 4_000;
const DEFAULT_SNAPSHOT_MAX_DEPTH: usize = 8;
const DEFAULT_SNAPSHOT_MAX_NODES: usize = 250;
const POLL_INTERVAL: Duration = Duration::from_millis(25);

static DPI_INIT: Once = Once::new();
static DESKTOP_INPUT_LOCK: Mutex<()> = Mutex::new(());

type WorkerJob = Box<dyn FnOnce(&WorkerCtx) + Send>;

struct WorkerCtx {
    automation: UIAutomation,
}

struct MtaWorker {
    tx: Option<mpsc::SyncSender<WorkerJob>>,
    join: Option<JoinHandle<()>>,
}

impl MtaWorker {
    fn new() -> Result<Self, String> {
        // Align UIA, GDI capture, and SendInput coordinate spaces to physical
        // per-monitor pixels once before creating UIAutomation COM objects.
        DPI_INIT.call_once(|| {
            let _ = win_screenshot::capture::capture_window(0);
        });

        let (tx, rx) = mpsc::sync_channel::<WorkerJob>(16);
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

        let join = thread::Builder::new()
            .name("da-desktop-uia-mta".to_string())
            .spawn(move || {
                let automation = match UIAutomation::new() {
                    Ok(auto) => {
                        let _ = init_tx.send(Ok(()));
                        auto
                    }
                    Err(err) => {
                        let _ = init_tx.send(Err(format!(
                            "failed to initialize Windows UI Automation MTA worker: {err}"
                        )));
                        return;
                    }
                };
                let ctx = WorkerCtx { automation };
                while let Ok(job) = rx.recv() {
                    job(&ctx);
                }
            })
            .map_err(|err| format!("failed to spawn desktop UIA worker thread: {err}"))?;

        match init_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: Some(tx),
                join: Some(join),
            }),
            Ok(Err(msg)) => {
                let _ = join.join();
                Err(msg)
            }
            Err(err) => {
                let _ = join.join();
                Err(format!("desktop UIA worker exited before ready: {err}"))
            }
        }
    }

    fn exec<R, F>(&self, f: F) -> Result<R, StepError>
    where
        R: Send + 'static,
        F: FnOnce(&WorkerCtx) -> R + Send + 'static,
    {
        let tx = self.tx.as_ref().ok_or_else(|| {
            StepError::Environment("desktop UIA worker is already closed".to_string())
        })?;
        let (res_tx, res_rx) = mpsc::sync_channel::<R>(1);
        tx.send(Box::new(move |ctx| {
            let out = f(ctx);
            let _ = res_tx.send(out);
        }))
        .map_err(|_| {
            StepError::Environment("desktop UIA worker channel closed unexpectedly".to_string())
        })?;
        res_rx.recv().map_err(|_| {
            StepError::Environment("desktop UIA worker dropped task without response".to_string())
        })
    }
}

impl Drop for MtaWorker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct DesktopInputGuard {
    _thread_guard: MutexGuard<'static, ()>,
    _file_guard: File,
}

fn acquire_desktop_input_lock(timeout_ms: u64) -> Result<DesktopInputGuard, StepError> {
    let thread_guard = DESKTOP_INPUT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock_path = std::env::temp_dir().join("do-anything-desktop-input.lock");
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&lock_path)
        {
            Ok(file_guard) => {
                return Ok(DesktopInputGuard {
                    _thread_guard: thread_guard,
                    _file_guard: file_guard,
                });
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(StepError::Timeout(format!(
                        "timed out after {timeout_ms}ms acquiring exclusive desktop input lock: {err}"
                    )));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredRef {
    runtime_id: Vec<i32>,
    #[serde(default)]
    locator: Option<Value>,
    #[serde(default)]
    fragile: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedHandle {
    pid: u32,
    hwnd: isize,
    owned_launch: bool,
    #[serde(default)]
    local_only_reason: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    refs: BTreeMap<String, StoredRef>,
}

/// Pure `Send` metadata describing one cached node in a desktop window tree.
#[derive(Debug, Clone)]
struct NodeData {
    index: usize,
    parent: Option<usize>,
    depth: usize,
    subtree_end: usize,
    runtime_id: Vec<i32>,
    role: DesktopRole,
    name: String,
    automation_id: Option<String>,
    class_name: String,
    value: Option<String>,
    enabled: bool,
    offscreen: bool,
    is_password: bool,
    screen_rect: (i32, i32, i32, i32),
    bounds: BoundsRect,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct BoundsRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone)]
struct TreeData {
    win_left: i32,
    win_top: i32,
    win_width: u32,
    win_height: u32,
    title: String,
    nodes: Vec<NodeData>,
    truncated: bool,
}

struct LiveTree {
    data: TreeData,
    elements: Vec<UIElement>,
}

/// Native Windows UI Automation backend managing one target desktop window.
pub struct DesktopBackend {
    worker: MtaWorker,
    child: Option<Child>,
    pid: u32,
    hwnd: isize,
    owned_launch: bool,
    local_only_reason: Option<String>,
    title: String,
    refs: BTreeMap<String, StoredRef>,
    last_step_meta: Option<Value>,
}

impl DesktopBackend {
    /// Launches an executable and waits for its main visible top-level window.
    pub fn launch(
        args: &LaunchArgs,
        base_dir: &Path,
        interactive: bool,
    ) -> Result<(Self, Value), StepError> {
        let worker = MtaWorker::new().map_err(StepError::Environment)?;

        let command_path = resolve_command(&args.command, base_dir);
        let resolved_cwd = args.cwd.as_deref().map(|cwd| {
            let cwd_path = Path::new(cwd);
            if cwd_path.is_absolute() {
                cwd_path.to_path_buf()
            } else {
                base_dir.join(cwd_path)
            }
        });

        let (mut child, pid) = if interactive {
            let pid = spawn_detached_via_powershell(
                &command_path,
                &args.args,
                resolved_cwd.as_deref(),
                &args.env,
            )
            .map_err(|err| {
                StepError::Environment(format!(
                    "failed to launch detached desktop command `{}`: {err}",
                    args.command
                ))
            })?;
            (None, pid)
        } else {
            let spawned = spawn_direct(
                &command_path,
                &args.args,
                resolved_cwd.as_deref(),
                &args.env,
                &args.command,
            )?;
            let pid = spawned.id();
            (Some(spawned), pid)
        };

        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
        let title_filter = args.window_title_contains.clone();

        loop {
            if let Some(c) = &mut child {
                if let Ok(Some(status)) = c.try_wait() {
                    return Err(StepError::Environment(format!(
                        "desktop application `{}` (PID {pid}) exited with {status} before opening a window",
                        args.command
                    )));
                }
            } else if !is_pid_running(pid) {
                return Err(StepError::Environment(format!(
                    "desktop application `{}` (PID {pid}) exited before opening a window",
                    args.command
                )));
            }

            let filter_clone = title_filter.clone();
            let found = worker.exec(move |ctx| {
                find_top_level_window(&ctx.automation, Some(pid), None, filter_clone.as_deref())
            })?;

            match found {
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
                        worker,
                        child,
                        pid: win.pid,
                        hwnd: win.hwnd,
                        owned_launch: true,
                        local_only_reason: None,
                        title: win.title,
                        refs: BTreeMap::new(),
                        last_step_meta: None,
                    };
                    return Ok((backend, result));
                }
                Ok(None) => {}
                Err(err) => {
                    terminate_pid_or_child(child.as_mut(), pid);
                    return Err(err);
                }
            }

            if Instant::now() >= deadline {
                terminate_pid_or_child(child.as_mut(), pid);
                return Err(StepError::Timeout(format!(
                    "timed out after {}ms waiting for main window of `{}` (PID {pid})",
                    args.timeout_ms, args.command
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Attaches to an already-running top-level window without owning its lifetime.
    pub fn attach(args: &AttachArgs) -> Result<(Self, Value), StepError> {
        let worker = MtaWorker::new().map_err(StepError::Environment)?;
        let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);

        loop {
            let pid_filter = args.pid;
            let exe_filter = args.executable.clone();
            let title_filter = args.title_contains.clone();

            let found = worker.exec(move |ctx| {
                find_top_level_window(
                    &ctx.automation,
                    pid_filter,
                    exe_filter.as_deref(),
                    title_filter.as_deref(),
                )
            })?;

            if let Some(win) = found? {
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
                    worker,
                    child: None,
                    pid: win.pid,
                    hwnd: win.hwnd,
                    owned_launch: false,
                    local_only_reason: Some(reason),
                    title: win.title,
                    refs: BTreeMap::new(),
                    last_step_meta: None,
                };
                return Ok((backend, result));
            }

            if Instant::now() >= deadline {
                return Err(StepError::Timeout(format!(
                    "timed out after {}ms waiting for desktop window matching attach criteria",
                    args.timeout_ms
                )));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Reconstructs a `DesktopBackend` from persisted interactive session handle state.
    pub fn reattach(handle: &Value) -> Result<Self, String> {
        let parsed: PersistedHandle = serde_json::from_value(handle.clone())
            .map_err(|err| format!("invalid persisted desktop session handle: {err}"))?;
        let worker = MtaWorker::new()?;

        let pid = parsed.pid;
        let hwnd = parsed.hwnd;
        let alive = worker
            .exec(move |ctx| ensure_window_alive(&ctx.automation, hwnd, pid, None).map(|_| ()))
            .map_err(|err| step_error_message(&err).to_string())?;
        if let Err(err) = alive {
            return Err(step_error_message(&err).to_string());
        }

        Ok(Self {
            worker,
            child: None,
            pid,
            hwnd,
            owned_launch: parsed.owned_launch,
            local_only_reason: parsed.local_only_reason,
            title: parsed.title,
            refs: parsed.refs,
            last_step_meta: None,
        })
    }

    /// Disposes a persisted desktop handle, terminating only owned launched processes.
    pub fn dispose_handle(handle: &Value) -> Result<(), String> {
        let parsed: PersistedHandle = serde_json::from_value(handle.clone())
            .map_err(|err| format!("invalid persisted desktop session handle: {err}"))?;
        if parsed.owned_launch {
            kill_pid_tree(parsed.pid)?;
        }
        Ok(())
    }

    pub fn local_only_reason(&self) -> Option<String> {
        self.local_only_reason.clone()
    }

    pub fn detach(&mut self) -> Result<Option<Value>, String> {
        let handle = PersistedHandle {
            pid: self.pid,
            hwnd: self.hwnd,
            owned_launch: self.owned_launch,
            local_only_reason: self.local_only_reason.clone(),
            title: self.title.clone(),
            refs: self.refs.clone(),
        };
        // Detach relinquishes ownership of `self.child` so process Drop will not
        // kill a persisted `da session` process between CLI commands.
        self.child.take();
        serde_json::to_value(handle)
            .map(Some)
            .map_err(|err| format!("failed to serialize desktop session handle: {err}"))
    }

    pub fn close(&mut self) -> Result<(), String> {
        if self.owned_launch {
            if let Some(child) = &mut self.child {
                terminate_child(child, self.pid)?;
                self.child = None;
            } else {
                kill_pid_tree(self.pid)?;
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

        let target_clone = target.cloned();
        let ref_clone = ref_id.map(str::to_string);
        let refs_clone = self.refs.clone();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let hwnd = self.hwnd;
            let pid = self.pid;
            let exit_status = self.try_child_status();
            let t = target_clone.clone();
            let r = ref_clone.clone();
            let refs = refs_clone.clone();

            let outcome = self.worker.exec(move |ctx| {
                let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
                let live = collect_live_tree(&ctx.automation, &win_el, pid, hwnd)?;
                let Some(idx) = resolve_node_in_tree(&live.data, t.as_ref(), r.as_deref(), &refs)?
                else {
                    return Ok(None);
                };
                let node = live.data.nodes[idx].clone();
                if !node.enabled {
                    return Ok(Some((false, live.data, node, false)));
                }
                let clicked_via_pattern = button == ClickButton::Left
                    && try_pattern_click(&live.elements[idx], node.role).unwrap_or(false);
                Ok(Some((true, live.data, node, clicked_via_pattern)))
            })??;

            match outcome {
                Some((true, tree_data, node, clicked_via_pattern)) => {
                    let (locator, fragile) = synthesize_locator(node.index, &tree_data.nodes);
                    if interactive {
                        self.last_step_meta = Some(json!({
                            "ref": ref_id,
                            "synthesized_locator": locator,
                            "fragile": fragile,
                        }));
                    }

                    if !clicked_via_pattern {
                        let _lock = acquire_desktop_input_lock(timeout_ms)?;
                        let cx = node.screen_rect.0 + (node.screen_rect.2 - node.screen_rect.0) / 2;
                        let cy = node.screen_rect.1 + (node.screen_rect.3 - node.screen_rect.1) / 2;
                        let exit_status2 = self.try_child_status();
                        self.worker.exec(move |ctx| {
                            let win_el =
                                ensure_window_alive(&ctx.automation, hwnd, pid, exit_status2)?;
                            focus_window(&win_el).map_err(|err| {
                                classify_uia_error(&ctx.automation, hwnd, pid, err)
                            })?;
                            send_mouse_click(Point::new(cx, cy), button).map_err(|msg| {
                                StepError::Execution(format!("desktop mouse click failed: {msg}"))
                            })
                        })??;
                    }

                    return Ok(json!({
                        "clicked": true,
                        "button": click_button_name(button),
                        "role": node.role.as_str(),
                        "name": node.name,
                        "automation_id": node.automation_id,
                        "window": { "width": tree_data.win_width, "height": tree_data.win_height },
                    }));
                }
                Some((false, _, node, _)) => {
                    if Instant::now() >= deadline {
                        return Err(StepError::Execution(format!(
                            "desktop element (`{}`) is disabled",
                            node.role.as_str()
                        )));
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
        let hwnd = self.hwnd;
        let pid = self.pid;
        let exit_status = self.try_child_status();
        let point_clone = point.clone();

        let _lock = acquire_desktop_input_lock(timeout_ms)?;
        let (win_width, win_height, reverse_meta) = self.worker.exec(move |ctx| {
            let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
            focus_window(&win_el)
                .map_err(|err| classify_uia_error(&ctx.automation, hwnd, pid, err))?;
            let live = collect_live_tree(&ctx.automation, &win_el, pid, hwnd)?;
            check_window_point_bounds(&live.data, &point_clone)?;
            let sx = live.data.win_left + point_clone.x;
            let sy = live.data.win_top + point_clone.y;
            let meta = if interactive {
                reverse_lookup_at(&ctx.automation, &live.data, pid, Point::new(sx, sy))
            } else {
                None
            };
            send_mouse_click(Point::new(sx, sy), button).map_err(|msg| {
                StepError::Execution(format!("desktop mouse click failed: {msg}"))
            })?;
            Ok::<(u32, u32, Option<(Value, bool)>), StepError>((
                live.data.win_width,
                live.data.win_height,
                meta,
            ))
        })??;

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
            "window": { "width": win_width, "height": win_height },
            "viewport": { "width": win_width, "height": win_height },
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
            let hwnd = self.hwnd;
            let pid = self.pid;
            let exit_status = self.try_child_status();
            let point_clone = point.clone();
            let owned_text = text.to_string();

            let _lock = acquire_desktop_input_lock(timeout_ms)?;
            let (win_width, win_height, reverse_meta) = self.worker.exec(move |ctx| {
                let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
                focus_window(&win_el)
                    .map_err(|err| classify_uia_error(&ctx.automation, hwnd, pid, err))?;
                let live = collect_live_tree(&ctx.automation, &win_el, pid, hwnd)?;
                check_window_point_bounds(&live.data, &point_clone)?;
                let sx = live.data.win_left + point_clone.x;
                let sy = live.data.win_top + point_clone.y;
                let meta = if interactive {
                    reverse_lookup_at(&ctx.automation, &live.data, pid, Point::new(sx, sy))
                } else {
                    None
                };
                send_mouse_click(Point::new(sx, sy), ClickButton::Left).map_err(|msg| {
                    StepError::Execution(format!("desktop focus click failed: {msg}"))
                })?;
                Keyboard::new()
                    .interval(5)
                    .send_text(&owned_text)
                    .map_err(|err| {
                        StepError::Execution(format!("desktop keyboard type failed: {err}"))
                    })?;
                Ok::<(u32, u32, Option<(Value, bool)>), StepError>((
                    live.data.win_width,
                    live.data.win_height,
                    meta,
                ))
            })??;

            if let Some((locator, fragile)) = reverse_meta {
                self.last_step_meta = Some(json!({
                    "synthesized_locator": locator,
                    "fragile": fragile,
                }));
            }

            return Ok(json!({
                "typed_chars": text.chars().count(),
                "window": { "width": win_width, "height": win_height },
                "viewport": { "width": win_width, "height": win_height },
            }));
        }

        let target_clone = target.cloned();
        let ref_clone = ref_id.map(str::to_string);
        let refs_clone = self.refs.clone();
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let hwnd = self.hwnd;
            let pid = self.pid;
            let exit_status = self.try_child_status();
            let t = target_clone.clone();
            let r = ref_clone.clone();
            let refs = refs_clone.clone();
            let owned_text = text.to_string();

            let outcome = self.worker.exec(move |ctx| {
                let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
                let live = collect_live_tree(&ctx.automation, &win_el, pid, hwnd)?;
                let Some(idx) = resolve_node_in_tree(&live.data, t.as_ref(), r.as_deref(), &refs)?
                else {
                    return Ok(None);
                };
                let node = live.data.nodes[idx].clone();
                if !node.enabled {
                    return Ok(Some((false, live.data, node, false)));
                }
                let el = &live.elements[idx];
                // Prefer ValuePattern when non-readonly so typing succeeds off-screen
                // without moving the cursor or stealing foreground focus.
                let typed_via_pattern = if let Ok(vp) = el.get_pattern::<UIValuePattern>()
                    && !vp.is_readonly().unwrap_or(false)
                {
                    vp.set_value(&owned_text).is_ok()
                } else {
                    false
                };
                Ok(Some((true, live.data, node, typed_via_pattern)))
            })??;

            match outcome {
                Some((true, tree_data, node, typed_via_pattern)) => {
                    let (locator, fragile) = synthesize_locator(node.index, &tree_data.nodes);
                    if interactive {
                        self.last_step_meta = Some(json!({
                            "ref": ref_id,
                            "synthesized_locator": locator,
                            "fragile": fragile,
                        }));
                    }

                    if !typed_via_pattern {
                        let _lock = acquire_desktop_input_lock(timeout_ms)?;
                        let t2 = target_clone.clone();
                        let r2 = ref_clone.clone();
                        let refs2 = refs_clone.clone();
                        let owned_text2 = text.to_string();
                        let exit_status2 = self.try_child_status();
                        self.worker.exec(move |ctx| {
                            let win_el =
                                ensure_window_alive(&ctx.automation, hwnd, pid, exit_status2)?;
                            focus_window(&win_el).map_err(|err| {
                                classify_uia_error(&ctx.automation, hwnd, pid, err)
                            })?;
                            let live = collect_live_tree(&ctx.automation, &win_el, pid, hwnd)?;
                            let idx = resolve_node_in_tree(
                                &live.data,
                                t2.as_ref(),
                                r2.as_deref(),
                                &refs2,
                            )?
                            .ok_or_else(|| {
                                StepError::Execution(
                                    "desktop target control disappeared before keyboard fallback"
                                        .to_string(),
                                )
                            })?;
                            let _ = live.elements[idx].set_focus();
                            Keyboard::new()
                                .interval(5)
                                .send_keys("{ctrl}(a){BACKSPACE}")
                                .map_err(|err| {
                                    StepError::Execution(format!("desktop clear failed: {err}"))
                                })?;
                            Keyboard::new()
                                .interval(5)
                                .send_text(&owned_text2)
                                .map_err(|err| {
                                    StepError::Execution(format!(
                                        "desktop keyboard type failed: {err}"
                                    ))
                                })
                        })??;
                    }

                    return Ok(json!({
                        "typed_chars": text.chars().count(),
                        "role": node.role.as_str(),
                        "window": { "width": tree_data.win_width, "height": tree_data.win_height },
                    }));
                }
                Some((false, _, node, _)) => {
                    if Instant::now() >= deadline {
                        return Err(StepError::Execution(format!(
                            "desktop element (`{}`) is disabled",
                            node.role.as_str()
                        )));
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
        let _lock = acquire_desktop_input_lock(timeout_ms)?;
        let hwnd = self.hwnd;
        let pid = self.pid;
        let exit_status = self.try_child_status();
        let expr = uia_key_expr.to_string();

        self.worker.exec(move |ctx| {
            let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
            focus_window(&win_el)
                .map_err(|err| classify_uia_error(&ctx.automation, hwnd, pid, err))?;
            Keyboard::new()
                .interval(5)
                .send_keys(&expr)
                .map_err(|err| StepError::Execution(format!("desktop key press failed: {err}")))
        })??;

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
            let hwnd = self.hwnd;
            let pid = self.pid;
            let exit_status = self.try_child_status();

            let check = self.worker.exec(move |ctx| {
                match ensure_window_alive(&ctx.automation, hwnd, pid, exit_status) {
                    Ok(win_el) => collect_live_tree(&ctx.automation, &win_el, pid, hwnd)
                        .map(|live| (false, Some(live.data))),
                    Err(err) => {
                        if state == WaitState::Gone && step_error_message(&err).contains("closed") {
                            Ok((true, None))
                        } else {
                            Err(err)
                        }
                    }
                }
            })??;

            let (window_closed, maybe_tree) = check;
            if window_closed && state == WaitState::Gone {
                return Ok(json!({
                    "matched": true,
                    "state": "gone",
                    "window_closed": true,
                    "elapsed_ms": start.elapsed().as_millis() as u64,
                }));
            }

            if let Some(tree) = maybe_tree {
                let matched_idx = resolve_node_in_tree(&tree, target, ref_id, &self.refs)?;
                let satisfied = match (state, matched_idx) {
                    (WaitState::Gone, None) => true,
                    (WaitState::Gone, Some(_)) => false,
                    (WaitState::Present, Some(_)) => true,
                    (WaitState::Enabled, Some(idx)) => tree.nodes[idx].enabled,
                    (WaitState::Visible, Some(idx)) => !tree.nodes[idx].offscreen,
                    (_, None) => false,
                };

                if satisfied {
                    if let Some(idx) = matched_idx
                        && interactive
                    {
                        let (locator, fragile) = synthesize_locator(idx, &tree.nodes);
                        self.last_step_meta = Some(json!({
                            "ref": ref_id,
                            "synthesized_locator": locator,
                            "fragile": fragile,
                        }));
                    }
                    let element = matched_idx.map(|idx| node_summary(&tree.nodes[idx]));
                    return Ok(json!({
                        "matched": true,
                        "state": wait_state_name(state),
                        "elapsed_ms": start.elapsed().as_millis() as u64,
                        "element": element,
                    }));
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
        let (tree, node_idx) = self.poll_tree_node(target, ref_id, timeout_ms)?;
        let node = &tree.nodes[node_idx];

        if interactive {
            let (locator, fragile) = synthesize_locator(node_idx, &tree.nodes);
            self.last_step_meta = Some(json!({
                "ref": ref_id,
                "synthesized_locator": locator,
                "fragile": fragile,
            }));
        }

        Ok(node_summary(node))
    }

    pub fn screenshot(
        &mut self,
        path: &Path,
        mask_inputs: bool,
        _timeout_ms: u64,
    ) -> Result<Value, StepError> {
        self.last_step_meta = None;
        let hwnd = self.hwnd;
        let pid = self.pid;
        let exit_status = self.try_child_status();

        let (png_bytes, width, height, masked_count) = self.worker.exec(move |ctx| {
            let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
            let live = collect_live_tree(&ctx.automation, &win_el, pid, hwnd)?;
            capture_window_png(&live.data, hwnd, mask_inputs)
        })??;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                StepError::Execution(format!(
                    "failed to create screenshot directory `{}`: {err}",
                    paths::display(parent)
                ))
            })?;
        }
        std::fs::write(path, &png_bytes).map_err(|err| {
            StepError::Execution(format!(
                "failed to write desktop screenshot `{}`: {err}",
                paths::display(path)
            ))
        })?;

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
        let hwnd = self.hwnd;
        let pid = self.pid;
        let exit_status = self.try_child_status();

        let tree = self
            .worker
            .exec(move |ctx| {
                let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
                collect_live_tree(&ctx.automation, &win_el, pid, hwnd).map(|live| live.data)
            })
            .map_err(|e| step_error_message(&e).to_string())?
            .map_err(|e| step_error_message(&e).to_string())?;

        self.title = tree.title.clone();
        Ok(Some(self.build_snapshot_payload(
            &tree,
            scope_ref,
            max_depth.unwrap_or(DEFAULT_SNAPSHOT_MAX_DEPTH),
            max_nodes.unwrap_or(DEFAULT_SNAPSHOT_MAX_NODES),
            true,
        )?))
    }

    pub fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        let hwnd = self.hwnd;
        let pid = self.pid;
        let exit_status = self.try_child_status();
        let Ok(Ok(tree)) = self.worker.exec(move |ctx| {
            let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
            collect_live_tree(&ctx.automation, &win_el, pid, hwnd).map(|live| live.data)
        }) else {
            return Ok(None);
        };
        self.title = tree.title.clone();
        Ok(Some(self.build_snapshot_payload(
            &tree,
            None,
            DEFAULT_SNAPSHOT_MAX_DEPTH,
            DEFAULT_SNAPSHOT_MAX_NODES,
            true,
        )?))
    }

    pub fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        let hwnd = self.hwnd;
        let pid = self.pid;
        let exit_status = self.try_child_status();

        let captured = self
            .worker
            .exec(move |ctx| {
                let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
                let live = collect_live_tree(&ctx.automation, &win_el, pid, hwnd)?;
                let (png_bytes, _, _, _) = capture_window_png(&live.data, hwnd, true)?;
                Ok::<(TreeData, Vec<u8>), StepError>((live.data, png_bytes))
            })
            .map_err(|e| step_error_message(&e).to_string())?;

        let mut items = Vec::new();
        match captured {
            Ok((tree, png_bytes)) => {
                items.push(Evidence::bytes("screenshot.png", png_bytes)?);
                let payload = self.build_snapshot_payload(
                    &tree,
                    None,
                    DEFAULT_SNAPSHOT_MAX_DEPTH,
                    MAX_COLLECTED_NODES,
                    false,
                )?;
                let tree_json = serde_json::to_string_pretty(&payload)
                    .map_err(|e| format!("failed to serialize desktop UIA tree: {e}"))?;
                items.push(Evidence::text("uia-tree.json", tree_json)?);
            }
            Err(err) => {
                let msg = step_error_message(&err);
                let payload = json!({
                    "kind": "desktop",
                    "pid": self.pid,
                    "hwnd": self.hwnd,
                    "title": self.title,
                    "window_closed": msg.contains("closed"),
                    "capture_error": msg,
                });
                let tree_json = serde_json::to_string_pretty(&payload)
                    .map_err(|e| format!("failed to serialize fallback desktop UIA tree: {e}"))?;
                items.push(Evidence::text("uia-tree.json", tree_json)?);
            }
        }
        Ok(items)
    }

    fn build_snapshot_payload(
        &mut self,
        tree: &TreeData,
        scope_ref: Option<&str>,
        max_depth: usize,
        max_nodes: usize,
        update_refs: bool,
    ) -> Result<Value, String> {
        let (range_start, range_end, base_depth) = if let Some(ref_key) = scope_ref {
            let idx = resolve_node_in_tree(tree, None, Some(ref_key), &self.refs)
                .map_err(|e| step_error_message(&e).to_string())?
                .ok_or_else(|| format!("snapshot scope ref `{ref_key}` was not found"))?;
            let node = &tree.nodes[idx];
            (idx, node.subtree_end, node.depth)
        } else {
            (0, tree.nodes.len(), 0)
        };

        let mut new_refs: BTreeMap<String, StoredRef> = BTreeMap::new();
        let mut elements: Vec<Value> = Vec::new();
        let mut next_id = 1usize;
        let mut truncated = tree.truncated;
        let mut ref_by_node_idx: HashMap<usize, String> = HashMap::new();

        for idx in range_start..range_end {
            let node = &tree.nodes[idx];
            let rel_depth = node.depth.saturating_sub(base_depth);
            if rel_depth > max_depth {
                truncated = true;
                continue;
            }
            if elements.len() >= max_nodes {
                truncated = true;
                break;
            }

            let is_leaf = node.subtree_end == idx + 1;
            let include_as_ref = idx > 0
                && (node.role.is_interactive()
                    || (node.role.is_readable_leaf_role() && is_leaf)
                    || (node.role == DesktopRole::Text && !node.name.is_empty()));

            if !include_as_ref {
                continue;
            }

            let ref_name = format!("e{next_id}");
            next_id += 1;

            let (locator, fragile) = synthesize_locator(idx, &tree.nodes);
            if !node.runtime_id.is_empty() {
                new_refs.insert(
                    ref_name.clone(),
                    StoredRef {
                        runtime_id: node.runtime_id.clone(),
                        locator: Some(locator),
                        fragile,
                    },
                );
            }
            ref_by_node_idx.insert(idx, ref_name.clone());

            let mut item = node_summary(node);
            if let Some(obj) = item.as_object_mut() {
                obj.insert("ref".to_string(), json!(ref_name));
            }
            elements.push(item);
        }

        if update_refs {
            self.refs = new_refs;
        }

        let tree_json =
            build_nested_tree_json(tree, range_start, base_depth, max_depth, &ref_by_node_idx);

        Ok(json!({
            "kind": "desktop",
            "pid": self.pid,
            "hwnd": self.hwnd,
            "title": tree.title,
            "window": { "width": tree.win_width, "height": tree.win_height },
            "node_count": elements.len(),
            "truncated": truncated,
            "elements": elements,
            "tree": tree_json,
        }))
    }

    fn poll_tree_node(
        &mut self,
        target: Option<&DesktopLocator>,
        ref_id: Option<&str>,
        timeout_ms: u64,
    ) -> Result<(TreeData, usize), StepError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        loop {
            let hwnd = self.hwnd;
            let pid = self.pid;
            let exit_status = self.try_child_status();
            let tree = self.worker.exec(move |ctx| {
                let win_el = ensure_window_alive(&ctx.automation, hwnd, pid, exit_status)?;
                collect_live_tree(&ctx.automation, &win_el, pid, hwnd).map(|live| live.data)
            })??;

            if let Some(idx) = resolve_node_in_tree(&tree, target, ref_id, &self.refs)? {
                return Ok((tree, idx));
            }
            if Instant::now() >= deadline {
                return Err(missing_target_timeout(ref_id, timeout_ms));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn try_child_status(&mut self) -> Option<String> {
        self.child
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten())
            .map(|status| status.to_string())
    }
}

impl Drop for DesktopBackend {
    fn drop(&mut self) {
        if self.owned_launch
            && let Some(child) = &mut self.child
        {
            let _ = terminate_child(child, self.pid);
        }
    }
}

fn step_error_message(err: &StepError) -> &str {
    match err {
        StepError::InvalidArguments(msg)
        | StepError::Environment(msg)
        | StepError::Timeout(msg)
        | StepError::Execution(msg) => msg,
    }
}

fn missing_target_timeout(ref_id: Option<&str>, timeout_ms: u64) -> StepError {
    if let Some(r) = ref_id {
        StepError::Timeout(format!(
            "timed out after {timeout_ms}ms resolving session ref `{r}`"
        ))
    } else {
        StepError::Timeout(format!(
            "timed out after {timeout_ms}ms finding desktop control"
        ))
    }
}

struct FoundWindow {
    pid: u32,
    hwnd: isize,
    title: String,
    width: u32,
    height: u32,
}

fn build_window_summary_cache(automation: &UIAutomation) -> Result<UICacheRequest, StepError> {
    let cache = automation
        .create_cache_request()
        .map_err(|e| StepError::Environment(format!("UIA CreateCacheRequest failed: {e}")))?;
    for prop in [
        UIProperty::ProcessId,
        UIProperty::Name,
        UIProperty::NativeWindowHandle,
        UIProperty::BoundingRectangle,
        UIProperty::IsOffscreen,
    ] {
        cache
            .add_property(prop)
            .map_err(|e| StepError::Environment(format!("UIA AddProperty failed: {e}")))?;
    }
    Ok(cache)
}

fn find_top_level_window(
    automation: &UIAutomation,
    pid_filter: Option<u32>,
    exe_filter: Option<&str>,
    title_contains: Option<&str>,
) -> Result<Option<FoundWindow>, StepError> {
    let cache = build_window_summary_cache(automation)?;
    let exe_map = exe_filter.map(|_| pid_executable_map());
    let exe_lower = exe_filter.map(|s| s.to_ascii_lowercase());
    let title_lower = title_contains.map(|s| s.to_ascii_lowercase());

    // Use lightweight Win32 EnumWindows (via win_screenshot) instead of
    // querying `Desktop` root children in UIA, which can stall indefinitely on
    // unresponsive third-party windows or invisible COM proxies.
    let windows = win_screenshot::utils::window_list().map_err(|err| {
        StepError::Environment(format!("failed to enumerate visible windows: {err:?}"))
    })?;

    let mut candidates: Vec<UIElement> = Vec::new();
    for w in windows {
        if let Some(needle) = &title_lower
            && !w.window_name.to_ascii_lowercase().contains(needle)
        {
            continue;
        }
        if let Ok(el) = automation.element_from_handle_build_cache(Handle::from(w.hwnd), &cache) {
            candidates.push(el);
        }
    }

    for win in candidates {
        let pid = win.get_cached_process_id().unwrap_or(0) as u32;
        if pid == 0 {
            continue;
        }
        if let Some(expected_pid) = pid_filter
            && pid != expected_pid
        {
            continue;
        }
        let hwnd: isize = win
            .get_cached_native_window_handle()
            .map(isize::from)
            .unwrap_or(0);
        if hwnd == 0 {
            continue;
        }
        let rect = win.get_cached_bounding_rectangle().unwrap_or_default();
        let (w, h) = rect_dimensions(&rect);
        if w == 0 || h == 0 {
            continue;
        }
        let title = win.get_cached_name().unwrap_or_default();
        if let Some(needle) = &title_lower
            && !title.to_ascii_lowercase().contains(needle)
        {
            continue;
        }
        if let (Some(needle_exe), Some(map)) = (&exe_lower, &exe_map) {
            let matches_exe = map
                .get(&pid)
                .is_some_and(|img| img.to_ascii_lowercase() == *needle_exe);
            if !matches_exe {
                continue;
            }
        }
        return Ok(Some(FoundWindow {
            pid,
            hwnd,
            title,
            width: w,
            height: h,
        }));
    }
    Ok(None)
}

fn ensure_window_alive(
    automation: &UIAutomation,
    hwnd: isize,
    pid: u32,
    exit_status: Option<String>,
) -> Result<UIElement, StepError> {
    if let Some(status) = exit_status {
        return Err(StepError::Environment(format!(
            "desktop window closed: target process (PID {pid}) exited with {status}"
        )));
    }

    match automation.element_from_handle(Handle::from(hwnd)) {
        Ok(win_el) => {
            if let Ok(actual_pid) = win_el.get_process_id()
                && actual_pid != pid
            {
                return Err(StepError::Environment(format!(
                    "desktop window closed: HWND {hwnd} now belongs to PID {actual_pid} instead of {pid}"
                )));
            }
            Ok(win_el)
        }
        Err(err) => Err(classify_uia_error(automation, hwnd, pid, err)),
    }
}

fn classify_uia_error(
    automation: &UIAutomation,
    hwnd: isize,
    pid: u32,
    err: uiautomation::Error,
) -> StepError {
    let code = err.code();
    if matches!(
        code,
        UIA_E_ELEMENTNOTAVAILABLE_HRESULT
            | ERROR_INVALID_WINDOW_HANDLE_HRESULT
            | E_INVALIDARG_HRESULT
    ) {
        return StepError::Environment(format!(
            "desktop window closed: HWND {hwnd} (PID {pid}) is no longer active ({err})"
        ));
    }

    if code == E_ACCESSDENIED_HRESULT {
        // Distinguish stale HWND / exited process from real UIPI elevation mismatch
        // BEFORE reporting access denied (`dec_D4BF13B9660E0339DFA7300BCD`).
        let handle_valid = automation.element_from_handle(Handle::from(hwnd)).is_ok();
        let pid_alive = is_pid_running(pid);
        if !handle_valid || !pid_alive {
            return StepError::Environment(format!(
                "desktop window closed: target HWND {hwnd} (PID {pid}) is no longer active ({err})"
            ));
        }
        return StepError::Environment(format!(
            "access denied (UIPI / elevation mismatch) communicating with desktop window HWND {hwnd} (PID {pid}): {err}"
        ));
    }

    StepError::Environment(format!(
        "desktop UIA communication error with HWND {hwnd} (PID {pid}): {err}"
    ))
}

fn collect_live_tree(
    automation: &UIAutomation,
    win_el: &UIElement,
    pid: u32,
    hwnd: isize,
) -> Result<LiveTree, StepError> {
    let cache = build_tree_cache_request(automation)
        .map_err(|e| StepError::Environment(format!("UIA cache setup failed: {e}")))?;
    let cached_root = win_el
        .build_updated_cache(&cache)
        .map_err(|err| classify_uia_error(automation, hwnd, pid, err))?;

    let win_rect = cached_root
        .get_cached_bounding_rectangle()
        .unwrap_or_default();
    let (win_width, win_height) = rect_dimensions(&win_rect);
    let win_left = win_rect.get_left();
    let win_top = win_rect.get_top();
    let title = cached_root.get_cached_name().unwrap_or_default();

    let mut nodes: Vec<NodeData> = Vec::new();
    let mut elements: Vec<UIElement> = Vec::new();
    let mut truncated = false;
    flatten_cached_element(
        &cached_root,
        None,
        0,
        win_left,
        win_top,
        &mut nodes,
        &mut elements,
        &mut truncated,
    );

    Ok(LiveTree {
        data: TreeData {
            win_left,
            win_top,
            win_width,
            win_height,
            title,
            nodes,
            truncated,
        },
        elements,
    })
}

fn build_tree_cache_request(automation: &UIAutomation) -> Result<UICacheRequest, String> {
    let cache = automation
        .create_cache_request()
        .map_err(|e| e.to_string())?;
    for prop in [
        UIProperty::RuntimeId,
        UIProperty::BoundingRectangle,
        UIProperty::ProcessId,
        UIProperty::ControlType,
        UIProperty::Name,
        UIProperty::IsEnabled,
        UIProperty::AutomationId,
        UIProperty::ClassName,
        UIProperty::IsPassword,
        UIProperty::NativeWindowHandle,
        UIProperty::IsOffscreen,
        UIProperty::ValueValue,
        UIProperty::ValueIsReadOnly,
    ] {
        cache.add_property(prop).map_err(|e| e.to_string())?;
    }
    cache
        .add_pattern(UIPatternType::Value)
        .map_err(|e| e.to_string())?;
    cache
        .set_element_mode(ElementMode::Full)
        .map_err(|e| e.to_string())?;
    cache
        .set_tree_scope(TreeScope::Subtree)
        .map_err(|e| e.to_string())?;
    Ok(cache)
}

// Recursive tree flattener passes window origin and output buffers explicitly to avoid per-node context cloning.
#[allow(clippy::too_many_arguments)]
fn flatten_cached_element(
    el: &UIElement,
    parent: Option<usize>,
    depth: usize,
    win_left: i32,
    win_top: i32,
    nodes: &mut Vec<NodeData>,
    elements: &mut Vec<UIElement>,
    truncated: &mut bool,
) {
    if nodes.len() >= MAX_COLLECTED_NODES {
        *truncated = true;
        return;
    }

    let role = el
        .get_cached_control_type()
        .ok()
        .and_then(control_type_to_role)
        .unwrap_or(DesktopRole::Custom);
    let native_hwnd = el
        .get_cached_native_window_handle()
        .map(isize::from)
        .unwrap_or(0);
    let raw_aid = el.get_cached_automation_id().unwrap_or_default();
    let automation_id = normalize_automation_id(&raw_aid, native_hwnd);
    let name = el.get_cached_name().unwrap_or_default();
    let class_name = el.get_cached_classname().unwrap_or_default();
    let is_password = el.is_cached_password().unwrap_or(false);
    let enabled = el.is_cached_enabled().unwrap_or(true);
    let offscreen = el.is_cached_offscreen().unwrap_or(false);
    let rect = el.get_cached_bounding_rectangle().unwrap_or_default();
    let (w, h) = rect_dimensions(&rect);
    let bounds = BoundsRect {
        x: rect.get_left() - win_left,
        y: rect.get_top() - win_top,
        width: w as i32,
        height: h as i32,
    };

    let value = if !is_password
        && matches!(
            role,
            DesktopRole::Edit
                | DesktopRole::ComboBox
                | DesktopRole::Document
                | DesktopRole::Spinner
        ) {
        el.get_cached_pattern::<UIValuePattern>()
            .ok()
            .and_then(|vp| vp.get_cached_value().ok())
    } else {
        None
    };

    let runtime_id = cached_runtime_id(el);
    let idx = nodes.len();
    nodes.push(NodeData {
        index: idx,
        parent,
        depth,
        subtree_end: idx + 1,
        runtime_id,
        role,
        name,
        automation_id,
        class_name,
        value,
        enabled,
        offscreen,
        is_password,
        screen_rect: (
            rect.get_left(),
            rect.get_top(),
            rect.get_right(),
            rect.get_bottom(),
        ),
        bounds,
    });
    elements.push(el.clone());

    if let Ok(children) = el.get_cached_children() {
        for child in children {
            if nodes.len() >= MAX_COLLECTED_NODES {
                *truncated = true;
                break;
            }
            flatten_cached_element(
                &child,
                Some(idx),
                depth + 1,
                win_left,
                win_top,
                nodes,
                elements,
                truncated,
            );
        }
    }
    nodes[idx].subtree_end = nodes.len();
}

fn cached_runtime_id(el: &UIElement) -> Vec<i32> {
    if let Ok(variant) = el.get_cached_property_value(UIProperty::RuntimeId)
        && let Ok(UiaValue::ArrayI4(ids)) = variant.get_value()
        && !ids.is_empty()
    {
        return ids;
    }
    el.get_runtime_id().unwrap_or_default()
}

fn normalize_automation_id(raw: &str, native_hwnd: isize) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "0" {
        return None;
    }
    if native_hwnd != 0 {
        let hwnd_i32 = native_hwnd as i32;
        let hwnd_u32 = native_hwnd as u32;
        if trimmed == native_hwnd.to_string()
            || trimmed == hwnd_i32.to_string()
            || trimmed == hwnd_u32.to_string()
        {
            return None;
        }
    }
    Some(trimmed.to_string())
}

fn rect_dimensions(rect: &Rect) -> (u32, u32) {
    let w = (rect.get_right() - rect.get_left()).max(0) as u32;
    let h = (rect.get_bottom() - rect.get_top()).max(0) as u32;
    (w, h)
}

fn matches_filter(node: &NodeData, filter: &LocatorFilter) -> bool {
    if let Some(expected_aid) = &filter.automation_id
        && node.automation_id.as_deref() != Some(expected_aid.as_str())
    {
        return false;
    }
    if let Some(expected_role) = filter.role
        && node.role != expected_role
    {
        return false;
    }
    if let Some(expected_name) = &filter.name
        && node.name != *expected_name
    {
        return false;
    }
    if let Some(expected_class) = &filter.class_name
        && node.class_name != *expected_class
    {
        return false;
    }
    if let Some(needle) = &filter.text {
        let lower = needle.to_ascii_lowercase();
        let name_hit = node.name.to_ascii_lowercase().contains(&lower);
        let value_hit = node
            .value
            .as_deref()
            .is_some_and(|v| v.to_ascii_lowercase().contains(&lower));
        if !name_hit && !value_hit {
            return false;
        }
    }
    true
}

fn resolve_node_in_tree(
    tree: &TreeData,
    target: Option<&DesktopLocator>,
    ref_id: Option<&str>,
    refs: &BTreeMap<String, StoredRef>,
) -> Result<Option<usize>, StepError> {
    if let Some(key) = ref_id {
        let stored = refs.get(key).ok_or_else(|| {
            StepError::InvalidArguments(format!(
                "unknown desktop snapshot ref `{key}`; run `da session snapshot` to inspect available controls"
            ))
        })?;
        let found = tree
            .nodes
            .iter()
            .position(|n| !stored.runtime_id.is_empty() && n.runtime_id == stored.runtime_id);
        return Ok(found);
    }

    let locator = target
        .ok_or_else(|| StepError::InvalidArguments("missing desktop locator target".to_string()))?;

    let (start, end) = if let Some(scope) = &locator.within {
        let scope_node = tree
            .nodes
            .iter()
            .filter(|n| matches_filter(n, scope))
            .nth(scope.nth);
        let Some(scope_node) = scope_node else {
            return Ok(None);
        };
        (scope_node.index + 1, scope_node.subtree_end)
    } else {
        (0, tree.nodes.len())
    };

    let matched = tree.nodes[start..end]
        .iter()
        .filter(|n| matches_filter(n, &locator.filter))
        .nth(locator.filter.nth)
        .map(|n| n.index);

    Ok(matched)
}

fn synthesize_locator(idx: usize, nodes: &[NodeData]) -> (Value, bool) {
    let node = &nodes[idx];

    // Tier 1: non-volatile, unique AutomationId within the window.
    if let Some(aid) = &node.automation_id {
        let count = nodes
            .iter()
            .skip(1)
            .filter(|n| n.automation_id.as_deref() == Some(aid.as_str()))
            .count();
        if count == 1 {
            return (json!({ "automation_id": aid }), false);
        }
    }

    // Tier 2: unique (role + name) globally, or scoped within nearest identifiable container.
    if !node.name.is_empty() {
        let global_count = nodes
            .iter()
            .skip(1)
            .filter(|n| n.role == node.role && n.name == node.name)
            .count();
        if global_count == 1 {
            return (
                json!({
                    "role": node.role.as_str(),
                    "name": node.name,
                }),
                false,
            );
        }

        if let Some((within_json, scope_start, scope_end)) = nearest_within_scope(idx, nodes) {
            let scoped_count = nodes[scope_start..scope_end]
                .iter()
                .filter(|n| n.role == node.role && n.name == node.name)
                .count();
            if scoped_count == 1 {
                return (
                    json!({
                        "within": within_json,
                        "role": node.role.as_str(),
                        "name": node.name,
                    }),
                    false,
                );
            }
        }
    }

    // Tier 3: structural fallback using role + optional name/class_name/within + nth (fragile).
    let (within_json, scope_start, scope_end) = nearest_within_scope(idx, nodes)
        .map(|(w, s, e)| (Some(w), s, e))
        .unwrap_or((None, 1.min(nodes.len()), nodes.len()));

    let use_name = !node.name.is_empty();
    let use_class = !use_name && !node.class_name.is_empty();
    let matches_before = nodes[scope_start..idx]
        .iter()
        .filter(|n| {
            n.role == node.role
                && (!use_name || n.name == node.name)
                && (!use_class || n.class_name == node.class_name)
        })
        .count();
    let total_matches = nodes[scope_start..scope_end]
        .iter()
        .filter(|n| {
            n.role == node.role
                && (!use_name || n.name == node.name)
                && (!use_class || n.class_name == node.class_name)
        })
        .count();

    let mut obj = serde_json::Map::new();
    if let Some(w) = within_json {
        obj.insert("within".to_string(), w);
    }
    obj.insert("role".to_string(), json!(node.role.as_str()));
    if use_name {
        obj.insert("name".to_string(), json!(node.name));
    } else if use_class {
        obj.insert("class_name".to_string(), json!(node.class_name));
    }
    if total_matches > 1 || matches_before > 0 {
        obj.insert("nth".to_string(), json!(matches_before));
    }
    (Value::Object(obj), true)
}

fn nearest_within_scope(idx: usize, nodes: &[NodeData]) -> Option<(Value, usize, usize)> {
    let mut cur = nodes[idx].parent;
    while let Some(parent_idx) = cur {
        if parent_idx == 0 {
            break;
        }
        let parent = &nodes[parent_idx];
        if let Some(aid) = &parent.automation_id {
            let count = nodes
                .iter()
                .skip(1)
                .filter(|n| n.automation_id.as_deref() == Some(aid.as_str()))
                .count();
            if count == 1 {
                return Some((
                    json!({ "automation_id": aid }),
                    parent.index + 1,
                    parent.subtree_end,
                ));
            }
        }
        if !parent.name.is_empty() {
            let count = nodes
                .iter()
                .skip(1)
                .filter(|n| n.role == parent.role && n.name == parent.name)
                .count();
            if count == 1 {
                return Some((
                    json!({ "role": parent.role.as_str(), "name": parent.name }),
                    parent.index + 1,
                    parent.subtree_end,
                ));
            }
        }
        cur = parent.parent;
    }
    None
}

fn reverse_lookup_at(
    automation: &UIAutomation,
    tree: &TreeData,
    target_pid: u32,
    screen_pt: Point,
) -> Option<(Value, bool)> {
    let hit = automation.element_from_point(screen_pt).ok()?;
    let hit_pid = hit.get_process_id().ok()?;
    if hit_pid != target_pid {
        return None;
    }

    let walker = automation.get_control_view_walker().ok()?;
    let mut cur = hit;
    for _ in 0..=2 {
        let rid = cur.get_runtime_id().ok()?;
        if let Some(idx) = tree.nodes.iter().position(|n| n.runtime_id == rid) {
            let node = &tree.nodes[idx];
            if node.role.is_interactive() {
                let (loc, fragile) = synthesize_locator(idx, &tree.nodes);
                if !fragile {
                    return Some((loc, false));
                }
                return None;
            }
            if !matches!(node.role, DesktopRole::Text | DesktopRole::Image) {
                // Canvas / Pane / container click must remain coordinate-based (`at`).
                return None;
            }
        }
        cur = walker.get_parent(&cur).ok()?;
    }
    None
}

fn check_window_point_bounds(tree: &TreeData, point: &DesktopPoint) -> Result<(), StepError> {
    if let Some(expected) = point.window
        && (expected.width != tree.win_width || expected.height != tree.win_height)
    {
        return Err(StepError::Execution(format!(
            "desktop window size {}x{} does not match expected {}x{} in `at.window`",
            tree.win_width, tree.win_height, expected.width, expected.height
        )));
    }
    if point.x < 0
        || point.y < 0
        || point.x >= tree.win_width as i32
        || point.y >= tree.win_height as i32
    {
        return Err(StepError::Execution(format!(
            "coordinates ({}, {}) fall outside target window bounds {}x{}",
            point.x, point.y, tree.win_width, tree.win_height
        )));
    }
    Ok(())
}

fn try_pattern_click(el: &UIElement, role: DesktopRole) -> Result<bool, uiautomation::Error> {
    match role {
        DesktopRole::CheckBox => {
            if let Ok(toggle) = el.get_pattern::<UITogglePattern>() {
                toggle.toggle()?;
                return Ok(true);
            }
        }
        DesktopRole::RadioButton | DesktopRole::TabItem | DesktopRole::ListItem => {
            if let Ok(select) = el.get_pattern::<UISelectionItemPattern>() {
                select.select()?;
                return Ok(true);
            }
        }
        _ => {}
    }

    if let Ok(invoke) = el.get_pattern::<UIInvokePattern>() {
        invoke.invoke()?;
        return Ok(true);
    }

    if matches!(
        role,
        DesktopRole::ComboBox | DesktopRole::MenuItem | DesktopRole::TreeItem
    ) && let Ok(ec) = el.get_pattern::<UIExpandCollapsePattern>()
    {
        ec.expand()?;
        return Ok(true);
    }

    if !matches!(
        role,
        DesktopRole::Edit | DesktopRole::Pane | DesktopRole::Window | DesktopRole::Document
    ) && let Ok(legacy) = el.get_pattern::<UILegacyIAccessiblePattern>()
        && legacy.do_default_action().is_ok()
    {
        return Ok(true);
    }

    Ok(false)
}

fn focus_window(win_el: &UIElement) -> Result<(), uiautomation::Error> {
    if let Ok(wp) = win_el.get_pattern::<UIWindowPattern>()
        && wp.get_window_visual_state().ok() == Some(WindowVisualState::Minimized)
    {
        let _ = wp.set_window_visual_state(WindowVisualState::Normal);
    }
    win_el.set_focus()
}

fn send_mouse_click(pt: Point, button: ClickButton) -> Result<(), String> {
    let mouse = Mouse::new().move_time(0).interval(10);
    match button {
        ClickButton::Left => mouse.click(&pt),
        ClickButton::Right => mouse.right_click(&pt),
        ClickButton::Double => mouse.double_click(&pt),
    }
    .map_err(|err| err.to_string())
}

fn capture_window_png(
    tree: &TreeData,
    hwnd: isize,
    mask_inputs: bool,
) -> Result<(Vec<u8>, u32, u32, usize), StepError> {
    let mut rgb = win_screenshot::capture::capture_window(hwnd).map_err(|err| {
        StepError::Environment(format!(
            "failed to capture desktop window screenshot (HWND {hwnd}): {err}"
        ))
    })?;
    let width = rgb.width;
    let height = rgb.height;
    let expected_len = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4);
    if rgb.pixels.len() < expected_len {
        return Err(StepError::Environment(
            "desktop window capture returned incomplete RGBA buffer".to_string(),
        ));
    }

    // GDI PrintWindow can leave byte 3 (alpha) as 0; force opaque pixels so
    // PNG viewers and decoders observe crisp RGB values plus solid black masks.
    for px in rgb.pixels[..expected_len].as_chunks_mut::<4>().0 {
        px[3] = 255;
    }

    let mut masked_count = 0usize;
    if mask_inputs {
        for node in &tree.nodes {
            if !should_mask_node(node) {
                continue;
            }
            if mask_rgba_rect(&mut rgb.pixels[..expected_len], width, height, node.bounds) {
                masked_count += 1;
            }
        }
    }

    let mut png_bytes = Vec::new();
    {
        let writer = BufWriter::new(&mut png_bytes);
        let mut encoder = png::Encoder::new(writer, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut png_writer = encoder
            .write_header()
            .map_err(|err| StepError::Execution(format!("PNG header error: {err}")))?;
        png_writer
            .write_image_data(&rgb.pixels[..expected_len])
            .map_err(|err| StepError::Execution(format!("PNG encode error: {err}")))?;
    }

    Ok((png_bytes, width, height, masked_count))
}

fn should_mask_node(node: &NodeData) -> bool {
    if node.is_password || node.role == DesktopRole::Edit {
        return true;
    }
    let class_lower = node.class_name.to_ascii_lowercase();
    class_lower == "edit" || class_lower.contains("richedit")
}

fn mask_rgba_rect(pixels: &mut [u8], width: u32, height: u32, bounds: BoundsRect) -> bool {
    let x0 = bounds.x.max(0).min(width as i32) as u32;
    let y0 = bounds.y.max(0).min(height as i32) as u32;
    let x1 = (bounds.x + bounds.width).max(0).min(width as i32) as u32;
    let y1 = (bounds.y + bounds.height).max(0).min(height as i32) as u32;
    if x1 <= x0 || y1 <= y0 {
        return false;
    }
    for y in y0..y1 {
        let row_start = ((y * width + x0) * 4) as usize;
        let row_end = ((y * width + x1) * 4) as usize;
        for px in pixels[row_start..row_end].as_chunks_mut::<4>().0 {
            *px = [0, 0, 0, 255];
        }
    }
    true
}

fn node_summary(node: &NodeData) -> Value {
    json!({
        "role": node.role.as_str(),
        "name": node.name,
        "value": if node.is_password { Value::Null } else { node.value.clone().map(Value::String).unwrap_or(Value::Null) },
        "automation_id": node.automation_id,
        "class_name": if node.class_name.is_empty() { Value::Null } else { Value::String(node.class_name.clone()) },
        "bounds": node.bounds,
        "enabled": node.enabled,
        "is_password": node.is_password,
    })
}

fn build_nested_tree_json(
    tree: &TreeData,
    idx: usize,
    base_depth: usize,
    max_depth: usize,
    ref_by_idx: &HashMap<usize, String>,
) -> Value {
    let node = &tree.nodes[idx];
    let rel_depth = node.depth.saturating_sub(base_depth);
    let mut children = Vec::new();
    if rel_depth < max_depth {
        let mut child_idx = idx + 1;
        while child_idx < node.subtree_end {
            children.push(build_nested_tree_json(
                tree, child_idx, base_depth, max_depth, ref_by_idx,
            ));
            child_idx = tree.nodes[child_idx].subtree_end;
        }
    }
    let mut obj = serde_json::Map::new();
    if let Some(r) = ref_by_idx.get(&idx) {
        obj.insert("ref".to_string(), json!(r));
    }
    obj.insert("role".to_string(), json!(node.role.as_str()));
    if !node.name.is_empty() {
        obj.insert("name".to_string(), json!(node.name));
    }
    if let Some(aid) = &node.automation_id {
        obj.insert("automation_id".to_string(), json!(aid));
    }
    if node.is_password {
        obj.insert("is_password".to_string(), json!(true));
    } else if let Some(val) = &node.value {
        obj.insert("value".to_string(), json!(val));
    }
    if !children.is_empty() {
        obj.insert("children".to_string(), Value::Array(children));
    }
    Value::Object(obj)
}

fn spawn_direct(
    command_path: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    raw_command: &str,
) -> Result<Child, StepError> {
    let mut cmd = Command::new(command_path);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if !env.is_empty() {
        cmd.envs(env);
    }
    cmd.spawn().map_err(|err| {
        StepError::Environment(format!(
            "failed to launch desktop command `{raw_command}`: {err}"
        ))
    })
}

fn windows_system_tool(parts: &[&str], fallback: &str) -> std::path::PathBuf {
    let Some(root) = std::env::var_os("SystemRoot") else {
        return std::path::PathBuf::from(fallback);
    };
    let mut path = std::path::PathBuf::from(root).join("System32");
    for part in parts {
        path = path.join(part);
    }
    if path.is_file() {
        path
    } else {
        std::path::PathBuf::from(fallback)
    }
}

fn spawn_detached_via_powershell(
    command_path: &str,
    args: &[String],
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<u32, String> {
    let arg_line = args
        .iter()
        .map(|arg| quote_windows_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");

    let script = concat!(
        "$exe = $env:DA_DESKTOP_LAUNCH_EXE; ",
        "$argStr = $env:DA_DESKTOP_LAUNCH_ARGS; ",
        "$cwd = $env:DA_DESKTOP_LAUNCH_CWD; ",
        "Remove-Item Env:DA_DESKTOP_LAUNCH_EXE,Env:DA_DESKTOP_LAUNCH_ARGS,Env:DA_DESKTOP_LAUNCH_CWD -ErrorAction SilentlyContinue; ",
        "$sp = @{ FilePath = $exe; PassThru = $true }; ",
        "if ($argStr) { $sp.ArgumentList = $argStr }; ",
        "if ($cwd) { $sp.WorkingDirectory = $cwd }; ",
        "$p = Start-Process @sp; ",
        "[Console]::Out.Write($p.Id)"
    );

    let ps = windows_system_tool(
        &["WindowsPowerShell", "v1.0", "powershell.exe"],
        "powershell.exe",
    );
    let mut cmd = Command::new(&ps);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !env.is_empty() {
        cmd.envs(env);
    }
    cmd.env("DA_DESKTOP_LAUNCH_EXE", command_path);
    if !arg_line.is_empty() {
        cmd.env("DA_DESKTOP_LAUNCH_ARGS", &arg_line);
    } else {
        cmd.env_remove("DA_DESKTOP_LAUNCH_ARGS");
    }
    if let Some(dir) = cwd {
        cmd.env("DA_DESKTOP_LAUNCH_CWD", dir);
    } else {
        cmd.env_remove("DA_DESKTOP_LAUNCH_CWD");
    }

    let output = cmd
        .output()
        .map_err(|e| format!("failed to spawn `{}`: {e}", ps.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("powershell Start-Process exited with {}", output.status)
        } else {
            stderr
        });
    }
    let pid_text = String::from_utf8_lossy(&output.stdout);
    pid_text
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("invalid PID from Start-Process `{pid_text}`: {e}"))
}

fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            out.push_str(&"\\".repeat(backslashes * 2 + 1));
            out.push('"');
            backslashes = 0;
        } else {
            out.push_str(&"\\".repeat(backslashes));
            backslashes = 0;
            out.push(ch);
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn terminate_pid_or_child(child: Option<&mut Child>, pid: u32) {
    if let Some(c) = child {
        let _ = terminate_child(c, pid);
    } else {
        let _ = kill_pid_tree(pid);
    }
}

fn resolve_command(raw: &str, base_dir: &Path) -> String {
    let p = Path::new(raw);
    if p.is_absolute() {
        return raw.to_string();
    }
    if raw.contains('\\') || raw.contains('/') {
        let joined = base_dir.join(p);
        return joined.to_string_lossy().into_owned();
    }
    let candidate = base_dir.join(p);
    if candidate.is_file() {
        candidate.to_string_lossy().into_owned()
    } else {
        raw.to_string()
    }
}

fn pid_executable_map() -> HashMap<u32, String> {
    let mut map = HashMap::new();
    let tasklist = windows_system_tool(&["tasklist.exe"], "tasklist");
    let Ok(output) = Command::new(tasklist)
        .args(["/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()
    else {
        return map;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let mut cols = line.split("\",\"");
        let Some(image) = cols.next() else {
            continue;
        };
        let Some(pid_col) = cols.next() else {
            continue;
        };
        let image_clean = image.trim().trim_start_matches('"');
        if let Ok(pid) = pid_col.trim().trim_matches('"').parse::<u32>() {
            map.insert(pid, image_clean.to_string());
        }
    }
    map
}

fn is_pid_running(pid: u32) -> bool {
    let tasklist = windows_system_tool(&["tasklist.exe"], "tasklist");
    let Ok(output) = Command::new(tasklist)
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let needle = format!("\"{pid}\"");
    text.lines().any(|line| line.contains(&needle))
}

fn terminate_child(child: &mut Child, pid: u32) -> Result<(), String> {
    if child.try_wait().map_err(|e| e.to_string())?.is_some() {
        return Ok(());
    }
    let _ = child.kill();
    let _ = child.wait();
    if is_pid_running(pid) {
        let _ = kill_pid_tree(pid);
    }
    Ok(())
}

fn kill_pid_tree(pid: u32) -> Result<(), String> {
    if !is_pid_running(pid) {
        return Ok(());
    }
    let taskkill = windows_system_tool(&["taskkill.exe"], "taskkill");
    let status = Command::new(taskkill)
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| format!("failed to run taskkill for PID {pid}: {err}"))?;
    if !status.success() && is_pid_running(pid) {
        return Err(format!("taskkill failed for PID {pid} ({status})"));
    }
    Ok(())
}

fn click_button_name(button: ClickButton) -> &'static str {
    match button {
        ClickButton::Left => "left",
        ClickButton::Right => "right",
        ClickButton::Double => "double",
    }
}

fn wait_state_name(state: WaitState) -> &'static str {
    match state {
        WaitState::Present => "present",
        WaitState::Gone => "gone",
        WaitState::Enabled => "enabled",
        WaitState::Visible => "visible",
    }
}

fn control_type_to_role(ct: ControlType) -> Option<DesktopRole> {
    match ct {
        ControlType::Button => Some(DesktopRole::Button),
        ControlType::Calendar => Some(DesktopRole::Calendar),
        ControlType::CheckBox => Some(DesktopRole::CheckBox),
        ControlType::ComboBox => Some(DesktopRole::ComboBox),
        ControlType::Edit => Some(DesktopRole::Edit),
        ControlType::Hyperlink => Some(DesktopRole::Hyperlink),
        ControlType::Image => Some(DesktopRole::Image),
        ControlType::ListItem => Some(DesktopRole::ListItem),
        ControlType::List => Some(DesktopRole::List),
        ControlType::Menu => Some(DesktopRole::Menu),
        ControlType::MenuBar => Some(DesktopRole::MenuBar),
        ControlType::MenuItem => Some(DesktopRole::MenuItem),
        ControlType::ProgressBar => Some(DesktopRole::ProgressBar),
        ControlType::RadioButton => Some(DesktopRole::RadioButton),
        ControlType::ScrollBar => Some(DesktopRole::ScrollBar),
        ControlType::Slider => Some(DesktopRole::Slider),
        ControlType::Spinner => Some(DesktopRole::Spinner),
        ControlType::StatusBar => Some(DesktopRole::StatusBar),
        ControlType::Tab => Some(DesktopRole::Tab),
        ControlType::TabItem => Some(DesktopRole::TabItem),
        ControlType::Text => Some(DesktopRole::Text),
        ControlType::ToolBar => Some(DesktopRole::ToolBar),
        ControlType::ToolTip => Some(DesktopRole::ToolTip),
        ControlType::Tree => Some(DesktopRole::Tree),
        ControlType::TreeItem => Some(DesktopRole::TreeItem),
        ControlType::Custom => Some(DesktopRole::Custom),
        ControlType::Group => Some(DesktopRole::Group),
        ControlType::Thumb => Some(DesktopRole::Thumb),
        ControlType::DataGrid => Some(DesktopRole::DataGrid),
        ControlType::DataItem => Some(DesktopRole::DataItem),
        ControlType::Document => Some(DesktopRole::Document),
        ControlType::SplitButton => Some(DesktopRole::SplitButton),
        ControlType::Window => Some(DesktopRole::Window),
        ControlType::Pane => Some(DesktopRole::Pane),
        ControlType::Header => Some(DesktopRole::Header),
        ControlType::HeaderItem => Some(DesktopRole::HeaderItem),
        ControlType::Table => Some(DesktopRole::Table),
        ControlType::TitleBar => Some(DesktopRole::TitleBar),
        ControlType::Separator => Some(DesktopRole::Separator),
        ControlType::SemanticZoom => Some(DesktopRole::SemanticZoom),
        ControlType::AppBar => Some(DesktopRole::AppBar),
    }
}
