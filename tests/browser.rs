#![cfg(feature = "browser")]

//! Tests for the `browser.*` steps.
//!
//! The argument, registration and no-session tests need no browser, so they run
//! on every machine. The tests that launch Chrome are gated on
//! `DA_TEST_BROWSER`: when Chrome cannot be started they print why and skip,
//! and with `DA_TEST_BROWSER=1` they fail instead, which is what CI sets.
//!
//! Every page comes from `tests/support`, so no test reaches the network.

mod support;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::{TempDir, TestServer, closed_port};

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

/// Runs the binary with `dir` as its working directory.
fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(da())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run the da binary")
}

/// Runs the binary with extra environment variables.
fn run_da_with_env(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Output {
    let mut command = Command::new(da());
    command.current_dir(dir).args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("failed to run the da binary")
}

fn exit_code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("test paths must be valid UTF-8")
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout must contain exactly one JSON value")
}

/// Whether `bytes` starts with the eight-byte PNG signature.
fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Every file under `dir`, recursively.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).expect("the directory must be readable") {
        let path = entry.expect("the entry must be readable").path();
        if path.is_dir() {
            files.extend(files_under(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// The file names directly inside `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("the directory must be readable")
        .map(|entry| {
            entry
                .expect("the entry must be readable")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// The message of the one case a run reported, if it failed.
fn case_error(value: &Value) -> &str {
    value["cases"][0]["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the case must carry an error: {value}"))
}

/// Writes a diagnostic without a print macro.
///
/// The project denies `print_stderr` because a print macro panics when the
/// reader closed the pipe; the skip message must not replace the test result.
fn note(message: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}

/// Serializes the browser tests that launch Chrome: one browser test at a time
/// keeps four `--jobs` cases from competing with other tests for memory and
/// makes the profile-directory check meaningful.
static CHROME_TESTS: Mutex<()> = Mutex::new(());

/// Whether this machine can launch Chrome, probed once per test process.
fn chrome_ready(server: &TestServer) -> bool {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    let result = READY.get_or_init(|| probe_chrome(server));
    match result {
        Ok(()) => true,
        Err(message) => {
            if std::env::var("DA_TEST_BROWSER").as_deref() == Ok("1") {
                panic!("DA_TEST_BROWSER=1 requires a working Chrome: {message}");
            }
            note(&format!(
                "skipping browser test: {message}; set DA_TEST_BROWSER=1 to fail instead of skipping"
            ));
            false
        }
    }
}

/// Takes the Chrome-test lock, or returns `None` when Chrome is unavailable and
/// the test must skip.
fn chrome_test(server: &TestServer) -> Option<MutexGuard<'static, ()>> {
    if !chrome_ready(server) {
        return None;
    }
    Some(
        CHROME_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// Opens one page, so a missing Chrome is reported before any test body runs.
fn probe_chrome(server: &TestServer) -> Result<(), String> {
    let dir = TempDir::new("browser-probe");
    let case = dir.write(
        "probe.yaml",
        &format!(
            "name: browser probe\nsteps:\n  - browser.open:\n      url: \"{}\"\n",
            server.url("/browser/form?case=probe")
        ),
    );
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    if exit_code(&output) == Some(0) {
        return Ok(());
    }
    let detail = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| {
            value["cases"][0]["error"]["message"]
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("{}{}", stdout(&output), stderr(&output)));
    Err(format!("exit {:?}: {detail}", exit_code(&output)))
}

/// A case file that fails an assertion after opening the page.
fn assertion_failure_case(url: &str) -> String {
    format!(
        r##"name: close on assertion failure
steps:
  - browser.open:
      url: "{url}"
  - browser.read:
      css: "#greeting"
    expect:
      - path: $.text
        equals: "not set yet"
"##
    )
}

/// The Chrome the connect test drives, as the user's own browser.
///
/// The test starts it, opens the tab the user is supposedly looking at, runs a
/// case against it and then checks that the case neither closed the process nor
/// touched that tab. `Drop` ends the browser the test owns; the case must not.
struct UserBrowser {
    child: Child,
    profile: PathBuf,
    port: u16,
}

impl UserBrowser {
    /// Starts a browser with a debug port the test can name to `connect`.
    fn start() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let profile = std::env::temp_dir().join(format!(
            "da-test-user-browser-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&profile).expect("the test browser profile must be created");
        let port = free_port();
        let mut command = Command::new(chrome_executable());
        command
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--headless=new")
            .arg("--disable-extensions")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command.spawn().expect("the test browser must start");
        let browser = Self {
            child,
            profile,
            port,
        };
        browser.wait_until_ready();
        browser
    }

    /// Waits until the DevTools endpoint answers, so a case can connect.
    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok((200, _)) = http_request(self.port, "GET", "/json/version") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the test browser did not open its debug port {port}",
                port = self.port
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The loopback URL a case passes as `connect`.
    fn connect_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Opens one more tab, as the user's own tab, and returns its target id.
    fn open_tab(&self, url: &str) -> String {
        let (status, body) = http_request(self.port, "PUT", &format!("/json/new?{url}"))
            .expect("the test browser must accept a new tab");
        assert_eq!(status, 200, "the test browser refused a new tab: {body}");
        let value: Value = serde_json::from_str(&body).expect("the new tab must be a JSON object");
        value["id"]
            .as_str()
            .unwrap_or_else(|| panic!("the new tab must have an id: {value}"))
            .to_string()
    }

    /// Every page target the browser currently has, in the order Chrome lists
    /// them.
    fn page_tabs(&self) -> Vec<Value> {
        let (status, body) = http_request(self.port, "GET", "/json/list")
            .expect("the test browser must list its tabs");
        assert_eq!(status, 200, "the test browser refused to list tabs: {body}");
        let tabs: Vec<Value> =
            serde_json::from_str(&body).expect("the tab list must be a JSON array");
        tabs.into_iter()
            .filter(|tab| tab["type"].as_str() == Some("page"))
            .collect()
    }
}

impl Drop for UserBrowser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.profile);
    }
}

/// A loopback port nothing is listening on.
fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("a free port must be available");
    let port = listener
        .local_addr()
        .expect("the free port must have an address")
        .port();
    drop(listener);
    port
}

/// The Chrome executable, found the same way the driver finds it.
fn chrome_executable() -> PathBuf {
    if let Some(value) = std::env::var_os("DA_CHROME") {
        let path = PathBuf::from(value);
        assert!(
            path.is_file(),
            "DA_CHROME points to a missing file: {}",
            path.display()
        );
        return path;
    }
    let mut candidates = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            let Some(root) = std::env::var_os(variable) else {
                continue;
            };
            let root = PathBuf::from(root);
            candidates.push(root.join("Google/Chrome/Application/chrome.exe"));
            candidates.push(root.join("Chromium/Application/chrome.exe"));
            candidates.push(root.join("Microsoft/Edge/Application/msedge.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
        candidates.push(PathBuf::from(
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ));
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
            candidates.push(PathBuf::from(path));
        }
    }
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return path;
    }
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
    ] {
        if let Some(path) = find_program_on_path(name) {
            return path;
        }
    }
    panic!("the connect test needs Chrome or Chromium; set DA_CHROME to its executable");
}

/// Finds `name`, or `name.exe` on Windows, in one of the `PATH` directories.
fn find_program_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|directory| {
        [directory.join(name), directory.join(format!("{name}.exe"))]
            .into_iter()
            .find(|candidate| candidate.is_file())
    })
}

/// One HTTP/1.1 request to the test browser's DevTools endpoint.
///
/// The answer is read by `content-length`; a DevTools endpoint keeps the
/// connection open, so reading to end-of-file would wait for the timeout.
fn http_request(port: u16, method: &str, path: &str) -> Result<(u16, String), String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|error| format!("could not connect to {address}: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("could not configure the connection: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("could not configure the connection: {error}"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("could not send the request: {error}"))?;

    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    loop {
        if header_end.is_none()
            && let Some(index) = response.windows(4).position(|window| window == b"\r\n\r\n")
        {
            header_end = Some(index + 4);
        }
        if let Some(body_start) = header_end {
            let header = String::from_utf8_lossy(&response[..body_start]);
            if let Some(length) = header.lines().skip(1).find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            }) && response.len() >= body_start + length
            {
                break;
            }
        }
        assert!(
            response.len() < 1 << 20,
            "the DevTools endpoint answered more than 1 MiB"
        );
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&buffer[..read]),
            Err(error) => return Err(format!("could not read the answer: {error}")),
        }
    }

    let body_start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| "the answer has no header block".to_string())?;
    let header = String::from_utf8_lossy(&response[..body_start]);
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("the answer has no status line: {header}"))?;
    let body = String::from_utf8_lossy(&response[body_start..]).into_owned();
    Ok((status, body))
}

/// Waits until the browser lists at least `wanted` pages.
fn wait_for_pages(browser: &UserBrowser, wanted: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pages = browser.page_tabs();
        if pages.len() >= wanted {
            return pages;
        }
        assert!(
            Instant::now() < deadline,
            "the test browser listed {} pages, expected at least {wanted}: {pages:?}",
            pages.len()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until no page URL contains `fragment`.
fn wait_for_page_to_close(browser: &UserBrowser, fragment: &str) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pages = browser.page_tabs();
        if !pages.iter().any(|page| {
            page["url"]
                .as_str()
                .is_some_and(|url| url.contains(fragment))
        }) {
            return pages;
        }
        assert!(
            Instant::now() < deadline,
            "the page `{fragment}` was still open after the case: {pages:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// The target id of one listed page.
fn page_id(page: &Value) -> &str {
    page["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a listed page must have an id: {page}"))
}

#[test]
fn a_case_can_drive_a_page_through_every_step() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-flow");
    let case = dir.write(
        "flow.yaml",
        &format!(
            r##"name: browser flow
steps:
  - browser.open:
      url: "{}"
    expect:
      - path: $.title
        equals: Browser form
      - path: $.url
        contains: /browser/form
  - browser.type:
      css: "#name"
      text: Ada
  - browser.press:
      key: Enter
  - browser.wait:
      css: "#greeting"
  - browser.read:
      xpath: "//*[@id='greeting']"
    expect:
      - path: $.text
        equals: "Hello, Ada!"
  - browser.read:
      css: "#greeting"
      attribute: data-state
    expect:
      - path: $.value
        equals: ready
  - browser.click:
      css: "#later"
    expect:
      - path: $.text
        equals: Later
  - browser.wait:
      css: "#late"
      timeout_ms: 2000
  - browser.read:
      css: "#late"
    expect:
      - path: $.text
        equals: late result
  - browser.click:
      xpath: "//*[@id='next']"
  - browser.wait:
      url_contains: /browser/second
  - browser.read:
      css: "#second"
    expect:
      - path: $.text
        equals: second page
"##,
            server.url("/browser/form?case=flow")
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(value["cases"][0]["status"], "passed", "{value}");
}

#[test]
fn a_second_open_navigates_the_same_browser() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-second-open");
    let first = server.url("/browser/form?case=first");
    let second = server.url("/browser/form?case=second");
    let case = dir.write(
        "reopen.yaml",
        &format!(
            r##"name: reopen the page
steps:
  - browser.open:
      url: "{first}"
      headless: true
      width: 1280
      height: 800
  - browser.read:
      css: "#visits"
    expect:
      - path: $.text
        equals: "visits: 1"
  - browser.open:
      url: "{second}"
  - browser.read:
      css: "#visits"
    expect:
      - path: $.text
        equals: "visits: 2"
  - browser.read:
      css: "#case"
    expect:
      - path: $.text
        equals: second
"##
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(
        value["cases"][0]["status"], "passed",
        "the second open must navigate the page the first one opened: {value}"
    );
}

#[test]
fn a_missing_element_fails_with_its_locator_and_timeout() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-missing");
    let url = server.url("/browser/form?case=missing");

    for (name, step, timeout, locator) in [
        (
            "click.yaml",
            "  - browser.click:\n      css: \"#never\"\n      timeout_ms: 300\n",
            "300",
            "#never",
        ),
        (
            "wait.yaml",
            "  - browser.wait:\n      css: \"#never\"\n      timeout_ms: 200\n",
            "200",
            "#never",
        ),
        (
            "click-text.yaml",
            "  - browser.click:\n      text: \"No such text\"\n      timeout_ms: 300\n",
            "300",
            "No such text",
        ),
        (
            "wait-role.yaml",
            "  - browser.wait:\n      role: button\n      name: \"No such button\"\n      timeout_ms: 200\n",
            "200",
            "No such button",
        ),
    ] {
        let case = dir.write(
            name,
            &format!(
                "name: missing element\nsteps:\n  - browser.open:\n      url: \"{url}\"\n{step}"
            ),
        );
        let output = run_da_in(
            dir.path(),
            &["--output", "json", "run", path_str(&case), "--no-bundle"],
        );

        assert_eq!(
            exit_code(&output),
            Some(1),
            "{name}: stdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        let value = json_stdout(&output);
        let message = case_error(&value);
        assert!(
            message.contains(locator),
            "{name}: the message must name the locator, got: {message}"
        );
        assert!(
            message.contains(timeout),
            "{name}: the message must name the timeout, got: {message}"
        );
    }
}

#[test]
fn changing_launch_options_after_open_is_an_error() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-options");
    let url = server.url("/browser/form?case=options");
    let case = dir.write(
        "options.yaml",
        &format!(
            r##"name: changed options
steps:
  - browser.open:
      url: "{url}"
      headless: true
  - browser.open:
      url: "{url}"
      headless: false
"##
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("headless"),
        "the message must name the changed option, got: {message}"
    );
}

#[test]
fn four_cases_run_their_own_browser_under_jobs_4() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-jobs");
    for label in ["a", "b", "c", "d"] {
        dir.write(
            &format!("{label}.yaml"),
            &format!(
                r##"name: case {label}
steps:
  - browser.open:
      url: "{url}"
  - browser.read:
      css: "#case"
    expect:
      - path: $.text
        equals: {label}
  - browser.read:
      css: "#visits"
    expect:
      - path: $.text
        equals: "visits: 1"
  - browser.type:
      css: "#name"
      text: {label}
  - browser.press:
      key: Enter
  - browser.wait:
      css: "#greeting"
  - browser.read:
      css: "#greeting"
    expect:
      - path: $.text
        equals: "Hello, {label}!"
"##,
                url = server.url(&format!("/browser/form?case={label}"))
            ),
        );
    }

    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(dir.path()),
            "--jobs",
            "4",
            "--no-bundle",
        ],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let cases = value["cases"]
        .as_array()
        .expect("the run must report every case");
    assert_eq!(cases.len(), 4, "{value}");
    for case in cases {
        assert_eq!(
            case["status"], "passed",
            "every case must drive its own page: {case}"
        );
    }
}

#[test]
fn a_click_changes_the_page_through_a_real_mouse_event() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-mouse");
    let case = dir.write(
        "mouse.yaml",
        &format!(
            r##"name: real mouse click
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#trusted"
  - browser.read:
      css: "#trusted-result"
    expect:
      - path: $.text
        equals: "trusted click"
"##,
            server.url("/browser/form?case=mouse")
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(
        value["cases"][0]["status"], "passed",
        "the click must be a trusted browser mouse event, not a script call: {value}"
    );
}

#[test]
fn browser_open_honours_its_navigation_timeout() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-open-timeout");
    let url = server.url("/browser/stall?case=timeout");
    let case = dir.write(
        "timeout.yaml",
        &format!(
            "name: slow page\nsteps:\n  - browser.open:\n      url: \"{url}\"\n      timeout_ms: 300\n"
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(5),
        "a navigation that exceeds `timeout_ms` is a timeout: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("/browser/stall") && message.contains("300"),
        "the timeout must name the URL and the wait, got: {message}"
    );
}

#[test]
fn an_assertion_failure_closes_the_browser() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-close");
    // `std::env::temp_dir` reads TEMP/TMP on Windows and TMPDIR elsewhere.
    // Putting them under the test's own directory makes the browser profile
    // observable and keeps the check independent of any other browser on the
    // machine.
    let profile_home = dir.path().join("tmp");
    fs::create_dir_all(&profile_home).expect("the profile directory must be created");
    let close_log = dir.path().join("close.log");
    let case = dir.write(
        "close.yaml",
        &assertion_failure_case(&server.url("/browser/form?case=close")),
    );
    let profile = profile_home.to_string_lossy().into_owned();
    let close_log_text = close_log.to_string_lossy().into_owned();

    let output = run_da_with_env(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
        &[
            ("TEMP", profile.clone()),
            ("TMP", profile.clone()),
            ("TMPDIR", profile),
            ("DA_TEST_BROWSER_CLOSE_LOG", close_log_text),
        ],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(value["cases"][0]["status"], "failed", "{value}");

    let record = fs::read_to_string(&close_log)
        .expect("the session must record that it closed, even after a failed assertion");
    assert!(
        record.contains("browser"),
        "the close record must name the browser session, got: {record:?}"
    );
    let port: u16 = record
        .split("port=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("the close record must name the debug port, got: {record:?}"));
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_err(),
        "the browser debug port {port} still accepts connections after the case"
    );

    // A leftover profile directory means the process outlived the case or the
    // cleanup failed.
    let leftovers: Vec<PathBuf> = fs::read_dir(&profile_home)
        .expect("the profile directory must be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("da-chrome-profile"))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "the browser profile must be removed after the case, found: {leftovers:?}"
    );
}

#[test]
fn typed_and_read_text_is_redacted() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-secrets");
    let secret = "browser-s3cr3t-value";
    let case = dir.write(
        "secret.yaml",
        &format!(
            r##"name: secret text
vars:
  token: "${{env:DA_BROWSER_TEST_SECRET}}"
steps:
  - browser.open:
      url: "{}"
  - browser.type:
      css: "#name"
      text: "${{token}}"
  - browser.press:
      key: Enter
  - browser.wait:
      css: "#greeting"
  - browser.read:
      css: "#greeting"
    expect:
      - path: $.text
        equals: "wrong"
"##,
            server.url("/browser/form?case=secret")
        ),
    );
    let env = [("DA_BROWSER_TEST_SECRET", secret.to_string())];

    for mode in ["text", "json"] {
        let output = run_da_with_env(
            dir.path(),
            &["--output", mode, "run", path_str(&case), "--no-bundle"],
            &env,
        );
        assert_eq!(
            exit_code(&output),
            Some(1),
            "{mode}: stdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        let rendered = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            !rendered.contains(secret),
            "{mode}: the output leaked the typed and read text: {rendered}"
        );
        assert!(
            rendered.contains("***"),
            "{mode}: the output must show the redaction marker: {rendered}"
        );
    }
}

#[test]
fn every_step_reports_its_argument_errors_in_lint() {
    let dir = TempDir::new("browser-lint");
    let cases = [
        (
            "click-both.yaml",
            "  - browser.click:\n      css: \"#a\"\n      xpath: \"//*[@id='a']\"\n",
            "cannot both be set",
        ),
        (
            "click-neither.yaml",
            "  - browser.click:\n      timeout_ms: 100\n",
            "one locator",
        ),
        (
            "open-no-url.yaml",
            "  - browser.open:\n      headless: true\n",
            "url",
        ),
        (
            "type-no-text.yaml",
            "  - browser.type:\n      css: \"#a\"\n",
            "text",
        ),
        ("press-no-key.yaml", "  - browser.press:\n      {}\n", "key"),
        (
            "press-unknown-key.yaml",
            "  - browser.press:\n      key: NotAKey\n",
            "not a key",
        ),
        (
            "wait-both.yaml",
            "  - browser.wait:\n      css: \"#a\"\n      url_contains: next\n",
            "exactly one of",
        ),
        (
            "read-neither.yaml",
            "  - browser.read:\n      timeout_ms: 100\n",
            "one locator",
        ),
        (
            "wait-zero.yaml",
            "  - browser.wait:\n      css: \"#a\"\n      timeout_ms: 0\n",
            "timeout_ms",
        ),
        (
            "open-zero-width.yaml",
            "  - browser.open:\n      url: \"http://127.0.0.1:1/\"\n      width: 0\n",
            "width",
        ),
        (
            "unknown-argument.yaml",
            "  - browser.open:\n      url: \"http://127.0.0.1:1/\"\n      bogus: true\n",
            "bogus",
        ),
        (
            "connect-remote.yaml",
            "  - browser.open:\n      connect: \"http://192.168.1.10:9222\"\n      url: \"http://127.0.0.1:1/\"\n",
            "loopback",
        ),
        (
            "connect-headless.yaml",
            "  - browser.open:\n      connect: \"http://127.0.0.1:9222\"\n      url: \"http://127.0.0.1:1/\"\n      headless: true\n",
            "headless",
        ),
        (
            "click-two-locators.yaml",
            "  - browser.click:\n      css: \"#a\"\n      text: Later\n",
            "cannot both be set",
        ),
        (
            "read-name-alone.yaml",
            "  - browser.read:\n      name: Submit\n",
            "role",
        ),
        (
            "type-name-alone.yaml",
            "  - browser.type:\n      name: Submit\n      text: hello\n",
            "role",
        ),
        (
            "wait-role-and-url.yaml",
            "  - browser.wait:\n      role: button\n      url_contains: next\n",
            "cannot be combined",
        ),
    ];
    for (name, step, _) in cases {
        dir.write(name, &format!("name: invalid browser step\nsteps:\n{step}"));
    }

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "lint", path_str(dir.path())],
    );

    assert_eq!(
        exit_code(&output),
        Some(3),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let errors = value["errors"]
        .as_array()
        .expect("lint must report its errors");
    assert_eq!(errors.len(), cases.len(), "{value}");
    for (name, _, fragment) in cases {
        let message = errors
            .iter()
            .find_map(|error| {
                let file = error["file"].as_str()?;
                if file.ends_with(name) {
                    error["message"].as_str()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| panic!("{name} must be reported, got: {errors:?}"));
        assert!(
            message.contains(fragment),
            "{name}: the message must contain {fragment:?}, got: {message:?}"
        );
    }
}

#[test]
fn a_browser_step_without_a_session_names_browser_open() {
    let dir = TempDir::new("browser-no-session");
    let case = dir.write(
        "no-session.yaml",
        "name: no session\nsteps:\n  - browser.click:\n      css: \"#go\"\n",
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("browser.open"),
        "the message must say to run `browser.open` first, got: {message}"
    );
}

#[test]
fn a_missing_da_chrome_is_an_environment_error() {
    let dir = TempDir::new("browser-chrome");
    let missing = dir.path().join("no-such-chrome");
    let case = dir.write(
        "chrome.yaml",
        "name: missing chrome\nsteps:\n  - browser.open:\n      url: \"http://127.0.0.1:9/\"\n",
    );

    let output = run_da_with_env(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
        &[("DA_CHROME", missing.to_string_lossy().into_owned())],
    );

    assert_eq!(
        exit_code(&output),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("DA_CHROME"),
        "the environment error must say how to set the path, got: {message}"
    );
}

#[test]
fn a_fragment_open_navigates_within_the_page_without_waiting_for_a_load() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-fragment-open");
    let page = server.url("/browser/form?case=fragment");
    let case = dir.write(
        "fragment.yaml",
        &format!(
            r##"name: open a fragment of the same page
steps:
  - browser.open:
      url: "{page}"
  - browser.open:
      url: "{page}#greeting"
      timeout_ms: 3000
    expect:
      - path: $.url
        contains: "#greeting"
  - browser.read:
      css: "#visits"
    expect:
      - path: $.text
        equals: "visits: 1"
"##
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "a same-document open must not wait for a load event: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn connect_drives_a_page_in_a_browser_it_does_not_close() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let mut browser = UserBrowser::start();
    let user_tab = browser.open_tab(&server.url("/browser/form?case=user-tab"));
    let before = wait_for_pages(&browser, 2);
    assert_eq!(
        before.len(),
        2,
        "the user's browser must start with its own page and the user tab: {before:?}"
    );
    assert!(
        before.iter().any(|page| page_id(page) == user_tab),
        "the user tab must be listed before the case: {before:?}"
    );
    let initial_ids: Vec<String> = before
        .iter()
        .map(|page| page_id(page).to_string())
        .collect();

    let dir = TempDir::new("browser-connect");
    let case_url = server.url("/browser/form?case=connect");
    let case = dir.write(
        "connect.yaml",
        &format!(
            r##"name: connect to the user's browser
steps:
  - browser.open:
      connect: "{}"
      url: "{case_url}"
    expect:
      - path: $.title
        equals: Browser form
  - browser.read:
      css: "#case"
    expect:
      - path: $.text
        equals: connect
  - browser.read:
      text: "no greeting yet"
    expect:
      - path: $.text
        equals: "no greeting yet"
"##,
            browser.connect_url()
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(value["cases"][0]["status"], "passed", "{value}");

    assert!(
        browser
            .child
            .try_wait()
            .expect("the test must watch the browser process")
            .is_none(),
        "connect mode must not end the user's browser"
    );
    let after = wait_for_page_to_close(&browser, "case=connect");
    assert_eq!(
        after.len(),
        before.len(),
        "the case must close only the tab it opened: {after:?}"
    );
    let after_ids: Vec<&str> = after.iter().map(page_id).collect();
    for id in &initial_ids {
        assert!(
            after_ids.contains(&id.as_str()),
            "the case must leave the user's tab {id} open: {after:?}"
        );
    }
    assert!(
        after_ids.contains(&user_tab.as_str()),
        "the case must leave the user tab open: {after:?}"
    );
}

#[test]
fn a_second_open_with_a_different_connect_is_a_step_error() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let browser = UserBrowser::start();
    let dir = TempDir::new("browser-connect-change");
    let url = server.url("/browser/form?case=connect-change");
    let other_port = closed_port();
    let case = dir.write(
        "connect-change.yaml",
        &format!(
            r##"name: change the connected browser
steps:
  - browser.open:
      connect: "{}"
      url: "{url}"
  - browser.open:
      connect: "http://127.0.0.1:{other_port}"
      url: "{url}"
"##,
            browser.connect_url()
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("connect"),
        "the message must name the changed option, got: {message}"
    );
}

#[test]
fn connect_to_a_closed_port_is_an_environment_error() {
    let dir = TempDir::new("browser-connect-closed");
    let port = closed_port();
    let case = dir.write(
        "connect-closed.yaml",
        &format!(
            r##"name: connect to nothing
steps:
  - browser.open:
      connect: "http://127.0.0.1:{port}"
      url: "http://127.0.0.1:9/"
"##
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("--remote-debugging-port"),
        "the message must say how to start Chrome, got: {message}"
    );
}

#[test]
fn connect_to_a_non_devtools_server_is_an_environment_error() {
    let server = TestServer::start();
    let dir = TempDir::new("browser-connect-not-devtools");
    let case = dir.write(
        "connect-not-devtools.yaml",
        &format!(
            r##"name: connect to another server
steps:
  - browser.open:
      connect: "http://127.0.0.1:{}"
      url: "http://127.0.0.1:9/"
"##,
            server.port()
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("--remote-debugging-port"),
        "the message must say how to start Chrome, got: {message}"
    );
}

#[test]
fn text_and_role_locators_drive_a_page() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-locators");
    let case = dir.write(
        "locators.yaml",
        &format!(
            r##"name: text and role locators
steps:
  - browser.open:
      url: "{}"
  - browser.type:
      role: textbox
      name: Name
      text: Ada
  - browser.click:
      role: button
      name: Greet
  - browser.wait:
      text: "Hello, Ada!"
  - browser.read:
      text: "Hello, Ada!"
    expect:
      - path: $.text
        equals: "Hello, Ada!"
  - browser.click:
      text: Later
  - browser.wait:
      text: "late result"
      timeout_ms: 2000
  - browser.read:
      role: button
      name: Later
    expect:
      - path: $.text
        equals: Later
  - browser.read:
      text: "late result"
    expect:
      - path: $.text
        equals: "late result"
  - browser.type:
      text: Editor
      value: " typed"
  - browser.read:
      css: "#editor"
    expect:
      - path: $.text
        contains: typed
  - browser.click:
      role: link
      name: Next
  - browser.wait:
      url_contains: /browser/second
  - browser.read:
      text: "second page"
    expect:
      - path: $.text
        equals: "second page"
"##,
            server.url("/browser/form?case=locators")
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(value["cases"][0]["status"], "passed", "{value}");
}

#[test]
fn a_failed_browser_case_bundles_a_valid_png_and_a_dom_snapshot() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-evidence");
    let case = dir.write(
        "evidence.yaml",
        &format!(
            r##"name: browser evidence
steps:
  - browser.open:
      url: "{}"
  - browser.type:
      css: "#name"
      text: Ada
  - browser.read:
      css: "#greeting"
    expect:
      - path: $.text
        equals: "not set yet"
"##,
            server.url("/browser/form?case=evidence")
        ),
    );
    let bundles = dir.path().join("failures");

    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&case),
            "--bundle-dir",
            path_str(&bundles),
        ],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let bundle = PathBuf::from(
        value["cases"][0]["bundle"]
            .as_str()
            .expect("the failed case must name its bundle"),
    );
    let png = fs::read(bundle.join("browser/screenshot.png"))
        .expect("the bundle must hold the browser screenshot");
    assert!(
        is_png(&png),
        "the screenshot must be a PNG, first bytes: {:?}",
        &png[..png.len().min(8)]
    );
    let dom = fs::read_to_string(bundle.join("browser/dom.html"))
        .expect("the bundle must hold the DOM snapshot");
    assert!(dom.contains("<html"), "the snapshot must be HTML: {dom}");
    assert!(
        dom.contains("id=\"name\""),
        "the snapshot must be the page that failed: {dom}"
    );

    let rendered = stdout(&output);
    assert!(
        !rendered.contains("screenshot.png") && !rendered.contains("dom.html"),
        "evidence paths must not enter the JSON output: {rendered}"
    );
}

#[test]
fn screenshot_covers_every_visible_editable_field_but_not_buttons_or_unsupported_inputs() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-mask");
    let case = dir.write(
        "mask.yaml",
        &format!(
            r##"name: screenshot masks the form
steps:
  - browser.open:
      url: "{}"
  - browser.type:
      css: "#name"
      text: "must-not-appear-in-pixels"
  - browser.screenshot:
      path: shots/masked.png
  - browser.read:
      css: "#mask-report"
    expect:
      - path: $.text
        equals: "name=true,notes=true,choice=true,editor=true,button=false,checkbox=false,hidden=false,range=false,color=false"
"##,
            server.url("/browser/mask")
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "the mask probe must see every editable field covered: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(value["cases"][0]["status"], "passed", "{value}");
    let screenshot = dir.path().join("shots/masked.png");
    assert!(
        fs::read(&screenshot)
            .expect("the screenshot step must write its file")
            .starts_with(b"\x89PNG\r\n\x1a\n"),
        "the screenshot step must write a PNG"
    );
    let result = &value["cases"][0]["steps"][2]["result"];
    assert_eq!(
        result["bytes"],
        json!(
            fs::metadata(&screenshot)
                .expect("the screenshot must exist")
                .len()
        )
    );
    assert!(
        result["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("shots/masked.png")),
        "the result must name the written path: {result}"
    );
}

#[test]
fn a_secret_written_into_page_html_is_redacted_from_the_bundle() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-secret-evidence");
    let secret = "browser-s3cr3t-in-html";
    let case = dir.write(
        "secret.yaml",
        &format!(
            r##"name: secret in page html
vars:
  token: "${{env:DA_BROWSER_EVIDENCE_SECRET}}"
steps:
  - browser.open:
      url: "{}/browser/form?case=${{token}}"
  - browser.read:
      css: "#case"
    expect:
      - path: $.text
        equals: "wrong"
"##,
            server.url("")
        ),
    );
    let bundles = dir.path().join("failures");

    let output = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&case),
            "--bundle-dir",
            path_str(&bundles),
        ],
        &[("DA_BROWSER_EVIDENCE_SECRET", secret.to_string())],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let bundle = PathBuf::from(
        value["cases"][0]["bundle"]
            .as_str()
            .expect("the failed case must name its bundle"),
    );
    let dom = fs::read_to_string(bundle.join("browser/dom.html"))
        .expect("the bundle must hold the DOM snapshot");
    assert!(
        !dom.contains(secret),
        "the DOM evidence leaked the secret: {dom}"
    );
    assert!(
        dom.contains("***"),
        "the DOM evidence must show the redaction marker: {dom}"
    );
    for file in files_under(&bundle) {
        let bytes = fs::read(&file).expect("a bundle file must be readable");
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "{} leaked the secret",
            file.display()
        );
    }
}

#[test]
fn no_bundle_and_passing_cases_do_not_take_evidence_screenshots() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-no-evidence-shot");
    let log = dir.path().join("screenshots.log");
    let failing = dir.write(
        "failing.yaml",
        &format!(
            r##"name: failing
steps:
  - browser.open:
      url: "{}"
  - browser.read:
      css: "#greeting"
    expect:
      - path: $.text
        equals: "wrong"
"##,
            server.url("/browser/form?case=no-bundle")
        ),
    );
    let passing = dir.write(
        "passing.yaml",
        &format!(
            "name: passing\nsteps:\n  - browser.open:\n      url: \"{}\"\n",
            server.url("/browser/form?case=passing")
        ),
    );
    let env = [(
        "DA_TEST_BROWSER_SCREENSHOT_LOG",
        log.to_string_lossy().into_owned(),
    )];

    let bundled = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&failing),
            "--bundle-dir",
            path_str(&dir.path().join("failures")),
        ],
        &env,
    );
    assert_eq!(
        exit_code(&bundled),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&bundled),
        stderr(&bundled)
    );
    let recorded = fs::read_to_string(&log).expect("the bundled run must take a screenshot");
    assert!(
        recorded.contains("screenshot"),
        "the screenshot hook must record the attempt, got: {recorded:?}"
    );

    fs::remove_file(&log).expect("the screenshot log must be removable");
    let unbundled = run_da_with_env(
        dir.path(),
        &["--output", "json", "run", path_str(&failing), "--no-bundle"],
        &env,
    );
    assert_eq!(
        exit_code(&unbundled),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&unbundled),
        stderr(&unbundled)
    );
    assert!(
        !log.exists(),
        "`--no-bundle` must not take a screenshot for a failed case"
    );

    let passed = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&passing),
            "--bundle-dir",
            path_str(&dir.path().join("passing-failures")),
        ],
        &env,
    );
    assert_eq!(
        exit_code(&passed),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&passed),
        stderr(&passed)
    );
    assert!(
        !log.exists(),
        "a passing case must not take a screenshot even when bundles are enabled"
    );
}

#[test]
fn a_failed_case_without_a_browser_session_keeps_the_existing_bundle_files() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-no-session-bundle");
    let case = dir.write(
        "no-session.yaml",
        "name: no browser session\nsteps:\n  - browser.read:\n      css: \"#nothing\"\n",
    );
    let bundles = dir.path().join("failures");

    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&case),
            "--bundle-dir",
            path_str(&bundles),
        ],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let bundle = PathBuf::from(
        value["cases"][0]["bundle"]
            .as_str()
            .expect("the failed case must name its bundle"),
    );
    assert_eq!(
        names_in(&bundle),
        vec!["case.yaml", "meta.json", "report.json"],
        "a case with no browser session must keep the old bundle layout"
    );
}

#[test]
fn browser_screenshot_resolves_a_relative_path_against_the_case_file() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-screenshot-path");
    let case = dir.write(
        "cases/shot.yaml",
        &format!(
            "name: relative screenshot\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.screenshot:\n      path: shots/out.png\n",
            server.url("/browser/form?case=relative")
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let result = &value["cases"][0]["steps"][1]["result"];
    assert!(
        result["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("cases/shots/out.png")),
        "the result must name the case-relative path: {result}"
    );
    let screenshot = dir.path().join("cases/shots/out.png");
    assert!(
        is_png(&fs::read(&screenshot).expect("the screenshot must be written")),
        "the screenshot must be a PNG"
    );
}
