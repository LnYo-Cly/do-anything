//! Finds and starts the Chrome-family browser one case drives.
//!
//! The browser is ordinary Chrome, Chromium or Edge that is already installed;
//! nothing is downloaded. It runs with a fresh temporary profile and the
//! debugger bound to a port the operating system chooses, so two cases that run
//! at the same time cannot share a profile or a port.
//!
//! [`Launched::shutdown`] asks the browser to close through CDP, waits for the
//! process, kills it when the deadline passes and removes the profile. The
//! `Drop` impl repeats that best effort when a case never reached its close
//! path, so no browser survives the run.

use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::cdp::Deadline;

/// Prefix of the temporary profile directories, also used by the close test.
pub const PROFILE_PREFIX: &str = "da-chrome-profile";

/// How long a hung browser gets to exit after `Browser.close`.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the debug-port file and the child process are polled.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// `taskkill` exit code for "the process was not found" (ERROR_NOT_FOUND).
#[cfg(windows)]
const TASKKILL_PROCESS_NOT_FOUND: i32 = 128;

/// A browser process started for one case.
pub struct Launched {
    child: Option<Child>,
    detached_pid: Option<u32>,
    profile: Option<PathBuf>,
    port: u16,
    websocket_url: String,
}

impl Launched {
    /// Starts the browser in a pipe-isolated manner when `detached` is `true`
    /// so Windows callers capturing CLI stdout/stderr do not block after `da` exits.
    pub fn launch_with_mode(
        headless: bool,
        width: u32,
        height: u32,
        deadline: Deadline,
        detached: bool,
    ) -> Result<Self, String> {
        let executable = find_browser()?;
        let profile = create_profile()?;
        let mut args = launch_args(headless, width, height, &profile);
        args.push("about:blank".to_string());

        #[cfg(windows)]
        let (mut child, detached_pid) = if detached {
            (
                None,
                Some(spawn_windows_isolated(&executable, &profile, &args)?),
            )
        } else {
            (
                Some(spawn_direct(&executable, &profile, &args, detached)?),
                None,
            )
        };
        #[cfg(not(windows))]
        let (mut child, detached_pid) = (
            Some(spawn_direct(&executable, &profile, &args, detached)?),
            None,
        );

        let port_file = profile.join("DevToolsActivePort");
        loop {
            if let Some(child_ref) = child.as_mut() {
                match child_ref.try_wait() {
                    Ok(Some(status)) => {
                        let _ = remove_profile(&profile);
                        return Err(format!(
                            "`{}` exited with {status} before it was ready; set `DA_CHROME` to the Chrome or Chromium executable",
                            executable.display()
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = child_ref.kill();
                        let _ = child_ref.wait();
                        let _ = remove_profile(&profile);
                        return Err(format!("could not watch the browser process: {error}"));
                    }
                }
            }

            if let Ok(text) = fs::read_to_string(&port_file)
                && let Some((port, path)) = parse_devtools_active_port(&text)
            {
                return Ok(Self {
                    child,
                    detached_pid,
                    profile: Some(profile),
                    port,
                    websocket_url: format!("ws://127.0.0.1:{port}{path}"),
                });
            }

            if deadline.passed() {
                let mut cleanup_errors = Vec::new();
                if let Some(mut c) = child.take() {
                    if let Err(error) = c.kill().and_then(|()| c.wait().map(drop)) {
                        cleanup_errors.push(format!("could not kill the browser process: {error}"));
                    }
                } else if let Some(pid) = detached_pid
                    && let Err(error) = kill_process(pid, Some(&profile))
                {
                    cleanup_errors.push(error);
                }
                if let Err(error) = remove_profile(&profile) {
                    cleanup_errors.push(error);
                }
                let cleanup = if cleanup_errors.is_empty() {
                    String::new()
                } else {
                    format!("; cleanup also failed: {}", cleanup_errors.join("; "))
                };
                return Err(format!(
                    "the browser did not report a debugging port within {} ms; set `DA_CHROME` to the Chrome or Chromium executable{cleanup}",
                    deadline.timeout_ms()
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// The debug port the browser is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The browser's DevTools WebSocket URL.
    pub fn websocket_url(&self) -> &str {
        &self.websocket_url
    }

    /// Releases local ownership of the process and profile without closing Chrome
    /// so a later `da session` command can reconnect to it.
    pub fn detach(mut self) -> Option<DetachedProcess> {
        if self.profile.is_none() || (self.child.is_none() && self.detached_pid.is_none()) {
            return None;
        }
        let profile = self.profile.take()?;
        let child_pid = self.child.take().map(|child| child.id());
        let detached_pid = self.detached_pid.take();
        let pid = child_pid.or(detached_pid)?;
        Some(DetachedProcess {
            pid,
            profile,
            port: self.port,
            websocket_url: self.websocket_url.clone(),
        })
    }

    /// Waits for the process to exit, kills it when it does not, and removes
    /// the temporary profile.
    pub fn shutdown(mut self) -> Result<(), String> {
        self.release()
    }

    /// Releases what is left. Called once by [`Self::shutdown`] or by `Drop`.
    fn release(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Some(mut child) = self.child.take() {
            // A browser that is slow to exit (seen on loaded CI runners) is
            // still released once killing it succeeds; only a browser that
            // cannot be stopped is a close error.
            if let Err(error) = wait_for_exit(&mut child, EXIT_TIMEOUT)
                && let Err(kill_error) = child.kill().and_then(|()| child.wait().map(drop))
            {
                errors.push(format!(
                    "the browser did not exit within {} ms after `Browser.close` ({error}) and could not be killed: {kill_error}",
                    EXIT_TIMEOUT.as_millis()
                ));
            }
        } else if let Some(pid) = self.detached_pid.take() {
            let profile = self.profile.take();
            if let Err(err) = dispose_detached(Some(pid), Some(self.port), profile.as_deref()) {
                errors.push(err);
            }
        }
        if let Some(profile) = self.profile.take()
            && let Err(error) = remove_profile(&profile)
        {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        if self.child.is_some() || self.detached_pid.is_some() || self.profile.is_some() {
            let _ = self.release();
        }
    }
}

fn spawn_direct(
    executable: &Path,
    profile: &Path,
    args: &[String],
    detached: bool,
) -> Result<Child, String> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if detached {
        configure_detached(&mut command);
    }
    command.spawn().map_err(|error| {
        let _ = remove_profile(profile);
        format!(
            "could not start `{}`: {error}; set `DA_CHROME` to the Chrome or Chromium executable",
            executable.display()
        )
    })
}

#[cfg(windows)]
fn spawn_windows_isolated(
    executable: &Path,
    profile: &Path,
    args: &[String],
) -> Result<u32, String> {
    let arg_line = args
        .iter()
        .map(|arg| quote_win_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    let powershell = windows_system_tool(
        &["WindowsPowerShell", "v1.0", "powershell.exe"],
        "powershell.exe",
    );

    let output = Command::new(&powershell)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$p = Start-Process -FilePath $env:DA_LAUNCH_EXE -ArgumentList $env:DA_LAUNCH_ARGS -WindowStyle Hidden -PassThru; Write-Output $p.Id",
        ])
        .env("DA_LAUNCH_EXE", executable)
        .env("DA_LAUNCH_ARGS", &arg_line)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            let _ = remove_profile(profile);
            format!(
                "could not start `{}`: {error}; set `DA_CHROME` to the Chrome or Chromium executable",
                executable.display()
            )
        })?;

    if !output.status.success() {
        let _ = remove_profile(profile);
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "could not start `{}`: {stderr}; set `DA_CHROME` to the Chrome or Chromium executable",
            executable.display()
        ));
    }

    let pid_text = String::from_utf8_lossy(&output.stdout);
    pid_text.trim().parse::<u32>().map_err(|error| {
        let _ = remove_profile(profile);
        format!(
            "could not read pid for `{}`: {error} (`{}`)",
            executable.display(),
            pid_text.trim()
        )
    })
}

#[cfg(windows)]
fn quote_win_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
        } else if ch == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            backslashes = 0;
            out.push(ch);
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Waits for `child` to exit, polling until `timeout` passes.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Ok(None) => return Err("the process was still running".to_string()),
            Err(error) => return Err(format!("could not watch the process: {error}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileEntry {
    Missing,
    Directory,
    Unsafe,
}

/// Pure validator checking that `profile` is a direct `da-chrome-profile-<pid>-<seq>`
/// child directory of `temp_root`, with no traversal or non-canonical segments.
pub(crate) fn validate_owned_profile_in(profile: &Path, temp_root: &Path) -> Result<(), String> {
    let raw = profile
        .to_str()
        .ok_or_else(|| "browser profile path must be valid UTF-8".to_string())?;
    if raw.is_empty() {
        return Err("browser profile path must not be empty".to_string());
    }
    if !profile.is_absolute() || !temp_root.is_absolute() {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: path must be absolute"
        ));
    }
    if raw.ends_with('/')
        || raw.ends_with('\\')
        || raw.ends_with("/.")
        || raw.ends_with("\\.")
        || raw.contains("/./")
        || raw.contains("\\.\\")
        || raw.contains("/.\\")
        || raw.contains("\\./")
    {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: path must be canonical"
        ));
    }
    if profile
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: path must not contain `.` or `..`"
        ));
    }

    let temp_components: Vec<Component<'_>> = temp_root.components().collect();
    let profile_components: Vec<Component<'_>> = profile.components().collect();
    if profile_components.len() != temp_components.len() + 1 {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: expected a direct child of `{}`",
            temp_root.display()
        ));
    }
    if !profile_components[..temp_components.len()]
        .iter()
        .zip(temp_components.iter())
        .all(|(left, right)| same_path_component(*left, *right))
    {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: outside temporary root `{}`",
            temp_root.display()
        ));
    }

    let Some(Component::Normal(dir_name)) = profile_components.last() else {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: invalid directory name"
        ));
    };
    let Some(dir_name) = dir_name.to_str() else {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: non-UTF-8 directory name"
        ));
    };
    if !is_owned_profile_dir_name(dir_name) {
        return Err(format!(
            "refusing unowned browser profile `{raw}`: directory name `{dir_name}` is not owned by `da`"
        ));
    }
    Ok(())
}

fn same_path_component(left: Component<'_>, right: Component<'_>) -> bool {
    #[cfg(windows)]
    match (left, right) {
        (Component::RootDir, Component::RootDir) => true,
        (Component::Prefix(a), Component::Prefix(b)) => a
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()),
        (Component::Normal(a), Component::Normal(b)) => a
            .to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy()),
        _ => false,
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn is_owned_profile_dir_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(PROFILE_PREFIX) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix('-') else {
        return false;
    };
    let Some((pid_part, seq_part)) = rest.split_once('-') else {
        return false;
    };
    !pid_part.is_empty()
        && pid_part.bytes().all(|byte| byte.is_ascii_digit())
        && pid_part.parse::<u32>().is_ok_and(|pid| pid > 0)
        && !seq_part.is_empty()
        && seq_part.bytes().all(|byte| byte.is_ascii_digit())
        && seq_part.parse::<u64>().is_ok()
}

fn inspect_profile_entry(path: &Path) -> Result<ProfileEntry, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                Ok(ProfileEntry::Unsafe)
            } else {
                Ok(ProfileEntry::Directory)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ProfileEntry::Missing),
        Err(error) => Err(format!(
            "could not inspect temporary browser profile `{}`: {error}",
            path.display()
        )),
    }
}

/// Removes the temporary profile after validating ownership and rejecting symlinks,
/// retrying while Windows releases its handles.
fn remove_profile(profile: &Path) -> Result<(), String> {
    remove_profile_with(
        profile,
        &std::env::temp_dir(),
        inspect_profile_entry,
        |path| fs::remove_dir_all(path),
    )
}

fn remove_profile_with<I, R>(
    profile: &Path,
    temp_root: &Path,
    mut inspect_entry: I,
    mut remove_dir_all: R,
) -> Result<(), String>
where
    I: FnMut(&Path) -> Result<ProfileEntry, String>,
    R: FnMut(&Path) -> std::io::Result<()>,
{
    validate_owned_profile_in(profile, temp_root)?;
    match inspect_entry(profile)? {
        ProfileEntry::Missing => return Ok(()),
        ProfileEntry::Unsafe => {
            return Err(format!(
                "refusing to remove unowned or non-directory browser profile `{}`",
                profile.display()
            ));
        }
        ProfileEntry::Directory => {}
    }

    let mut last = None;
    for _ in 0..40 {
        match remove_dir_all(profile) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                last = Some(error);
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let error = last.map_or_else(|| "unknown error".to_string(), |error| error.to_string());
    Err(format!(
        "could not remove the temporary browser profile `{}`: {error}",
        profile.display()
    ))
}

/// The switches that start the browser for one case, before `about:blank`.
///
/// A headless launch turns GPU acceleration off, and that has a cost: the page is
/// composited in software and WebGL falls back to a software ANGLE renderer
/// (WARP, or SwiftShader where ANGLE asks for one), so a GPU-heavy page draws more
/// slowly than on the hardware path and its pixels can differ. It buys CPU for the
/// machine that runs the cases, which is what issue #59 is about: with four cases
/// on one CPU the GPU process spent 8.4 s of that core on a hardware GPU and
/// 8.8-9.3 s on the software one, and next to it the browser processes could not
/// write their first navigation request until the 30 s navigation budget was gone.
/// With this switch the same GPU process used 0.8 s and 1.0-1.4 s, the whole tree
/// used 25-40% less CPU, and ten of ten starved runs finished on both hosts where
/// six of ten (hardware) and three of ten (software) had failed. A visible window
/// keeps its GPU. No switch here touches the system proxy, so a case behind a
/// corporate proxy still reaches its pages.
fn launch_args(headless: bool, width: u32, height: u32, profile: &Path) -> Vec<String> {
    let mut args = vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-port=0".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        format!("--window-size={width},{height}"),
    ];
    if headless {
        args.push("--headless=new".to_string());
        args.push("--disable-gpu".to_string());
    }
    args
}

/// Metadata describing a detached Chrome process that survives across CLI calls.
pub struct DetachedProcess {
    pub pid: u32,
    pub profile: PathBuf,
    pub port: u16,
    pub websocket_url: String,
}

/// Ensures a detached browser process is terminated and its temporary profile
/// directory is removed.
///
/// When `port` is no longer listening (because `Browser.close` closed the
/// socket first, the handle's port is stale, or the browser crashed), any live
/// process at `pid` is still verified against `profile` before termination so
/// a recycled PID belonging to an unrelated process is left untouched.
pub fn dispose_detached(
    pid: Option<u32>,
    port: Option<u16>,
    profile: Option<&Path>,
) -> Result<(), String> {
    dispose_detached_with(pid, port, profile, EXIT_TIMEOUT)
}

fn dispose_detached_with(
    pid: Option<u32>,
    port: Option<u16>,
    profile: Option<&Path>,
    exit_timeout: Duration,
) -> Result<(), String> {
    dispose_detached_with_ops(
        pid,
        port,
        profile,
        &std::env::temp_dir(),
        exit_timeout,
        default_grace_window(),
        is_port_listening,
        default_pid_exists,
        pid_matches_profile,
        force_kill_pid,
        remove_profile,
    )
}

/// [`dispose_detached_with`] with termination injected so a test can prove that
/// a failing or non-zero kill reaches the caller instead of being swallowed.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "test seam for termination error propagation")
)]
fn dispose_detached_with_kill<K>(
    pid: Option<u32>,
    port: Option<u16>,
    profile: Option<&Path>,
    exit_timeout: Duration,
    kill: K,
) -> Result<(), String>
where
    K: FnMut(u32, Option<&Path>) -> Result<(), String>,
{
    dispose_detached_with_ops(
        pid,
        port,
        profile,
        &std::env::temp_dir(),
        exit_timeout,
        default_grace_window(),
        is_port_listening,
        default_pid_exists,
        pid_matches_profile,
        kill,
        remove_profile,
    )
}

fn default_grace_window() -> Duration {
    #[cfg(windows)]
    {
        Duration::from_millis(250)
    }
    #[cfg(unix)]
    {
        Duration::from_millis(500)
    }
    #[cfg(not(any(windows, unix)))]
    {
        Duration::ZERO
    }
}

fn default_pid_exists(pid: u32) -> Option<bool> {
    #[cfg(windows)]
    {
        windows_pid_exists(pid)
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal callback seam for pure unit tests without touching real processes or disk"
)]
fn dispose_detached_with_ops<P, E, V, K, R>(
    pid: Option<u32>,
    port: Option<u16>,
    profile: Option<&Path>,
    temp_root: &Path,
    exit_timeout: Duration,
    grace_window: Duration,
    mut port_listening: P,
    mut pid_exists: E,
    mut verify: V,
    mut kill: K,
    mut remove_dir: R,
) -> Result<(), String>
where
    P: FnMut(u16) -> bool,
    E: FnMut(u32) -> Option<bool>,
    V: FnMut(u32, &Path) -> PidProfileMatch,
    K: FnMut(u32, Option<&Path>) -> Result<(), String>,
    R: FnMut(&Path) -> Result<(), String>,
{
    let Some(profile) = profile else {
        return Err(match pid {
            Some(pid) => format!(
                "could not verify or terminate browser pid {pid}: the session handle has no profile path"
            ),
            None => "could not verify detached browser: the session handle has no profile path"
                .to_string(),
        });
    };
    validate_owned_profile_in(profile, temp_root)?;
    if pid == Some(0) {
        return Err("could not verify or terminate browser pid 0: invalid pid".to_string());
    }

    let mut errors = Vec::new();
    let deadline = Instant::now() + exit_timeout;

    let mut port_open = port.is_some_and(&mut port_listening);
    if let Some(port) = port
        && port_open
    {
        while port_listening(port) && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        port_open = port_listening(port);
    }

    let mut kill_attempted = false;
    match pid {
        Some(pid) if port_open => match verify(pid, profile) {
            PidProfileMatch::Matches => {
                kill_attempted = true;
                if let Err(err) = kill(pid, Some(profile)) {
                    errors.push(err);
                }
            }
            PidProfileMatch::Unknown => {
                errors.push(format!(
                    "could not verify command line for browser pid {pid} before termination"
                ));
            }
            PidProfileMatch::DoesNotMatch => {
                errors.push(format!(
                    "could not confirm ownership of browser pid {pid} for listening debug port"
                ));
            }
        },
        Some(pid) => {
            ensure_profile_process_stopped_with(
                pid,
                profile,
                grace_window,
                &mut errors,
                &mut pid_exists,
                &mut verify,
                &mut kill,
            );
        }
        None => {
            errors.push("could not confirm browser process ownership: missing pid".to_string());
        }
    }

    if let Some(port) = port
        && port_open
    {
        if kill_attempted {
            let socket_deadline = Instant::now() + Duration::from_secs(1);
            while port_listening(port) && Instant::now() < socket_deadline {
                thread::sleep(POLL_INTERVAL);
            }
        }
        if port_listening(port) {
            errors.push(format!(
                "browser debug port {port} is still listening after cleanup"
            ));
        }
    }

    if errors.is_empty()
        && let Some(pid) = pid
        && let Err(err) = remove_dir(profile)
    {
        match verify(pid, profile) {
            PidProfileMatch::Matches => {
                if let Err(kill_err) = kill(pid, Some(profile)) {
                    errors.push(kill_err);
                }
                if errors.is_empty()
                    && let Err(retry_err) = remove_dir(profile)
                {
                    errors.push(retry_err);
                }
            }
            PidProfileMatch::Unknown => {
                errors.push(format!(
                    "could not verify command line for browser pid {pid} before cleanup retry"
                ));
                errors.push(err);
            }
            PidProfileMatch::DoesNotMatch => {
                errors.push(err);
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) fn verify_launched_process_before_close(
    pid: u32,
    port: u16,
    profile: &Path,
) -> Result<bool, String> {
    verify_launched_process_before_close_with(
        pid,
        port,
        profile,
        &std::env::temp_dir(),
        pid_matches_profile,
        is_port_listening,
    )
}

fn verify_launched_process_before_close_with<V, P>(
    pid: u32,
    port: u16,
    profile: &Path,
    temp_root: &Path,
    mut verify: V,
    mut port_open: P,
) -> Result<bool, String>
where
    V: FnMut(u32, &Path) -> PidProfileMatch,
    P: FnMut(u16) -> bool,
{
    if pid == 0 {
        return Err("could not verify browser pid 0 before closing".to_string());
    }
    validate_owned_profile_in(profile, temp_root)?;
    match verify(pid, profile) {
        PidProfileMatch::Matches => Ok(true),
        PidProfileMatch::Unknown => Err(format!(
            "could not verify command line for browser pid {pid} before closing port {port}"
        )),
        PidProfileMatch::DoesNotMatch => {
            if port_open(port) {
                Err(format!(
                    "refusing to close browser on port {port}: pid {pid} does not match temporary profile `{}`",
                    profile.display()
                ))
            } else {
                Ok(false)
            }
        }
    }
}

fn configure_detached(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
}

pub(crate) fn is_port_listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PidProfileMatch {
    Matches,
    DoesNotMatch,
    Unknown,
}

/// Checks whether a process `pid` is still alive and owns `--user-data-dir=<profile>`
/// after its debug port stopped listening, allowing a short grace window for a
/// clean post-`Browser.close` exit before force-killing.
fn ensure_profile_process_stopped_with<E, V, K>(
    pid: u32,
    profile: &Path,
    grace_window: Duration,
    errors: &mut Vec<String>,
    pid_exists: &mut E,
    verify: &mut V,
    kill: &mut K,
) where
    E: FnMut(u32) -> Option<bool>,
    V: FnMut(u32, &Path) -> PidProfileMatch,
    K: FnMut(u32, Option<&Path>) -> Result<(), String>,
{
    #[cfg(windows)]
    {
        match pid_exists(pid) {
            Some(false) => return,
            // Give a closing browser a brief window to exit cleanly after closing
            // its debug port so the fast `tasklist` check avoids spawning PowerShell.
            Some(true) if !grace_window.is_zero() => {
                thread::sleep(grace_window);
                if pid_exists(pid) == Some(false) {
                    return;
                }
            }
            Some(true) | None => {}
        }
        match verify(pid, profile) {
            PidProfileMatch::DoesNotMatch => {}
            PidProfileMatch::Matches => {
                if let Err(err) = kill(pid, Some(profile)) {
                    errors.push(err);
                }
            }
            PidProfileMatch::Unknown => {
                errors.push(format!(
                    "could not verify command line for surviving browser pid {pid}"
                ));
            }
        }
    }
    #[cfg(unix)]
    {
        let _ = pid_exists;
        match verify(pid, profile) {
            PidProfileMatch::DoesNotMatch => return,
            PidProfileMatch::Unknown => {
                errors.push(format!(
                    "could not verify command line for surviving browser pid {pid}"
                ));
                return;
            }
            PidProfileMatch::Matches => {}
        }
        let grace_deadline = Instant::now() + grace_window;
        while Instant::now() < grace_deadline {
            thread::sleep(Duration::from_millis(50));
            match verify(pid, profile) {
                PidProfileMatch::DoesNotMatch => return,
                PidProfileMatch::Matches => {}
                PidProfileMatch::Unknown => {
                    errors.push(format!(
                        "could not verify command line for surviving browser pid {pid}"
                    ));
                    return;
                }
            }
        }
        if let Err(err) = kill(pid, Some(profile)) {
            errors.push(err);
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (pid, profile, grace_window, errors, pid_exists, verify, kill);
    }
}

fn kill_process(pid: u32, profile: Option<&Path>) -> Result<(), String> {
    kill_process_in(
        pid,
        profile,
        &std::env::temp_dir(),
        pid_matches_profile,
        force_kill_pid,
    )
}

/// [`kill_process`] with ownership verification and termination injected.
///
/// The real [`pid_matches_profile`] refuses to signal a pid whose command line
/// does not name `profile`; that is what keeps a recycled pid safe. This seam
/// exists so a test can prove both halves of that contract without depending on
/// the host having a kill utility that fails on demand.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "test seam for ownership verification")
)]
fn kill_process_with<V, K>(
    pid: u32,
    profile: Option<&Path>,
    verify: V,
    kill: K,
) -> Result<(), String>
where
    V: FnMut(u32, &Path) -> PidProfileMatch,
    K: FnMut(u32, Option<&Path>) -> Result<(), String>,
{
    kill_process_in(pid, profile, &std::env::temp_dir(), verify, kill)
}

fn kill_process_in<V, K>(
    pid: u32,
    profile: Option<&Path>,
    temp_root: &Path,
    mut verify: V,
    mut kill: K,
) -> Result<(), String>
where
    V: FnMut(u32, &Path) -> PidProfileMatch,
    K: FnMut(u32, Option<&Path>) -> Result<(), String>,
{
    if pid == 0 {
        return Err("could not terminate browser pid 0: invalid pid".to_string());
    }
    let Some(profile) = profile else {
        return Err(format!(
            "could not verify or terminate browser pid {pid}: the session handle has no profile path"
        ));
    };
    validate_owned_profile_in(profile, temp_root)?;
    match verify(pid, profile) {
        PidProfileMatch::Matches => {}
        PidProfileMatch::DoesNotMatch => return Ok(()),
        PidProfileMatch::Unknown => {
            return Err(format!(
                "could not verify command line for browser pid {pid} before termination"
            ));
        }
    }
    kill(pid, Some(profile))
}

fn force_kill_pid(pid: u32, profile: Option<&Path>) -> Result<(), String> {
    #[cfg(windows)]
    {
        force_kill_with(
            pid,
            profile,
            Duration::from_secs(10),
            run_windows_taskkill,
            windows_pid_exists,
            pid_matches_profile,
        )
    }
    #[cfg(unix)]
    {
        force_kill_with(
            pid,
            profile,
            Duration::from_secs(2),
            run_unix_kill,
            unix_pid_exists_for_profile,
        )
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (pid, profile);
        Ok(())
    }
}

#[cfg(windows)]
fn run_windows_taskkill(args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    let taskkill = windows_system_tool(&["taskkill.exe"], "taskkill");
    Command::new(taskkill)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

/// How often the ownership probe is retried after `taskkill` before the
/// termination is reported as unverified.
#[cfg(any(windows, test))]
const OWNERSHIP_PROBE_ATTEMPTS: usize = 3;

/// Pause between ownership probes when the previous one was inconclusive.
#[cfg(any(windows, test))]
const OWNERSHIP_PROBE_INTERVAL: Duration = Duration::from_millis(150);

/// Resolves a pid's ownership after `taskkill`, retrying a probe that could not
/// read the command line (a transient PowerShell/WMI failure under load).
///
/// A definitive [`PidProfileMatch::DoesNotMatch`] proves the recorded pid now
/// belongs to another process, so the browser that owned the profile is gone.
/// An ownership that stays [`PidProfileMatch::Unknown`] is returned as such so
/// the caller can stay fail-closed.
#[cfg(any(windows, test))]
fn settle_pid_profile_match(
    pid: u32,
    profile: &Path,
    verify: &mut impl FnMut(u32, &Path) -> PidProfileMatch,
) -> PidProfileMatch {
    settle_pid_profile_match_with(pid, profile, verify, thread::sleep)
}

#[cfg(any(windows, test))]
fn settle_pid_profile_match_with(
    pid: u32,
    profile: &Path,
    verify: &mut impl FnMut(u32, &Path) -> PidProfileMatch,
    mut sleep: impl FnMut(Duration),
) -> PidProfileMatch {
    let mut last = PidProfileMatch::Unknown;
    for attempt in 0..OWNERSHIP_PROBE_ATTEMPTS {
        last = verify(pid, profile);
        if last != PidProfileMatch::Unknown {
            return last;
        }
        if attempt + 1 < OWNERSHIP_PROBE_ATTEMPTS {
            sleep(OWNERSHIP_PROBE_INTERVAL);
        }
    }
    last
}

#[cfg(windows)]
fn force_kill_with<R, E, V>(
    pid: u32,
    profile: Option<&Path>,
    wait_timeout: Duration,
    mut run_taskkill: R,
    mut pid_exists: E,
    mut verify: V,
) -> Result<(), String>
where
    R: FnMut(&[&str]) -> std::io::Result<std::process::ExitStatus>,
    E: FnMut(u32) -> Option<bool>,
    V: FnMut(u32, &Path) -> PidProfileMatch,
{
    let pid_str = pid.to_string();
    let status = run_taskkill(&["/PID", &pid_str, "/T", "/F"])
        .map_err(|error| format!("could not terminate browser pid {pid}: {error}"))?;

    // `taskkill` exit code 128 means "the process was not found": either it had
    // already exited between the ownership check and this call, or a short-lived
    // Chrome child exited mid-walk. The caller verified that this pid owned the
    // temporary profile before signalling, so there is nothing left to kill and
    // nothing of ours survives under this pid.
    if status.code() == Some(TASKKILL_PROCESS_NOT_FOUND) {
        return Ok(());
    }

    // `taskkill /T` can exit non-zero (for example 128) when a short-lived
    // Chrome child exits mid-walk even though the target process was killed,
    // and Windows kernel rundown can keep the terminated PID visible to
    // `tasklist` for several seconds. Poll until the PID disappears or times out.
    let deadline = Instant::now() + wait_timeout;
    loop {
        match pid_exists(pid) {
            Some(false) => return Ok(()),
            Some(true) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Some(true) => {
                let Some(path) = profile else {
                    return Err(format!(
                        "could not verify whether browser pid {pid} exited after taskkill (`{status}`): the handle has no profile path"
                    ));
                };
                return match settle_pid_profile_match(pid, path, &mut verify) {
                    // A recycled pid now belongs to an unrelated process; the
                    // browser that owned this profile is gone.
                    PidProfileMatch::DoesNotMatch => Ok(()),
                    PidProfileMatch::Unknown => Err(format!(
                        "could not verify whether browser pid {pid} exited after taskkill (`{status}`): its command line could not be read"
                    )),
                    PidProfileMatch::Matches => retry_kill_owned_process(
                        pid,
                        wait_timeout,
                        &status,
                        &mut run_taskkill,
                        &mut pid_exists,
                    ),
                };
            }
            None => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "could not terminate browser pid {pid} (taskkill exited with {status})"
                    ))
                };
            }
        }
    }
}

/// Terminates a process that still owns `profile` after the first `taskkill`,
/// bounded to one extra attempt and a short wait. Fail-closed: a process that
/// is still visible and still owns the profile is reported as an error.
#[cfg(windows)]
fn retry_kill_owned_process<R, E>(
    pid: u32,
    wait_timeout: Duration,
    first_status: &std::process::ExitStatus,
    run_taskkill: &mut R,
    pid_exists: &mut E,
) -> Result<(), String>
where
    R: FnMut(&[&str]) -> std::io::Result<std::process::ExitStatus>,
    E: FnMut(u32) -> Option<bool>,
{
    let pid_str = pid.to_string();
    let retry = run_taskkill(&["/PID", &pid_str, "/T", "/F"])
        .map_err(|error| format!("could not terminate browser pid {pid}: {error}"))?;
    if retry.code() == Some(TASKKILL_PROCESS_NOT_FOUND) {
        return Ok(());
    }
    let deadline = Instant::now() + wait_timeout.min(Duration::from_secs(1));
    loop {
        match pid_exists(pid) {
            Some(false) => return Ok(()),
            Some(true) if Instant::now() < deadline => thread::sleep(POLL_INTERVAL),
            Some(true) | None => {
                return Err(format!(
                    "could not terminate browser pid {pid}: process is still running after taskkill was retried (first taskkill: {first_status}; retry: {retry})"
                ));
            }
        }
    }
}

#[cfg(unix)]
fn run_unix_kill(args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    Command::new("kill")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

#[cfg(unix)]
fn unix_pid_exists_for_profile(pid: u32, profile: Option<&Path>) -> Option<bool> {
    match profile {
        Some(path) => match unix_pid_matches_profile(pid, path) {
            PidProfileMatch::Matches => Some(true),
            PidProfileMatch::DoesNotMatch => Some(false),
            PidProfileMatch::Unknown => None,
        },
        None => Some(is_unix_pid_alive(pid)),
    }
}

#[cfg(unix)]
fn force_kill_with<R, A>(
    pid: u32,
    profile: Option<&Path>,
    wait_timeout: Duration,
    mut run_kill: R,
    mut is_alive: A,
) -> Result<(), String>
where
    R: FnMut(&[&str]) -> std::io::Result<std::process::ExitStatus>,
    A: FnMut(u32, Option<&Path>) -> Option<bool>,
{
    // Signals target the browser pid itself, never `-pid`. A detached Chrome is
    // a group leader, but a handle can also point at a browser that was started
    // before that (or at a helper that never got its own group), and `kill -TERM
    // -<pid>` then either fails with ESRCH or reaches an unrelated group. The
    // per-pid form (`kill -TERM <pid>`, `kill -KILL <pid>`, `kill -0 <pid>`) is
    // the form that passed the Linux and macOS CI runs and the repeated browser
    // suite; Chrome tears down its own helper processes when the browser exits.
    let pid_str = pid.to_string();
    let term_status = run_kill(&["-TERM", &pid_str])
        .map_err(|error| format!("could not terminate browser pid {pid}: {error}"))?;
    if !term_status.success() && is_alive(pid, profile) == Some(false) {
        return Ok(());
    }

    let term_deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < term_deadline {
        if is_alive(pid, profile) == Some(false) {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }

    let kill_status = run_kill(&["-KILL", &pid_str])
        .map_err(|error| format!("could not terminate browser pid {pid}: {error}"))?;
    if !kill_status.success() {
        if is_alive(pid, profile) == Some(false) {
            return Ok(());
        }
        return Err(format!(
            "could not terminate browser pid {pid} (kill -KILL exited with {kill_status})"
        ));
    }

    let kill_deadline = Instant::now() + wait_timeout;
    loop {
        match is_alive(pid, profile) {
            Some(false) => return Ok(()),
            Some(true) if Instant::now() < kill_deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Some(true) => {
                return Err(format!(
                    "could not terminate browser pid {pid}: process is still running after SIGKILL"
                ));
            }
            None if Instant::now() < kill_deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            None => {
                return Err(format!(
                    "could not verify whether browser pid {pid} exited after SIGKILL"
                ));
            }
        }
    }
}

fn pid_matches_profile(pid: u32, profile: &Path) -> PidProfileMatch {
    #[cfg(windows)]
    {
        windows_pid_matches_profile(pid, profile)
    }
    #[cfg(unix)]
    {
        unix_pid_matches_profile(pid, profile)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (pid, profile);
        PidProfileMatch::DoesNotMatch
    }
}

fn cmdline_matches_profile(cmdline: &str, profile: &Path) -> bool {
    let needle = profile.to_string_lossy();
    if needle.is_empty() {
        return false;
    }
    #[cfg(windows)]
    let haystack_owned = cmdline.to_lowercase();
    #[cfg(windows)]
    let needle_owned = needle.to_lowercase();
    #[cfg(windows)]
    let (haystack, target) = (haystack_owned.as_str(), needle_owned.as_str());
    #[cfg(not(windows))]
    let (haystack, target) = (cmdline, needle.as_ref());

    let mut start = 0;
    while let Some(offset) = haystack[start..].find(target) {
        let match_start = start + offset;
        let end = match_start + target.len();
        let prev_is_boundary = haystack[..match_start]
            .chars()
            .next_back()
            .is_none_or(|ch| {
                ch == '=' || ch == '\0' || ch == '"' || ch == '\'' || ch.is_whitespace()
            });
        let next_is_boundary = haystack[end..]
            .chars()
            .next()
            .is_none_or(|ch| ch == '\0' || ch == '"' || ch == '\'' || ch.is_whitespace());
        if prev_is_boundary && next_is_boundary {
            return true;
        }
        let step = haystack[match_start..]
            .chars()
            .next()
            .map_or(1, char::len_utf8);
        start = match_start + step;
    }
    false
}

#[cfg(windows)]
fn windows_system_tool(parts: &[&str], fallback: &str) -> PathBuf {
    let Some(root) = std::env::var_os("SystemRoot") else {
        return PathBuf::from(fallback);
    };
    let mut path = PathBuf::from(root).join("System32");
    for part in parts {
        path = path.join(part);
    }
    if path.is_file() {
        path
    } else {
        PathBuf::from(fallback)
    }
}

#[cfg(any(windows, test))]
fn parse_tasklist_pid_exists(stdout: &str, pid: u32) -> bool {
    let pid_text = pid.to_string();
    stdout.lines().any(|line| {
        let trimmed = line.trim();
        // CSV rows start with `"<image>","<pid>",...`; locale-specific INFO messages do not.
        if !trimmed.starts_with('"') {
            return false;
        }
        let mut fields = trimmed.split("\",\"");
        let _image = fields.next();
        fields
            .next()
            .is_some_and(|field| field.trim_matches('"') == pid_text)
    })
}

#[cfg(windows)]
fn windows_pid_exists(pid: u32) -> Option<bool> {
    let tasklist = windows_system_tool(&["tasklist.exe"], "tasklist");
    let filter = format!("PID eq {pid}");
    let output = Command::new(tasklist)
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(parse_tasklist_pid_exists(&stdout, pid))
}

#[cfg(windows)]
fn windows_pid_matches_profile(pid: u32, profile: &Path) -> PidProfileMatch {
    let powershell = windows_system_tool(
        &["WindowsPowerShell", "v1.0", "powershell.exe"],
        "powershell.exe",
    );
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); \
         $p = Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}'; \
         if ($p -and $p.CommandLine) {{ [Console]::Out.Write($p.CommandLine) }}"
    );
    let Ok(output) = Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return PidProfileMatch::Unknown;
    };
    if !output.status.success() {
        return PidProfileMatch::Unknown;
    }
    let cmdline = String::from_utf8_lossy(&output.stdout);
    if cmdline_matches_profile(&cmdline, profile) {
        PidProfileMatch::Matches
    } else {
        PidProfileMatch::DoesNotMatch
    }
}

#[cfg(unix)]
fn is_unix_pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(any(unix, test))]
fn match_proc_cmdline(proc_root: &Path, pid: u32, profile: &Path) -> Option<PidProfileMatch> {
    if !proc_root.is_dir() {
        return None;
    }
    let proc_cmdline = proc_root.join(pid.to_string()).join("cmdline");
    match fs::read(&proc_cmdline) {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            Some(if cmdline_matches_profile(&text, profile) {
                PidProfileMatch::Matches
            } else {
                PidProfileMatch::DoesNotMatch
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Some(PidProfileMatch::DoesNotMatch)
        }
        Err(_) => None,
    }
}

#[cfg(unix)]
fn unix_pid_matches_profile(pid: u32, profile: &Path) -> PidProfileMatch {
    if let Some(matched) = match_proc_cmdline(Path::new("/proc"), pid, profile) {
        return matched;
    }
    if let Ok(output) = Command::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "args="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            return if cmdline_matches_profile(&text, profile) {
                PidProfileMatch::Matches
            } else {
                PidProfileMatch::DoesNotMatch
            };
        }
        if !is_unix_pid_alive(pid) {
            return PidProfileMatch::DoesNotMatch;
        }
        return PidProfileMatch::Unknown;
    }
    if !is_unix_pid_alive(pid) {
        PidProfileMatch::DoesNotMatch
    } else {
        PidProfileMatch::Unknown
    }
}

/// Maximum number of counter values tried when creating an exclusive temporary
/// browser profile directory after `AlreadyExists` collisions.
const PROFILE_CREATE_ATTEMPTS: usize = 256;

/// Creates an empty profile directory with a unique name.
fn create_profile() -> Result<PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    create_profile_in(&std::env::temp_dir(), std::process::id(), &COUNTER)
}

fn create_profile_in(temp_root: &Path, pid: u32, counter: &AtomicU64) -> Result<PathBuf, String> {
    create_profile_in_with(temp_root, pid, counter, PROFILE_CREATE_ATTEMPTS, |path| {
        fs::create_dir(path)
    })
}

fn create_profile_in_with<C>(
    temp_root: &Path,
    pid: u32,
    counter: &AtomicU64,
    max_attempts: usize,
    mut create_dir: C,
) -> Result<PathBuf, String>
where
    C: FnMut(&Path) -> std::io::Result<()>,
{
    if pid == 0 {
        return Err("could not create temporary browser profile: invalid pid 0".to_string());
    }
    if max_attempts == 0 {
        return Err(format!(
            "could not create a unique temporary browser profile under `{}`: zero attempts allowed",
            temp_root.display()
        ));
    }
    fs::create_dir_all(temp_root).map_err(|error| {
        format!(
            "could not prepare temporary directory `{}`: {error}",
            temp_root.display()
        )
    })?;
    for _ in 0..max_attempts {
        let seq = counter.fetch_add(1, Ordering::Relaxed);
        let path = temp_root.join(format!("{PROFILE_PREFIX}-{pid}-{seq}"));
        match create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not create the temporary browser profile `{}`: {error}",
                    path.display()
                ));
            }
        }
    }
    Err(format!(
        "could not create a unique temporary browser profile under `{}` after {max_attempts} attempts",
        temp_root.display()
    ))
}

/// Reads the port and browser path Chrome writes to `DevToolsActivePort`.
fn parse_devtools_active_port(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let port = lines.next()?.trim().parse::<u16>().ok()?;
    let path = lines.next()?.trim().to_string();
    if !path.starts_with('/') {
        return None;
    }
    Some((port, path))
}

/// The browser executable to start.
///
/// `DA_CHROME` wins, then the usual install locations, then the program names
/// on `PATH`. Every failure names `DA_CHROME`, because that is the one thing the
/// user can set.
fn find_browser() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("DA_CHROME")
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "`DA_CHROME` points to `{}`, which is not a file; set `DA_CHROME` to the Chrome or Chromium executable",
            path.display()
        ));
    }
    if let Some(path) = known_install_paths()
        .into_iter()
        .find(|candidate| candidate.is_file())
    {
        return Ok(path);
    }
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
    ] {
        if let Some(path) = find_on_path(name) {
            return Ok(path);
        }
    }
    Err(
        "could not find Chrome or Chromium: no `DA_CHROME`, no install location and no program on `PATH` matched; set `DA_CHROME` to the Chrome or Chromium executable"
            .to_string(),
    )
}

/// The usual per-platform install locations, in preference order.
fn known_install_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            let Some(root) = std::env::var_os(variable) else {
                continue;
            };
            let root = PathBuf::from(root);
            paths.push(root.join("Google/Chrome/Application/chrome.exe"));
            paths.push(root.join("Chromium/Application/chrome.exe"));
            paths.push(root.join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        paths.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
        paths.push(PathBuf::from(
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ));
        if let Some(home) = std::env::var_os("HOME") {
            paths.push(
                PathBuf::from(home)
                    .join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            );
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for path in [
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ] {
            paths.push(PathBuf::from(path));
        }
    }
    paths
}

/// Finds `name`, or `name.exe` on Windows, in one of the `PATH` directories.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| {
        [directory.join(name), directory.join(format!("{name}.exe"))]
            .into_iter()
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn the_debug_port_file_yields_the_port_and_the_browser_path() {
        assert_eq!(
            parse_devtools_active_port("41234\n/devtools/browser/abc-123\n"),
            Some((41234, "/devtools/browser/abc-123".to_string()))
        );
        assert_eq!(
            parse_devtools_active_port("\n 41234 \r\n/devtools/browser/abc\r\n"),
            Some((41234, "/devtools/browser/abc".to_string()))
        );
    }

    #[test]
    fn an_incomplete_debug_port_file_is_ignored() {
        for text in [
            "",
            "41234\n",
            "not-a-port\n/devtools/browser/x\n",
            "1\nnot-a-path\n",
        ] {
            assert_eq!(
                parse_devtools_active_port(text),
                None,
                "`{text}` must not name a usable port"
            );
        }
    }

    #[test]
    fn every_platform_checks_an_install_path() {
        assert!(
            !known_install_paths().is_empty(),
            "a browser install location must be checked on every supported platform"
        );
    }

    #[test]
    fn a_headless_launch_turns_gpu_acceleration_off_and_a_visible_one_keeps_it() {
        let profile = Path::new("profile");
        let headless = launch_args(true, 1280, 800, profile);
        let visible = launch_args(false, 1280, 800, profile);

        assert!(
            headless.iter().any(|arg| arg == "--headless=new"),
            "a headless launch must ask for the new headless mode: {headless:?}"
        );
        assert!(
            headless.iter().any(|arg| arg == "--disable-gpu"),
            "a headless launch must composite in software: {headless:?}"
        );
        assert!(
            !visible.iter().any(|arg| arg == "--disable-gpu"),
            "a visible window must keep its GPU: {visible:?}"
        );
    }

    #[test]
    fn a_launch_keeps_the_system_proxy() {
        let profile = Path::new("profile");
        for headless in [true, false] {
            let args = launch_args(headless, 1280, 800, profile);
            for forbidden in ["--no-proxy-server", "--proxy-server", "--proxy-pac-url"] {
                assert!(
                    !args.iter().any(|arg| arg.starts_with(forbidden)),
                    "`{forbidden}` would break a case behind a corporate proxy: {args:?}"
                );
            }
            for required in [
                "--user-data-dir=profile",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--window-size=1280,800",
            ] {
                assert!(
                    args.iter().any(|arg| arg == required),
                    "a launch must keep `{required}`: {args:?}"
                );
            }
        }
    }

    #[test]
    fn detach_clears_all_owned_fields_and_skips_release_on_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind dummy debug listener");
        let port = listener.local_addr().expect("listener addr").port();
        let profile = create_profile().expect("create temp profile");
        let exe = std::env::current_exe().expect("current test binary");
        let mut child = Command::new(exe)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived child");
        let child_pid = child.id();
        let _ = child.wait();

        // Populate both `child` and `detached_pid` to cover the Unix bug where
        // leaving `detached_pid` populated triggered Drop::release().
        let launched = Launched {
            child: Some(child),
            detached_pid: Some(child_pid),
            profile: Some(profile.clone()),
            port,
            websocket_url: format!("ws://127.0.0.1:{port}/devtools/browser/test"),
        };

        // Sanity-check that Drop would run release() before detach().
        assert!(
            launched.child.is_some()
                && launched.detached_pid.is_some()
                && launched.profile.is_some()
        );

        let started = Instant::now();
        let detached = launched.detach().expect("detach returns handle");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "detach must not run Drop::release() and wait on the listening port"
        );
        assert_eq!(detached.pid, child_pid);
        assert_eq!(detached.port, port);
        assert_eq!(detached.profile, profile);
        assert!(
            profile.is_dir(),
            "detach must leave the profile directory intact on disk"
        );

        // Also check the pipe-isolated path (`child: None, detached_pid: Some(...)`).
        let launched_isolated = Launched {
            child: None,
            detached_pid: Some(child_pid),
            profile: Some(profile.clone()),
            port,
            websocket_url: format!("ws://127.0.0.1:{port}/devtools/browser/test"),
        };
        let started_isolated = Instant::now();
        let detached_isolated = launched_isolated
            .detach()
            .expect("detach returns handle for detached_pid-only state");
        assert!(started_isolated.elapsed() < Duration::from_secs(2));
        assert_eq!(detached_isolated.pid, child_pid);
        assert!(profile.is_dir());

        // Incomplete state returns None and still cleans up the remaining profile on Drop.
        let incomplete = Launched {
            child: None,
            detached_pid: None,
            profile: Some(profile.clone()),
            port,
            websocket_url: format!("ws://127.0.0.1:{port}/devtools/browser/test"),
        };
        assert!(incomplete.detach().is_none());
        assert!(
            !profile.exists(),
            "failed detach must leave cleanup to Drop::release()"
        );

        drop(listener);
        let _ = fs::remove_dir_all(&profile);
    }

    fn spawn_sleeping_process_for_profile(profile: Option<&Path>) -> Child {
        #[cfg(windows)]
        {
            let powershell = windows_system_tool(
                &["WindowsPowerShell", "v1.0", "powershell.exe"],
                "powershell.exe",
            );
            let script = match profile {
                Some(path) => format!(
                    "Start-Sleep -Seconds 30 # --user-data-dir={}",
                    path.display()
                ),
                None => "Start-Sleep -Seconds 30".to_string(),
            };
            Command::new(powershell)
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sleeping powershell helper")
        }
        #[cfg(unix)]
        {
            let mut cmd = Command::new("sh");
            cmd.args([
                "-c",
                "i=0; while [ $i -lt 30 ]; do sleep 1; i=$((i+1)); done",
                "_",
            ]);
            if let Some(path) = profile {
                cmd.arg(format!("--user-data-dir={}", path.display()));
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn sleeping sh helper")
        }
    }

    fn synthetic_exit_status(code: u32) -> std::process::ExitStatus {
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(code)
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            let raw = i32::try_from(code << 8).unwrap_or(1 << 8);
            std::process::ExitStatus::from_raw(raw)
        }
    }

    fn force_kill_with_runner<R>(pid: u32, profile: Option<&Path>, runner: R) -> Result<(), String>
    where
        R: FnMut(&[&str]) -> std::io::Result<std::process::ExitStatus>,
    {
        #[cfg(windows)]
        {
            force_kill_with(
                pid,
                profile,
                Duration::from_millis(200),
                runner,
                windows_pid_exists,
                pid_matches_profile,
            )
        }
        #[cfg(unix)]
        {
            force_kill_with(
                pid,
                profile,
                Duration::from_millis(200),
                runner,
                unix_pid_exists_for_profile,
            )
        }
    }

    #[test]
    fn cmdline_matches_profile_respects_boundaries() {
        let profile = Path::new("/tmp/da-chrome-profile-10-0");
        assert!(cmdline_matches_profile(
            "chrome --user-data-dir=/tmp/da-chrome-profile-10-0 --headless=new",
            profile
        ));
        assert!(cmdline_matches_profile(
            "chrome\0--user-data-dir=/tmp/da-chrome-profile-10-0\0--headless=new",
            profile
        ));
        assert!(cmdline_matches_profile(
            "chrome \"--user-data-dir=/tmp/da-chrome-profile-10-0\"",
            profile
        ));
        assert!(!cmdline_matches_profile(
            "chrome --user-data-dir=/tmp/da-chrome-profile-10-00",
            profile
        ));
        assert!(!cmdline_matches_profile(
            "chrome --user-data-dir=/tmp/da-chrome-profile-10-0/sub",
            profile
        ));
    }

    #[test]
    fn parse_tasklist_pid_exists_handles_csv_and_localized_info() {
        assert!(parse_tasklist_pid_exists(
            "\"chrome.exe\",\"4242\",\"Console\",\"1\",\"70,328 K\"\r\n",
            4242
        ));
        assert!(!parse_tasklist_pid_exists(
            "INFO: No tasks are running which match the specified criteria.\r\n",
            4242
        ));
        assert!(!parse_tasklist_pid_exists(
            "\"chrome.exe\",\"42420\",\"Console\",\"1\",\"70,328 K\"\r\n",
            4242
        ));
    }

    #[test]
    fn dispose_detached_kills_live_process_when_debug_port_is_closed() {
        let profile = create_profile().expect("create temp profile");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind dummy port");
        let closed_port = listener.local_addr().expect("addr").port();
        drop(listener);

        let mut child = spawn_sleeping_process_for_profile(Some(&profile));
        let pid = child.id();
        // Let the helper initialize briefly so its command line is visible to OS tools.
        thread::sleep(Duration::from_millis(300));

        let result = dispose_detached(Some(pid), Some(closed_port), Some(&profile));
        let exited = wait_for_exit(&mut child, Duration::from_secs(2));
        if exited.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&profile);

        assert!(result.is_ok(), "dispose_detached failed: {result:?}");
        assert!(
            exited.is_ok(),
            "live process pid {pid} must be terminated even when debug port {closed_port} is closed"
        );
        assert!(!profile.exists(), "temp profile must be removed");
    }

    #[test]
    fn dispose_detached_leaves_recycled_mismatched_pid_untouched() {
        let profile = create_profile().expect("create temp profile");
        let other_profile = create_profile().expect("create unrelated profile");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind dummy port");
        let closed_port = listener.local_addr().expect("addr").port();
        drop(listener);

        let mut unrelated = spawn_sleeping_process_for_profile(Some(&other_profile));
        let unrelated_pid = unrelated.id();
        thread::sleep(Duration::from_millis(300));

        let result = dispose_detached(Some(unrelated_pid), Some(closed_port), Some(&profile));
        let still_running = matches!(unrelated.try_wait(), Ok(None));
        let _ = unrelated.kill();
        let _ = unrelated.wait();
        let _ = fs::remove_dir_all(&profile);
        let _ = fs::remove_dir_all(&other_profile);

        assert!(
            result.is_ok(),
            "dispose_detached should ignore recycled mismatched pid: {result:?}"
        );
        assert!(
            still_running,
            "unrelated process pid {unrelated_pid} must not be killed"
        );
        assert!(!profile.exists(), "target temp profile must be removed");
    }

    #[test]
    fn dispose_detached_succeeds_when_process_already_exited() {
        let profile = create_profile().expect("create temp profile");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind dummy port");
        let closed_port = listener.local_addr().expect("addr").port();
        drop(listener);

        let exe = std::env::current_exe().expect("current test binary");
        let mut child = Command::new(exe)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived process");
        let dead_pid = child.id();
        let _ = child.wait();

        let result = dispose_detached(Some(dead_pid), Some(closed_port), Some(&profile));
        let _ = fs::remove_dir_all(&profile);

        assert!(
            result.is_ok(),
            "already-dead pid should dispose cleanly: {result:?}"
        );
        assert!(!profile.exists(), "temp profile must be removed");
    }

    #[test]
    fn force_kill_returns_error_when_kill_command_fails_on_live_process() {
        let profile = create_profile().expect("create temp profile");
        let mut child = spawn_sleeping_process_for_profile(Some(&profile));
        let pid = child.id();
        thread::sleep(Duration::from_millis(300));

        let result =
            force_kill_with_runner(pid, Some(&profile), |_args| Ok(synthetic_exit_status(1)));
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&profile);

        let err = result.expect_err("failed kill on live process must return Err");
        assert!(
            err.contains(&pid.to_string()),
            "error must name surviving pid {pid}: {err}"
        );
    }

    #[test]
    fn force_kill_returns_error_when_process_survives_zero_exit_kill() {
        let profile = create_profile().expect("create temp profile");
        let mut child = spawn_sleeping_process_for_profile(Some(&profile));
        let pid = child.id();
        thread::sleep(Duration::from_millis(300));

        let result =
            force_kill_with_runner(pid, Some(&profile), |_args| Ok(synthetic_exit_status(0)));
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&profile);

        let err = result.expect_err("surviving live process must return Err even after exit 0");
        assert!(
            err.contains(&pid.to_string()),
            "error must name surviving pid {pid}: {err}"
        );
    }

    #[test]
    fn force_kill_succeeds_when_kill_command_fails_because_process_already_exited() {
        let exe = std::env::current_exe().expect("current test binary");
        let mut child = Command::new(exe)
            .arg("--help")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived process");
        let dead_pid = child.id();
        let _ = child.wait();

        let result = force_kill_with_runner(dead_pid, None, |_args| Ok(synthetic_exit_status(128)));
        assert!(
            result.is_ok(),
            "non-zero kill exit when process is already gone must succeed: {result:?}"
        );
    }

    #[test]
    fn ownership_probe_retries_transient_unknown_and_keeps_a_final_unknown() {
        let profile = Path::new("/tmp/da-chrome-profile-4242-0");

        let mut calls = 0;
        let mut sleeps = 0;
        let mut sequence = [PidProfileMatch::Unknown, PidProfileMatch::DoesNotMatch].into_iter();
        let settled = settle_pid_profile_match_with(
            4242,
            profile,
            &mut |_, _| {
                calls += 1;
                sequence.next().unwrap_or(PidProfileMatch::DoesNotMatch)
            },
            |_| sleeps += 1,
        );
        assert_eq!(settled, PidProfileMatch::DoesNotMatch);
        assert_eq!(calls, 2, "a transient Unknown must be probed again");
        assert_eq!(sleeps, 1);

        let mut calls = 0;
        let mut sleeps = 0;
        let exhausted = settle_pid_profile_match_with(
            4242,
            profile,
            &mut |_, _| {
                calls += 1;
                PidProfileMatch::Unknown
            },
            |_| sleeps += 1,
        );
        assert_eq!(
            exhausted,
            PidProfileMatch::Unknown,
            "an ownership that stays unreadable must be reported as Unknown"
        );
        assert_eq!(calls, OWNERSHIP_PROBE_ATTEMPTS);
        assert_eq!(sleeps, OWNERSHIP_PROBE_ATTEMPTS - 1);

        let mut calls = 0;
        let mut sleeps = 0;
        let matched = settle_pid_profile_match_with(
            4242,
            profile,
            &mut |_, _| {
                calls += 1;
                PidProfileMatch::Matches
            },
            |_| sleeps += 1,
        );
        assert_eq!(matched, PidProfileMatch::Matches);
        assert_eq!(calls, 1, "a definitive answer must not be probed again");
        assert_eq!(sleeps, 0);
    }

    #[cfg(windows)]
    #[test]
    fn force_kill_treats_taskkill_not_found_as_terminated_without_a_pid_probe() {
        let profile = create_profile().expect("create temp profile");
        let result = force_kill_with(
            4242,
            Some(&profile),
            Duration::from_millis(50),
            |_| Ok(synthetic_exit_status(128)),
            |_| None,
            |_, _| panic!("a 128 result proves the process is gone; no probe is needed"),
        );
        let _ = fs::remove_dir_all(&profile);

        assert!(
            result.is_ok(),
            "taskkill exit 128 means the verified process is already gone: {result:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn force_kill_accepts_a_visible_pid_that_no_longer_owns_the_profile() {
        let profile = create_profile().expect("create temp profile");
        let result = force_kill_with(
            4242,
            Some(&profile),
            Duration::from_millis(50),
            |_| Ok(synthetic_exit_status(0)),
            |_| Some(true),
            |_, _| PidProfileMatch::DoesNotMatch,
        );
        let _ = fs::remove_dir_all(&profile);

        assert!(
            result.is_ok(),
            "a recycled pid that no longer owns the profile must not be reported as our browser: {result:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn force_kill_stays_fail_closed_when_ownership_cannot_be_read_after_kill() {
        let profile = create_profile().expect("create temp profile");
        let result = force_kill_with(
            4242,
            Some(&profile),
            Duration::from_millis(50),
            |_| Ok(synthetic_exit_status(0)),
            |_| Some(true),
            |_, _| PidProfileMatch::Unknown,
        );
        let _ = fs::remove_dir_all(&profile);

        let err = result.expect_err("unverifiable ownership must stay an error");
        assert!(
            err.contains("could not verify whether browser pid 4242 exited after taskkill"),
            "error must name the unverified pid instead of claiming it runs: {err}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn force_kill_retries_once_when_the_owned_process_is_slow_to_exit() {
        let profile = create_profile().expect("create temp profile");
        let mut child = spawn_sleeping_process_for_profile(Some(&profile));
        let pid = child.id();
        thread::sleep(Duration::from_millis(300));

        let mut attempts = 0;
        let result = force_kill_with(
            pid,
            Some(&profile),
            Duration::from_millis(100),
            |_| {
                attempts += 1;
                Ok(synthetic_exit_status(if attempts == 1 { 0 } else { 128 }))
            },
            |_| Some(true),
            |_, _| PidProfileMatch::Matches,
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_dir_all(&profile);

        assert!(
            result.is_ok(),
            "a second taskkill that reports the process gone must succeed: {result:?}"
        );
        assert_eq!(
            attempts, 2,
            "the owned process must be signalled again once"
        );
    }

    #[test]
    fn dispose_detached_returns_error_when_debug_port_remains_open() {
        let profile = create_profile().expect("create temp profile");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind live port");
        let open_port = listener.local_addr().expect("addr").port();

        let result = dispose_detached_with(
            None,
            Some(open_port),
            Some(&profile),
            Duration::from_millis(150),
        );
        drop(listener);
        let _ = fs::remove_dir_all(&profile);

        let err = result.expect_err("open debug port after cleanup must fail");
        assert!(
            err.contains(&open_port.to_string()),
            "error must name open debug port {open_port}: {err}"
        );
    }

    #[test]
    fn dispose_detached_propagates_termination_error_and_reports_surviving_port() {
        let profile = create_profile().expect("create temp profile");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind live port");
        let open_port = listener.local_addr().expect("addr").port();

        let mut child = spawn_sleeping_process_for_profile(Some(&profile));
        let pid = child.id();
        thread::sleep(Duration::from_millis(300));

        let kill_error = format!("could not terminate browser pid {pid}: kill -KILL exited with 1");
        let result = dispose_detached_with_kill(
            Some(pid),
            Some(open_port),
            Some(&profile),
            Duration::from_millis(100),
            |_, _| Err(kill_error.clone()),
        );
        let still_running = matches!(child.try_wait(), Ok(None));
        let _ = child.kill();
        let _ = child.wait();
        drop(listener);
        let _ = fs::remove_dir_all(&profile);

        let err = result.expect_err("a failing kill must never be reported as success");
        assert!(
            err.contains(&kill_error),
            "termination error must reach the caller: {err}"
        );
        assert!(
            err.contains(&open_port.to_string()),
            "surviving debug port must be reported too: {err}"
        );
        assert!(
            still_running,
            "the injected kill did not run, so the helper must still be alive"
        );
    }

    #[test]
    fn dispose_detached_refuses_unverified_pid_without_profile() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind dummy port");
        let closed_port = listener.local_addr().expect("addr").port();
        drop(listener);

        let mut child = spawn_sleeping_process_for_profile(None);
        let pid = child.id();
        thread::sleep(Duration::from_millis(300));

        let result = dispose_detached_with(
            Some(pid),
            Some(closed_port),
            None,
            Duration::from_millis(100),
        );
        let still_running = matches!(child.try_wait(), Ok(None));
        let _ = child.kill();
        let _ = child.wait();

        let err = result.expect_err("an unverifiable pid must not be reported as cleaned up");
        assert!(
            err.contains(&pid.to_string()),
            "error must name the unverified pid {pid}: {err}"
        );
        assert!(
            still_running,
            "an unverified pid must not be killed because it may have been recycled"
        );
    }

    #[test]
    fn kill_process_propagates_non_zero_termination_errors() {
        let profile = create_profile().expect("create temp profile");
        let mut calls = 0;
        let result = kill_process_with(
            4242,
            Some(&profile),
            |_, _| PidProfileMatch::Matches,
            |_, _| {
                calls += 1;
                Err("could not terminate browser pid 4242 (taskkill exited with 1)".to_string())
            },
        );
        let _ = fs::remove_dir_all(&profile);

        let err = result.expect_err("termination failure must propagate through kill_process");
        assert!(
            err.contains("taskkill exited with 1"),
            "termination error must be preserved: {err}"
        );
        assert_eq!(calls, 1, "the kill must be attempted exactly once");
    }

    #[test]
    fn kill_process_refuses_unknown_ownership_and_skips_the_kill() {
        let profile = create_profile().expect("create temp profile");
        let mut calls = 0;
        let unknown = kill_process_with(
            4242,
            Some(&profile),
            |_, _| PidProfileMatch::Unknown,
            |_, _| {
                calls += 1;
                Ok(())
            },
        );
        assert!(
            unknown.is_err(),
            "an unverifiable command line must be an error"
        );
        assert_eq!(calls, 0, "an unverified pid must not be signalled");

        let recycled = kill_process_with(
            4242,
            Some(&profile),
            |_, _| PidProfileMatch::DoesNotMatch,
            |_, _| {
                calls += 1;
                Ok(())
            },
        );
        let _ = fs::remove_dir_all(&profile);
        assert!(
            recycled.is_ok(),
            "a recycled pid that does not own the profile is a no-op"
        );
        assert_eq!(calls, 0, "a mismatched pid must not be signalled");
    }

    #[test]
    fn match_proc_cmdline_falls_back_when_proc_is_absent_and_checks_when_present() {
        let root = create_profile().expect("create temp proc root");
        let missing_proc = root.join("missing-proc");
        let profile = Path::new("/tmp/da-chrome-profile-42-0");

        // On systems without /proc (such as macOS), return None so callers fall back to `ps -ww`.
        assert_eq!(match_proc_cmdline(&missing_proc, 123, profile), None);

        // When /proc exists and `<pid>/cmdline` is missing, the process is gone.
        let proc_root = root.join("proc");
        fs::create_dir_all(&proc_root).expect("create proc root");
        assert_eq!(
            match_proc_cmdline(&proc_root, 123, profile),
            Some(PidProfileMatch::DoesNotMatch)
        );

        let pid_dir = proc_root.join("123");
        fs::create_dir_all(&pid_dir).expect("create pid dir");
        fs::write(
            pid_dir.join("cmdline"),
            b"/usr/bin/chrome\0--user-data-dir=/tmp/da-chrome-profile-42-0\0",
        )
        .expect("write matching cmdline");
        assert_eq!(
            match_proc_cmdline(&proc_root, 123, profile),
            Some(PidProfileMatch::Matches)
        );

        fs::write(
            pid_dir.join("cmdline"),
            b"/usr/bin/chrome\0--user-data-dir=/tmp/da-chrome-profile-42-00\0",
        )
        .expect("write mismatched cmdline");
        assert_eq!(
            match_proc_cmdline(&proc_root, 123, profile),
            Some(PidProfileMatch::DoesNotMatch)
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn validate_owned_profile_accepts_only_direct_owned_temp_children() {
        let temp_root = std::env::temp_dir();
        let valid = temp_root.join(format!("{PROFILE_PREFIX}-1234-0"));
        assert!(
            validate_owned_profile_in(&valid, &temp_root).is_ok(),
            "direct da profile child must pass: {valid:?}"
        );

        let relative = PathBuf::from(format!("{PROFILE_PREFIX}-1234-0"));
        let nested = valid.join("sub");
        let traversal = temp_root
            .join(format!("{PROFILE_PREFIX}-1234-0"))
            .join("..")
            .join("victim");
        let dot_suffix = PathBuf::from(format!("{}/.", valid.display()));
        let slash_suffix = PathBuf::from(format!("{}/", valid.display()));
        let wrong_prefix = temp_root.join("other-dir-1234-0");
        let zero_pid = temp_root.join(format!("{PROFILE_PREFIX}-0-0"));
        let missing_seq = temp_root.join(format!("{PROFILE_PREFIX}-1234"));
        let extra_suffix = temp_root.join(format!("{PROFILE_PREFIX}-1234-0-extra"));
        let non_numeric = temp_root.join(format!("{PROFILE_PREFIX}-abc-0"));
        let outside = temp_root
            .parent()
            .unwrap_or(&temp_root)
            .join(format!("{PROFILE_PREFIX}-1234-0"));

        for invalid in [
            Path::new(""),
            relative.as_path(),
            temp_root.as_path(),
            nested.as_path(),
            traversal.as_path(),
            dot_suffix.as_path(),
            slash_suffix.as_path(),
            wrong_prefix.as_path(),
            zero_pid.as_path(),
            missing_seq.as_path(),
            extra_suffix.as_path(),
            non_numeric.as_path(),
            outside.as_path(),
        ] {
            assert!(
                validate_owned_profile_in(invalid, &temp_root).is_err(),
                "unowned or non-canonical profile `{}` must be rejected",
                invalid.display()
            );
        }
    }

    #[test]
    fn remove_profile_refuses_invalid_paths_and_symlinks_without_removing() {
        let temp_root = std::env::temp_dir();
        let valid = temp_root.join(format!("{PROFILE_PREFIX}-4321-7"));

        let mut inspect_calls = 0usize;
        let mut remove_calls = 0usize;
        for invalid in [
            temp_root.as_path(),
            Path::new("relative-dir"),
            &temp_root.join("unrelated-dir"),
            &valid.join("..").join("unrelated"),
        ] {
            let err = remove_profile_with(
                invalid,
                &temp_root,
                |_| {
                    inspect_calls += 1;
                    Ok(ProfileEntry::Directory)
                },
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .expect_err("unowned profile must be rejected before inspection or deletion");
            assert!(
                err.contains("refusing") || err.contains("empty"),
                "unexpected error: {err}"
            );
        }
        assert_eq!(inspect_calls, 0, "invalid paths must not be inspected");
        assert_eq!(
            remove_calls, 0,
            "invalid paths must never reach remove_dir_all"
        );

        let symlink_err = remove_profile_with(
            &valid,
            &temp_root,
            |_| Ok(ProfileEntry::Unsafe),
            |_| {
                remove_calls += 1;
                Ok(())
            },
        )
        .expect_err("symlinked or non-directory profile must be refused");
        assert!(symlink_err.contains("refusing"), "{symlink_err}");
        assert_eq!(remove_calls, 0, "symlinks must never reach remove_dir_all");

        assert!(
            remove_profile_with(
                &valid,
                &temp_root,
                |_| Ok(ProfileEntry::Missing),
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(remove_calls, 0);

        assert!(
            remove_profile_with(
                &valid,
                &temp_root,
                |_| Ok(ProfileEntry::Directory),
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(remove_calls, 1);
    }

    #[test]
    fn kill_process_refuses_missing_or_unowned_profile_and_zero_pid_without_signaling() {
        let temp_root = std::env::temp_dir();
        let owned = temp_root.join(format!("{PROFILE_PREFIX}-4242-0"));
        let unowned = temp_root.join("not-da-profile");

        let mut verify_calls = 0usize;
        let mut kill_calls = 0usize;

        for (pid, profile) in [
            (0, Some(owned.as_path())),
            (4242, None),
            (4242, Some(unowned.as_path())),
        ] {
            let result = kill_process_in(
                pid,
                profile,
                &temp_root,
                |_, _| {
                    verify_calls += 1;
                    PidProfileMatch::Matches
                },
                |_, _| {
                    kill_calls += 1;
                    Ok(())
                },
            );
            assert!(
                result.is_err(),
                "expected refusal for pid={pid}, profile={profile:?}"
            );
        }
        assert_eq!(verify_calls, 0, "invalid inputs must not reach verify");
        assert_eq!(
            kill_calls, 0,
            "invalid inputs must never signal any process"
        );
    }

    #[test]
    fn dispose_detached_refuses_open_port_without_profile_and_skips_kill_and_remove() {
        let temp_root = std::env::temp_dir();
        let mut kill_calls = 0usize;
        let mut remove_calls = 0usize;
        let mut verify_calls = 0usize;

        let err = dispose_detached_with_ops(
            Some(4242),
            Some(9222),
            None,
            &temp_root,
            Duration::ZERO,
            Duration::ZERO,
            |_| true,
            |_| Some(true),
            |_, _| {
                verify_calls += 1;
                PidProfileMatch::Matches
            },
            |_, _| {
                kill_calls += 1;
                Ok(())
            },
            |_| {
                remove_calls += 1;
                Ok(())
            },
        )
        .expect_err("open port without profile must be rejected");
        assert!(err.contains("4242"), "{err}");
        assert_eq!(verify_calls, 0);
        assert_eq!(
            kill_calls, 0,
            "listening port alone must never authorize kill"
        );
        assert_eq!(remove_calls, 0);
    }

    #[test]
    fn dispose_detached_skips_profile_removal_when_ownership_unconfirmed_or_cleanup_fails() {
        let temp_root = std::env::temp_dir();
        let owned = temp_root.join(format!("{PROFILE_PREFIX}-5555-1"));

        // 1. Missing pid (even with closed port and valid profile): no removal.
        let mut remove_calls = 0usize;
        let mut kill_calls = 0usize;
        assert!(
            dispose_detached_with_ops(
                None,
                Some(9222),
                Some(&owned),
                &temp_root,
                Duration::ZERO,
                Duration::ZERO,
                |_| false,
                |_| Some(false),
                |_, _| PidProfileMatch::DoesNotMatch,
                |_, _| {
                    kill_calls += 1;
                    Ok(())
                },
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(kill_calls, 0);
        assert_eq!(remove_calls, 0);

        // 2. Open port + mismatched pid: no kill and no removal.
        assert!(
            dispose_detached_with_ops(
                Some(4242),
                Some(9222),
                Some(&owned),
                &temp_root,
                Duration::ZERO,
                Duration::ZERO,
                |_| true,
                |_| Some(true),
                |_, _| PidProfileMatch::DoesNotMatch,
                |_, _| {
                    kill_calls += 1;
                    Ok(())
                },
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(kill_calls, 0);
        assert_eq!(remove_calls, 0);

        // 3. Closed port + Unknown ownership: no kill and no removal.
        assert!(
            dispose_detached_with_ops(
                Some(4242),
                Some(9222),
                Some(&owned),
                &temp_root,
                Duration::ZERO,
                Duration::ZERO,
                |_| false,
                |_| Some(true),
                |_, _| PidProfileMatch::Unknown,
                |_, _| {
                    kill_calls += 1;
                    Ok(())
                },
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(kill_calls, 0);
        assert_eq!(remove_calls, 0);

        // 4. Open port + Matches + kill fails: no removal.
        assert!(
            dispose_detached_with_ops(
                Some(4242),
                Some(9222),
                Some(&owned),
                &temp_root,
                Duration::ZERO,
                Duration::ZERO,
                |_| true,
                |_| Some(true),
                |_, _| PidProfileMatch::Matches,
                |_, _| {
                    kill_calls += 1;
                    Err("kill failed".to_string())
                },
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(kill_calls, 1);
        assert_eq!(remove_calls, 0);

        // 5. Closed port + live unrelated (recycled) pid (DoesNotMatch): no kill, removes owned profile.
        kill_calls = 0;
        remove_calls = 0;
        assert!(
            dispose_detached_with_ops(
                Some(4242),
                Some(9222),
                Some(&owned),
                &temp_root,
                Duration::ZERO,
                Duration::ZERO,
                |_| false,
                |_| Some(true),
                |_, _| PidProfileMatch::DoesNotMatch,
                |_, _| {
                    kill_calls += 1;
                    Ok(())
                },
                |_| {
                    remove_calls += 1;
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(kill_calls, 0);
        assert_eq!(remove_calls, 1);
    }

    #[test]
    fn verify_launched_process_before_close_requires_matching_ownership_on_open_port() {
        let temp_root = std::env::temp_dir();
        let owned = temp_root.join(format!("{PROFILE_PREFIX}-5555-2"));

        assert_eq!(
            verify_launched_process_before_close_with(
                4242,
                9222,
                &owned,
                &temp_root,
                |_, _| PidProfileMatch::Matches,
                |_| true,
            ),
            Ok(true)
        );
        assert!(
            verify_launched_process_before_close_with(
                4242,
                9222,
                &owned,
                &temp_root,
                |_, _| PidProfileMatch::Unknown,
                |_| true,
            )
            .is_err()
        );
        let mismatch_open = verify_launched_process_before_close_with(
            4242,
            9222,
            &owned,
            &temp_root,
            |_, _| PidProfileMatch::DoesNotMatch,
            |_| true,
        )
        .expect_err("listening port with mismatched pid must refuse Browser.close");
        assert!(mismatch_open.contains("9222") && mismatch_open.contains("4242"));

        assert_eq!(
            verify_launched_process_before_close_with(
                4242,
                9222,
                &owned,
                &temp_root,
                |_, _| PidProfileMatch::DoesNotMatch,
                |_| false,
            ),
            Ok(false)
        );
    }

    struct IsolatedProfileFixture {
        path: PathBuf,
    }

    impl IsolatedProfileFixture {
        fn new(label: &str) -> Self {
            static FIXTURE_COUNTER: AtomicU64 = AtomicU64::new(0);
            let base = std::env::temp_dir();
            fs::create_dir_all(&base).expect("create base temp directory");
            let pid = std::process::id();
            for _ in 0..PROFILE_CREATE_ATTEMPTS {
                let seq = FIXTURE_COUNTER.fetch_add(1, Ordering::Relaxed);
                let candidate = base.join(format!("da-launch-fixture-{label}-{pid}-{seq}"));
                match fs::create_dir(&candidate) {
                    Ok(()) => return Self { path: candidate },
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        panic!(
                            "could not create isolated fixture `{}`: {error}",
                            candidate.display()
                        );
                    }
                }
            }
            panic!("could not allocate isolated fixture `{label}`");
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for IsolatedProfileFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn create_profile_retries_after_already_exists_and_protects_existing_entries() {
        let fixture = IsolatedProfileFixture::new("collision");
        let pid = 42420;
        let counter = AtomicU64::new(0);

        let dir_collision = fixture.path().join(format!("{PROFILE_PREFIX}-{pid}-0"));
        let nested_dir = dir_collision.join("nested");
        fs::create_dir_all(&nested_dir).expect("create colliding directory");
        let dir_sentinel = nested_dir.join("sentinel.txt");
        fs::write(&dir_sentinel, "keep-existing-dir").expect("write dir sentinel");

        let file_collision = fixture.path().join(format!("{PROFILE_PREFIX}-{pid}-1"));
        fs::write(&file_collision, "keep-existing-file").expect("write colliding file");

        let empty_dir_collision = fixture.path().join(format!("{PROFILE_PREFIX}-{pid}-2"));
        fs::create_dir(&empty_dir_collision).expect("create colliding empty directory");

        let created = create_profile_in(fixture.path(), pid, &counter)
            .expect("collision on -0..-2 must advance to -3");
        assert_eq!(
            created,
            fixture.path().join(format!("{PROFILE_PREFIX}-{pid}-3"))
        );
        assert!(created.is_dir(), "created profile must be a directory");
        assert_eq!(
            fs::read_dir(&created)
                .expect("read created profile")
                .count(),
            0,
            "created profile must be empty"
        );
        assert!(
            validate_owned_profile_in(&created, fixture.path()).is_ok(),
            "created profile must satisfy ownership validation"
        );
        assert_eq!(counter.load(Ordering::Relaxed), 4);

        let next = create_profile_in(fixture.path(), pid, &counter)
            .expect("subsequent creation must advance to -4");
        assert_eq!(
            next,
            fixture.path().join(format!("{PROFILE_PREFIX}-{pid}-4"))
        );
        assert_eq!(counter.load(Ordering::Relaxed), 5);

        // Removing the newly created profile must leave colliding entries untouched.
        remove_profile_with(&created, fixture.path(), inspect_profile_entry, |path| {
            fs::remove_dir_all(path)
        })
        .expect("remove newly created profile");
        assert!(!created.exists());
        assert_eq!(
            fs::read_to_string(&dir_sentinel).expect("dir sentinel must remain intact"),
            "keep-existing-dir"
        );
        assert!(file_collision.is_file());
        assert_eq!(
            fs::read_to_string(&file_collision).expect("file sentinel must remain intact"),
            "keep-existing-file"
        );
        assert!(empty_dir_collision.is_dir());
    }

    #[test]
    fn create_profile_fails_after_finite_limit_on_repeated_already_exists() {
        let fixture = IsolatedProfileFixture::new("finite-limit");
        let pid = 7777;
        let counter = AtomicU64::new(0);

        for seq in 0..PROFILE_CREATE_ATTEMPTS {
            let entry = fixture.path().join(format!("{PROFILE_PREFIX}-{pid}-{seq}"));
            fs::create_dir(&entry).expect("create colliding dir");
        }
        let sentinel = fixture
            .path()
            .join(format!("{PROFILE_PREFIX}-{pid}-0"))
            .join("keep.txt");
        fs::write(&sentinel, "do-not-delete").expect("write sentinel");

        let err = create_profile_in(fixture.path(), pid, &counter)
            .expect_err("exhausting finite retry limit must fail");
        assert!(
            err.contains(&PROFILE_CREATE_ATTEMPTS.to_string()),
            "error must report finite attempt limit {PROFILE_CREATE_ATTEMPTS}: {err}"
        );
        assert_eq!(
            counter.load(Ordering::Relaxed),
            PROFILE_CREATE_ATTEMPTS as u64,
            "counter must stop after exact attempt limit"
        );
        assert!(
            !fixture
                .path()
                .join(format!("{PROFILE_PREFIX}-{pid}-{PROFILE_CREATE_ATTEMPTS}"))
                .exists(),
            "must not probe past finite limit"
        );
        assert_eq!(
            fs::read_to_string(&sentinel).expect("existing sentinel must remain intact"),
            "do-not-delete"
        );
    }

    #[test]
    fn create_profile_propagates_non_already_exists_errors_without_retrying() {
        let fixture = IsolatedProfileFixture::new("other-errors");
        let pid = 8888;
        let counter = AtomicU64::new(0);

        let colliding_file = fixture.path().join(format!("{PROFILE_PREFIX}-{pid}-0"));
        fs::write(&colliding_file, "keep-file").expect("write colliding file");

        let mut immediate_calls = 0usize;
        let immediate_counter = AtomicU64::new(0);
        let immediate_err = create_profile_in_with(
            fixture.path(),
            pid,
            &immediate_counter,
            PROFILE_CREATE_ATTEMPTS,
            |_| {
                immediate_calls += 1;
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "immediate denial",
                ))
            },
        )
        .expect_err("non-AlreadyExists error on first attempt must propagate immediately");
        assert_eq!(immediate_calls, 1);
        assert_eq!(immediate_counter.load(Ordering::Relaxed), 1);
        assert!(
            immediate_err.contains("immediate denial"),
            "{immediate_err}"
        );
        assert_eq!(
            fs::read_to_string(&colliding_file).expect("colliding file must remain intact"),
            "keep-file"
        );

        let mut calls = 0usize;
        let err = create_profile_in_with(
            fixture.path(),
            pid,
            &counter,
            PROFILE_CREATE_ATTEMPTS,
            |path| {
                calls += 1;
                if calls == 1 {
                    fs::create_dir(path)
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "injected permission denied",
                    ))
                }
            },
        )
        .expect_err("non-AlreadyExists error must propagate immediately");
        assert_eq!(calls, 2, "must stop immediately on non-AlreadyExists error");
        assert_eq!(counter.load(Ordering::Relaxed), 2);
        assert!(
            err.contains(&format!("{PROFILE_PREFIX}-{pid}-1"))
                && err.contains("injected permission denied"),
            "error must name path and root cause: {err}"
        );
        assert_eq!(
            fs::read_to_string(&colliding_file).expect("colliding file must remain intact"),
            "keep-file"
        );
        assert!(
            !fixture
                .path()
                .join(format!("{PROFILE_PREFIX}-{pid}-1"))
                .exists()
        );

        let blocking_root = fixture.path().join("not-a-directory");
        fs::write(&blocking_root, "root-file").expect("write blocking root file");
        let root_counter = AtomicU64::new(0);
        let root_err = create_profile_in(&blocking_root, pid, &root_counter)
            .expect_err("non-directory temp_root must fail before creating profiles");
        assert_eq!(root_counter.load(Ordering::Relaxed), 0);
        assert!(root_err.contains("not-a-directory"), "{root_err}");
        assert_eq!(
            fs::read_to_string(&blocking_root).expect("blocking file must remain intact"),
            "root-file"
        );

        let zero_pid_counter = AtomicU64::new(0);
        assert!(create_profile_in(fixture.path(), 0, &zero_pid_counter).is_err());
        assert_eq!(zero_pid_counter.load(Ordering::Relaxed), 0);

        let zero_limit_counter = AtomicU64::new(0);
        assert!(
            create_profile_in_with(fixture.path(), pid, &zero_limit_counter, 0, |_| Ok(()))
                .is_err()
        );
        assert_eq!(zero_limit_counter.load(Ordering::Relaxed), 0);
    }
}
