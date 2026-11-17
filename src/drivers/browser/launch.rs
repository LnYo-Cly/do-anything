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
    profile: Option<PathBuf>,
    port: u16,
    websocket_url: String,
}

impl Launched {
    /// Starts the browser and returns once its DevTools port is usable.
    pub fn launch(
        headless: bool,
        width: u32,
        height: u32,
        deadline: Deadline,
    ) -> Result<Self, String> {
        let executable = find_browser()?;
        let profile = create_profile()?;
        let mut command = Command::new(&executable);
        command.args(launch_args(headless, width, height, &profile));
        command
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().map_err(|error| {
            let _ = remove_profile(&profile);
            format!(
                "could not start `{}`: {error}; set `DA_CHROME` to the Chrome or Chromium executable",
                executable.display()
            )
        })?;

        let port_file = profile.join("DevToolsActivePort");
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let _ = remove_profile(&profile);
                    return Err(format!(
                        "`{}` exited with {status} before it was ready; set `DA_CHROME` to the Chrome or Chromium executable",
                        executable.display()
                    ));
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = remove_profile(&profile);
                    return Err(format!("could not watch the browser process: {error}"));
                }
            }

            if let Ok(text) = fs::read_to_string(&port_file)
                && let Some((port, path)) = parse_devtools_active_port(&text)
            {
                return Ok(Self {
                    child: Some(child),
                    profile: Some(profile),
                    port,
                    websocket_url: format!("ws://127.0.0.1:{port}{path}"),
                });
            }

            if deadline.passed() {
                let _ = child.kill();
                let _ = child.wait();
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
        if self.child.is_some() || self.profile.is_some() {
            let _ = self.release();
        }
    }
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
    for _ in 0..20 {
        match fs::remove_dir_all(profile) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                last = Some(error);
                thread::sleep(Duration::from_millis(25));
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
}
