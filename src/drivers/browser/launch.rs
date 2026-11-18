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
use std::path::{Path, PathBuf};
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
                if let Some(mut c) = child.take() {
                    let _ = c.kill();
                    let _ = c.wait();
                } else if let Some(pid) = detached_pid {
                    let _ = kill_process(pid, Some(&profile));
                }
                let _ = remove_profile(&profile);
                return Err(format!(
                    "the browser did not report a debugging port within {} ms; set `DA_CHROME` to the Chrome or Chromium executable",
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

/// Removes the temporary profile, retrying while Windows releases its handles.
fn remove_profile(profile: &Path) -> Result<(), String> {
    let mut last = None;
    for _ in 0..40 {
        match fs::remove_dir_all(profile) {
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
    let mut errors = Vec::new();
    let deadline = Instant::now() + exit_timeout;

    let mut port_open = port.is_some_and(is_port_listening);
    if let Some(port) = port
        && port_open
    {
        while is_port_listening(port) && Instant::now() < deadline {
            thread::sleep(POLL_INTERVAL);
        }
        port_open = is_port_listening(port);
    }

    let mut kill_attempted = false;
    if let Some(pid) = pid {
        if port_open {
            kill_attempted = true;
            if let Err(err) = kill_process(pid, profile) {
                errors.push(err);
            }
        } else if let Some(profile) = profile {
            ensure_profile_process_stopped(pid, profile, &mut errors);
        }
    }

    if let Some(port) = port
        && port_open
    {
        if kill_attempted {
            let socket_deadline = Instant::now() + Duration::from_secs(1);
            while is_port_listening(port) && Instant::now() < socket_deadline {
                thread::sleep(POLL_INTERVAL);
            }
        }
        if is_port_listening(port) {
            errors.push(format!(
                "browser debug port {port} is still listening after cleanup"
            ));
        }
    }

    if let Some(profile) = profile
        && let Err(err) = remove_profile(profile)
    {
        if let Some(pid) = pid
            && errors.is_empty()
        {
            if let Err(kill_err) = kill_process(pid, Some(profile)) {
                errors.push(kill_err);
            }
            if let Err(retry_err) = remove_profile(profile) {
                errors.push(retry_err);
            }
        } else {
            errors.push(err);
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
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

fn is_port_listening(port: u16) -> bool {
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
fn ensure_profile_process_stopped(pid: u32, profile: &Path, errors: &mut Vec<String>) {
    #[cfg(windows)]
    {
        match windows_pid_exists(pid) {
            Some(false) => return,
            Some(true) => {
                // Give a closing browser a brief window to exit cleanly after closing
                // its debug port so the fast `tasklist` check avoids spawning PowerShell.
                thread::sleep(Duration::from_millis(250));
                if windows_pid_exists(pid) == Some(false) {
                    return;
                }
            }
            None => {}
        }
        match windows_pid_matches_profile(pid, profile) {
            PidProfileMatch::DoesNotMatch => {}
            PidProfileMatch::Matches => {
                if let Err(err) = force_kill_pid(pid, Some(profile)) {
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
        match unix_pid_matches_profile(pid, profile) {
            PidProfileMatch::DoesNotMatch => return,
            PidProfileMatch::Unknown => {
                errors.push(format!(
                    "could not verify command line for surviving browser pid {pid}"
                ));
                return;
            }
            PidProfileMatch::Matches => {}
        }
        let grace_deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < grace_deadline {
            thread::sleep(Duration::from_millis(50));
            match unix_pid_matches_profile(pid, profile) {
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
        if let Err(err) = force_kill_pid(pid, Some(profile)) {
            errors.push(err);
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (pid, profile, errors);
    }
}

fn kill_process(pid: u32, profile: Option<&Path>) -> Result<(), String> {
    if let Some(profile) = profile {
        match pid_matches_profile(pid, profile) {
            PidProfileMatch::Matches => {}
            PidProfileMatch::DoesNotMatch => return Ok(()),
            PidProfileMatch::Unknown => {
                return Err(format!(
                    "could not verify command line for browser pid {pid} before termination"
                ));
            }
        }
    }
    force_kill_pid(pid, profile)
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

#[cfg(windows)]
fn force_kill_with<R, E>(
    pid: u32,
    profile: Option<&Path>,
    wait_timeout: Duration,
    mut run_taskkill: R,
    mut pid_exists: E,
) -> Result<(), String>
where
    R: FnMut(&[&str]) -> std::io::Result<std::process::ExitStatus>,
    E: FnMut(u32) -> Option<bool>,
{
    let pid_str = pid.to_string();
    let status = run_taskkill(&["/PID", &pid_str, "/T", "/F"])
        .map_err(|error| format!("could not terminate browser pid {pid}: {error}"))?;

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
                if let Some(path) = profile
                    && pid_matches_profile(pid, path) == PidProfileMatch::DoesNotMatch
                {
                    return Ok(());
                }
                return Err(if status.success() {
                    format!(
                        "could not terminate browser pid {pid}: process is still running after taskkill"
                    )
                } else {
                    format!("could not terminate browser pid {pid} (taskkill exited with {status})")
                });
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
    let pid_str = format!("-{}", pid);
    let term_status = run_kill(&["-TERM", &pid_str])
        .map_err(|error| format!("could not terminate browser process group {pid}: {error}"))?;
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
        .map_err(|error| format!("could not terminate browser process group {pid}: {error}"))?;
    if !kill_status.success() {
        if is_alive(pid, profile) == Some(false) {
            return Ok(());
        }
        return Err(format!(
            "could not terminate browser process group {pid} (kill -KILL exited with {kill_status})"
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
        let next_is_boundary = haystack[end..]
            .chars()
            .next()
            .is_none_or(|ch| ch == '\0' || ch == '"' || ch == '\'' || ch.is_whitespace());
        if next_is_boundary {
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
    // Check if the process group exists by sending signal 0 to the group
    Command::new("kill")
        .args(["-0", &format!("-{}", pid)])
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

/// Creates an empty profile directory with a unique name.
fn create_profile() -> Result<PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "{PROFILE_PREFIX}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).map_err(|error| {
        format!(
            "could not create the temporary browser profile `{}`: {error}",
            path.display()
        )
    })?;
    Ok(path)
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
}
