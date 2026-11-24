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
    ///
    /// A loaded CI runner can take far longer than a healthy machine to bind the
    /// port, and a tab test that starts this browser runs next to other Chrome
    /// launches, so the window is generous; a real failure still ends here with
    /// the pid and profile named.
    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
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
fn the_test_server_reports_when_a_request_arrived_and_was_answered() {
    let server = TestServer::start();
    // A connection the browser opens and never uses, which is what its
    // speculative sockets look like from the server: it proves the browser
    // reached the server even when it asks for nothing.
    let idle = TcpStream::connect(("127.0.0.1", server.port())).expect("the probe must connect");
    let mut request =
        TcpStream::connect(("127.0.0.1", server.port())).expect("the request must connect");
    request
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("the probe must bound its read");
    request
        .write_all(b"GET /health HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n\r\n")
        .expect("the probe must send its request");
    let mut response = String::new();
    request
        .read_to_string(&mut response)
        .expect("the probe must read the response");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    drop(idle);

    let timings = server.timings();
    let answered = timings
        .iter()
        .find_map(|timing| timing.request.as_ref())
        .expect("the connection that sent a request must report it");
    assert_eq!(answered.target, "GET /health", "{timings:?}");
    let arrival = answered.arrived;
    let written = answered
        .answered
        .expect("an answered request must name when it was answered");
    assert!(
        arrival
            >= timings
                .iter()
                .map(|timing| timing.accepted)
                .min()
                .expect("a connection must be recorded"),
        "a request cannot arrive before its connection was accepted: {arrival:?}"
    );
    assert!(
        written >= arrival,
        "a response cannot be written before its request arrived: {written:?} < {arrival:?}"
    );
    assert!(
        timings.iter().any(|timing| timing.request.is_none()),
        "the connection that asked for nothing must be reported too: {timings:?}"
    );

    let report = server.timing_report();
    assert!(
        report.contains("accepted 2 connection(s)") && report.contains("answered 1 request(s)"),
        "the report must count what the server saw: {report}"
    );
    assert!(
        report.contains("request `GET /health`"),
        "the report must name the request: {report}"
    );
    assert!(
        report.contains(", no request"),
        "the report must mark the connection that asked for nothing: {report}"
    );
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

    // A navigation that stalls says only that the page did not load. The
    // server's own timings say whether the request reached it late or it
    // answered late, which is the difference between the Chrome and the server
    // being at fault.
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}\n{}",
        stdout(&output),
        stderr(&output),
        server.timing_report()
    );
    let value = json_stdout(&output);
    let cases = value["cases"]
        .as_array()
        .expect("the run must report every case");
    assert_eq!(cases.len(), 4, "{value}");
    for case in cases {
        assert_eq!(
            case["status"],
            "passed",
            "every case must drive its own page: {case}\n{}",
            server.timing_report()
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
        "a navigation that exceeds `timeout_ms` is a timeout: stdout: {}\nstderr: {}\n{}",
        stdout(&output),
        stderr(&output),
        server.timing_report()
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
            "upload-no-files.yaml",
            "  - browser.upload:\n      css: \"#a\"\n",
            "files",
        ),
        (
            "upload-empty-path.yaml",
            "  - browser.upload:\n      css: \"#a\"\n      files: [\"\"]\n",
            "files[0]",
        ),
        (
            "upload-two-locators.yaml",
            "  - browser.upload:\n      css: \"#a\"\n      testid: b\n      files: [\"a.txt\"]\n",
            "cannot both be set",
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
observe:
  net:
    browser.network: {{}}
steps:
  - browser.open:
      connect: "{}"
      url: "{case_url}"
    expect:
      - path: $.title
        equals: Browser form
      - path: "$.observed.net.requests[?(@.url == '{case_url}')].status"
        equals: 200
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
        "stdout: {}\nstderr: {}\n{}",
        stdout(&output),
        stderr(&output),
        server.timing_report()
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
fn network_observer_reports_each_request_once_in_the_step_that_completes_it() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-network");
    let closed = closed_port();
    let page = server.url(&format!("/browser/network?case=network&closed={closed}"));
    let ok = server.url("/health?kind=post");
    let missing = server.url("/nowhere?kind=missing");
    let failed = format!("http://127.0.0.1:{closed}/closed");
    let case = dir.write(
        "network.yaml",
        &format!(
            r##"name: network observer
observe:
  net:
    browser.network: {{}}
steps:
  - browser.open:
      url: "{page}"
    expect:
      - path: "$.observed.net.requests[?(@.url == '{page}')].status"
        equals: 200
      - path: "$.observed.net.requests[?(@.url == '{page}')].type"
        equals: Document
      - path: "$.observed.net.requests[?(@.url == '{page}')].method"
        equals: GET
  - browser.click:
      css: "#net-all"
  - browser.wait:
      css: "#network-done"
      timeout_ms: 10000
    expect:
      - path: "$.observed.net.requests[?(@.url == '{ok}')].status"
        equals: 200
      - path: "$.observed.net.requests[?(@.url == '{ok}')].method"
        equals: POST
      - path: "$.observed.net.requests[?(@.url == '{ok}')].type"
        equals: Fetch
      - path: "$.observed.net.requests[?(@.url == '{missing}')].status"
        equals: 404
      - path: "$.observed.net.requests[?(@.url == '{failed}')].status"
        equals: null
      - path: "$.observed.net.requests[?(@.url == '{failed}')].failed"
        exists: true
      - path: $.observed.net.pending
        equals: 0
  - browser.read:
      css: "#network-done"
    expect:
      - path: $.observed.net.requests
        equals: []
"##
        ),
    );

    let before_case = server.request_count();
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
        "every network assertion must pass: {value}"
    );
    let targets: Vec<String> = server
        .requests()
        .into_iter()
        .skip(before_case)
        .map(|req| req.target)
        .collect();
    assert!(
        !targets.iter().any(|target| target == "/favicon.ico"),
        "browser_network page must not trigger implicit /favicon.ico requests, saw: {targets:?}"
    );
}

#[test]
fn a_network_url_built_from_the_environment_is_redacted() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-network-secret");
    let secret = "browser-s3cr3t-network-url";
    let closed = closed_port();
    let case = dir.write(
        "secret.yaml",
        &format!(
            r##"name: network url secret
vars:
  token: "${{env:DA_NETWORK_TEST_SECRET}}"
observe:
  net:
    browser.network: {{}}
steps:
  - browser.open:
      url: "{}/browser/network?case=${{token}}&closed={closed}"
    expect:
      - path: "$.observed.net.requests[?(@.type == 'Document')].url"
        equals: "unexpected"
"##,
            server.url("")
        ),
    );
    let environment = [("DA_NETWORK_TEST_SECRET", secret.to_string())];

    for mode in ["text", "json"] {
        let output = run_da_with_env(
            dir.path(),
            &["--output", mode, "run", path_str(&case), "--no-bundle"],
            &environment,
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
            "{mode}: the output leaked the URL secret: {rendered}"
        );
        assert!(
            rendered.contains("***"),
            "{mode}: the output must show the redaction marker: {rendered}"
        );
    }

    let json = run_da_with_env(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
        &environment,
    );
    let value = json_stdout(&json);
    let actual = value["cases"][0]["steps"][0]["assertions"][0]["actual"]
        .as_str()
        .unwrap_or_else(|| panic!("the observed URL must be the actual value: {value}"));
    assert!(
        actual.contains("case=***"),
        "the observed request URL must be redacted where the environment value was: {actual}"
    );
    assert!(
        actual.contains("/browser/network"),
        "the observed request URL must still name the page: {actual}"
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

#[test]
fn schema_and_lint_validate_testid_within_nth_and_testid_attribute() {
    let dir = TempDir::new("browser-locator-modifiers");
    let schema_output = run_da_in(dir.path(), &["schema"]);
    assert_eq!(
        exit_code(&schema_output),
        Some(0),
        "da schema failed: {}",
        stderr(&schema_output)
    );
    let schema_value: Value =
        serde_json::from_str(&stdout(&schema_output)).expect("da schema must emit valid JSON");
    let validator =
        jsonschema::validator_for(&schema_value).expect("da schema must compile as Draft 2020-12");

    let valid_yaml = concat!(
        "name: Valid new locator modifiers\n",
        "steps:\n",
        "  - browser.open:\n",
        "      url: http://127.0.0.1:8080/\n",
        "      testid_attribute: data-qa\n",
        "  - browser.click:\n",
        "      testid: submit-btn\n",
        "      nth: 0\n",
        "      within:\n",
        "        css: form.checkout\n",
        "        within:\n",
        "          role: region\n",
        "          name: Payment\n",
        "          nth: 1\n",
        "  - browser.type:\n",
        "      testid: card-input\n",
        "      text: \"4242\"\n",
        "  - browser.wait:\n",
        "      testid: confirmation\n",
        "      within:\n",
        "        css: main\n",
        "  - browser.read:\n",
        "      testid: receipt\n",
        "      nth: 0\n",
    );
    let valid_path = dir.write("valid.yaml", valid_yaml);
    let valid_instance: Value = serde_saphyr::from_str(valid_yaml).expect("valid YAML must parse");
    assert!(
        validator.is_valid(&valid_instance),
        "schema must accept valid testid/within/nth/testid_attribute: {:?}",
        validator
            .iter_errors(&valid_instance)
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
    );
    let valid_lint = run_da_in(
        dir.path(),
        &["--output", "json", "lint", path_str(&valid_path)],
    );
    assert_eq!(
        exit_code(&valid_lint),
        Some(0),
        "lint must accept valid case: stdout: {}\nstderr: {}",
        stdout(&valid_lint),
        stderr(&valid_lint)
    );

    let invalid_cases = [
        (
            "empty_testid_attribute",
            "name: Bad\nsteps:\n  - browser.open:\n      url: http://127.0.0.1/\n      testid_attribute: \"\"\n",
        ),
        (
            "space_in_testid_attribute",
            "name: Bad\nsteps:\n  - browser.open:\n      url: http://127.0.0.1/\n      testid_attribute: \"data qa\"\n",
        ),
        (
            "multiple_locators_with_testid",
            "name: Bad\nsteps:\n  - browser.click:\n      css: \"#a\"\n      testid: btn\n",
        ),
        (
            "name_without_role_and_testid",
            "name: Bad\nsteps:\n  - browser.click:\n      testid: btn\n      name: Submit\n",
        ),
        (
            "negative_nth",
            "name: Bad\nsteps:\n  - browser.click:\n      testid: btn\n      nth: -1\n",
        ),
        (
            "float_nth",
            "name: Bad\nsteps:\n  - browser.click:\n      testid: btn\n      nth: 1.5\n",
        ),
        (
            "null_nth",
            "name: Bad\nsteps:\n  - browser.click:\n      testid: btn\n      nth: null\n",
        ),
        (
            "invalid_empty_within",
            "name: Bad\nsteps:\n  - browser.click:\n      testid: btn\n      within: {}\n",
        ),
        (
            "invalid_within_name_without_role",
            "name: Bad\nsteps:\n  - browser.read:\n      testid: label\n      within:\n        testid: panel\n        name: Panel\n",
        ),
        (
            "url_contains_with_within",
            "name: Bad\nsteps:\n  - browser.wait:\n      url_contains: /done\n      within:\n        css: main\n",
        ),
        (
            "url_contains_with_nth",
            "name: Bad\nsteps:\n  - browser.wait:\n      url_contains: /done\n      nth: 0\n",
        ),
        (
            "url_contains_with_testid",
            "name: Bad\nsteps:\n  - browser.wait:\n      url_contains: /done\n      testid: done\n",
        ),
    ];

    for (label, yaml) in invalid_cases {
        let instance: Value = serde_saphyr::from_str(yaml).expect("case YAML must parse");
        assert!(
            !validator.is_valid(&instance),
            "da schema must reject `{label}`"
        );
        let file = dir.write(&format!("{label}.yaml"), yaml);
        let lint = run_da_in(dir.path(), &["--output", "json", "lint", path_str(&file)]);
        assert_ne!(
            exit_code(&lint),
            Some(0),
            "da lint must reject `{label}`, got stdout: {}",
            stdout(&lint)
        );
    }
}

#[test]
fn testid_and_custom_testid_attribute_locate_and_act() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-testid");
    let url = server.url("/browser/locators");

    let default_case = dir.write(
        "default-testid.yaml",
        &format!(
            r##"name: default data-testid
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      testid: solo-btn
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: solo
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&default_case),
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

    let custom_case = dir.write(
        "custom-testid.yaml",
        &format!(
            r##"name: custom testid_attribute
steps:
  - browser.open:
      url: "{url}"
      testid_attribute: data-qa
  - browser.wait:
      testid: custom-btn
  - browser.click:
      testid: custom-btn
  - browser.type:
      testid: custom-input
      text: hello qa
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: qa
  - browser.read:
      testid: custom-btn
      attribute: data-qa
    expect:
      - path: $.value
        equals: custom-btn
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&custom_case),
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

    let hidden_case = dir.write(
        "hidden-testid.yaml",
        &format!(
            r##"name: hidden testid waits for visibility
steps:
  - browser.open:
      url: "{url}"
  - browser.read:
      testid: hidden-note
      timeout_ms: 250
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&hidden_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "hidden-testid must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("not visible"),
        "expected visibility error, got: {message}"
    );
}

#[test]
fn within_scopes_locators_and_supports_nesting_and_nth() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-within");
    let url = server.url("/browser/locators");

    let valid_case = dir.write(
        "within.yaml",
        &format!(
            r##"name: within scopes every locator kind
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      testid: save-btn
      within:
        css: ".group"
        within:
          role: region
          name: Beta Panel
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: beta
  - browser.click:
      xpath: "//button[@data-testid='save-btn']"
      within:
        css: "#panel-a"
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: alpha
  - browser.click:
      role: button
      name: Save
      within:
        css: "#panel-b"
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: beta
  - browser.click:
      text: Save
      within:
        css: "#panel-a"
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: alpha
  - browser.click:
      testid: save-btn
      within:
        css: ".panel"
        nth: 1
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: beta
  - browser.type:
      testid: note-input
      text: scoped text
      within:
        role: region
        name: Alpha Panel
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&valid_case),
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

    let ambiguous_within = dir.write(
        "ambiguous-within.yaml",
        &format!(
            r##"name: ambiguous within fails immediately
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      testid: save-btn
      timeout_ms: 10000
      within:
        css: ".panel"
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&ambiguous_within),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "ambiguous-within must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("2") && message.contains(".panel"),
        "ambiguous `within` must report count 2 and container locator, got: {message}"
    );
}

#[test]
fn nth_selects_zero_based_match_waits_for_async_match_and_fails_when_out_of_range() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-nth");
    let url = server.url("/browser/locators");

    let case = dir.write(
        "nth.yaml",
        &format!(
            r##"name: nth indexing
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      testid: nth-item
      nth: 1
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: nth-1
  - browser.click:
      css: ".nth-btn"
      nth: 2
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: nth-2
  - browser.click:
      css: ".hidden-plus-visible"
      nth: 1
  - browser.click:
      css: "#spawn-second"
  - browser.click:
      css: ".async-nth"
      nth: 1
      timeout_ms: 2000
  - browser.read:
      css: "#locator-result"
    expect:
      - path: $.text
        equals: async-1
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

    let oob_case = dir.write(
        "nth-oob.yaml",
        &format!(
            r##"name: nth out of range fails
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: ".nth-btn"
      nth: 5
      timeout_ms: 300
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&oob_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "nth-oob must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("5") && message.contains("3") && message.contains("300"),
        "out-of-range nth must name index 5, count 3, and timeout 300, got: {message}"
    );
}

#[test]
fn multiple_matches_fail_immediately_for_all_five_locators_and_redact_secrets() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-strict-unique");
    let url = server.url("/browser/locators");

    for (name, locator_yaml, fragment) in [
        (
            "dup-css.yaml",
            "      css: \".dup-gate-btn\"\n",
            ".dup-gate-btn",
        ),
        (
            "dup-xpath.yaml",
            "      xpath: \"//button[@class='dup-gate-btn']\"\n",
            "dup-gate-btn",
        ),
        (
            "dup-text.yaml",
            "      text: \"Gate Duplicate\"\n",
            "Gate Duplicate",
        ),
        (
            "dup-role.yaml",
            "      role: button\n      name: \"Gate Duplicate\"\n",
            "Gate Duplicate",
        ),
        (
            "dup-testid.yaml",
            "      testid: \"dup-gate-testid\"\n",
            "dup-gate-testid",
        ),
        (
            "dup-hidden-css.yaml",
            "      css: \".hidden-plus-visible\"\n",
            ".hidden-plus-visible",
        ),
        (
            "dup-hidden-testid.yaml",
            "      testid: \"hv-btn\"\n",
            "hv-btn",
        ),
    ] {
        let case = dir.write(
            name,
            &format!(
                "name: strict match {name}\nsteps:\n  - browser.open:\n      url: \"{url}\"\n  - browser.click:\n{locator_yaml}      timeout_ms: 10000\n"
            ),
        );
        let output = run_da_in(
            dir.path(),
            &["--output", "json", "run", path_str(&case), "--no-bundle"],
        );
        assert_eq!(
            exit_code(&output),
            Some(1),
            "{name} must fail: stdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        let value = json_stdout(&output);
        let message = case_error(&value);
        assert!(
            message.contains('2') && message.contains(fragment),
            "{name}: error must mention count 2 and locator `{fragment}`, got: {message}"
        );
        let duration_ms = value["cases"][0]["steps"][1]["duration_ms"]
            .as_u64()
            .unwrap_or(u64::MAX);
        assert!(
            duration_ms < 5000,
            "{name}: multi-match must fail immediately instead of waiting for timeout_ms (took {duration_ms} ms)"
        );
    }

    let secret = "locator-secret-xyz-987";
    let many_case = dir.write(
        "many-redacted.yaml",
        &format!(
            r##"name: many candidates truncated and redacted
steps:
  - browser.open:
      url: "{url}?secret=${{env:DA_LOCATOR_SECRET}}"
  - browser.read:
      css: ".many-item"
      timeout_ms: 10000
"##
        ),
    );
    let output = run_da_with_env(
        dir.path(),
        &["--output", "json", "run", path_str(&many_case)],
        &[("DA_LOCATOR_SECRET", secret.to_string())],
    );
    let raw_stdout = stdout(&output);
    assert_eq!(
        exit_code(&output),
        Some(1),
        "many-redacted must fail: stdout: {raw_stdout}\nstderr: {}",
        stderr(&output)
    );
    assert!(
        !raw_stdout.contains(secret),
        "secret leaked in JSON stdout: {raw_stdout}"
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains('7')
            && message.contains("li")
            && message.contains("many-1")
            && message.contains("many-2")
            && message.contains("***"),
        "expected count 7, tag, ids, and redacted secret in error, got: {message}"
    );
    assert!(
        !message.contains("definitely exceeds sixty characters in length"),
        "candidate text longer than 60 chars must be truncated, got: {message}"
    );
    assert!(
        !message.contains("Item 6") && !message.contains("Item 7"),
        "at most 5 candidates may appear in error, got: {message}"
    );

    let bundle_rel = value["cases"][0]["bundle"]
        .as_str()
        .expect("failed case must write a failure bundle");
    let report_path = dir.path().join(bundle_rel).join("report.json");
    let report_text = fs::read_to_string(&report_path).expect("report.json must exist");
    assert!(
        !report_text.contains(secret),
        "secret leaked in bundle report.json: {report_text}"
    );
}

#[test]
fn click_and_type_wait_for_enabled_stable_and_unoccluded_elements() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-actionability-wait");
    let url = server.url("/browser/actionability");

    let case = dir.write(
        "actionability-wait.yaml",
        &format!(
            r##"name: click and type wait for actionability checks to pass
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#start-enable"
  - browser.click:
      css: "#enable-later"
      timeout_ms: 2000
  - browser.read:
      css: "#action-result"
    expect:
      - path: $.text
        equals: enabled-clicked
  - browser.click:
      css: "#start-enable-input"
  - browser.type:
      css: "#enable-input-later"
      text: now ready
      timeout_ms: 2000
  - browser.read:
      css: "#action-result"
    expect:
      - path: $.text
        equals: aria-typed:now ready
  - browser.click:
      css: "#start-move"
  - browser.click:
      css: "#settle-later"
      timeout_ms: 2000
  - browser.read:
      css: "#action-result"
    expect:
      - path: $.text
        equals: settle:yes
  - browser.click:
      css: "#start-uncover"
  - browser.click:
      css: "#uncover-later"
      timeout_ms: 2000
  - browser.read:
      css: "#action-result"
    expect:
      - path: $.text
        equals: uncovered-clicked
  - browser.click:
      css: "#child-btn"
  - browser.read:
      css: "#action-result"
    expect:
      - path: $.text
        equals: child-clicked
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
}

#[test]
fn actionability_timeouts_name_failed_check_and_blocker_while_read_and_wait_stay_unchanged() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-actionability-fail");
    let url = server.url("/browser/actionability");

    // 1. disabled attribute on click -> fails with disabled check name
    let disabled_click = dir.write(
        "disabled-click.yaml",
        &format!(
            r##"name: disabled click times out naming disabled check
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#always-disabled"
      timeout_ms: 250
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&disabled_click),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "disabled-click must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("disabled"),
        "expected disabled actionability timeout, got: {message}"
    );

    // 2. aria-disabled="true" on click -> fails with disabled check name
    let aria_disabled_click = dir.write(
        "aria-disabled-click.yaml",
        &format!(
            r##"name: aria-disabled click times out naming disabled check
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#always-aria-disabled"
      timeout_ms: 250
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&aria_disabled_click),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "aria-disabled-click must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("disabled"),
        "expected aria-disabled actionability timeout, got: {message}"
    );

    // 3. disabled attribute on type -> fails with disabled check name
    let disabled_type = dir.write(
        "disabled-type.yaml",
        &format!(
            r##"name: disabled type times out naming disabled check
steps:
  - browser.open:
      url: "{url}"
  - browser.type:
      css: "#disabled-input"
      text: ignored
      timeout_ms: 250
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&disabled_type),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "disabled-type must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("disabled"),
        "expected disabled type timeout, got: {message}"
    );

    // 4. moving element on click -> fails naming unstable / moving position
    let moving_click = dir.write(
        "moving-click.yaml",
        &format!(
            r##"name: moving element click times out naming stability check
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#always-moving"
      timeout_ms: 300
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&moving_click),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "moving-click must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("stable") || message.contains("moving"),
        "expected stability actionability timeout, got: {message}"
    );

    // 5. occluded click -> fails naming occluding #blocker-overlay element
    let occluded_click = dir.write(
        "occluded-click.yaml",
        &format!(
            r##"name: occluded click times out naming blocking element
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#occluded-btn"
      timeout_ms: 300
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&occluded_click),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "occluded-click must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("blocker-overlay"),
        "expected occluding element `blocker-overlay` in error, got: {message}"
    );

    // 6. occluded type -> fails naming occluding #input-blocker element
    let occluded_type = dir.write(
        "occluded-type.yaml",
        &format!(
            r##"name: occluded type times out naming blocking element
steps:
  - browser.open:
      url: "{url}"
  - browser.type:
      css: "#occluded-input"
      text: blocked
      timeout_ms: 300
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&occluded_type),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "occluded-type must fail: stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let message = case_error(&value);
    assert!(
        message.contains("input-blocker"),
        "expected occluding element `input-blocker` in error, got: {message}"
    );

    // 7. read and wait only check existence + visibility, so disabled/moving/occluded still succeed
    let read_wait_case = dir.write(
        "read-and-wait-unchanged.yaml",
        &format!(
            r##"name: read and wait ignore disabled moving and occluded checks
steps:
  - browser.open:
      url: "{url}"
  - browser.wait:
      css: "#always-disabled"
  - browser.read:
      css: "#always-disabled"
    expect:
      - path: $.text
        equals: Always Disabled
  - browser.wait:
      css: "#always-moving"
  - browser.wait:
      css: "#occluded-btn"
  - browser.read:
      css: "#occluded-btn"
    expect:
      - path: $.text
        equals: Occluded Target
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&read_wait_case),
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
}

#[test]
fn unsupported_capabilities_probe_current_behavior() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-probes");

    // 1. iframe (unscoped locator stays in the main document; `within` enters same-origin iframes)
    let iframe_case = dir.write(
        "probe-iframe.yaml",
        &format!(
            "name: probe iframe\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.click:\n      css: \"#inside-frame\"\n      timeout_ms: 400\n",
            server.url("/browser/probe/iframe")
        ),
    );
    let iframe_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&iframe_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&iframe_out),
        Some(1),
        "unexpected iframe probe exit code: stdout={}\nstderr={}",
        stdout(&iframe_out),
        stderr(&iframe_out)
    );
    let iframe_json = json_stdout(&iframe_out);
    let iframe_err = case_error(&iframe_json);
    assert!(
        iframe_err.contains("could not find CSS selector `#inside-frame`"),
        "unexpected iframe probe error: {iframe_err}"
    );

    let scoped_iframe_case = dir.write(
        "probe-iframe-within.yaml",
        &format!(
            "name: probe iframe within\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.click:\n      css: \"#inside-frame\"\n      within:\n        css: \"#probe-frame\"\n  - browser.read:\n      css: \"#probe-status\"\n      within:\n        css: \"#probe-frame\"\n    expect:\n      - path: $.text\n        equals: clicked\n",
            server.url("/browser/probe/iframe")
        ),
    );
    let scoped_iframe_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&scoped_iframe_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&scoped_iframe_out),
        Some(0),
        "scoped iframe probe must succeed: stdout={}\nstderr={}",
        stdout(&scoped_iframe_out),
        stderr(&scoped_iframe_out)
    );

    // 2. shadow DOM (locators and actionability pierce open shadow roots)
    let shadow_css = dir.write(
        "probe-shadow-css.yaml",
        &format!(
            "name: probe shadow css\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.click:\n      css: \"#inside-shadow\"\n  - browser.read:\n      css: \"#shadow-status\"\n    expect:\n      - path: $.text\n        equals: clicked\n",
            server.url("/browser/probe/shadow")
        ),
    );
    let shadow_css_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&shadow_css),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&shadow_css_out),
        Some(0),
        "the css locator must pierce an open shadow root: stdout={}\nstderr={}",
        stdout(&shadow_css_out),
        stderr(&shadow_css_out)
    );

    let shadow_role = dir.write(
        "probe-shadow-role.yaml",
        &format!(
            "name: probe shadow role\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.click:\n      role: button\n      name: Inside Shadow\n  - browser.read:\n      css: \"#shadow-status\"\n    expect:\n      - path: $.text\n        equals: clicked\n",
            server.url("/browser/probe/shadow")
        ),
    );
    let shadow_role_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&shadow_role),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&shadow_role_out),
        Some(0),
        "a role click on a shadow child must not report its own host as the blocker: stdout={}\nstderr={}",
        stdout(&shadow_role_out),
        stderr(&shadow_role_out)
    );

    // 3. new tab (target="_blank" opens a second tab; the session stays on the
    //    original tab until `browser.switch_tab` names the new one)
    let new_tab_case = dir.write(
        "probe-new-tab.yaml",
        &format!(
            "name: probe new tab\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.click:\n      css: \"#open-new-tab\"\n  - browser.read:\n      css: \"#new-tab-marker\"\n      timeout_ms: 400\n",
            server.url("/browser/probe/new-tab")
        ),
    );
    let new_tab_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&new_tab_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&new_tab_out),
        Some(1),
        "unexpected new-tab probe exit code: stdout={}\nstderr={}",
        stdout(&new_tab_out),
        stderr(&new_tab_out)
    );
    let new_tab_json = json_stdout(&new_tab_out);
    let new_tab_err = case_error(&new_tab_json);
    assert!(
        new_tab_err.contains("could not find CSS selector `#new-tab-marker`"),
        "a click that opens a tab must not move the session by itself: {new_tab_err}"
    );

    // The same page after an explicit switch: the tab the click opened is the
    // one every later step then drives.
    let switched_case = dir.write(
        "probe-new-tab-switched.yaml",
        &format!(
            "name: probe new tab switched\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.click:\n      css: \"#open-new-tab\"\n  - browser.switch_tab:\n      url_contains: \"/browser/probe/new-tab-target\"\n  - browser.read:\n      css: \"#new-tab-marker\"\n    expect:\n      - path: $.text\n        equals: \"Opened\"\n",
            server.url("/browser/probe/new-tab")
        ),
    );
    let switched_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&switched_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&switched_out),
        Some(0),
        "switching to the opened tab must reach its content: stdout={}\nstderr={}",
        stdout(&switched_out),
        stderr(&switched_out)
    );

    // 4. dialog (window.alert fails fast with dialog type and message instead of waiting for timeout)
    let dialog_case = dir.write(
        "probe-dialog.yaml",
        &format!(
            "name: probe dialog\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.click:\n      css: \"#trigger-alert\"\n      timeout_ms: 5000\n",
            server.url("/browser/probe/dialog")
        ),
    );
    let dialog_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&dialog_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&dialog_out),
        Some(1),
        "unexpected dialog probe exit code: stdout={}\nstderr={}",
        stdout(&dialog_out),
        stderr(&dialog_out)
    );
    let dialog_json = json_stdout(&dialog_out);
    let dialog_step_ms = dialog_json["cases"][0]["steps"][1]["duration_ms"]
        .as_u64()
        .unwrap_or(u64::MAX);
    assert!(
        dialog_step_ms < 3000,
        "unhandled dialog must fail fast instead of waiting 5000 ms (step took {dialog_step_ms} ms): {}",
        stdout(&dialog_out)
    );
    let dialog_err = case_error(&dialog_json);
    assert!(
        dialog_err.contains("alert") && dialog_err.contains("probe alert"),
        "unexpected dialog probe error: {dialog_err}"
    );

    // 5. file upload (browser.type attaches no file; browser.upload is the step
    //    that sets one, and the page's change handler proves the event fired)
    dir.write("sample.txt", "probe upload bytes\n");
    let upload_case = dir.write(
        "probe-upload.yaml",
        &format!(
            r##"name: probe file upload
steps:
  - browser.open:
      url: "{url}"
  - browser.type:
      css: "#file-upload"
      text: "sample.txt"
      timeout_ms: 5000
  - browser.read:
      css: "#file-upload"
      attribute: value
    expect:
      - path: $.value
        equals: null
      - path: $.text
        equals: ""
  - browser.read:
      css: "#file-upload-events"
    expect:
      - path: $.text
        equals: "idle"
  - browser.upload:
      css: "#file-upload"
      files: ["sample.txt"]
    expect:
      - path: $.files
        equals: 1
      - path: $.names
        equals: ["sample.txt"]
  - browser.read:
      css: "#file-upload-events"
    expect:
      - path: $.text
        equals: "change:1"
"##,
            url = server.url("/browser/probe/upload")
        ),
    );
    let upload_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&upload_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&upload_out),
        Some(0),
        "unexpected upload probe failure: stdout={}\nstderr={}",
        stdout(&upload_out),
        stderr(&upload_out)
    );

    // 6. coordinate click (a canvas painting has no DOM node to name, so `at`
    //    coordinates click it and the page proves the click landed)
    let coords_case = dir.write(
        "probe-coords.yaml",
        &format!(
            r##"name: probe coords
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      at: {{ x: 100, y: 50 }}
    expect:
      - path: $.x
        equals: 100
      - path: $.y
        equals: 50
      - path: $.viewport.width
        exists: true
  - browser.read:
      css: "#probe-status"
    expect:
      - path: $.text
        equals: canvas 100,50
"##,
            url = server.url("/browser/probe/coords")
        ),
    );
    let coords_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&coords_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&coords_out),
        Some(0),
        "a coordinate click on a canvas must succeed: stdout={}\nstderr={}",
        stdout(&coords_out),
        stderr(&coords_out)
    );
    let coords_json = json_stdout(&coords_out);
    assert_eq!(
        coords_json["cases"][0]["steps"][1]["result"]["x"],
        json!(100),
        "the step must report the clicked x: {coords_json}"
    );
    assert_eq!(
        coords_json["cases"][0]["steps"][1]["result"]["y"],
        json!(50),
        "the step must report the clicked y: {coords_json}"
    );
}

#[test]
fn explicit_coordinates_pin_the_recorded_viewport() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-coordinates");
    let url = server.url("/browser/probe/coords");

    // A bare `at` is allowed. The step reports the live viewport, which is how a
    // caller records the size the coordinates were measured at.
    let bare = dir.write(
        "bare-coords.yaml",
        &format!(
            r##"name: bare coordinates
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      at: {{ x: 100, y: 50 }}
    expect:
      - path: $.x
        equals: 100
      - path: $.y
        equals: 50
  - browser.read:
      css: "#probe-status"
    expect:
      - path: $.text
        equals: canvas 100,50
"##
        ),
    );
    let bare_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&bare), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&bare_out),
        Some(0),
        "a bare `at` must run: stdout={}\nstderr={}",
        stdout(&bare_out),
        stderr(&bare_out)
    );
    let bare_json = json_stdout(&bare_out);
    let viewport = &bare_json["cases"][0]["steps"][1]["result"]["viewport"];
    let width = viewport["width"]
        .as_u64()
        .unwrap_or_else(|| panic!("a coordinate step must report viewport width: {bare_json}"));
    let height = viewport["height"]
        .as_u64()
        .unwrap_or_else(|| panic!("a coordinate step must report viewport height: {bare_json}"));
    assert!(width > 0 && height > 0, "got viewport {width}x{height}");

    // The same click with the recorded viewport replays at that size.
    let pinned = dir.write(
        "pinned-coords.yaml",
        &format!(
            r##"name: pinned coordinates
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      at:
        x: 100
        y: 50
        viewport:
          width: {width}
          height: {height}
  - browser.read:
      css: "#probe-status"
    expect:
      - path: $.text
        equals: canvas 100,50
"##
        ),
    );
    let pinned_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&pinned), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&pinned_out),
        Some(0),
        "a coordinate click at its recorded viewport must succeed: stdout={}\nstderr={}",
        stdout(&pinned_out),
        stderr(&pinned_out)
    );
    let pinned_json = json_stdout(&pinned_out);
    assert_eq!(
        pinned_json["cases"][0]["steps"][1]["result"]["viewport"],
        json!({"width": width, "height": height}),
        "the step must report the viewport it used: {pinned_json}"
    );

    // `browser.type` clicks the coordinates to focus the target, then types.
    let typed = dir.write(
        "typed-coords.yaml",
        &format!(
            r##"name: typed coordinates
steps:
  - browser.open:
      url: "{url}"
  - browser.type:
      at:
        x: 100
        y: 140
        viewport:
          width: {width}
          height: {height}
      text: hello
  - browser.read:
      css: "#probe-editor"
    expect:
      - path: $.text
        equals: hello
"##
        ),
    );
    let typed_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&typed), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&typed_out),
        Some(0),
        "`browser.type` must focus the clicked coordinates and type: stdout={}\nstderr={}",
        stdout(&typed_out),
        stderr(&typed_out)
    );
    let typed_json = json_stdout(&typed_out);
    assert_eq!(
        typed_json["cases"][0]["steps"][1]["result"]["x"],
        json!(100),
        "`browser.type` must report the clicked x: {typed_json}"
    );

    // A different declared viewport fails loudly before any click is sent.
    let mismatch_width = width + 1;
    let mismatch = dir.write(
        "mismatched-coords.yaml",
        &format!(
            r##"name: mismatched coordinates
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      at:
        x: 100
        y: 50
        viewport:
          width: {mismatch_width}
          height: {height}
"##
        ),
    );
    let mismatch_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&mismatch),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&mismatch_out),
        Some(1),
        "a different live viewport must fail the step: stdout={}\nstderr={}",
        stdout(&mismatch_out),
        stderr(&mismatch_out)
    );
    let mismatch_json = json_stdout(&mismatch_out);
    let mismatch_error = case_error(&mismatch_json);
    assert!(
        mismatch_error.contains(&format!("{mismatch_width}x{height}"))
            && mismatch_error.contains(&format!("{width}x{height}")),
        "the error must name the recorded and the live viewport: {mismatch_error}"
    );
    assert!(
        mismatch_error.contains("viewport"),
        "the error must explain the viewport mismatch: {mismatch_error}"
    );

    // A coordinate outside the live viewport is refused as well.
    let outside = dir.write(
        "outside-coords.yaml",
        &format!(
            r##"name: outside coordinates
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      at: {{ x: {outside_x}, y: 10 }}
"##,
            outside_x = width + 10
        ),
    );
    let outside_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&outside), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&outside_out),
        Some(1),
        "a coordinate outside the viewport must fail the step: stdout={}\nstderr={}",
        stdout(&outside_out),
        stderr(&outside_out)
    );
    let outside_json = json_stdout(&outside_out);
    let outside_error = case_error(&outside_json);
    assert!(
        outside_error.contains("outside") && outside_error.contains(&format!("{width}x{height}")),
        "the error must name the coordinate and the viewport: {outside_error}"
    );
}

#[test]
fn a_coordinate_click_works_right_after_switching_tabs() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-coords-tab");
    let case = dir.write(
        "coords-tab.yaml",
        &format!(
            r##"name: coordinates after switching tabs
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#probe-tab-link"
  - browser.switch_tab:
      url_contains: "case=tab"
  - browser.click:
      at: {{ x: 100, y: 50 }}
    expect:
      - path: $.x
        equals: 100
      - path: $.y
        equals: 50
      - path: $.viewport.width
        exists: true
  - browser.read:
      css: "#probe-status"
    expect:
      - path: $.text
        equals: canvas 100,50
"##,
            url = server.url("/browser/probe/coords")
        ),
    );
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&output),
        Some(0),
        "a coordinate click after `browser.switch_tab` must succeed: stdout={}\nstderr={}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert!(
        value["cases"][0]["steps"][3]["result"]["viewport"]["width"]
            .as_u64()
            .is_some_and(|width| width > 0),
        "the coordinate click must report the switched tab's viewport: {value}"
    );
    assert_eq!(
        value["cases"][0]["steps"][4]["assertions"][0]["passed"],
        json!(true),
        "the coordinate click must hit the canvas of the switched tab: {value}"
    );
}

#[test]
fn a_coordinate_click_reaches_into_an_open_shadow_root_page() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-coords-shadow");
    let case = dir.write(
        "coords-shadow.yaml",
        &format!(
            r##"name: coordinates on a page with an open shadow root
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      at: {{ x: 260, y: 50 }}
    expect:
      - path: $.x
        equals: 260
      - path: $.y
        equals: 50
  - browser.read:
      css: "#probe-status"
    expect:
      - path: $.text
        equals: shadow-clicked
  - browser.click:
      at: {{ x: 100, y: 50 }}
  - browser.read:
      css: "#probe-status"
    expect:
      - path: $.text
        equals: canvas 100,50
  - browser.click:
      role: button
      name: Shadow target
  - browser.read:
      css: "#probe-status"
    expect:
      - path: $.text
        equals: shadow-clicked
"##,
            url = server.url("/browser/probe/coords")
        ),
    );
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&output),
        Some(0),
        "coordinates must work on a page that contains an open shadow root: stdout={}\nstderr={}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn switch_tab_selects_a_tab_by_url_and_switches_back_by_title() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-switch-tab-select");
    let case = dir.write(
        "switch-select.yaml",
        &format!(
            r##"name: switch by url and back by title
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#open-new-tab"
  - browser.switch_tab:
      url_contains: "/browser/probe/new-tab-target"
    expect:
      - path: $.url
        contains: "/browser/probe/new-tab-target"
      - path: $.title
        equals: "New Tab Target"
  - browser.read:
      css: "#new-tab-marker"
    expect:
      - path: $.text
        equals: "Opened"
  - browser.switch_tab:
      title_contains: "Probe New Tab"
    expect:
      - path: $.title
        equals: "Probe New Tab"
  - browser.read:
      css: "#open-new-tab"
    expect:
      - path: $.text
        equals: "Open Tab"
"##,
            server.url("/browser/probe/new-tab")
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
}

#[test]
fn switch_tab_selects_the_most_recently_opened_tab_and_by_index() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-switch-tab-order");
    let case = dir.write(
        "switch-order.yaml",
        &format!(
            r##"name: switch by creation order
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#open-new-tab"
  - browser.click:
      css: "#open-second-tab"
  - browser.switch_tab:
      timeout_ms: 5000
    expect:
      - path: $.title
        equals: "Second Tab"
  - browser.read:
      css: "#second-marker"
    expect:
      - path: $.text
        equals: "Second"
  - browser.switch_tab:
      index: 1
    expect:
      - path: $.title
        equals: "New Tab Target"
  - browser.switch_tab:
      index: 0
    expect:
      - path: $.title
        equals: "Probe New Tab"
"##,
            server.url("/browser/probe/new-tab")
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
}

#[test]
fn switch_tab_times_out_with_the_tabs_it_could_see() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-switch-tab-timeout");
    let nothing_opened = dir.write(
        "switch-no-tab.yaml",
        &format!(
            r##"name: switch with no tab opened
steps:
  - browser.open:
      url: "{}"
  - browser.switch_tab:
      timeout_ms: 400
"##,
            server.url("/browser/probe/new-tab")
        ),
    );
    let no_match = dir.write(
        "switch-no-match.yaml",
        &format!(
            r##"name: switch with no match
steps:
  - browser.open:
      url: "{}"
  - browser.switch_tab:
      url_contains: "/browser/probe/absent"
      timeout_ms: 400
"##,
            server.url("/browser/probe/new-tab")
        ),
    );
    let no_position = dir.write(
        "switch-no-position.yaml",
        &format!(
            r##"name: switch with no position
steps:
  - browser.open:
      url: "{}"
  - browser.switch_tab:
      index: 4
      timeout_ms: 400
"##,
            server.url("/browser/probe/new-tab")
        ),
    );

    let mut messages = Vec::new();
    for case in [&nothing_opened, &no_match, &no_position] {
        let output = run_da_in(
            dir.path(),
            &["--output", "json", "run", path_str(case), "--no-bundle"],
        );
        assert_eq!(
            exit_code(&output),
            Some(1),
            "stdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
        let value = json_stdout(&output);
        messages.push(case_error(&value).to_string());
    }

    assert!(
        messages[0].contains("no tab was opened by this case's pages within 400 ms"),
        "the message must say that nothing opened a tab: {}",
        messages[0]
    );
    assert!(
        messages[0].contains("tabs: 0: "),
        "the message must list the tabs the case owns: {}",
        messages[0]
    );
    assert!(
        messages[1]
            .contains("no tab matched a URL containing `/browser/probe/absent` within 400 ms"),
        "the message must name the URL filter: {}",
        messages[1]
    );
    assert!(
        messages[2].contains("no tab matched position 4 within 400 ms"),
        "the message must name the position: {}",
        messages[2]
    );
}

#[test]
fn switch_tab_reports_more_than_one_matching_tab() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-switch-tab-ambiguous");
    let case = dir.write(
        "switch-ambiguous.yaml",
        &format!(
            r##"name: switch with two matches
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#open-new-tab"
  - browser.switch_tab:
      url_contains: "/browser/probe/new-tab-target"
  - browser.switch_tab:
      url_contains: "/browser/probe/new-tab"
"##,
            server.url("/browser/probe/new-tab")
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
        message.contains("`browser.switch_tab` matched 2 tabs"),
        "the message must count the matches: {message}"
    );
    assert!(
        message.contains("candidates: 0:") && message.contains("1:"),
        "the message must list both candidates: {message}"
    );
}

#[test]
fn switch_tab_applies_read_write_screenshot_and_network_to_the_new_tab() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-switch-tab-work");
    let fetch_url = server.url("/browser/probe/new-tab-fetch?case=tab");
    let case = dir.write(
        "switch-work.yaml",
        &format!(
            r##"name: work on the opened tab
observe:
  net:
    browser.network: {{}}
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#open-new-tab"
  - browser.switch_tab:
      url_contains: "/browser/probe/new-tab-target"
    expect:
      - path: $.title
        equals: "New Tab Target"
  - browser.click:
      css: "#new-tab-fetch"
  - browser.wait:
      css: "#new-tab-fetched"
      timeout_ms: 10000
    expect:
      - path: "$.observed.net.requests[?(@.url == '{fetch_url}')].status"
        equals: 200
  - browser.read:
      css: "#new-tab-marker"
    expect:
      - path: $.text
        equals: "Opened"
  - browser.click:
      css: "#new-tab-shadow-button"
    expect:
      - path: $.text
        equals: "Shadow Button"
  - browser.read:
      css: "#new-tab-shadow-status"
    expect:
      - path: $.text
        equals: "shadow-clicked"
  - browser.screenshot:
      path: "new-tab.png"
    expect:
      - path: $.bytes
        exists: true
"##,
            server.url("/browser/probe/new-tab")
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
    let png = fs::read(dir.path().join("new-tab.png"))
        .expect("the screenshot of the switched tab must be written");
    assert!(
        is_png(&png),
        "the screenshot must be a PNG: {:?}",
        &png[..png.len().min(8)]
    );
}

#[test]
fn failure_evidence_after_a_switch_is_the_new_tabs_document() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-switch-tab-evidence");
    let case = dir.write(
        "switch-evidence.yaml",
        &format!(
            r##"name: evidence of the opened tab
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#open-new-tab"
  - browser.switch_tab:
      url_contains: "/browser/probe/new-tab-target"
  - browser.read:
      css: "#absent-marker"
      timeout_ms: 300
"##,
            server.url("/browser/probe/new-tab")
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
    let dom = fs::read_to_string(bundle.join("browser/dom.html"))
        .expect("the bundle must hold the DOM snapshot");
    assert!(
        dom.contains("id=\"new-tab-marker\""),
        "the DOM evidence must be the tab the case switched to: {dom}"
    );
    assert!(
        !dom.contains("id=\"open-new-tab\""),
        "the DOM evidence must not be the tab the case left: {dom}"
    );
    let png = fs::read(bundle.join("browser/screenshot.png"))
        .expect("the bundle must hold the screenshot of the switched tab");
    assert!(is_png(&png));
}

#[test]
fn connect_switch_tab_closes_the_tabs_the_case_opened_and_keeps_the_users_tab() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let mut browser = UserBrowser::start();
    let user_tab = browser.open_tab(&server.url("/browser/form?case=user-tab"));
    let before = wait_for_pages(&browser, 2);
    let initial_ids: Vec<String> = before
        .iter()
        .map(|page| page_id(page).to_string())
        .collect();

    let dir = TempDir::new("browser-switch-tab-connect");
    let case = dir.write(
        "switch-connect.yaml",
        &format!(
            r##"name: switch tabs in the connected browser
steps:
  - browser.open:
      connect: "{}"
      url: "{}"
  - browser.click:
      css: "#open-new-tab"
  - browser.switch_tab:
      url_contains: "/browser/probe/new-tab-target"
    expect:
      - path: $.title
        equals: "New Tab Target"
  - browser.read:
      css: "#new-tab-marker"
    expect:
      - path: $.text
        equals: "Opened"
"##,
            browser.connect_url(),
            server.url("/browser/probe/new-tab?case=connect-tab")
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}\n{}",
        stdout(&output),
        stderr(&output),
        server.timing_report()
    );

    assert!(
        browser
            .child
            .try_wait()
            .expect("the test must watch the browser process")
            .is_none(),
        "connect mode must not end the user's browser"
    );

    // The user's own tab matches the filter, but no tab of this case was opened
    // by it: the step must not adopt it.
    let forbidden = dir.write(
        "switch-user-tab.yaml",
        &format!(
            r##"name: never switch to the user tab
steps:
  - browser.open:
      connect: "{}"
      url: "{}"
  - browser.switch_tab:
      url_contains: "case=user-tab"
      timeout_ms: 400
"##,
            browser.connect_url(),
            server.url("/browser/probe/new-tab?case=forbidden-tab")
        ),
    );
    let forbidden_output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&forbidden),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&forbidden_output),
        Some(1),
        "a tab the case does not own must not be selectable: stdout={}\nstderr={}",
        stdout(&forbidden_output),
        stderr(&forbidden_output)
    );
    let forbidden_json = json_stdout(&forbidden_output);
    let forbidden_error = case_error(&forbidden_json);
    assert!(
        forbidden_error.contains("no tab matched a URL containing `case=user-tab` within 400 ms"),
        "the step must report that no tab of the case matched: {forbidden_error}"
    );
    assert!(
        !forbidden_error.contains("Browser form"),
        "the user's tab must not appear among the case's tabs: {forbidden_error}"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut after = browser.page_tabs();
    while after.len() > before.len() {
        assert!(
            Instant::now() < deadline,
            "the case left a tab it opened open: {after:?}"
        );
        thread::sleep(Duration::from_millis(50));
        after = browser.page_tabs();
    }
    assert_eq!(
        after.len(),
        before.len(),
        "the case must close the tab it opened and the tab its page opened: {after:?}"
    );
    let after_ids: Vec<&str> = after.iter().map(page_id).collect();
    for id in &initial_ids {
        assert!(
            after_ids.contains(&id.as_str()),
            "the case must leave the tab {id} it found open: {after:?}"
        );
    }
    assert!(
        after_ids.contains(&user_tab.as_str()),
        "the case must leave the user tab open: {after:?}"
    );
    assert!(
        after.iter().all(|page| !page["url"]
            .as_str()
            .unwrap_or_default()
            .contains("case=connect-tab")),
        "the case's own tab must be closed: {after:?}"
    );
}

#[test]
fn a_session_keeps_the_switched_tab_across_commands_and_closes_every_tab_it_opened() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let mut browser = UserBrowser::start();
    let user_tab = browser.open_tab(&server.url("/browser/form?case=session-user"));
    let before = wait_for_pages(&browser, 2);

    let dir = TempDir::new("browser-session-switch-tab");
    let started = json_stdout(&run_da_in(
        dir.path(),
        &["--output", "json", "session", "start"],
    ));
    let id = started["session_id"]
        .as_str()
        .expect("session start must name the session")
        .to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("connect={}", browser.connect_url()),
            "--arg",
            &format!(
                "url={}",
                server.url("/browser/probe/new-tab?case=session-tab")
            ),
        ],
    );
    assert_eq!(
        exit_code(&open),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&open),
        stderr(&open)
    );

    let click = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &id,
            "browser.click",
            "--arg",
            "css=#open-new-tab",
        ],
    );
    assert_eq!(
        exit_code(&click),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&click),
        stderr(&click)
    );

    // A later command reattaches the session; it must still see the tab the
    // click opened and switch to it there.
    let switch = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &id,
            "browser.switch_tab",
            "--arg",
            "url_contains=/browser/probe/new-tab-target",
        ],
    );
    assert_eq!(
        exit_code(&switch),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&switch),
        stderr(&switch)
    );
    let switch_json = json_stdout(&switch);
    assert_eq!(
        switch_json["snapshot"]["title"], "New Tab Target",
        "the session snapshot must be the tab the step switched to: {switch_json}"
    );

    let read = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &id,
            "browser.read",
            "--arg",
            "css=#new-tab-marker",
        ],
    );
    assert_eq!(
        exit_code(&read),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&read),
        stderr(&read)
    );
    assert_eq!(
        json_stdout(&read)["step"]["result"]["text"],
        "Opened",
        "a later command must read the switched tab"
    );

    let stop = run_da_in(dir.path(), &["--output", "json", "session", "stop", &id]);
    assert_eq!(
        exit_code(&stop),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&stop),
        stderr(&stop)
    );

    assert!(
        browser
            .child
            .try_wait()
            .expect("the test must watch the browser process")
            .is_none(),
        "stopping a connected session must not end the user's browser"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut after = browser.page_tabs();
    while after.len() > before.len() {
        assert!(
            Instant::now() < deadline,
            "the session left a tab it opened open: {after:?}"
        );
        thread::sleep(Duration::from_millis(50));
        after = browser.page_tabs();
    }
    assert_eq!(
        after.len(),
        before.len(),
        "stopping the session must close the tab it opened and the tab its page opened: {after:?}"
    );
    assert!(
        after.iter().map(page_id).any(|id| id == user_tab),
        "stopping the session must leave the user tab open: {after:?}"
    );
}

/// Runs `steps` (a YAML fragment after `steps:`) against `url` and returns the
/// run's output.
fn run_upload_steps(dir: &Path, name: &str, url: &str, steps: &str) -> Output {
    let case = dir.join(name);
    fs::write(
        &case,
        format!("name: {name}\nsteps:\n  - browser.open:\n      url: \"{url}\"\n{steps}"),
    )
    .expect("the upload case file must be written");
    run_da_in(
        dir,
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    )
}

#[test]
fn upload_sets_single_and_multiple_files_and_clears_with_an_empty_list() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-upload");
    dir.write("single.txt", "single upload bytes\n");
    dir.write("first.txt", "first file\n");
    dir.write("second.txt", "second file\n");

    let output = run_upload_steps(
        dir.path(),
        "upload.yaml",
        &server.url("/browser/upload"),
        r##"  - browser.upload:
      css: "#single-file"
      files: ["single.txt"]
    expect:
      - path: $.files
        equals: 1
      - path: $.names
        equals: ["single.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "single-file:1:single.txt"
  - browser.upload:
      css: "#multi-file"
      files: ["first.txt", "second.txt"]
    expect:
      - path: $.files
        equals: 2
      - path: $.names
        equals: ["first.txt", "second.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "multi-file:2:first.txt,second.txt"
  - browser.upload:
      css: "#multi-file"
      files: []
    expect:
      - path: $.files
        equals: 0
      - path: $.names
        equals: []
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "multi-file:0:"
"##,
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn upload_reaches_a_hidden_input_with_every_locator_and_modifier() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-upload-locators");
    dir.write("first.txt", "first file\n");
    dir.write("second.txt", "second file\n");

    // `text` names the visible label of a `display: none` input, `css` reaches an
    // input hidden by `opacity: 0` and zero size, `testid` + `within` + `nth`
    // picks one of two inputs, `xpath` names the other, `role` + `within`
    // reaches the single file input inside its container, and `css` pierces an
    // open shadow root to reach the input a component keeps in it.
    let output = run_upload_steps(
        dir.path(),
        "upload-locators.yaml",
        &server.url("/browser/upload"),
        r##"  - browser.upload:
      text: "Choose avatar"
      files: ["first.txt"]
    expect:
      - path: $.names
        equals: ["first.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "hidden-file:1:first.txt"
  - browser.upload:
      css: "#faded-file"
      files: ["first.txt", "second.txt"]
    expect:
      - path: $.names
        equals: ["first.txt", "second.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "faded-file:2:first.txt,second.txt"
  - browser.upload:
      testid: "nested-file"
      within:
        css: "#picker"
      nth: 1
      files: ["second.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "nested-two:1:second.txt"
  - browser.upload:
      xpath: "//*[@id='nested-one']"
      files: ["first.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "nested-one:1:first.txt"
  - browser.upload:
      role: button
      within:
        css: "#single-picker"
      files: ["second.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "single-file:1:second.txt"
  - browser.upload:
      css: "#shadow-file"
      files: ["first.txt"]
    expect:
      - path: $.names
        equals: ["first.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "shadow-file:1:first.txt"
"##,
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn upload_works_on_the_tab_a_page_action_opened() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-upload-tab");
    dir.write("first.txt", "first file\n");

    // The click opens a second tab; `browser.switch_tab` moves the session to
    // it, and the upload must apply to that tab's file input.
    let output = run_upload_steps(
        dir.path(),
        "upload-tab.yaml",
        &server.url("/browser/upload/tab"),
        r##"  - browser.click:
      css: "#open-upload-tab"
  - browser.switch_tab:
      url_contains: "/browser/upload/tab-form"
    expect:
      - path: $.title
        equals: "Upload Tab Form"
  - browser.upload:
      text: "Tab file"
      files: ["first.txt"]
    expect:
      - path: $.files
        equals: 1
      - path: $.names
        equals: ["first.txt"]
  - browser.read:
      css: "#tab-events"
    expect:
      - path: $.text
        equals: "tab-file:1:first.txt"
"##,
    );

    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn upload_reports_a_missing_file_a_directory_and_a_wrong_target() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-upload-errors");
    dir.write("first.txt", "first file\n");
    dir.write("second.txt", "second file\n");
    // A directory that exists, so the step must tell "not a file" from "missing".
    dir.write("folder/inner.txt", "inner\n");

    let cases = [
        (
            "missing-file.yaml",
            r##"  - browser.upload:
      css: "#single-file"
      files: ["nope.txt"]
"##,
            "does not exist",
        ),
        (
            "directory.yaml",
            r##"  - browser.upload:
      css: "#single-file"
      files: ["folder"]
"##,
            "is not a regular file",
        ),
        (
            "plain-div.yaml",
            r##"  - browser.upload:
      css: "#plain-div"
      files: ["first.txt"]
"##,
            "the matched element is `<div>`, not an `<input type=\"file\">`",
        ),
        (
            "text-input.yaml",
            r##"  - browser.upload:
      css: "#not-a-file-input"
      files: ["first.txt"]
"##,
            "the matched element is `<input type=\"text\">`",
        ),
        (
            "two-files-one-input.yaml",
            r##"  - browser.upload:
      css: "#single-file"
      files: ["first.txt", "second.txt"]
"##,
            "no `multiple` attribute",
        ),
        (
            "disabled.yaml",
            r##"  - browser.upload:
      css: "#disabled-file"
      files: ["first.txt"]
"##,
            "is disabled",
        ),
        (
            "disabled-fieldset.yaml",
            r##"  - browser.upload:
      css: "#fieldset-file"
      files: ["first.txt"]
"##,
            "is disabled",
        ),
    ];

    for (name, steps, fragment) in cases {
        let output = run_upload_steps(dir.path(), name, &server.url("/browser/upload"), steps);
        assert_eq!(
            exit_code(&output),
            Some(1),
            "{name} must fail the case: stdout={}\nstderr={}",
            stdout(&output),
            stderr(&output)
        );
        let value = json_stdout(&output);
        let message = case_error(&value);
        assert!(
            message.contains(fragment),
            "{name}: the message must contain {fragment:?}, got: {message}"
        );
        assert_eq!(
            value["cases"][0]["error"]["kind"], "execution",
            "{name}: the error must not be an environment failure: {value}"
        );
    }
}

#[test]
fn upload_resolves_a_relative_path_against_the_case_file_and_the_form_sends_the_bytes() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-upload-form");
    // The fixture lives only beside the case file: a run from the parent
    // directory must still find it, which it can only do by resolving the path
    // against the case file's directory.
    let content = "avatar upload bytes \u{2603}\nsecond line\n";
    dir.write("sub/avatar.txt", content);
    let case = dir.write(
        "sub/upload.yaml",
        &format!(
            r##"name: upload form
steps:
  - browser.open:
      url: "{url}"
  - browser.upload:
      text: "Choose avatar"
      files: ["avatar.txt"]
    expect:
      - path: $.files
        equals: 1
      - path: $.names
        equals: ["avatar.txt"]
  - browser.read:
      css: "#file-events"
    expect:
      - path: $.text
        equals: "avatar-file:1:avatar.txt"
  - browser.click:
      css: "#send-avatar"
  - browser.read:
      css: "#upload-received"
    expect:
      - path: $.text
        equals: "received"
"##,
            url = server.url("/browser/upload/form")
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

    let posted = server
        .requests()
        .into_iter()
        .find(|request| request.method == "POST" && request.path() == "/browser/upload/submit")
        .unwrap_or_else(|| {
            panic!(
                "the submitted form must reach the server: {}",
                server.timing_report()
            )
        });
    assert!(
        posted.body.contains("name=\"avatar\""),
        "the form must submit the field the input names: {}",
        posted.body
    );
    assert!(
        posted.body.contains("filename=\"avatar.txt\""),
        "the server must see the uploaded file name: {}",
        posted.body
    );
    assert!(
        posted.body.contains(content),
        "the server must see the uploaded file's bytes: {:?}",
        posted.body
    );
}

fn spawn_mock_devtools<F>(ws_handler: F) -> (u16, thread::JoinHandle<()>)
where
    F: FnOnce(tungstenite::WebSocket<TcpStream>) + Send + 'static,
{
    let http_listener = TcpListener::bind(("127.0.0.1", 0)).expect("must bind mock HTTP port");
    let http_port = http_listener
        .local_addr()
        .expect("must read mock HTTP port")
        .port();
    let ws_listener = TcpListener::bind(("127.0.0.1", 0)).expect("must bind mock WS port");
    let ws_port = ws_listener
        .local_addr()
        .expect("must read mock WS port")
        .port();

    let handle = thread::spawn(move || {
        let (mut http_stream, _) = http_listener
            .accept()
            .expect("mock DevTools HTTP must accept");
        let mut buf = [0_u8; 1024];
        let _ = http_stream.read(&mut buf);
        let body = json!({
            "webSocketDebuggerUrl": format!("ws://127.0.0.1:{ws_port}/devtools/browser/mock")
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        http_stream
            .write_all(response.as_bytes())
            .expect("mock HTTP must write /json/version");
        drop(http_stream);

        let (ws_stream, _) = ws_listener.accept().expect("mock DevTools WS must accept");
        let _ = ws_stream.set_nodelay(true);
        let socket = tungstenite::accept(ws_stream).expect("mock WS handshake must succeed");
        ws_handler(socket);
    });
    (http_port, handle)
}

#[cfg(unix)]
fn spawn_mock_ws<F>(ws_handler: F) -> (u16, thread::JoinHandle<()>)
where
    F: FnOnce(tungstenite::WebSocket<TcpStream>) + Send + 'static,
{
    let ws_listener = TcpListener::bind(("127.0.0.1", 0)).expect("must bind mock WS port");
    let ws_port = ws_listener
        .local_addr()
        .expect("must read mock WS port")
        .port();

    let handle = thread::spawn(move || {
        let (ws_stream, _) = ws_listener.accept().expect("mock DevTools WS must accept");
        let _ = ws_stream.set_nodelay(true);
        let socket = tungstenite::accept(ws_stream).expect("mock WS handshake must succeed");
        ws_handler(socket);
    });
    (ws_port, handle)
}

fn read_mock_cdp(socket: &mut tungstenite::WebSocket<TcpStream>) -> Option<Value> {
    loop {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                return Some(
                    serde_json::from_str(text.as_str()).expect("mock CDP command must be JSON"),
                );
            }
            Ok(
                tungstenite::Message::Ping(_)
                | tungstenite::Message::Pong(_)
                | tungstenite::Message::Binary(_)
                | tungstenite::Message::Frame(_),
            ) => {}
            Ok(tungstenite::Message::Close(_)) | Err(_) => return None,
        }
    }
}

fn send_mock_cdp(socket: &mut tungstenite::WebSocket<TcpStream>, payload: &Value) {
    socket
        .send(tungstenite::Message::text(payload.to_string()))
        .expect("mock CDP must send frame");
}

#[test]
fn session_creation_recovers_from_target_crash_and_reports_diagnostics_when_all_fail() {
    let dir = TempDir::new("browser-attach-mock");

    // 1. Recovery path: target-1 crashes during Page.enable, target-2 succeeds.
    let (recover_port, recover_server) = spawn_mock_devtools(|mut socket| {
        let create1 = read_mock_cdp(&mut socket).expect("create1");
        assert_eq!(create1["method"], "Target.createTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": create1["id"], "result": { "targetId": "t-1" } }),
        );
        let attach1 = read_mock_cdp(&mut socket).expect("attach1");
        assert_eq!(attach1["method"], "Target.attachToTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": attach1["id"], "result": { "sessionId": "s-1" } }),
        );
        let page1 = read_mock_cdp(&mut socket).expect("page1");
        assert_eq!(page1["method"], "Page.enable");
        send_mock_cdp(
            &mut socket,
            &json!({ "method": "Inspector.targetCrashed", "sessionId": "s-1" }),
        );
        let close1 = read_mock_cdp(&mut socket).expect("close1");
        assert_eq!(close1["method"], "Target.closeTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": close1["id"], "result": { "success": true } }),
        );

        let create2 = read_mock_cdp(&mut socket).expect("create2");
        assert_eq!(create2["method"], "Target.createTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": create2["id"], "result": { "targetId": "t-2" } }),
        );
        let attach2 = read_mock_cdp(&mut socket).expect("attach2");
        assert_eq!(attach2["method"], "Target.attachToTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": attach2["id"], "result": { "sessionId": "s-2" } }),
        );

        while let Some(cmd) = read_mock_cdp(&mut socket) {
            let method = cmd["method"].as_str().unwrap_or_default();
            let result = match method {
                "Page.enable"
                | "Runtime.enable"
                | "Network.enable"
                | "Page.navigate"
                | "Target.setDiscoverTargets" => json!({}),
                "Runtime.evaluate" => {
                    let expr = cmd["params"]["expression"].as_str().unwrap_or_default();
                    if expr == "location.href" {
                        json!({ "result": { "value": "http://127.0.0.1/ok" } })
                    } else {
                        json!({ "result": { "value": "Recovered" } })
                    }
                }
                "Target.closeTarget" => {
                    send_mock_cdp(
                        &mut socket,
                        &json!({ "id": cmd["id"], "result": { "success": true } }),
                    );
                    break;
                }
                other => panic!("unexpected CDP command: {other}"),
            };
            send_mock_cdp(&mut socket, &json!({ "id": cmd["id"], "result": result }));
        }
    });

    let recover_case = dir.write(
        "recover.yaml",
        &format!(
            "name: recover attach\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{recover_port}\"\n      url: \"http://127.0.0.1/ok\"\n    expect:\n      - path: $.title\n        equals: \"Recovered\"\n"
        ),
    );
    let recover_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&recover_case),
            "--no-bundle",
        ],
    );
    recover_server.join().expect("recover mock server finish");
    assert_eq!(
        exit_code(&recover_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&recover_out),
        stderr(&recover_out)
    );

    // 2. Failure path: all 3 attempts crash on Page.enable -> fast exit code 4 with diagnostics.
    let (fail_port, fail_server) = spawn_mock_devtools(|mut socket| {
        for attempt in 1..=3 {
            let tid = format!("t-{attempt}");
            let sid = format!("s-{attempt}");
            let create = read_mock_cdp(&mut socket).expect("create");
            send_mock_cdp(
                &mut socket,
                &json!({ "id": create["id"], "result": { "targetId": tid } }),
            );
            let attach = read_mock_cdp(&mut socket).expect("attach");
            send_mock_cdp(
                &mut socket,
                &json!({ "id": attach["id"], "result": { "sessionId": sid } }),
            );
            let page = read_mock_cdp(&mut socket).expect("page");
            assert_eq!(page["method"], "Page.enable");
            send_mock_cdp(
                &mut socket,
                &json!({
                    "method": "Target.targetCrashed",
                    "params": { "targetId": tid, "status": "crashed", "errorCode": 11 }
                }),
            );
            if attempt < 3 {
                let close = read_mock_cdp(&mut socket).expect("close");
                send_mock_cdp(
                    &mut socket,
                    &json!({ "id": close["id"], "result": { "success": true } }),
                );
            }
        }
        while read_mock_cdp(&mut socket).is_some() {}
    });

    let fail_case = dir.write(
        "fail.yaml",
        &format!(
            "name: fail attach\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{fail_port}\"\n      url: \"http://127.0.0.1/fail\"\n"
        ),
    );
    let started = Instant::now();
    let fail_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&fail_case),
            "--no-bundle",
        ],
    );
    fail_server.join().expect("fail mock server finish");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "crashed attach must fail fast, took {:?}",
        started.elapsed()
    );
    assert_eq!(
        exit_code(&fail_out),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&fail_out),
        stderr(&fail_out)
    );
    let fail_json = json_stdout(&fail_out);
    let fail_msg = case_error(&fail_json);
    assert!(
        fail_msg.contains("Target.targetCrashed") && fail_msg.contains("target crashed"),
        "expected crash diagnostic in failure message, got: {fail_msg}"
    );
}

#[test]
fn session_creation_recovers_from_silent_attach_to_target_hang() {
    let dir = TempDir::new("browser-attach-target-stall");

    let (port, server) = spawn_mock_devtools(|mut socket| {
        let create1 = read_mock_cdp(&mut socket).expect("create1");
        assert_eq!(create1["method"], "Target.createTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": create1["id"], "result": { "targetId": "t-1" } }),
        );
        let attach1 = read_mock_cdp(&mut socket).expect("attach1");
        assert_eq!(attach1["method"], "Target.attachToTarget");
        // Never answer Target.attachToTarget for t-1; expect Target.closeTarget
        // within ATTACH_ENABLE_TIMEOUT and retry on t-2.
        let close1 = read_mock_cdp(&mut socket).expect("close1");
        assert_eq!(close1["method"], "Target.closeTarget");
        assert_eq!(close1["params"]["targetId"], "t-1");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": close1["id"], "result": { "success": true } }),
        );

        let create2 = read_mock_cdp(&mut socket).expect("create2");
        assert_eq!(create2["method"], "Target.createTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": create2["id"], "result": { "targetId": "t-2" } }),
        );
        let attach2 = read_mock_cdp(&mut socket).expect("attach2");
        assert_eq!(attach2["method"], "Target.attachToTarget");
        send_mock_cdp(
            &mut socket,
            &json!({ "id": attach2["id"], "result": { "sessionId": "s-2" } }),
        );

        while let Some(cmd) = read_mock_cdp(&mut socket) {
            let method = cmd["method"].as_str().unwrap_or_default();
            let result = match method {
                "Page.enable"
                | "Runtime.enable"
                | "Network.enable"
                | "Page.navigate"
                | "Target.setDiscoverTargets" => json!({}),
                "Runtime.evaluate" => {
                    let expr = cmd["params"]["expression"].as_str().unwrap_or_default();
                    if expr == "location.href" {
                        json!({ "result": { "value": "http://127.0.0.1/ok" } })
                    } else {
                        json!({ "result": { "value": "Recovered After Attach Stall" } })
                    }
                }
                "Target.closeTarget" => {
                    send_mock_cdp(
                        &mut socket,
                        &json!({ "id": cmd["id"], "result": { "success": true } }),
                    );
                    break;
                }
                other => panic!("unexpected CDP command: {other}"),
            };
            send_mock_cdp(&mut socket, &json!({ "id": cmd["id"], "result": result }));
        }
    });

    let case = dir.write(
        "recover-attach-stall.yaml",
        &format!(
            "name: recover attachToTarget stall\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{port}\"\n      url: \"http://127.0.0.1/ok\"\n    expect:\n      - path: $.title\n        equals: \"Recovered After Attach Stall\"\n"
        ),
    );
    let started = Instant::now();
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    server.join().expect("mock server finish");
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "silent Target.attachToTarget stall must recover inside LAUNCH_TIMEOUT, took {:?}",
        started.elapsed()
    );

    // 2. When connected to an existing browser and all owned-target attach attempts stall,
    //    `connect` mode must fail fast inside LAUNCH_TIMEOUT without sending `Browser.close`
    //    or retrying browser creation.
    let (fail_port, fail_server) = spawn_mock_devtools(|mut socket| {
        for attempt in 1..=3 {
            let tid = format!("t-fail-{attempt}");
            let create = read_mock_cdp(&mut socket).expect("create");
            assert_eq!(create["method"], "Target.createTarget");
            send_mock_cdp(
                &mut socket,
                &json!({ "id": create["id"], "result": { "targetId": tid } }),
            );
            let attach = read_mock_cdp(&mut socket).expect("attach");
            assert_eq!(attach["method"], "Target.attachToTarget");
            if attempt < 3 {
                let close = read_mock_cdp(&mut socket).expect("close");
                assert_eq!(close["method"], "Target.closeTarget");
                assert_eq!(close["params"]["targetId"], tid);
                send_mock_cdp(
                    &mut socket,
                    &json!({ "id": close["id"], "result": { "success": true } }),
                );
            }
        }
        if let Some(extra) = read_mock_cdp(&mut socket) {
            panic!(
                "connect mode must preserve browser ownership and not retry or send extra CDP command: {extra}"
            );
        }
    });

    let fail_case = dir.write(
        "connect-attach-stall-fail.yaml",
        &format!(
            "name: connect attachToTarget stall fails preserving browser\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{fail_port}\"\n      url: \"http://127.0.0.1/fail\"\n"
        ),
    );
    let fail_started = Instant::now();
    let fail_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&fail_case),
            "--no-bundle",
        ],
    );
    fail_server.join().expect("fail mock server finish");
    assert!(
        fail_started.elapsed() < Duration::from_secs(25),
        "3 stalled Target.attachToTarget attempts must fail inside LAUNCH_TIMEOUT, took {:?}",
        fail_started.elapsed()
    );
    assert_eq!(
        exit_code(&fail_out),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&fail_out),
        stderr(&fail_out)
    );
    let fail_json = json_stdout(&fail_out);
    let fail_msg = case_error(&fail_json);
    assert!(
        fail_msg.contains("attempt 1:")
            && fail_msg.contains("attempt 3:")
            && fail_msg.contains("Target.attachToTarget"),
        "expected attempt diagnostics in failure message, got: {fail_msg}"
    );
}

#[test]
fn connect_navigate_aborts_on_target_death_and_reports_diagnostics() {
    let dir = TempDir::new("browser-navigate-mock");

    // 1. Target dies during Page.navigate -> must abort immediately without waiting
    //    for the full timeout and report the received target/session event.
    let (crash_port, crash_server) = spawn_mock_devtools(|mut socket| {
        while let Some(cmd) = read_mock_cdp(&mut socket) {
            let method = cmd["method"].as_str().unwrap_or_default();
            match method {
                "Target.createTarget" => send_mock_cdp(
                    &mut socket,
                    &json!({ "id": cmd["id"], "result": { "targetId": "t-nav-1" } }),
                ),
                "Target.attachToTarget" => send_mock_cdp(
                    &mut socket,
                    &json!({ "id": cmd["id"], "result": { "sessionId": "s-nav-1" } }),
                ),
                "Page.enable"
                | "Runtime.enable"
                | "Network.enable"
                | "Target.setDiscoverTargets" => {
                    send_mock_cdp(&mut socket, &json!({ "id": cmd["id"], "result": {} }));
                }
                "Page.navigate" => {
                    send_mock_cdp(
                        &mut socket,
                        &json!({
                            "method": "Target.targetCrashed",
                            "params": {
                                "targetId": "t-nav-1",
                                "status": "crashed",
                                "errorCode": 11
                            }
                        }),
                    );
                }
                "Target.closeTarget" => {
                    send_mock_cdp(
                        &mut socket,
                        &json!({ "id": cmd["id"], "result": { "success": true } }),
                    );
                    break;
                }
                other => panic!("unexpected CDP command during crash test: {other}"),
            }
        }
    });

    let crash_case = dir.write(
        "navigate-crash.yaml",
        &format!(
            "name: navigate crash\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{crash_port}\"\n      url: \"http://127.0.0.1/crash\"\n      timeout_ms: 10000\n"
        ),
    );
    let crash_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&crash_case),
            "--no-bundle",
        ],
    );
    crash_server.join().expect("crash mock server finish");
    assert_eq!(
        exit_code(&crash_out),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&crash_out),
        stderr(&crash_out)
    );
    let crash_json = json_stdout(&crash_out);
    // The runner measures the step, so process start-up does not blur the
    // difference between an immediate abort and the 10 s navigation timeout.
    let crash_ms = crash_json["cases"][0]["steps"][0]["duration_ms"]
        .as_u64()
        .unwrap_or(u64::MAX);
    assert!(
        crash_ms < 5000,
        "crashed Page.navigate must fail fast instead of waiting 10 s (step took {crash_ms} ms): {}",
        stdout(&crash_out)
    );
    let crash_msg = case_error(&crash_json);
    assert!(
        crash_msg.contains("Page.navigate")
            && crash_msg.contains("Target.targetCrashed")
            && crash_msg.contains("target crashed"),
        "expected Page.navigate crash diagnostic, got: {crash_msg}"
    );

    // 2. Page.navigate never answers and emits no events -> timeout reports that
    //    no CDP events arrived for the target or session.
    let (silent_port, silent_server) = spawn_mock_devtools(|mut socket| {
        while let Some(cmd) = read_mock_cdp(&mut socket) {
            let method = cmd["method"].as_str().unwrap_or_default();
            match method {
                "Target.createTarget" => send_mock_cdp(
                    &mut socket,
                    &json!({ "id": cmd["id"], "result": { "targetId": "t-nav-2" } }),
                ),
                "Target.attachToTarget" => send_mock_cdp(
                    &mut socket,
                    &json!({ "id": cmd["id"], "result": { "sessionId": "s-nav-2" } }),
                ),
                "Page.enable"
                | "Runtime.enable"
                | "Network.enable"
                | "Target.setDiscoverTargets" => {
                    send_mock_cdp(&mut socket, &json!({ "id": cmd["id"], "result": {} }));
                }
                "Page.navigate" => {}
                "Target.closeTarget" => {
                    send_mock_cdp(
                        &mut socket,
                        &json!({ "id": cmd["id"], "result": { "success": true } }),
                    );
                    break;
                }
                other => panic!("unexpected CDP command during silent timeout test: {other}"),
            }
        }
    });

    let silent_case = dir.write(
        "navigate-silent.yaml",
        &format!(
            "name: navigate silent timeout\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{silent_port}\"\n      url: \"http://127.0.0.1/silent\"\n      timeout_ms: 500\n"
        ),
    );
    let silent_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&silent_case),
            "--no-bundle",
        ],
    );
    silent_server.join().expect("silent mock server finish");
    assert_eq!(
        exit_code(&silent_out),
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&silent_out),
        stderr(&silent_out)
    );
    let silent_json = json_stdout(&silent_out);
    let silent_msg = case_error(&silent_json);
    assert!(
        silent_msg.contains("Page.navigate")
            && silent_msg.contains("never answered")
            && silent_msg.contains("no CDP events"),
        "expected silent timeout diagnostic, got: {silent_msg}"
    );
}

#[test]
fn connect_click_aborts_on_target_death_and_reports_diagnostics() {
    let dir = TempDir::new("browser-click-mock");

    // The page dies while `browser.click` is resolving its locator, so the
    // click must fail at once instead of waiting out its 5 s timeout.
    let (port, server) = spawn_mock_devtools(|mut socket| {
        while let Some(cmd) = read_mock_cdp(&mut socket) {
            let method = cmd["method"].as_str().unwrap_or_default();
            match method {
                "Target.createTarget" => send_mock_cdp(
                    &mut socket,
                    &json!({ "id": cmd["id"], "result": { "targetId": "t-click-1" } }),
                ),
                "Target.attachToTarget" => send_mock_cdp(
                    &mut socket,
                    &json!({ "id": cmd["id"], "result": { "sessionId": "s-click-1" } }),
                ),
                "Page.enable"
                | "Runtime.enable"
                | "Network.enable"
                | "Target.setDiscoverTargets" => {
                    send_mock_cdp(&mut socket, &json!({ "id": cmd["id"], "result": {} }));
                }
                // No `loaderId`: a same-document navigation, so the session
                // reads the URL and title instead of waiting for a load event.
                "Page.navigate" => {
                    send_mock_cdp(&mut socket, &json!({ "id": cmd["id"], "result": {} }));
                }
                "Runtime.evaluate" => {
                    if cmd["params"]["returnByValue"] == json!(true) {
                        let value = if cmd["params"]["expression"] == json!("location.href") {
                            json!("http://127.0.0.1/ok")
                        } else {
                            json!("Click target")
                        };
                        send_mock_cdp(
                            &mut socket,
                            &json!({ "id": cmd["id"], "result": { "result": { "value": value } } }),
                        );
                    } else {
                        // The locator expression never answers; the page dies.
                        send_mock_cdp(
                            &mut socket,
                            &json!({
                                "method": "Target.targetCrashed",
                                "params": {
                                    "targetId": "t-click-1",
                                    "status": "crashed",
                                    "errorCode": 11
                                }
                            }),
                        );
                    }
                }
                "Target.closeTarget" => {
                    send_mock_cdp(
                        &mut socket,
                        &json!({ "id": cmd["id"], "result": { "success": true } }),
                    );
                    break;
                }
                other => panic!("unexpected CDP command during click crash test: {other}"),
            }
        }
    });

    let case = dir.write(
        "click-crash.yaml",
        &format!(
            "name: click crash\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{port}\"\n      url: \"http://127.0.0.1/ok\"\n  - browser.click:\n      css: \"#go\"\n      timeout_ms: 5000\n"
        ),
    );
    let out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    server.join().expect("click crash mock server finish");

    assert_eq!(
        exit_code(&out),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&out),
        stderr(&out)
    );
    let report = json_stdout(&out);
    let click_ms = report["cases"][0]["steps"][1]["duration_ms"]
        .as_u64()
        .unwrap_or(u64::MAX);
    assert!(
        click_ms < 5000,
        "a dead page must fail the click at once instead of waiting 5 s (step took {click_ms} ms): {}",
        stdout(&out)
    );
    let message = case_error(&report);
    assert!(
        message.contains("Target.targetCrashed") && message.contains("target crashed"),
        "expected crash diagnostic in the click error, got: {message}"
    );
}

#[test]
fn unhandled_dialog_fails_fast_and_does_not_block_evidence_or_next_steps() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-unhandled-dialog");
    let bundles = dir.path().join("failures");
    let case = dir.write(
        "unhandled-dialog.yaml",
        &format!(
            r##"name: unhandled dialog fails fast
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#btn-alert"
      timeout_ms: 10000
"##,
            server.url("/browser/dialogs")
        ),
    );

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
    let report = json_stdout(&output);
    let step_ms = report["cases"][0]["steps"][1]["duration_ms"]
        .as_u64()
        .unwrap_or(u64::MAX);
    assert!(
        step_ms < 3000,
        "unhandled alert must fail fast instead of waiting for 10000 ms timeout (step={step_ms} ms): {}",
        stdout(&output)
    );
    let err = case_error(&report);
    assert!(
        err.contains("alert") && err.contains("unexpected warning"),
        "error must name dialog type and message: {err}"
    );

    let bundle = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("failed case must write failure bundle without hanging"),
    );
    let png = fs::read(bundle.join("browser/screenshot.png"))
        .expect("screenshot evidence must still be collected after unhandled dialog");
    assert!(is_png(&png), "screenshot must be a valid PNG");
    let dom = fs::read_to_string(bundle.join("browser/dom.html"))
        .expect("DOM snapshot must still be collected after unhandled dialog");
    assert!(dom.contains("id=\"btn-alert\""), "{dom}");

    // An optional step hitting an unhandled dialog must leave the page responsive for later steps.
    let continued = dir.write(
        "unhandled-optional-dialog.yaml",
        &format!(
            r##"name: optional unhandled dialog leaves page responsive
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#btn-alert"
      timeout_ms: 10000
    optional: true
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "alert-done"
"##,
            server.url("/browser/dialogs")
        ),
    );
    let cont_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&continued),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&cont_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&cont_out),
        stderr(&cont_out)
    );
}

#[test]
fn dialog_option_handles_alert_confirm_and_prompt() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-handled-dialogs");
    let case = dir.write(
        "handled-dialogs.yaml",
        &format!(
            r##"name: handle alert confirm and prompt
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#btn-alert"
      dialog: accept
    expect:
      - path: $.dialog.type
        equals: alert
      - path: $.dialog.message
        equals: "unexpected warning"
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "alert-done"
  - browser.click:
      css: "#btn-confirm"
      dialog: accept
    expect:
      - path: $.dialog.type
        equals: confirm
      - path: $.dialog.message
        equals: "Proceed with delete?"
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "confirmed"
  - browser.click:
      css: "#btn-confirm"
      dialog: dismiss
    expect:
      - path: $.dialog.type
        equals: confirm
      - path: $.dialog.message
        equals: "Proceed with delete?"
      - path: $.dialog.accepted
        equals: false
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "cancelled"
  - browser.click:
      css: "#btn-prompt"
      dialog:
        action: accept
        prompt_text: "custom-token"
    expect:
      - path: $.dialog.type
        equals: prompt
      - path: $.dialog.message
        equals: "Enter token"
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "prompt:custom-token"
  - browser.click:
      css: "#btn-prompt"
      dialog:
        action: dismiss
    expect:
      - path: $.dialog.type
        equals: prompt
      - path: $.dialog.accepted
        equals: false
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "prompt-cancelled"
"##,
            server.url("/browser/dialogs")
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
    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["status"], "passed", "{report}");
}

#[test]
fn dialog_option_works_on_open_type_press_and_beforeunload() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-step-dialogs");
    let case = dir.write(
        "step-dialogs.yaml",
        &format!(
            r##"name: dialogs across open type press and beforeunload
steps:
  - browser.open:
      url: "{}"
      dialog: accept
    expect:
      - path: $.dialog.type
        equals: alert
      - path: $.dialog.message
        equals: "load alert"
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#loaded"
    expect:
      - path: $.text
        equals: "ready"
  - browser.open:
      url: "{}"
  - browser.type:
      css: "#type-dialog"
      text: "go"
      dialog: dismiss
    expect:
      - path: $.dialog.type
        equals: confirm
      - path: $.dialog.message
        equals: "Type confirm?"
      - path: $.dialog.accepted
        equals: false
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "type-cancelled"
  - browser.type:
      css: "#press-input"
      text: "x"
  - browser.press:
      key: Enter
      dialog: accept
    expect:
      - path: $.dialog.type
        equals: alert
      - path: $.dialog.message
        equals: "press alert"
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "press-done"
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#arm-leave"
  - browser.open:
      url: "{}"
      dialog: dismiss
    expect:
      - path: $.url
        contains: "/browser/dialogs/beforeunload"
      - path: $.dialog.type
        equals: beforeunload
      - path: $.dialog.accepted
        equals: false
  - browser.open:
      url: "{}"
      dialog: accept
    expect:
      - path: $.url
        contains: "/browser/dialogs/left"
      - path: $.dialog.type
        equals: beforeunload
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#left-status"
    expect:
      - path: $.text
        equals: "left"
"##,
            server.url("/browser/dialogs/onload"),
            server.url("/browser/dialogs"),
            server.url("/browser/dialogs/beforeunload"),
            server.url("/browser/dialogs/left"),
            server.url("/browser/dialogs/left"),
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
    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["status"], "passed", "{report}");
}

#[test]
fn dialog_messages_and_prompt_text_redact_env_secrets() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let secret = "s3cr3t-dialog-token-987";
    let dir = TempDir::new("browser-dialog-secrets");
    let bundles = dir.path().join("failures");

    // 1. Unhandled dialog error message and failure bundle redact the secret.
    let unhandled_case = dir.write(
        "unhandled-secret-dialog.yaml",
        &format!(
            r##"name: unhandled secret dialog
steps:
  - browser.open:
      url: "{}"
  - browser.type:
      css: "#secret-input"
      text: "${{env:DA_DIALOG_SECRET}}"
  - browser.click:
      css: "#btn-secret-alert"
      timeout_ms: 10000
"##,
            server.url("/browser/dialogs")
        ),
    );
    let unhandled_out = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&unhandled_case),
            "--bundle-dir",
            path_str(&bundles),
        ],
        &[("DA_DIALOG_SECRET", secret.to_string())],
    );
    assert_eq!(
        exit_code(&unhandled_out),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&unhandled_out),
        stderr(&unhandled_out)
    );
    let unhandled_stdout = stdout(&unhandled_out);
    let unhandled_stderr = stderr(&unhandled_out);
    assert!(
        !unhandled_stdout.contains(secret) && !unhandled_stderr.contains(secret),
        "unhandled dialog output leaked secret:\nstdout={unhandled_stdout}\nstderr={unhandled_stderr}"
    );
    assert!(
        unhandled_stdout.contains("secret=***"),
        "expected redacted dialog message in JSON output: {unhandled_stdout}"
    );
    let unhandled_json = json_stdout(&unhandled_out);
    let bundle = PathBuf::from(
        unhandled_json["cases"][0]["bundle"]
            .as_str()
            .expect("must write bundle"),
    );
    let bundle_report =
        fs::read_to_string(bundle.join("report.json")).expect("bundle report.json must exist");
    assert!(
        !bundle_report.contains(secret) && bundle_report.contains("secret=***"),
        "bundle report leaked or missed redaction: {bundle_report}"
    );

    // 2. Handled dialog result redacts secret in dialog.message.
    let handled_case = dir.write(
        "handled-secret-dialog.yaml",
        &format!(
            r##"name: handled secret dialog
steps:
  - browser.open:
      url: "{}"
  - browser.type:
      css: "#secret-input"
      text: "${{env:DA_DIALOG_SECRET}}"
  - browser.click:
      css: "#btn-secret-confirm"
      dialog: accept
    expect:
      - path: $.dialog.type
        equals: confirm
      - path: $.dialog.message
        equals: "confirm={secret}"
      - path: $.dialog.accepted
        equals: true
  - browser.click:
      css: "#btn-prompt"
      dialog:
        action: accept
        prompt_text: "${{env:DA_DIALOG_SECRET}}"
    expect:
      - path: $.dialog.type
        equals: prompt
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "prompt:{secret}"
"##,
            server.url("/browser/dialogs")
        ),
    );
    let handled_out = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&handled_case),
            "--no-bundle",
        ],
        &[("DA_DIALOG_SECRET", secret.to_string())],
    );
    assert_eq!(
        exit_code(&handled_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&handled_out),
        stderr(&handled_out)
    );
    let handled_stdout = stdout(&handled_out);
    assert!(
        !handled_stdout.contains(secret)
            && handled_stdout.contains("confirm=***")
            && handled_stdout.contains("prompt:***"),
        "handled dialog result leaked or missed redaction: {handled_stdout}"
    );
}

#[test]
fn retry_on_browser_read_and_wait_interacts_cleanly_with_dialogs() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-retry-dialogs");
    let case = dir.write(
        "retry-dialogs.yaml",
        &format!(
            r##"name: retry on browser.wait and browser.read with and without dialog
steps:
  - browser.open:
      url: "{}"
  - browser.click:
      css: "#arm-confirm"
  - browser.wait:
      css: "#status"
      dialog: accept
      timeout_ms: 300
    retry:
      until:
        - path: $.dialog.type
          equals: confirm
      timeout_ms: 8000
      interval_ms: 50
    expect:
      - path: $.dialog.message
        equals: "Ready now?"
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#status"
    retry:
      until:
        - path: $.text
          equals: "confirmed"
      timeout_ms: 5000
      interval_ms: 50
  - browser.click:
      css: "#arm-unexpected"
  - browser.read:
      css: "#status"
      timeout_ms: 5000
    retry:
      until:
        - path: $.text
          equals: "after-alert"
      timeout_ms: 8000
      interval_ms: 50
"##,
            server.url("/browser/dialogs/retry")
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
    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["status"], "passed", "{report}");
    assert_eq!(
        report["cases"][0]["steps"][2]["result"]["dialog"]["type"], "confirm",
        "{report}"
    );
    assert_eq!(
        report["cases"][0]["steps"][5]["result"]["text"], "after-alert",
        "{report}"
    );
}

#[test]
fn within_same_origin_and_nested_iframes_support_all_locators_and_actions() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-iframe-nested");
    let host_url = server.url("/browser/iframes/host");
    let slow_url = server.url("/browser/iframes/slow-host");
    let frameset_url = server.url("/browser/iframes/frameset");

    let main_case = dir.write(
        "iframes-all-locators.yaml",
        &format!(
            r##"name: same-origin and nested iframes support all locators and actions
steps:
  - browser.open:
      url: "{host_url}"
  - browser.wait:
      role: button
      name: Frame Action
      within:
        css: "#embed"
  - browser.click:
      css: "#frame-btn"
      within:
        css: "#embed"
  - browser.read:
      css: "#frame-result"
      within:
        css: "#embed"
    expect:
      - path: $.text
        equals: frame-clicked
  - browser.click:
      xpath: "//button[@id='panel-btn']"
      within:
        role: region
        name: Inner Panel
        within:
          testid: embed-frame
  - browser.read:
      css: "#frame-result"
      within:
        css: "#embed"
    expect:
      - path: $.text
        equals: inner-panel-clicked
  - browser.click:
      text: Other Save
      within:
        css: "#embed"
  - browser.read:
      css: "#frame-result"
      within:
        css: "#embed"
    expect:
      - path: $.text
        equals: other-panel-clicked
  - browser.type:
      testid: frame-input
      text: hello-iframe
      within:
        css: "#embed"
  - browser.read:
      css: "#frame-result"
      within:
        css: "#embed"
    expect:
      - path: $.text
        equals: "typed:hello-iframe"
  - browser.click:
      css: "#frame-confirm-btn"
      dialog: accept
      within:
        css: "#embed"
    expect:
      - path: $.dialog.type
        equals: confirm
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#frame-result"
      within:
        css: "#embed"
    expect:
      - path: $.text
        equals: confirmed
  - browser.click:
      testid: nested-btn
      within:
        css: ".nested-box"
        within:
          testid: nested-frame
          within:
            css: "#embed"
  - browser.read:
      css: "#nested-result"
      within:
        css: "#nested-frame"
        within:
          css: "#embed"
    expect:
      - path: $.text
        equals: nested-clicked
  - browser.type:
      role: textbox
      name: Nested Field
      text: deep-value
      within:
        css: "#nested-frame"
        within:
          css: "#embed"
  - browser.read:
      css: "#nested-result"
      within:
        css: "#nested-frame"
        within:
          css: "#embed"
    expect:
      - path: $.text
        equals: "nested-typed:deep-value"
  - browser.click:
      css: "#spawn-nested-badge"
      within:
        css: "#nested-frame"
        within:
          css: "#embed"
  - browser.wait:
      text: nested-ready
      timeout_ms: 2000
      within:
        css: "#nested-frame"
        within:
          css: "#embed"
"##
        ),
    );
    let main_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&main_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&main_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&main_out),
        stderr(&main_out)
    );

    // Unscoped `role` locator must stay in the main document and not pierce into `#embed`.
    let unscoped_role_case = dir.write(
        "iframe-unscoped-role.yaml",
        &format!(
            r##"name: unscoped role stays in main document
steps:
  - browser.open:
      url: "{host_url}"
  - browser.click:
      role: button
      name: Frame Action
      timeout_ms: 400
"##
        ),
    );
    let unscoped_role_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&unscoped_role_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&unscoped_role_out),
        Some(1),
        "unscoped-role must fail: stdout: {}\nstderr: {}",
        stdout(&unscoped_role_out),
        stderr(&unscoped_role_out)
    );
    let unscoped_role_json = json_stdout(&unscoped_role_out);
    let unscoped_role_err = case_error(&unscoped_role_json);
    assert!(
        unscoped_role_err.contains("could not find role `button` named `Frame Action`"),
        "expected unscoped role miss, got: {unscoped_role_err}"
    );

    // Unhandled JS dialog inside an iframe still fails fast via DialogWatch.
    let dialog_case = dir.write(
        "iframe-unhandled-dialog.yaml",
        &format!(
            r##"name: unhandled dialog inside iframe fails fast
steps:
  - browser.open:
      url: "{host_url}"
  - browser.click:
      css: "#frame-alert-btn"
      timeout_ms: 5000
      within:
        css: "#embed"
"##
        ),
    );
    let dialog_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&dialog_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&dialog_out),
        Some(1),
        "iframe-unhandled-dialog must fail: stdout: {}\nstderr: {}",
        stdout(&dialog_out),
        stderr(&dialog_out)
    );
    let dialog_json = json_stdout(&dialog_out);
    let dialog_ms = dialog_json["cases"][0]["steps"][1]["duration_ms"]
        .as_u64()
        .unwrap_or(u64::MAX);
    assert!(
        dialog_ms < 3000,
        "unhandled iframe dialog must fail fast, took {dialog_ms} ms"
    );
    let dialog_err = case_error(&dialog_json);
    assert!(
        dialog_err.contains("alert") && dialog_err.contains("inner alert"),
        "expected inner alert message, got: {dialog_err}"
    );

    // Slow-loading dynamically inserted same-origin iframe waits within timeout_ms.
    let slow_case = dir.write(
        "iframe-slow-ready.yaml",
        &format!(
            r##"name: waits for same-origin iframe document to become ready
steps:
  - browser.open:
      url: "{slow_url}"
  - browser.click:
      css: "#spawn-lazy-frame"
  - browser.wait:
      css: "#slow-ready"
      timeout_ms: 3000
      within:
        css: "#lazy-frame"
  - browser.click:
      css: "#slow-btn"
      timeout_ms: 3000
      within:
        css: "#lazy-frame"
  - browser.read:
      css: "#slow-ready"
      within:
        css: "#lazy-frame"
    expect:
      - path: $.text
        equals: slow-clicked
"##
        ),
    );
    let slow_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&slow_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&slow_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&slow_out),
        stderr(&slow_out)
    );

    // <frameset> / <frame> works identically to <iframe>.
    let frameset_case = dir.write(
        "frameset-frame.yaml",
        &format!(
            r##"name: within supports frame elements in a frameset
steps:
  - browser.open:
      url: "{frameset_url}"
  - browser.click:
      css: "#inside-frame"
      within:
        css: "#frame-left"
  - browser.read:
      css: "#probe-status"
      within:
        css: "#frame-left"
    expect:
      - path: $.text
        equals: clicked
"##
        ),
    );
    let frameset_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&frameset_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&frameset_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&frameset_out),
        stderr(&frameset_out)
    );
}

#[test]
fn within_iframe_click_accumulates_frame_offsets_instead_of_clicking_decoy() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-iframe-offset");
    let host_url = server.url("/browser/iframes/host");

    let case = dir.write(
        "offset-click.yaml",
        &format!(
            r##"name: iframe click accumulates outer frame viewport offset
steps:
  - browser.open:
      url: "{host_url}"
  - browser.click:
      css: "#frame-btn"
      timeout_ms: 2000
      within:
        css: "#embed"
  - browser.read:
      css: "#frame-result"
      within:
        css: "#embed"
    expect:
      - path: $.text
        equals: frame-clicked
  - browser.read:
      css: "#decoy-result"
    expect:
      - path: $.text
        equals: decoy-idle
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
}

#[test]
fn within_iframe_checks_top_level_and_inner_occlusion_and_scrolls_offscreen_frames() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-iframe-occlusion");
    let url = server.url("/browser/iframes/occlusion");

    // 1. Top-level modal backdrop over an iframe button blocks the click and names the backdrop.
    let top_blocked = dir.write(
        "top-blocked.yaml",
        &format!(
            r##"name: top-level backdrop over iframe button fails naming backdrop
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#top-blocked-btn"
      timeout_ms: 400
      within:
        css: "#occ-frame"
"##
        ),
    );
    let top_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&top_blocked),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&top_out),
        Some(1),
        "top-blocked must fail: stdout: {}\nstderr: {}",
        stdout(&top_out),
        stderr(&top_out)
    );
    let top_json = json_stdout(&top_out);
    let top_err = case_error(&top_json);
    assert!(
        top_err.contains("top-modal-backdrop"),
        "expected top-modal-backdrop blocker, got: {top_err}"
    );

    // 2. Inner overlay inside the iframe blocks the click and names the inner overlay.
    let inner_blocked = dir.write(
        "inner-blocked.yaml",
        &format!(
            r##"name: inner overlay inside iframe fails naming inner overlay
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#inner-blocked-btn"
      timeout_ms: 400
      within:
        css: "#occ-frame"
"##
        ),
    );
    let inner_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&inner_blocked),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&inner_out),
        Some(1),
        "inner-blocked must fail: stdout: {}\nstderr: {}",
        stdout(&inner_out),
        stderr(&inner_out)
    );
    let inner_json = json_stdout(&inner_out);
    let inner_err = case_error(&inner_json);
    assert!(
        inner_err.contains("inner-overlay"),
        "expected inner-overlay blocker, got: {inner_err}"
    );

    // 3. Transient top-level overlay clears -> click succeeds; offscreen iframe scrolls in -> click succeeds.
    let recover_and_scroll = dir.write(
        "uncover-and-scroll.yaml",
        &format!(
            r##"name: waits for top-level overlay to hide and scrolls offscreen iframe
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#uncover-iframe-modal"
  - browser.click:
      css: "#temp-blocked-btn"
      timeout_ms: 2000
      within:
        css: "#occ-frame"
  - browser.read:
      css: "#occ-result"
      within:
        css: "#occ-frame"
    expect:
      - path: $.text
        equals: temp-blocked-clicked
  - browser.click:
      css: "#inside-frame"
      within:
        css: "#offscreen-frame"
  - browser.read:
      css: "#probe-status"
      within:
        css: "#offscreen-frame"
    expect:
      - path: $.text
        equals: clicked
"##
        ),
    );
    let recover_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&recover_and_scroll),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&recover_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&recover_out),
        stderr(&recover_out)
    );
}

#[test]
fn within_cross_origin_iframe_fails_immediately_with_clear_error() {
    let server1 = TestServer::start();
    let Some(_guard) = chrome_test(&server1) else {
        return;
    };
    let server2 = TestServer::start();
    let dir = TempDir::new("browser-iframe-cross-origin");
    let url = format!(
        "{}?cross_port={}",
        server1.url("/browser/iframes/host"),
        server2.port()
    );

    for (name, frame_selector) in [
        ("cross-origin-port.yaml", "#cross-origin-frame"),
        ("cross-origin-sandbox.yaml", "#sandboxed-frame"),
    ] {
        let case = dir.write(
            name,
            &format!(
                r##"name: cross-origin iframe fails immediately
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#inside-frame"
      timeout_ms: 10000
      within:
        css: "{frame_selector}"
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
            "{name}: stdout={}\nstderr={}",
            stdout(&output),
            stderr(&output)
        );
        let value = json_stdout(&output);
        let duration_ms = value["cases"][0]["steps"][1]["duration_ms"]
            .as_u64()
            .unwrap_or(u64::MAX);
        assert!(
            duration_ms < 4000,
            "{name}: cross-origin iframe must fail immediately instead of waiting 10000 ms (took {duration_ms} ms)"
        );
        let message = case_error(&value);
        assert!(
            message.contains("cross-origin") && message.contains("same-origin"),
            "{name}: expected clear cross-origin iframe message, got: {message}"
        );
    }
}

#[test]
fn screenshot_and_failure_evidence_cover_editable_fields_inside_same_origin_iframes() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-mask-iframe");
    let url = server.url("/browser/mask-iframe");
    let secret = "iframe-pw-s3cr3t-42";

    let case = dir.write(
        "mask-iframe.yaml",
        &format!(
            r##"name: screenshot masks editable fields inside same-origin and nested iframes
steps:
  - browser.open:
      url: "{url}"
  - browser.type:
      css: "#iframe-pw"
      text: "${{env:DA_IFRAME_PW_SECRET}}"
      within:
        css: "#mask-frame"
  - browser.screenshot:
      path: shots/iframe-masked.png
  - browser.read:
      css: "#mask-report"
    expect:
      - path: $.text
        equals: "top_pw=true,iframe_pw=true,iframe_note=true,nested_pw=true"
  - browser.click:
      css: "#iframe-pw"
      within:
        css: "#mask-frame"
  - browser.read:
      css: "#mask-report"
    expect:
      - path: $.text
        equals: "force-failure-bundle"
"##
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
        &[("DA_IFRAME_PW_SECRET", secret.to_string())],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let raw_stdout = stdout(&output);
    assert!(
        !raw_stdout.contains(secret),
        "secret leaked in JSON output: {raw_stdout}"
    );
    let value = json_stdout(&output);
    // Step 3 (0-based index 3) is the mask-report expectation; it must pass before step 4 fails!
    assert_eq!(
        value["cases"][0]["steps"][3]["status"], "passed",
        "mask report check inside step 3 must pass: {value}"
    );
    let explicit_shot = dir.path().join("shots/iframe-masked.png");
    assert!(
        is_png(&fs::read(&explicit_shot).expect("explicit screenshot must exist")),
        "explicit screenshot must be a valid PNG"
    );

    let bundle_rel = value["cases"][0]["bundle"]
        .as_str()
        .expect("failed case must emit failure bundle");
    let bundle_dir = PathBuf::from(bundle_rel);
    assert!(
        is_png(
            &fs::read(bundle_dir.join("browser/screenshot.png"))
                .expect("failure bundle browser/screenshot.png must exist")
        ),
        "failure bundle screenshot must be a valid PNG"
    );
    let report_json =
        fs::read_to_string(bundle_dir.join("report.json")).expect("report.json must exist");
    assert!(
        !report_json.contains(secret),
        "secret leaked in failure report.json: {report_json}"
    );
}

#[test]
fn within_scaled_iframe_clicks_exact_target_and_masks_screenshot_pixels() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-iframe-scaled");
    let host_url = server.url("/browser/iframes/scaled-host");
    let unscaled_overlay_url = server.url("/browser/iframes/scaled-host?overlay=unscaled");
    let scaled_overlay_url = server.url("/browser/iframes/scaled-host?overlay=scaled");

    // 1. Click, type, nested scaled iframe click, screenshot masking at scale(1.5),
    //    and unscaled vs. scaled occlusion checks.
    let case = dir.write(
        "scaled-iframe.yaml",
        &format!(
            r##"name: scaled iframe click hits button and screenshot masks scaled field
steps:
  - browser.open:
      url: "{host_url}"
  - browser.type:
      css: "#scaled-pw"
      text: scaled-secret-value
      within:
        css: "#scaled-frame"
  - browser.click:
      css: "#scaled-btn"
      within:
        css: "#scaled-frame"
  - browser.read:
      css: "#scaled-result"
      within:
        css: "#scaled-frame"
    expect:
      - path: $.text
        equals: done
  - browser.read:
      css: "#decoy-hit"
      within:
        css: "#scaled-frame"
    expect:
      - path: $.text
        equals: idle
  - browser.click:
      css: "#scaled-btn"
      within:
        css: "#ancestor-scaled-frame"
  - browser.read:
      css: "#scaled-result"
      within:
        css: "#ancestor-scaled-frame"
    expect:
      - path: $.text
        equals: done
  - browser.click:
      css: "#nested-btn"
      within:
        css: "#nested-scaled"
        within:
          css: "#scaled-frame"
  - browser.read:
      css: "#nested-result"
      within:
        css: "#nested-scaled"
        within:
          css: "#scaled-frame"
    expect:
      - path: $.text
        equals: nested-clicked
  - browser.screenshot:
      path: shots/scaled-masked.png
  - browser.open:
      url: "{unscaled_overlay_url}"
  - browser.click:
      css: "#scaled-btn"
      within:
        css: "#scaled-frame"
  - browser.read:
      css: "#scaled-result"
      within:
        css: "#scaled-frame"
    expect:
      - path: $.text
        equals: done
  - browser.open:
      url: "{scaled_overlay_url}"
  - browser.click:
      css: "#scaled-btn"
      timeout_ms: 400
      within:
        css: "#scaled-frame"
    optional: true
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
    let main_json = json_stdout(&output);
    let steps = main_json["cases"][0]["steps"]
        .as_array()
        .expect("steps array");
    for (idx, step) in steps.iter().enumerate() {
        if idx == 14 {
            assert_eq!(
                step["status"], "skipped",
                "optional step 14 must be skipped when occluded: {step}"
            );
            let err = step["error"]["message"].as_str().unwrap_or_default();
            assert!(
                err.contains("scaled-modal"),
                "expected scaled-modal occlusion blocker, got: {err}"
            );
        } else {
            assert_eq!(step["status"], "passed", "step {idx} must pass: {step}");
        }
    }

    // 2. Inspect actual rendered pixels of shots/scaled-masked.png via headless Chrome <canvas>.
    let shot_bytes =
        fs::read(dir.path().join("shots/scaled-masked.png")).expect("screenshot PNG must exist");
    assert!(is_png(&shot_bytes), "must be a valid PNG");
    let png_b64 = support::base64_encode(&shot_bytes);
    let inspect_html = format!(
        r##"<!doctype html><html><body>
<p id="px-report">pending</p>
<script>
var img = new Image();
img.onload = function () {{
  var c = document.createElement('canvas');
  c.width = img.width;
  c.height = img.height;
  var ctx = c.getContext('2d');
  ctx.drawImage(img, 0, 0);
  var data = ctx.getImageData(0, 0, c.width, c.height).data;
  var redCount = 0;
  for (var i = 0; i < data.length; i += 4) {{
    if (data[i] > 180 && data[i + 1] < 60 && data[i + 2] < 60) redCount++;
  }}
  var allBlack = function (x0, y0, x1, y1) {{
    for (var y = y0; y <= y1; y++) {{
      for (var x = x0; x <= x1; x++) {{
        var idx = (y * c.width + x) * 4;
        if (data[idx] !== 0 || data[idx + 1] !== 0 || data[idx + 2] !== 0) return false;
      }}
    }}
    return true;
  }};
  var directBlack = allBlack(215, 208, 390, 248);
  var ancestorBlack = allBlack(750, 202, 926, 241);
  document.getElementById('px-report').textContent =
    'red=' + redCount + ',direct=' + directBlack + ',ancestor=' + ancestorBlack;
}};
img.src = 'data:image/png;base64,{png_b64}';
</script></body></html>"##
    );
    let inspect_url = format!(
        "data:text/html;base64,{}",
        support::base64_encode(inspect_html.as_bytes())
    );
    let inspect_case = dir.write(
        "inspect-pixels.yaml",
        &format!(
            r##"name: verify scaled screenshot pixels
steps:
  - browser.open:
      url: "{inspect_url}"
  - browser.wait:
      css: "#px-report"
  - browser.read:
      css: "#px-report"
    expect:
      - path: $.text
        equals: "red=0,direct=true,ancestor=true"
"##
        ),
    );
    let inspect_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&inspect_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&inspect_out),
        Some(0),
        "pixel inspection failed: stdout={}\nstderr={}",
        stdout(&inspect_out),
        stderr(&inspect_out)
    );
}

#[test]
fn within_unmappable_frame_transform_fails_clearly_for_click_and_screenshots() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-iframe-unmappable-transform");

    let rotate_url = server.url("/browser/iframes/unmappable-transform?kind=rotate");
    let skew_url = server.url("/browser/iframes/unmappable-transform?kind=skew");
    let flip_url = server.url("/browser/iframes/unmappable-transform?kind=flip");
    let d3_url = server.url("/browser/iframes/unmappable-transform?kind=3d");
    let ancestor_url = server.url("/browser/iframes/unmappable-transform?kind=ancestor_rotate");
    let no_inputs_url = server.url("/browser/iframes/unmappable-transform?kind=rotate_no_inputs");

    // 1. Check all 5 unmappable transforms fail fast on click, screenshot with fields refuses,
    //    and decorative rotated iframe without fields allows screenshot in one session.
    let checks_case = dir.write(
        "unmappable-checks.yaml",
        &format!(
            r##"name: unmappable frame transforms fail fast and refuse uncovered screenshots
steps:
  - browser.open:
      url: "{rotate_url}"
  - browser.click:
      css: "#scaled-btn"
      timeout_ms: 8000
      within:
        css: "#bad-frame"
    optional: true
  - browser.screenshot:
      path: shots/should-not-exist.png
    optional: true
  - browser.open:
      url: "{skew_url}"
  - browser.click:
      css: "#scaled-btn"
      timeout_ms: 8000
      within:
        css: "#bad-frame"
    optional: true
  - browser.open:
      url: "{flip_url}"
  - browser.click:
      css: "#scaled-btn"
      timeout_ms: 8000
      within:
        css: "#bad-frame"
    optional: true
  - browser.open:
      url: "{d3_url}"
  - browser.click:
      css: "#scaled-btn"
      timeout_ms: 8000
      within:
        css: "#bad-frame"
    optional: true
  - browser.open:
      url: "{ancestor_url}"
  - browser.click:
      css: "#scaled-btn"
      timeout_ms: 8000
      within:
        css: "#bad-frame"
    optional: true
  - browser.open:
      url: "{no_inputs_url}"
  - browser.screenshot:
      path: shots/no-inputs-ok.png
"##
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&checks_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout={}\nstderr={}",
        stdout(&output),
        stderr(&output)
    );
    let value = json_stdout(&output);
    let steps = value["cases"][0]["steps"].as_array().expect("steps array");
    for (idx, label) in [
        (1, "rotate"),
        (4, "skew"),
        (6, "flip"),
        (8, "3d"),
        (10, "ancestor_rotate"),
    ] {
        let step = &steps[idx];
        assert_eq!(step["status"], "skipped", "{label}: {step}");
        let duration_ms = step["duration_ms"].as_u64().unwrap_or(u64::MAX);
        assert!(
            duration_ms < 4000,
            "{label}: unmappable transform must fail immediately instead of waiting 8000 ms (took {duration_ms} ms)"
        );
        let err = step["error"]["message"].as_str().unwrap_or_default();
        assert!(
            err.contains("transform") && err.contains("cannot be mapped"),
            "{label}: expected clear transform mapping error, got: {err}"
        );
    }
    let shot_step = &steps[2];
    assert_eq!(shot_step["status"], "skipped", "{shot_step}");
    let shot_err = shot_step["error"]["message"].as_str().unwrap_or_default();
    assert!(
        shot_err.contains("transform") && shot_err.contains("cover"),
        "expected screenshot refusal naming transform, got: {shot_err}"
    );
    assert!(
        !dir.path().join("shots/should-not-exist.png").exists(),
        "screenshot must not write an uncovered PNG file on failure"
    );
    assert_eq!(steps[12]["status"], "passed", "{}", steps[12]);
    assert!(
        is_png(&fs::read(dir.path().join("shots/no-inputs-ok.png")).expect("PNG must exist")),
        "screenshot must succeed when rotated frame has no editable fields"
    );

    // 2. Failure bundle on a page with an unmappable field inside a rotated iframe must not write uncovered screenshot.png.
    let bundle_root = dir.path().join("failures");
    let bundle_case = dir.write(
        "bundle-rotate.yaml",
        &format!(
            r##"name: failure bundle refuses uncovered screenshot in rotated iframe
steps:
  - browser.open:
      url: "{rotate_url}"
  - browser.click:
      css: "#scaled-btn"
      within:
        css: "#bad-frame"
"##
        ),
    );
    let bundle_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&bundle_case),
            "--bundle-dir",
            path_str(&bundle_root),
        ],
    );
    assert_eq!(
        exit_code(&bundle_out),
        Some(1),
        "unmappable-transform-bundle must fail: stdout: {}\nstderr: {}",
        stdout(&bundle_out),
        stderr(&bundle_out)
    );
    let bundle_json = json_stdout(&bundle_out);
    let bundle_rel = bundle_json["cases"][0]["bundle"]
        .as_str()
        .expect("failed case must write failure bundle");
    let bundle_dir = PathBuf::from(bundle_rel);
    assert!(
        !bundle_dir.join("browser/screenshot.png").exists(),
        "failure bundle must not emit an uncovered screenshot.png"
    );
    let evidence_err = fs::read_to_string(bundle_dir.join("browser/evidence-error.txt"))
        .expect("evidence-error.txt must record why screenshot was withheld");
    assert!(
        evidence_err.contains("transform"),
        "evidence-error.txt must explain the transform failure: {evidence_err}"
    );
}

#[test]
fn within_iframe_retry_enforces_allowlist_and_handles_dialogs() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-iframe-retry-dialogs");
    let url = server.url("/browser/iframes/retry");

    for (name, action, body) in [
        (
            "click-within-retry.yaml",
            "browser.click",
            format!(
                "name: click within retry rejected\nsteps:\n  - browser.open:\n      url: \"{url}\"\n  - browser.click:\n      css: \"#arm-confirm\"\n      within:\n        css: \"#retry-frame\"\n    retry:\n      until:\n        - path: $.text\n          equals: ok\n      timeout_ms: 500\n"
            ),
        ),
        (
            "type-within-retry.yaml",
            "browser.type",
            format!(
                "name: type within retry rejected\nsteps:\n  - browser.open:\n      url: \"{url}\"\n  - browser.type:\n      css: \"#arm-confirm\"\n      text: hi\n      within:\n        css: \"#retry-frame\"\n    retry:\n      until:\n        - path: $.text\n          equals: ok\n      timeout_ms: 500\n"
            ),
        ),
    ] {
        let bad_case = dir.write(name, &body);
        let lint_out = run_da_in(
            dir.path(),
            &["--output", "json", "lint", path_str(&bad_case)],
        );
        assert_eq!(
            exit_code(&lint_out),
            Some(3),
            "{name} must fail lint with exit 3: stdout={}\nstderr={}",
            stdout(&lint_out),
            stderr(&lint_out)
        );
        let lint_err = stderr(&lint_out);
        let lint_stdout = stdout(&lint_out);
        assert!(
            (lint_stdout.contains(action) && lint_stdout.contains("cannot use `retry`"))
                || (lint_err.contains(action) && lint_err.contains("cannot use `retry`")),
            "{name}: expected retry disallow error, stdout={lint_stdout} stderr={lint_err}"
        );
    }

    let case = dir.write(
        "iframe-retry-dialogs.yaml",
        &format!(
            r##"name: retry on browser.wait and browser.read inside within iframe handles dialogs
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#arm-confirm"
      within:
        css: "#retry-frame"
  - browser.wait:
      css: "#status"
      dialog: accept
      timeout_ms: 300
      within:
        css: "#retry-frame"
    retry:
      until:
        - path: $.dialog.type
          equals: confirm
      timeout_ms: 8000
      interval_ms: 50
    expect:
      - path: $.dialog.message
        equals: "Ready now?"
      - path: $.dialog.accepted
        equals: true
  - browser.read:
      css: "#status"
      within:
        css: "#retry-frame"
    retry:
      until:
        - path: $.text
          equals: "confirmed"
      timeout_ms: 5000
      interval_ms: 50
  - browser.click:
      css: "#arm-unexpected"
      within:
        css: "#retry-frame"
  - browser.read:
      css: "#status"
      timeout_ms: 5000
      within:
        css: "#retry-frame"
    retry:
      until:
        - path: $.text
          equals: "after-alert"
      timeout_ms: 8000
      interval_ms: 50
"##
        ),
    );
    let lint_ok = run_da_in(dir.path(), &["--output", "json", "lint", path_str(&case)]);
    assert_eq!(
        exit_code(&lint_ok),
        Some(0),
        "iframe retry read/wait must pass lint: stdout={}\nstderr={}",
        stdout(&lint_ok),
        stderr(&lint_ok)
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
    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["status"], "passed", "{report}");
    assert_eq!(
        report["cases"][0]["steps"][2]["result"]["dialog"]["type"], "confirm",
        "{report}"
    );
    assert_eq!(
        report["cases"][0]["steps"][5]["result"]["text"], "after-alert",
        "{report}"
    );
}

/// Asserts that neither half of the browser auth state appears in `text`.
fn assert_no_state_secrets(label: &str, text: &str) {
    for secret in [
        support::AUTH_SESSION_COOKIE,
        support::AUTH_LOCAL_STORAGE_TOKEN,
    ] {
        assert!(
            !text.contains(secret),
            "{label} must not contain the browser state value `{secret}`: {text}"
        );
    }
}

/// Every file under `dir`, as lossy text, so a test can search a bundle.
fn bundle_texts(dir: &Path) -> Vec<(PathBuf, String)> {
    files_under(dir)
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).unwrap_or_default();
            (path, String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect()
}

#[test]
fn a_saved_storage_state_signs_a_second_run_in_without_typing() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-storage-state");

    let login_case = dir.write(
        "login.yaml",
        &format!(
            r##"name: sign in and save the state
steps:
  - browser.open:
      url: "{login}"
  - browser.type:
      css: "#user"
      text: alice
  - browser.type:
      css: "#password"
      text: "hunter2-typed-once"
  - browser.press:
      key: Enter
  - browser.wait:
      url_contains: /auth/me
  - browser.read:
      css: "#who"
    expect:
      - path: $.text
        equals: "signed in as alice"
  - browser.save_state:
      path: state/session.json
"##,
            login = server.url("/auth/login")
        ),
    );

    let first = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&login_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&first),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&first),
        stderr(&first)
    );
    let first_json = json_stdout(&first);
    let saved = &first_json["cases"][0]["steps"][6]["result"];
    assert!(
        saved["path"]
            .as_str()
            .is_some_and(|path| path.ends_with("state/session.json")),
        "the save step must report the file it wrote: {saved}"
    );
    assert!(
        saved["cookies"].as_u64().is_some_and(|count| count >= 1),
        "the save step must report the cookies it wrote: {saved}"
    );
    assert!(
        saved["local_storage"]
            .as_u64()
            .is_some_and(|count| count >= 1),
        "the save step must report the localStorage entries it wrote: {saved}"
    );
    assert_no_state_secrets("the first run's JSON output", &stdout(&first));

    let state_path = dir.path().join("state/session.json");
    let state_text = fs::read_to_string(&state_path).expect("the state file must exist");
    assert!(
        state_text.contains(support::AUTH_SESSION_COOKIE),
        "the state file must carry the session cookie: {state_text}"
    );
    assert!(
        state_text.contains(support::AUTH_LOCAL_STORAGE_TOKEN),
        "the state file must carry the localStorage token: {state_text}"
    );
    assert!(
        state_text.contains(&server.url("")),
        "the state file must name the origin: {state_text}"
    );

    // A second, independent run: a new `da run` process and a fresh browser
    // profile, so only the state file can sign this page in.
    let reuse_case = dir.write(
        "reuse.yaml",
        &format!(
            r##"name: reuse the saved login
steps:
  - browser.open:
      url: "{me}"
      storage_state: state/session.json
  - browser.read:
      css: "#who"
    expect:
      - path: $.text
        equals: "signed in as alice"
  - browser.read:
      css: "#token"
    expect:
      - path: $.text
        equals: "token present"
"##,
            me = server.url("/auth/me")
        ),
    );

    let second = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&reuse_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&second),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&second),
        stderr(&second)
    );
    let second_json = json_stdout(&second);
    assert_eq!(second_json["cases"][0]["status"], "passed", "{second_json}");
    assert_eq!(
        second_json["cases"][0]["steps"][1]["result"]["text"], "signed in as alice",
        "{second_json}"
    );
    assert_no_state_secrets("the second run's JSON output", &stdout(&second));

    // The server saw the cookie on the navigation, so the page was authorized
    // before any step typed anything.
    let gated: Vec<_> = server
        .requests()
        .into_iter()
        .filter(|request| request.path() == "/auth/me")
        .collect();
    assert!(
        gated.iter().any(|request| request
            .headers
            .get("cookie")
            .is_some_and(|header| header.contains(support::AUTH_SESSION_COOKIE))),
        "the server must see the saved cookie on the reused login: {gated:?}"
    );
}

#[test]
fn a_missing_or_malformed_storage_state_fails_closed() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-storage-state-fail");

    let missing_case = dir.write(
        "missing.yaml",
        &format!(
            r##"name: missing state
steps:
  - browser.open:
      url: "{}"
      storage_state: state/missing.json
"##,
            server.url("/auth/me")
        ),
    );
    let missing = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&missing_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&missing),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&missing),
        stderr(&missing)
    );
    let missing_json = json_stdout(&missing);
    let message = case_error(&missing_json);
    assert!(
        message.contains("could not read browser storage state")
            && message.contains("missing.json"),
        "got: {message}"
    );
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.path() != "/auth/me"),
        "a missing state file must fail before the browser opens the page"
    );

    dir.write("state/broken.json", "{\"cookies\": [");
    let malformed_case = dir.write(
        "malformed.yaml",
        &format!(
            r##"name: malformed state
steps:
  - browser.open:
      url: "{}"
      storage_state: state/broken.json
"##,
            server.url("/auth/me")
        ),
    );
    let malformed = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&malformed_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&malformed),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&malformed),
        stderr(&malformed)
    );
    let malformed_json = json_stdout(&malformed);
    let message = case_error(&malformed_json);
    assert!(
        message.contains("is malformed") && message.contains("broken.json"),
        "got: {message}"
    );
    assert_no_state_secrets("the failure output", &stdout(&malformed));

    dir.write(
        "state/future.json",
        "{\"version\": 99, \"cookies\": [{\"name\": \"sid\", \"value\": \"s3cr3t-session\", \"url\": \"http://127.0.0.1/\"}]}",
    );
    let future_case = dir.write(
        "future.yaml",
        &format!(
            r##"name: unsupported state version
steps:
  - browser.open:
      url: "{}"
      storage_state: state/future.json
"##,
            server.url("/auth/me")
        ),
    );
    let future = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&future_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&future),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&future),
        stderr(&future)
    );
    let future_json = json_stdout(&future);
    let message = case_error(&future_json);
    assert!(
        message.contains("unsupported `version` 99"),
        "got: {message}"
    );
    assert_no_state_secrets("the unsupported-version output", &stdout(&future));
}

#[test]
fn storage_state_arguments_are_refused_by_lint_before_any_browser_starts() {
    let dir = TempDir::new("browser-storage-state-lint");

    let conflicted = dir.write(
        "conflict.yaml",
        r##"name: connected state
steps:
  - browser.open:
      url: http://127.0.0.1:8080/orders
      connect: http://127.0.0.1:9222
      storage_state: state/session.json
"##,
    );
    let lint = run_da_in(dir.path(), &["lint", path_str(&conflicted)]);
    assert_eq!(
        exit_code(&lint),
        Some(3),
        "stdout: {}\nstderr: {}",
        stdout(&lint),
        stderr(&lint)
    );
    assert!(
        stdout(&lint).contains("`storage_state` and `connect` cannot both be set"),
        "got: {}",
        stdout(&lint)
    );

    let empty = dir.write(
        "empty.yaml",
        r##"name: empty state
steps:
  - browser.open:
      url: http://127.0.0.1:8080/orders
      storage_state: ""
"##,
    );
    let lint = run_da_in(dir.path(), &["lint", path_str(&empty)]);
    assert_eq!(
        exit_code(&lint),
        Some(3),
        "stdout: {}\nstderr: {}",
        stdout(&lint),
        stderr(&lint)
    );

    let save_without_path = dir.write(
        "save.yaml",
        r##"name: save without a path
steps:
  - browser.save_state: {}
"##,
    );
    let lint = run_da_in(dir.path(), &["lint", path_str(&save_without_path)]);
    assert_eq!(
        exit_code(&lint),
        Some(3),
        "stdout: {}\nstderr: {}",
        stdout(&lint),
        stderr(&lint)
    );
}

#[test]
fn saving_state_without_an_open_page_names_the_missing_step() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-save-state-no-open");
    let case = dir.write(
        "save.yaml",
        r##"name: save before opening
steps:
  - browser.save_state:
      path: state/session.json
"##,
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
    let output_json = json_stdout(&output);
    let message = case_error(&output_json);
    assert!(
        message.contains("no browser session is open"),
        "got: {message}"
    );
    assert!(
        !dir.path().join("state/session.json").exists(),
        "a refused save must not write a state file"
    );
}

#[test]
fn a_named_profile_keeps_a_login_across_runs() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-profile-reuse");
    let profile_dir = dir.path().join(".da/profiles/ci-login");

    let first_case = dir.write(
        "first.yaml",
        &format!(
            r##"name: write a persistent login
steps:
  - browser.open:
      url: "{}"
      profile: ci-login
  - browser.read:
      css: "#persisted"
    expect:
      - path: $.text
        equals: "saved"
"##,
            server.url("/auth/persist")
        ),
    );
    let first = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&first_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&first),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&first),
        stderr(&first)
    );
    assert_no_state_secrets("the profile run's output", &stdout(&first));
    assert!(
        profile_dir.is_dir(),
        "a named profile must survive the run that created it"
    );
    assert!(
        fs::read_dir(&profile_dir)
            .expect("the named profile must be readable")
            .next()
            .is_some(),
        "the named profile must keep the browser's files"
    );

    let second_case = dir.write(
        "second.yaml",
        &format!(
            r##"name: reuse the persistent login
steps:
  - browser.open:
      url: "{}"
      profile: ci-login
  - browser.read:
      css: "#who"
    expect:
      - path: $.text
        equals: "signed in as alice"
  - browser.read:
      css: "#token"
    expect:
      - path: $.text
        equals: "token present"
"##,
            server.url("/auth/me")
        ),
    );
    let second = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&second_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&second),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&second),
        stderr(&second)
    );
    let second_json = json_stdout(&second);
    assert_eq!(second_json["cases"][0]["status"], "passed", "{second_json}");
    assert_no_state_secrets("the reused profile run's output", &stdout(&second));
    assert!(
        profile_dir.is_dir(),
        "the second run must keep the named profile too"
    );
}

#[test]
fn a_named_profile_refuses_a_second_concurrent_run() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-profile-lock");

    let holding_case = dir.write(
        "holding.yaml",
        &format!(
            r##"name: hold the profile
steps:
  - browser.open:
      url: "{}"
      profile: ci-lock
"##,
            server.url("/browser/stall")
        ),
    );
    let mut holding = Command::new(da())
        .current_dir(dir.path())
        .args([
            "run",
            path_str(&holding_case),
            "--no-bundle",
            "--output",
            "json",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the first run must start");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let stalled = server
            .requests()
            .iter()
            .any(|request| request.path() == "/browser/stall");
        if stalled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first run never opened the stalling page"
        );
        assert!(
            holding
                .try_wait()
                .expect("the first run must be watchable")
                .is_none(),
            "the first run finished before the second one could compete for the profile"
        );
        thread::sleep(Duration::from_millis(50));
    }

    let competing_case = dir.write(
        "competing.yaml",
        &format!(
            r##"name: compete for the profile
steps:
  - browser.open:
      url: "{}"
      profile: ci-lock
"##,
            server.url("/health")
        ),
    );
    let competing = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&competing_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&competing),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&competing),
        stderr(&competing)
    );
    let competing_json = json_stdout(&competing);
    let message = case_error(&competing_json);
    assert!(
        message.contains("already in use") && message.contains("ci-lock"),
        "got: {message}"
    );

    let status = holding.wait().expect("the first run must finish");
    assert!(
        status.success(),
        "the run that held the profile must still pass: {status}"
    );
    assert!(
        dir.path().join(".da/profiles/ci-lock").is_dir(),
        "a refused competing run must not remove the profile"
    );
}

#[test]
fn a_profile_failure_bundle_marks_evidence_local_only() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-profile-local-only");
    let case = dir.write(
        "local.yaml",
        &format!(
            r##"name: profile evidence
steps:
  - browser.open:
      url: "{}"
      profile: ci-evidence
  - browser.read:
      css: "#never-there"
      timeout_ms: 300
"##,
            server.url("/auth/persist")
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
    assert_eq!(value["cases"][0]["local_only"], true, "{value}");
    assert!(
        value["cases"][0]["local_only_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("ci-evidence")),
        "{value}"
    );
    assert_no_state_secrets("the local-only output", &stdout(&output));

    let bundle = PathBuf::from(
        value["cases"][0]["bundle"]
            .as_str()
            .expect("the failed case must name its bundle"),
    );
    let meta: Value = serde_json::from_str(
        &fs::read_to_string(bundle.join("meta.json")).expect("the bundle must hold meta.json"),
    )
    .expect("meta.json must be JSON");
    assert_eq!(meta["local_only"], true, "{meta}");
    assert!(
        meta["local_only_reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("ci-evidence")),
        "{meta}"
    );

    for (path, text) in bundle_texts(&bundle) {
        assert_no_state_secrets(
            &format!("the failure bundle file {}", path.display()),
            &text,
        );
    }

    // A case that starts its own browser keeps its evidence shareable.
    let ephemeral = dir.write(
        "ephemeral.yaml",
        &format!(
            r##"name: ephemeral evidence
steps:
  - browser.open:
      url: "{}"
  - browser.read:
      css: "#never-there"
      timeout_ms: 300
"##,
            server.url("/auth/persist")
        ),
    );
    let ephemeral_output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&ephemeral),
            "--bundle-dir",
            path_str(&bundles),
        ],
    );
    assert_eq!(exit_code(&ephemeral_output), Some(1));
    let ephemeral_json = json_stdout(&ephemeral_output);
    assert!(
        ephemeral_json["cases"][0].get("local_only").is_none(),
        "an ephemeral browser must not mark its evidence local-only: {ephemeral_json}"
    );
    let ephemeral_bundle = PathBuf::from(
        ephemeral_json["cases"][0]["bundle"]
            .as_str()
            .expect("the failed case must name its bundle"),
    );
    let ephemeral_meta: Value = serde_json::from_str(
        &fs::read_to_string(ephemeral_bundle.join("meta.json"))
            .expect("the bundle must hold meta.json"),
    )
    .expect("meta.json must be JSON");
    assert_eq!(ephemeral_meta["local_only"], false, "{ephemeral_meta}");
}

#[test]
fn set_cookies_signs_the_page_in_before_and_after_it_opens() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-set-cookies");
    let origin = server.url("");

    let before_open = dir.write(
        "before.yaml",
        &format!(
            r##"name: inject before opening
steps:
  - browser.set_cookies:
      cookies:
        - name: sid
          value: "{cookie}"
          url: "{origin}/"
          http_only: true
          same_site: Lax
      local_storage:
        "{origin}":
          auth_token: "{token}"
  - browser.open:
      url: "{me}"
  - browser.read:
      css: "#who"
    expect:
      - path: $.text
        equals: "signed in as alice"
  - browser.read:
      css: "#token"
    expect:
      - path: $.text
        equals: "token present"
"##,
            cookie = support::AUTH_SESSION_COOKIE,
            token = support::AUTH_LOCAL_STORAGE_TOKEN,
            origin = origin,
            me = server.url("/auth/me")
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&before_open),
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
    assert_no_state_secrets("the set_cookies output", &stdout(&output));

    let after_open = dir.write(
        "after.yaml",
        &format!(
            r##"name: inject after opening
steps:
  - browser.open:
      url: "{login}"
  - browser.set_cookies:
      cookies:
        - name: sid
          value: "{cookie}"
          url: "{origin}/"
          http_only: true
          same_site: Lax
      local_storage:
        current:
          auth_token: "{token}"
  - browser.open:
      url: "{me}"
  - browser.read:
      css: "#who"
    expect:
      - path: $.text
        equals: "signed in as alice"
  - browser.read:
      css: "#token"
    expect:
      - path: $.text
        equals: "token present"
"##,
            cookie = support::AUTH_SESSION_COOKIE,
            token = support::AUTH_LOCAL_STORAGE_TOKEN,
            origin = origin,
            login = server.url("/auth/login"),
            me = server.url("/auth/me")
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&after_open),
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
    assert_no_state_secrets("the after-open set_cookies output", &stdout(&output));
}

#[test]
fn a_storage_state_file_wins_over_a_profile_login() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-storage-state-precedence");

    // The profile stores a persistent session cookie of its own.
    let seed_case = dir.write(
        "seed.yaml",
        &format!(
            r##"name: seed the profile
steps:
  - browser.open:
      url: "{}"
      profile: ci-precedence
  - browser.read:
      css: "#persisted"
    expect:
      - path: $.text
        equals: "saved"
"##,
            server.url("/auth/persist")
        ),
    );
    let seed = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&seed_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&seed),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&seed),
        stderr(&seed)
    );

    // A state file names a different value for the same cookie, and the page
    // reports which one arrived without ever printing a cookie value.
    dir.write(
        "state/override.json",
        &format!(
            "{{\"version\":1,\"cookies\":[{{\"name\":\"sid\",\"value\":\"state-file-session\",\"url\":\"{}/\"}}]}}",
            server.url("")
        ),
    );
    let override_case = dir.write(
        "override.yaml",
        &format!(
            r##"name: the state file wins
steps:
  - browser.open:
      url: "{}"
      profile: ci-precedence
      storage_state: state/override.json
  - browser.read:
      css: "#source"
    expect:
      - path: $.text
        equals: "state file"
"##,
            server.url("/auth/which")
        ),
    );
    let output = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&override_case),
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
    assert_no_state_secrets("the precedence output", &stdout(&output));
}

#[test]
fn record_with_trace_steps_captures_masked_per_step_artifacts_and_renders_report() {
    const SECRET: &str = "trace-browser-Secr3t/77";
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-trace-steps");
    let case_path = dir.write(
        "trace-browser.yaml",
        &format!(
            r##"name: Browser step traces
steps:
  - browser.open:
      url: "{base}/browser/form"
    expect:
      - path: $.url
        exists: true
  - browser.type:
      css: "#name"
      text: "${{env:DA_BR_TRACE_SEC}}"
  - browser.read:
      css: "#greeting"
    expect:
      - path: $.text
        equals: never
  - browser.read:
      css: "#greeting"
finally:
  - browser.open:
      url: "{base}/browser/record?secret=${{env:DA_BR_TRACE_SEC}}"
"##,
            base = server.url("")
        ),
    );

    // Without --trace-steps, no trace/ directory is written.
    let no_trace_rec = dir.path().join("rec-plain");
    let plain_out = run_da_with_env(
        dir.path(),
        &[
            "run",
            path_str(&case_path),
            "--record",
            path_str(&no_trace_rec),
            "--no-bundle",
        ],
        &[("DA_BR_TRACE_SEC", SECRET.to_string())],
    );
    assert_eq!(
        exit_code(&plain_out),
        Some(1),
        "stderr: {}",
        stderr(&plain_out)
    );
    let plain_case_dir = fs::read_dir(&no_trace_rec)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("recorded plain case dir");
    assert!(!plain_case_dir.join("trace").exists());

    // With --trace-steps, executed steps and finally steps capture redacted masked artifacts.
    let trace_rec = dir.path().join("rec-traced");
    let traced_out = run_da_with_env(
        dir.path(),
        &[
            "run",
            path_str(&case_path),
            "--record",
            path_str(&trace_rec),
            "--trace-steps",
            "--no-bundle",
        ],
        &[("DA_BR_TRACE_SEC", SECRET.to_string())],
    );
    assert_eq!(
        exit_code(&traced_out),
        Some(1),
        "stderr: {}",
        stderr(&traced_out)
    );
    let traced_case_dir = fs::read_dir(&trace_rec)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir())
        .expect("recorded traced case dir");

    for step_idx in [0usize, 1, 2] {
        let png_path =
            traced_case_dir.join(format!("trace/steps/{step_idx}/browser/screenshot.png"));
        let dom_path = traced_case_dir.join(format!("trace/steps/{step_idx}/browser/dom.html"));
        assert!(png_path.is_file(), "missing {}", png_path.display());
        assert!(is_png(&fs::read(&png_path).unwrap()));
        let dom = fs::read_to_string(&dom_path).unwrap();
        assert!(
            !dom.contains(SECRET),
            "raw secret leaked in {}",
            dom_path.display()
        );
    }
    assert!(
        !traced_case_dir.join("trace/steps/3").exists(),
        "post-failure unexecuted step must not be traced"
    );
    assert!(
        traced_case_dir
            .join("trace/finally/0/browser/screenshot.png")
            .is_file()
    );
    assert!(
        traced_case_dir
            .join("trace/finally/0/browser/dom.html")
            .is_file()
    );

    let html_out = dir.path().join("report.html");
    let rep = run_da_in(
        dir.path(),
        &["report", path_str(&trace_rec), "-o", path_str(&html_out)],
    );
    assert_eq!(exit_code(&rep), Some(0), "stderr: {}", stderr(&rep));
    let html = fs::read_to_string(&html_out).unwrap();
    assert!(!html.contains(SECRET), "secret leaked into report.html");
    assert!(html.contains("data:image/png;base64,"));
}

struct CuServer {
    port: u16,
    hits: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl CuServer {
    fn start(routes: Vec<(&str, &str)>) -> Self {
        let listener = support::bind_browser_safe_listener();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let hits_log = std::sync::Arc::clone(&hits);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = std::sync::Arc::clone(&stop);
        let owned_routes: Vec<(String, String)> = routes
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut buf = [0_u8; 4096];
                        let n = stream.read(&mut buf).unwrap_or(0);
                        let req = String::from_utf8_lossy(&buf[..n]);
                        let target = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("/");
                        if let Ok(mut guard) = hits_log.lock() {
                            guard.push(target.to_string());
                        }
                        let path = target.split('?').next().unwrap_or(target);
                        let (status, body) = owned_routes
                            .iter()
                            .find(|(p, _)| p == path)
                            .map(|(_, html)| ("200 OK", html.as_str()))
                            .unwrap_or((
                                "404 Not Found",
                                "<!doctype html><html><body>not found</body></html>",
                            ));
                        let resp = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(resp.as_bytes());
                        let _ = stream.flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            hits,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    fn hits_matching(&self, prefix: &str) -> Vec<String> {
        self.hits
            .lock()
            .expect("lock CuServer hits")
            .iter()
            .filter(|u| u.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn clear_hits(&self) {
        self.hits.lock().expect("lock CuServer hits").clear();
    }
}

impl Drop for CuServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn computer_use_lint_and_retry_policies_enforce_contracts() {
    let dir = TempDir::new("browser-cu-lint");
    let invalid = [
        ("scroll-empty.yaml", "  - browser.scroll: {}\n", "delta"),
        (
            "scroll-at-with-locator.yaml",
            "  - browser.scroll:\n      css: \"#box\"\n      at: { x: 10, y: 10 }\n      delta_y: 50\n",
            "at",
        ),
        (
            "scroll-into-view-without-target.yaml",
            "  - browser.scroll:\n      into_view: true\n",
            "into_view",
        ),
        (
            "hover-neither.yaml",
            "  - browser.hover: {}\n",
            "one locator",
        ),
        (
            "hover-at-with-css.yaml",
            "  - browser.hover:\n      css: \"#btn\"\n      at: { x: 10, y: 20 }\n",
            "at",
        ),
        (
            "drag-missing-to.yaml",
            "  - browser.drag:\n      from: { css: \"#a\" }\n",
            "to",
        ),
        (
            "drag-bad-from.yaml",
            "  - browser.drag:\n      from: { css: \"#a\", text: \"b\" }\n      to: { css: \"#c\" }\n",
            "cannot both be set",
        ),
        (
            "set-clipboard-missing-text.yaml",
            "  - browser.set_clipboard: {}\n",
            "text",
        ),
        (
            "read-clipboard-extra-arg.yaml",
            "  - browser.read_clipboard:\n      bogus: true\n",
            "bogus",
        ),
        (
            "scroll-retry-disallowed.yaml",
            "  - browser.scroll:\n      delta_y: 100\n    retry:\n      until:\n        - path: $.scrolled\n          equals: true\n      timeout_ms: 500\n      interval_ms: 50\n",
            "retry",
        ),
        (
            "hover-retry-disallowed.yaml",
            "  - browser.hover:\n      css: \"#menu\"\n    retry:\n      until:\n        - path: $.hovered\n          equals: true\n      timeout_ms: 500\n      interval_ms: 50\n",
            "retry",
        ),
        (
            "drag-retry-disallowed.yaml",
            "  - browser.drag:\n      from: { css: \"#a\" }\n      to: { css: \"#b\" }\n    retry:\n      until:\n        - path: $.dragged\n          equals: true\n      timeout_ms: 500\n      interval_ms: 50\n",
            "retry",
        ),
        (
            "set-clipboard-retry-disallowed.yaml",
            "  - browser.set_clipboard:\n      text: \"hi\"\n    retry:\n      until:\n        - path: $.characters\n          equals: 2\n      timeout_ms: 500\n      interval_ms: 50\n",
            "retry",
        ),
    ];

    for (name, step_yaml, expected_fragment) in invalid {
        let case = dir.write(
            name,
            &format!(
                "name: {name}\nsteps:\n  - browser.open:\n      url: \"http://127.0.0.1:1/\"\n{step_yaml}"
            ),
        );
        let out = run_da_in(dir.path(), &["lint", path_str(&case)]);
        assert_ne!(
            exit_code(&out),
            Some(0),
            "{name} must fail lint\nstdout={}\nstderr={}",
            stdout(&out),
            stderr(&out)
        );
        let combined = format!("{}\n{}", stdout(&out), stderr(&out)).to_lowercase();
        assert!(
            combined.contains(&expected_fragment.to_lowercase()),
            "{name} error must mention `{expected_fragment}`, got:\n{combined}"
        );
    }

    // Bare `at` coordinates emit warnings for scroll, hover, and drag.
    let warn_case = dir.write(
        "cu-bare-at-warnings.yaml",
        r##"name: cu bare at warnings
steps:
  - browser.open:
      url: "http://127.0.0.1:1/"
  - browser.scroll:
      at: { x: 10, y: 20 }
      delta_y: 50
  - browser.hover:
      at: { x: 10, y: 20 }
  - browser.drag:
      from: { at: { x: 10, y: 20 } }
      to: { at: { x: 80, y: 20 } }
  - browser.read_clipboard: {}
    retry:
      until:
        - path: $.text
          equals: "ready"
      timeout_ms: 1000
      interval_ms: 50
"##,
    );
    let warn_out = run_da_in(
        dir.path(),
        &["--output", "json", "lint", path_str(&warn_case)],
    );
    assert_eq!(
        exit_code(&warn_out),
        Some(0),
        "valid bare-at & retry read_clipboard must pass lint: stdout={}\nstderr={}",
        stdout(&warn_out),
        stderr(&warn_out)
    );
    let warn_json = json_stdout(&warn_out);
    let warnings = warn_json["warnings"]
        .as_array()
        .expect("lint JSON must report warnings");
    assert!(
        warnings.len() >= 4,
        "scroll, hover, drag.from, drag.to bare `at` should warn: {warn_json}"
    );
}

#[test]
fn browser_scroll_handles_viewport_container_into_view_and_coordinates() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![(
        "/scroll",
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; font-family: sans-serif; height: 2600px; }
  #scroll-box {
    margin: 20px; width: 220px; height: 100px; overflow: auto;
    border: 1px solid #444;
  }
  #scroll-box-inner { height: 600px; padding-top: 300px; }
  #wheel-box {
    position: fixed; left: 20px; top: 200px; width: 160px; height: 80px;
    overflow: auto; border: 1px solid #06c;
  }
  #wheel-inner { height: 500px; }
  #far-btn {
    position: absolute; left: 320px; top: 1700px; width: 140px; height: 40px;
  }
  #probe-state { position: fixed; top: 40px; right: 10px; }
  #status { position: fixed; top: 10px; right: 10px; }
</style></head>
<body>
  <div id="scroll-box"><div id="scroll-box-inner"><span id="inner-mark">inner</span></div></div>
  <div id="wheel-box"><div id="wheel-inner">wheel</div></div>
  <div id="wheel-box2" style="position:fixed;left:220px;top:200px;width:160px;height:80px;overflow:auto;border:1px solid #096;"><div id="wheel-inner2" style="height:500px;">wheel2</div></div>
  <button id="far-btn" onclick="document.getElementById('status').textContent = 'clicked:' + Math.round(window.scrollY)">Far Button</button>
  <button id="probe-state" onclick="
    const r = document.getElementById('far-btn').getBoundingClientRect();
    const ok = window.scrollY >= 200
      && document.getElementById('scroll-box').scrollTop >= 100
      && document.getElementById('wheel-box').scrollTop >= 60
      && document.getElementById('wheel-box2').scrollTop >= 40;
    document.getElementById('status').textContent =
      (ok ? 'ok' : 'bad') + ':visible=' + (r.top >= 0 && r.bottom <= window.innerHeight);
  ">Probe</button>
  <div id="status">init</div>
</body>
</html>"##,
    )]);

    let dir = TempDir::new("browser-scroll-e2e");
    let case = dir.write(
        "scroll.yaml",
        &format!(
            r##"name: scroll all modes
steps:
  - browser.open:
      url: "{}"
      width: 1280
      height: 720
  - browser.scroll:
      delta_y: 240
  - browser.scroll:
      css: "#scroll-box"
      delta_y: 120
  - browser.scroll:
      at: {{ x: 100, y: 240 }}
      delta_y: 100
  - browser.scroll:
      css: "#wheel-inner2"
      delta_y: 80
  - browser.click:
      css: "#probe-state"
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        equals: "ok:visible=false"
  - browser.scroll:
      css: "#far-btn"
      into_view: true
  - browser.click:
      css: "#probe-state"
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        contains: "visible=true"
  - browser.click:
      css: "#far-btn"
  - browser.read:
      css: "#status"
    expect:
      - path: $.text
        contains: "clicked:"
"##,
            cu.url("/scroll")
        ),
    );

    let out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&out),
        Some(0),
        "scroll case must pass: stdout={}\nstderr={}",
        stdout(&out),
        stderr(&out)
    );
    let j = json_stdout(&out);
    let vp = &j["cases"][0]["steps"][3]["result"]["viewport"];
    let vw = vp["width"]
        .as_u64()
        .unwrap_or_else(|| panic!("coordinate scroll must report viewport width: {j}"));
    let vh = vp["height"]
        .as_u64()
        .unwrap_or_else(|| panic!("coordinate scroll must report viewport height: {j}"));
    assert!(vw > 0 && vh > 0, "got viewport {vw}x{vh}");

    // Mismatched pinned viewport on scroll `at:` fails loudly instead of scrolling blindly.
    let mismatch_w = vw + 1;
    let bad_vp = dir.write(
        "scroll-bad-vp.yaml",
        &format!(
            r##"name: scroll viewport mismatch
steps:
  - browser.open:
      url: "{}"
      width: 1280
      height: 720
  - browser.scroll:
      at:
        x: 100
        y: 240
        viewport: {{ width: {mismatch_w}, height: {vh} }}
      delta_y: 80
"##,
            cu.url("/scroll")
        ),
    );
    let bad_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&bad_vp), "--no-bundle"],
    );
    assert_eq!(exit_code(&bad_out), Some(1), "{:?}", stdout(&bad_out));
    assert!(
        stdout(&bad_out).contains("viewport"),
        "viewport mismatch error must explain size mismatch: {}",
        stdout(&bad_out)
    );

    // Nested same-origin iframes (`top -> #outer -> #inner -> #deep-btn`) scroll
    // outward from innermost to outermost enclosing frame.
    let nested_cu = CuServer::start(vec![
        (
            "/nested-top",
            r##"<!doctype html><html><body style="margin:0;height:2200px">
              <iframe id="outer" src="/nested-outer" style="position:absolute;top:1500px;left:20px;width:400px;height:200px;border:1px solid #333"></iframe>
            </body></html>"##,
        ),
        (
            "/nested-outer",
            r##"<!doctype html><html><body style="margin:0;height:900px">
              <iframe id="inner" src="/nested-inner" style="position:absolute;top:520px;left:10px;width:320px;height:120px;border:1px solid #666"></iframe>
            </body></html>"##,
        ),
        (
            "/nested-inner",
            r##"<!doctype html><html><body style="margin:0;height:400px">
              <button id="deep-btn" style="position:absolute;top:240px;left:20px" onclick="this.textContent='clicked'">Deep</button>
            </body></html>"##,
        ),
    ]);
    let nested_case = dir.write(
        "scroll-nested-frames.yaml",
        &format!(
            r##"name: scroll nested enclosing frames into view
steps:
  - browser.open:
      url: "{}"
      width: 800
      height: 600
  - browser.scroll:
      css: "#deep-btn"
      within:
        css: "#inner"
        within:
          css: "#outer"
      into_view: true
    expect:
      - path: $.scrolled
        equals: true
      - path: $.scroll_y
        not_equals: 0
  - browser.click:
      css: "#deep-btn"
      within:
        css: "#inner"
        within:
          css: "#outer"
  - browser.read:
      css: "#deep-btn"
      within:
        css: "#inner"
        within:
          css: "#outer"
    expect:
      - path: $.text
        equals: "clicked"
"##,
            nested_cu.url("/nested-top")
        ),
    );
    let nested_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&nested_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&nested_out),
        Some(0),
        "nested iframe scroll into view must pass: stdout={}\nstderr={}",
        stdout(&nested_out),
        stderr(&nested_out)
    );
}

#[test]
fn browser_hover_triggers_css_hover_mouse_events_and_coordinates() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![(
        "/hover",
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; font-family: sans-serif; }
  #menu { position: relative; display: inline-block; padding: 12px 20px; background: #eee; margin: 20px; }
  #submenu { display: none; position: absolute; left: 0; top: 100%; background: #cce; padding: 8px; }
  #menu:hover #submenu { display: block; }
  #coord-pad { position: fixed; left: 40px; top: 200px; width: 120px; height: 50px; background: #efe; }
  #covered-wrap { position: relative; width: 120px; height: 40px; margin: 20px; }
  #covered-btn, #cover { position: absolute; inset: 0; }
  #cover { background: rgba(255,0,0,0.4); z-index: 2; }
</style></head>
<body>
  <div id="menu">Menu
    <button id="submenu" onclick="document.getElementById('out').textContent='sub-clicked'">Sub Item</button>
  </div>
  <div id="coord-pad" onmouseenter="document.getElementById('coord-out').textContent='coord-entered'">Pad</div>
  <div id="covered-wrap">
    <button id="covered-btn">Covered</button>
    <div id="cover">Blocker</div>
  </div>
  <div id="log"></div>
  <div id="out">none</div>
  <div id="coord-out">none</div>
  <script>
    const m = document.getElementById('menu');
    const l = document.getElementById('log');
    for (const ev of ['mouseover', 'mouseenter']) {
      m.addEventListener(ev, () => { l.textContent += ev + ';'; });
    }
  </script>
</body>
</html>"##,
    )]);

    let dir = TempDir::new("browser-hover-e2e");
    let bare_case = dir.write(
        "hover-bare.yaml",
        &format!(
            r##"name: hover bare coords
steps:
  - browser.open:
      url: "{}"
      width: 1280
      height: 720
  - browser.hover:
      at:
        x: 100
        y: 225
  - browser.read:
      css: "#coord-out"
    expect:
      - path: $.text
        equals: "coord-entered"
"##,
            cu.url("/hover")
        ),
    );
    let bare_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&bare_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&bare_out),
        Some(0),
        "stdout={}\nstderr={}",
        stdout(&bare_out),
        stderr(&bare_out)
    );
    let bare_json = json_stdout(&bare_out);
    let vw = bare_json["cases"][0]["steps"][1]["result"]["viewport"]["width"]
        .as_u64()
        .unwrap_or_else(|| panic!("hover must report viewport width: {bare_json}"));
    let vh = bare_json["cases"][0]["steps"][1]["result"]["viewport"]["height"]
        .as_u64()
        .unwrap_or_else(|| panic!("hover must report viewport height: {bare_json}"));

    let ok_case = dir.write(
        "hover.yaml",
        &format!(
            r##"name: hover tests
steps:
  - browser.open:
      url: "{}"
      width: 1280
      height: 720
  - browser.hover:
      css: "#menu"
  - browser.click:
      css: "#submenu"
  - browser.read:
      css: "#out"
    expect:
      - path: $.text
        equals: "sub-clicked"
  - browser.read:
      css: "#log"
    expect:
      - path: $.text
        contains: "mouseover;"
      - path: $.text
        contains: "mouseenter;"
  - browser.hover:
      at:
        x: 100
        y: 225
        viewport: {{ width: {vw}, height: {vh} }}
  - browser.read:
      css: "#coord-out"
    expect:
      - path: $.text
        equals: "coord-entered"
"##,
            cu.url("/hover")
        ),
    );

    let ok_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&ok_case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&ok_out),
        Some(0),
        "stdout={}\nstderr={}",
        stdout(&ok_out),
        stderr(&ok_out)
    );

    let occluded_case = dir.write(
        "hover-occluded.yaml",
        &format!(
            r##"name: hover occluded
steps:
  - browser.open:
      url: "{}"
  - browser.hover:
      css: "#covered-btn"
      timeout_ms: 400
"##,
            cu.url("/hover")
        ),
    );
    let occ_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&occluded_case),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&occ_out), Some(1), "{}", stdout(&occ_out));
    let msg = stdout(&occ_out).to_lowercase();
    assert!(
        msg.contains("covered") || msg.contains("cover") || msg.contains("actionable"),
        "occluded hover error must explain blocker: {}",
        stdout(&occ_out)
    );
}

#[test]
fn browser_drag_handles_html5_dnd_pointer_drag_iframe_and_shadow_dom() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![
        (
            "/drag",
            r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; font-family: sans-serif; }
  .row { display: flex; gap: 40px; padding: 16px; }
  .box { width: 120px; height: 50px; border: 1px solid #333; display: flex; align-items: center; justify-content: center; }
  #knob { width: 80px; height: 32px; background: #06c; color: #fff; user-select: none; }
  #track { width: 200px; height: 32px; background: #eee; margin-left: 16px; }
  #shadow-host { position: fixed; left: 40px; top: 340px; width: 320px; height: 60px; }
  #drag-frame { width: 360px; height: 110px; border: 1px solid #666; margin: 12px 16px; }
  #far-src { position: absolute; left: 40px; top: 1350px; }
  #far-dst { position: absolute; left: 260px; top: 1350px; }
</style></head>
<body>
  <div class="row">
    <div id="src-a" class="box" draggable="true">Card A</div>
    <div id="dst-a" class="box">Drop A</div>
    <div id="src-b" class="box" draggable="true">Card B</div>
    <div id="dst-b" class="box">Drop B</div>
  </div>
  <div id="track"><div id="knob">Knob</div></div>
  <iframe id="drag-frame" src="/drag-frame"></iframe>
  <div id="shadow-host"></div>
  <div id="far-src" class="box" draggable="true">Far Src</div>
  <div id="far-dst" class="box">Far Dst</div>
  <div id="dnd-log"></div>
  <div id="ptr-log"></div>
  <div id="shadow-log">none</div>
  <div id="far-log"></div>
  <script>
    function wireDnd(srcId, dstId, outEl, prefix) {
      const s = document.getElementById(srcId);
      const d = document.getElementById(dstId);
      s.addEventListener('dragstart', e => {
        e.dataTransfer.setData('text/plain', prefix + '-payload');
        outEl.textContent += prefix + ':start>';
      });
      s.addEventListener('dragend', () => { outEl.textContent += prefix + ':end;'; });
      d.addEventListener('dragenter', e => { e.preventDefault(); outEl.textContent += prefix + ':enter>'; });
      d.addEventListener('dragover', e => { e.preventDefault(); });
      d.addEventListener('drop', e => {
        e.preventDefault();
        outEl.textContent += prefix + ':drop(' + e.dataTransfer.getData('text/plain') + ')>';
      });
    }
    const dndLog = document.getElementById('dnd-log');
    wireDnd('src-a', 'dst-a', dndLog, 'A');
    wireDnd('src-b', 'dst-b', dndLog, 'B');
    wireDnd('far-src', 'far-dst', document.getElementById('far-log'), 'FAR');

    const knob = document.getElementById('knob');
    const ptrLog = document.getElementById('ptr-log');
    let dragging = false, startX = 0, movedDx = 0;
    knob.addEventListener('pointerdown', e => { dragging = true; startX = e.clientX; ptrLog.textContent = 'down'; });
    window.addEventListener('pointermove', e => { if (dragging) movedDx = Math.round(e.clientX - startX); });
    window.addEventListener('pointerup', e => { if (dragging) { dragging = false; ptrLog.textContent = movedDx > 80 ? 'moved:ok' : 'moved:short:' + movedDx; } });

    const root = document.getElementById('shadow-host').attachShadow({ mode: 'open' });
    root.innerHTML = `<div style="display:flex;justify-content:space-between;width:300px;height:50px">
      <div id="s-src" draggable="true" style="width:90px;height:50px;background:#ccc">S-Src</div>
      <div id="s-dst" style="width:90px;height:50px;background:#cec">S-Dst</div>
    </div>`;
    root.getElementById('s-src').addEventListener('dragstart', e => e.dataTransfer.setData('text/plain', 'shadow-ok'));
    root.getElementById('s-dst').addEventListener('dragover', e => e.preventDefault());
    root.getElementById('s-dst').addEventListener('drop', e => {
      e.preventDefault();
      document.getElementById('shadow-log').textContent = e.dataTransfer.getData('text/plain');
    });
  </script>
</body>
</html>"##,
        ),
        (
            "/drag-frame",
            r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; display: flex; gap: 30px; padding: 12px; }
  .fbox { width: 100px; height: 50px; border: 1px solid #333; }
</style></head>
<body>
  <div id="f-src" class="fbox" draggable="true">F-Src</div>
  <div id="f-dst" class="fbox">F-Dst</div>
  <div id="f-out">none</div>
  <script>
    const fs = document.getElementById('f-src');
    const fd = document.getElementById('f-dst');
    fs.addEventListener('dragstart', e => e.dataTransfer.setData('text/plain', 'iframe-payload'));
    fd.addEventListener('dragover', e => e.preventDefault());
    fd.addEventListener('drop', e => {
      e.preventDefault();
      document.getElementById('f-out').textContent = e.dataTransfer.getData('text/plain');
    });
  </script>
</body>
</html>"##,
        ),
    ]);

    let dir = TempDir::new("browser-drag-e2e");
    let probe_case = dir.write(
        "drag-probe.yaml",
        &format!(
            r##"name: drag probe viewport
steps:
  - browser.open:
      url: "{}"
      width: 1280
      height: 720
  - browser.hover:
      at: {{ x: 10, y: 10 }}
"##,
            cu.url("/drag")
        ),
    );
    let probe_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&probe_case),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&probe_out), Some(0), "{}", stdout(&probe_out));
    let probe_json = json_stdout(&probe_out);
    let vw = probe_json["cases"][0]["steps"][1]["result"]["viewport"]["width"]
        .as_u64()
        .unwrap_or_else(|| panic!("probe must report viewport width: {probe_json}"));
    let vh = probe_json["cases"][0]["steps"][1]["result"]["viewport"]["height"]
        .as_u64()
        .unwrap_or_else(|| panic!("probe must report viewport height: {probe_json}"));

    let case = dir.write(
        "drag.yaml",
        &format!(
            r##"name: drag all modes
steps:
  - browser.open:
      url: "{}"
      width: 1280
      height: 720
  - browser.drag:
      from: {{ css: "#src-a" }}
      to: {{ css: "#dst-a" }}
  - browser.drag:
      from: {{ css: "#knob" }}
      to: {{ at: {{ x: 180, y: 98, viewport: {{ width: {vw}, height: {vh} }} }} }}
  - browser.drag:
      from: {{ css: "#src-b" }}
      to: {{ css: "#dst-b" }}
  - browser.drag:
      from: {{ at: {{ x: 85, y: 365, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ at: {{ x: 295, y: 365, viewport: {{ width: {vw}, height: {vh} }} }} }}
  - browser.drag:
      from: {{ css: "#f-src", within: {{ css: "#drag-frame" }} }}
      to: {{ css: "#f-dst", within: {{ css: "#drag-frame" }} }}
  - browser.drag:
      from: {{ css: "#far-src" }}
      to: {{ css: "#far-dst" }}
  - browser.read:
      css: "#dnd-log"
    expect:
      - path: $.text
        equals: "A:start>A:enter>A:drop(A-payload)>A:end;B:start>B:enter>B:drop(B-payload)>B:end;"
  - browser.read:
      css: "#ptr-log"
    expect:
      - path: $.text
        equals: "moved:ok"
  - browser.read:
      css: "#shadow-log"
    expect:
      - path: $.text
        equals: "shadow-ok"
  - browser.read:
      css: "#f-out"
      within: {{ css: "#drag-frame" }}
    expect:
      - path: $.text
        equals: "iframe-payload"
  - browser.read:
      css: "#far-log"
    expect:
      - path: $.text
        equals: "FAR:start>FAR:enter>FAR:drop(FAR-payload)>FAR:end;"
"##,
            cu.url("/drag")
        ),
    );

    let out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&out),
        Some(0),
        "drag case must pass: stdout={}\nstderr={}",
        stdout(&out),
        stderr(&out)
    );

    let mismatch_w = vw + 1;
    let bad_drag = dir.write(
        "drag-bad-vp.yaml",
        &format!(
            r##"name: drag viewport mismatch
steps:
  - browser.open:
      url: "{}"
      width: 1280
      height: 720
  - browser.drag:
      from: {{ css: "#knob" }}
      to: {{ at: {{ x: 180, y: 98, viewport: {{ width: {mismatch_w}, height: {vh} }} }} }}
"##,
            cu.url("/drag")
        ),
    );
    let bad_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&bad_drag),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&bad_out), Some(1), "{}", stdout(&bad_out));
    assert!(
        stdout(&bad_out).contains("viewport"),
        "drag viewport mismatch must report viewport mismatch: {}",
        stdout(&bad_out)
    );
}

#[test]
fn browser_clipboard_writes_reads_pastes_isolates_and_redacts_secrets() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![(
        "/clip",
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"></head>
<body>
  <input id="paste-box" type="text" placeholder="paste here">
  <button id="paste-btn" onclick="navigator.clipboard.readText().then(t => { const box = document.getElementById('paste-box'); box.value = t; box.setAttribute('data-value', t); document.getElementById('paste-done').textContent = 'pasted:' + t; })">Paste</button>
  <div id="paste-done">none</div>
  <button id="async-write" onclick="setTimeout(() => navigator.clipboard.writeText('async-clip-ok'), 180)">Async Write</button>
  <button id="measure-clip" onclick="navigator.clipboard.readText().then(t => document.getElementById('clip-len').textContent = String(t.length))">Measure</button>
  <div id="clip-len">0</div>
</body>
</html>"##,
    )]);

    let dir = TempDir::new("browser-clipboard-e2e");

    // 1. Write clipboard -> read into page -> async page write -> retry read_clipboard.
    let happy = dir.write(
        "clip-happy.yaml",
        &format!(
            r##"name: clipboard happy path
steps:
  - browser.open:
      url: "{}"
  - browser.set_clipboard:
      text: "hello-clipboard"
    expect:
      - path: $.characters
        equals: 15
  - browser.read_clipboard: {{}}
    expect:
      - path: $.text
        equals: "hello-clipboard"
  - browser.click:
      css: "#paste-btn"
  - browser.wait:
      text: "pasted:hello-clipboard"
  - browser.read:
      css: "#paste-box"
      attribute: "data-value"
    expect:
      - path: $.value
        equals: "hello-clipboard"
  - browser.click:
      css: "#async-write"
  - browser.read_clipboard: {{}}
    retry:
      until:
        - path: $.text
          equals: "async-clip-ok"
      timeout_ms: 2500
      interval_ms: 50
"##,
            cu.url("/clip")
        ),
    );
    let happy_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&happy), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&happy_out),
        Some(0),
        "stdout={}\nstderr={}",
        stdout(&happy_out),
        stderr(&happy_out)
    );
    let happy_json = json_stdout(&happy_out);
    assert!(
        happy_json["cases"][0]["steps"][1]["result"]
            .get("text")
            .is_none(),
        "set_clipboard must never echo text in result: {happy_json}"
    );

    // 2. A fresh browser instance is isolated from previous run's clipboard.
    let iso = dir.write(
        "clip-isolated.yaml",
        &format!(
            r##"name: clipboard isolated
steps:
  - browser.open:
      url: "{}"
  - browser.read_clipboard: {{}}
    expect:
      - path: $.text
        equals: ""
"##,
            cu.url("/clip")
        ),
    );
    let iso_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&iso), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&iso_out),
        Some(0),
        "fresh browser clipboard must start empty: stdout={}\nstderr={}",
        stdout(&iso_out),
        stderr(&iso_out)
    );

    // 3. `${env:...}` secret stays real inside browser, but is redacted everywhere in output and failure bundles.
    let secret = "super-clip-secret-987654";
    let secret_case = dir.write(
        "clip-secret.yaml",
        &format!(
            r##"name: clipboard secret redaction
steps:
  - browser.open:
      url: "{}"
  - browser.set_clipboard:
      text: "${{env:DA_TEST_CLIP_SECRET}}"
  - browser.click:
      css: "#measure-clip"
  - browser.read:
      css: "#clip-len"
    retry:
      until:
        - path: $.text
          equals: "24"
      timeout_ms: 2500
      interval_ms: 50
  - browser.read_clipboard: {{}}
    expect:
      - path: $.text
        equals: "force-failure-to-inspect-bundle"
"##,
            cu.url("/clip")
        ),
    );
    let bundles = dir.path().join("clip-bundles");
    let sec_out = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&secret_case),
            "--bundle-dir",
            path_str(&bundles),
        ],
        &[("DA_TEST_CLIP_SECRET", secret.to_string())],
    );
    assert_eq!(exit_code(&sec_out), Some(1), "{}", stdout(&sec_out));
    assert!(
        !stdout(&sec_out).contains(secret) && !stderr(&sec_out).contains(secret),
        "raw clipboard env secret leaked to stdout/stderr!\nstdout={}\nstderr={}",
        stdout(&sec_out),
        stderr(&sec_out)
    );
    let sec_json = json_stdout(&sec_out);
    assert_eq!(
        sec_json["cases"][0]["steps"][4]["result"]["text"],
        json!("***"),
        "read_clipboard result must redact secret: {sec_json}"
    );
    let bundle = PathBuf::from(sec_json["cases"][0]["bundle"].as_str().expect("bundle"));
    for (path, text) in bundle_texts(&bundle) {
        assert!(
            !text.contains(secret),
            "raw clipboard env secret leaked in bundle {}:\n{text}",
            path.display()
        );
    }

    // 4. Non-secure context fails clearly instead of hanging.
    let insecure = dir.write(
        "clip-insecure.yaml",
        r##"name: clipboard insecure context error
steps:
  - browser.open:
      url: "data:text/html,<!doctype html><html><body>insecure</body></html>"
  - browser.read_clipboard: {}
"##,
    );
    let insec_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&insecure),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&insec_out), Some(1), "{}", stdout(&insec_out));
    assert!(
        stdout(&insec_out).to_lowercase().contains("clipboard"),
        "{}",
        stdout(&insec_out)
    );
}

#[test]
fn browser_drag_mixed_at_and_locator_fails_closed_when_locator_scrolls_page() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![(
        "/mixed-drag",
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; height: 2200px; font-family: sans-serif; position: relative; }
  .item { position: absolute; left: 40px; width: 100px; height: 40px; border: 1px solid #333; display: flex; align-items: center; justify-content: center; user-select: none; }
  #src { top: 80px; background: #cde; }
  #near-src { left: 200px; top: 1460px; background: #edc; }
  #dst { top: 1500px; background: #dfd; }
  #mouse-log { position: fixed; right: 4px; top: 4px; font-size: 11px; }
</style></head>
<body>
  <div id="src" class="item" draggable="true">Src</div>
  <div id="near-src" class="item" draggable="true">NearSrc</div>
  <div id="dst" class="item">Dst</div>
  <div id="mouse-log"></div>
  <script>
    const logEl = document.getElementById('mouse-log');
    function record(ev) {
      logEl.textContent += (logEl.textContent ? ';' : '') + ev;
      fetch('/mouse-event?ev=' + encodeURIComponent(ev)).catch(() => {});
    }
    window.addEventListener('mousedown', e => {
      const id = (e.target && e.target.id) ? e.target.id : (e.target ? e.target.tagName : 'none');
      record('down=' + id);
    }, true);
    window.addEventListener('mouseup', e => {
      const id = (e.target && e.target.id) ? e.target.id : (e.target ? e.target.tagName : 'none');
      record('up=' + id);
    }, true);
    for (const id of ['src', 'near-src']) {
      document.getElementById(id).addEventListener('dragstart', e => {
        e.dataTransfer.setData('text/plain', id);
        record('dragstart=' + id);
      });
    }
    const dst = document.getElementById('dst');
    dst.addEventListener('dragenter', e => e.preventDefault());
    dst.addEventListener('dragover', e => e.preventDefault());
    dst.addEventListener('drop', e => {
      e.preventDefault();
      record('drop=' + dst.id + '(' + e.dataTransfer.getData('text/plain') + ')');
    });
  </script>
</body>
</html>"##,
    )]);

    let dir = TempDir::new("browser-mixed-drag-scroll");

    // Probe actual inner viewport dimensions for `width: 640, height: 480`.
    let probe = dir.write(
        "probe-vp.yaml",
        &format!(
            r##"name: probe viewport
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 480
  - browser.hover:
      at: {{ x: 10, y: 10 }}
"##,
            cu.url("/mixed-drag")
        ),
    );
    let probe_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&probe), "--no-bundle"],
    );
    assert_eq!(exit_code(&probe_out), Some(0), "{}", stdout(&probe_out));
    let probe_json = json_stdout(&probe_out);
    let vp = &probe_json["cases"][0]["steps"][1]["result"]["viewport"];
    let vw = vp["width"].as_u64().expect("viewport width");
    let vh = vp["height"].as_u64().expect("viewport height");
    cu.clear_hits();

    // 1. `from.at` (#src at y=80..120 -> x=90, y=100) + `to.locator` (#dst at y=1500):
    // resolving `#dst` scrolls the document and MUST fail closed (exit 1) with ZERO mouse events!
    let mixed_from_at = dir.write(
        "mixed-from-at.yaml",
        &format!(
            r##"name: mixed from.at to.locator offscreen
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 480
  - browser.drag:
      from: {{ at: {{ x: 90, y: 100, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#dst" }}
"##,
            cu.url("/mixed-drag")
        ),
    );
    let bundles = dir.path().join("mixed-bundles");
    let fail_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&mixed_from_at),
            "--bundle-dir",
            path_str(&bundles),
        ],
    );
    assert_eq!(
        exit_code(&fail_out),
        Some(1),
        "mixed at+locator scroll must fail closed as TestFailure (1): stdout={}\nstderr={}",
        stdout(&fail_out),
        stderr(&fail_out)
    );
    let fail_json = json_stdout(&fail_out);
    let err_msg = case_error(&fail_json);
    assert!(
        err_msg.contains("scrolled the page") && err_msg.contains("browser.scroll"),
        "error must explain scroll invalidation and remediation: {err_msg}"
    );
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire before mixed drag fails closed, got: {:?}",
        cu.hits_matching("/mouse-event")
    );
    let bundle = PathBuf::from(fail_json["cases"][0]["bundle"].as_str().expect("bundle"));
    let dom_html = fs::read_to_string(bundle.join("browser/dom.html")).expect("bundle dom.html");
    assert!(
        dom_html.contains("<div id=\"mouse-log\"></div>"),
        "DOM mouse log must remain completely empty (no down=BODY!): {dom_html}"
    );

    // 2. Reverse mixed direction (`from.locator` offscreen + `to.at`) also fails closed with 0 mouse events.
    cu.clear_hits();
    let mixed_to_at = dir.write(
        "mixed-to-at.yaml",
        &format!(
            r##"name: mixed from.locator to.at offscreen
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 480
  - browser.drag:
      from: {{ css: "#dst" }}
      to: {{ at: {{ x: 90, y: 100, viewport: {{ width: {vw}, height: {vh} }} }} }}
"##,
            cu.url("/mixed-drag")
        ),
    );
    let fail2_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&mixed_to_at),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&fail2_out), Some(1), "{}", stdout(&fail2_out));
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire on reverse mixed drag failure, got: {:?}",
        cu.hits_matching("/mouse-event")
    );

    // 3. Locating BOTH `#src` (y=80) and `#dst` (y=1500) in `640x480` succeeds cleanly on `#src` -> `#dst`.
    cu.clear_hits();
    let both_located = dir.write(
        "both-located.yaml",
        &format!(
            r##"name: locate both src and offscreen dst
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 480
  - browser.drag:
      from: {{ css: "#src" }}
      to: {{ css: "#dst" }}
  - browser.read:
      css: "#mouse-log"
    expect:
      - path: $.text
        contains: "down=src"
      - path: $.text
        contains: "drop=dst(src)"
"##,
            cu.url("/mixed-drag")
        ),
    );
    let both_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&both_located),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&both_out),
        Some(0),
        "locating both #src and #dst must succeed: stdout={}\nstderr={}",
        stdout(&both_out),
        stderr(&both_out)
    );
    assert!(
        !stdout(&both_out).contains("down=BODY"),
        "located drag must press #src, never BODY: {}",
        stdout(&both_out)
    );

    // 4. Scrolling into view first with `browser.scroll`, then running mixed `at` + `#dst` succeeds!
    let scroll_probe = dir.write(
        "scroll-probe.yaml",
        &format!(
            r##"name: probe scroll offset after into_view
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 480
  - browser.scroll:
      css: "#dst"
      into_view: true
"##,
            cu.url("/mixed-drag")
        ),
    );
    let sp_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&scroll_probe),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&sp_out), Some(0), "{}", stdout(&sp_out));
    let scroll_y = json_stdout(&sp_out)["cases"][0]["steps"][1]["result"]["scroll_y"]
        .as_f64()
        .expect("scroll_y");
    let near_src_vy = (1480.0 - scroll_y).round() as i64;

    cu.clear_hits();
    let scroll_then_mixed = dir.write(
        "scroll-then-mixed.yaml",
        &format!(
            r##"name: scroll first then mixed drag
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 480
  - browser.scroll:
      css: "#dst"
      into_view: true
  - browser.drag:
      from: {{ at: {{ x: 250, y: {near_src_vy}, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#dst" }}
  - browser.read:
      css: "#mouse-log"
    expect:
      - path: $.text
        contains: "down=near-src"
      - path: $.text
        contains: "drop=dst(near-src)"
"##,
            cu.url("/mixed-drag")
        ),
    );
    let stm_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&scroll_then_mixed),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&stm_out),
        Some(0),
        "scrolling first then mixed drag must succeed: stdout={}\nstderr={}",
        stdout(&stm_out),
        stderr(&stm_out)
    );
}

#[test]
fn browser_drag_mixed_at_and_locator_fails_closed_when_locator_scrolls_container() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![
        (
            "/mixed-container-drag",
            r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  html, body { margin: 0; overflow: hidden; height: 100%; font-family: sans-serif; position: relative; }
  .item { position: absolute; width: 82px; height: 40px; border: 1px solid #333; display: flex; align-items: center; justify-content: center; user-select: none; box-sizing: border-box; }
  #panel { position: absolute; left: 20px; top: 20px; width: 300px; height: 220px; overflow: auto; border: 1px solid #333; }
  #src { left: 39px; top: 54px; height: 50px; background: #cde; }
  #h-dst { left: 700px; top: 54px; background: #fdd; }
  #near-src { left: 150px; top: 700px; width: 90px; background: #edc; }
  #dst { left: 39px; top: 700px; background: #dfd; }
  #shadow-host { position: absolute; left: 20px; top: 252px; width: 300px; height: 185px; }
  #drag-frame { position: absolute; left: 335px; top: 20px; width: 280px; height: 220px; border: 1px solid #333; }
  #doc-scroll-frame { position: absolute; left: 335px; top: 252px; width: 280px; height: 185px; border: 1px solid #333; }
  #mouse-log { position: fixed; right: 4px; bottom: 4px; font-size: 11px; }
</style></head>
<body>
  <div id="panel">
    <div id="src" class="item" draggable="true">Src</div>
    <div id="h-dst" class="item">HDst</div>
    <div id="near-src" class="item" draggable="true">NearSrc</div>
    <div id="dst" class="item">Dst</div>
  </div>
  <div id="shadow-host"></div>
  <iframe id="drag-frame" src="/mixed-container-frame"></iframe>
  <iframe id="doc-scroll-frame" src="/mixed-doc-frame"></iframe>
  <div id="mouse-log"></div>
  <script>
    const logEl = document.getElementById('mouse-log');
    window.__recordMouse = function record(ev) {
      logEl.textContent += (logEl.textContent ? ';' : '') + ev;
      fetch('/mouse-event?ev=' + encodeURIComponent(ev)).catch(() => {});
    };
    window.addEventListener('mousedown', e => {
      const path = e.composedPath ? e.composedPath() : [e.target];
      const hit = path[0];
      const id = (hit && hit.id) ? hit.id : (hit && hit.tagName ? hit.tagName : 'none');
      window.__recordMouse('down=' + id);
    }, true);
    window.addEventListener('mouseup', e => {
      const path = e.composedPath ? e.composedPath() : [e.target];
      const hit = path[0];
      const id = (hit && hit.id) ? hit.id : (hit && hit.tagName ? hit.tagName : 'none');
      window.__recordMouse('up=' + id);
    }, true);
    for (const id of ['src', 'near-src']) {
      document.getElementById(id).addEventListener('dragstart', e => {
        e.dataTransfer.setData('text/plain', id);
        window.__recordMouse('dragstart=' + id);
      });
    }
    const dst = document.getElementById('dst');
    dst.addEventListener('dragenter', e => e.preventDefault());
    dst.addEventListener('dragover', e => e.preventDefault());
    dst.addEventListener('drop', e => {
      e.preventDefault();
      window.__recordMouse('drop=' + dst.id + '(' + e.dataTransfer.getData('text/plain') + ')');
    });

    const sr = document.getElementById('shadow-host').attachShadow({ mode: 'open' });
    sr.innerHTML = `<style>
      .sitem { position: absolute; width: 82px; height: 40px; border: 1px solid #333; box-sizing: border-box; user-select: none; }
      #shadow-panel { position: relative; width: 300px; height: 185px; overflow: auto; border: 1px solid #333; }
      #s-src { left: 39px; top: 38px; height: 50px; background: #cde; }
      #s-dst { left: 39px; top: 650px; background: #dfd; }
    </style>
    <div id="shadow-panel">
      <div id="s-src" class="sitem" draggable="true">SSrc</div>
      <div id="s-dst" class="sitem">SDst</div>
    </div>`;
    sr.getElementById('s-src').addEventListener('dragstart', e => {
      e.dataTransfer.setData('text/plain', 's-src');
    });
    sr.getElementById('s-dst').addEventListener('dragenter', e => e.preventDefault());
    sr.getElementById('s-dst').addEventListener('dragover', e => e.preventDefault());
    sr.getElementById('s-dst').addEventListener('drop', e => {
      e.preventDefault();
      window.__recordMouse('drop=s-dst(' + e.dataTransfer.getData('text/plain') + ')');
    });
  </script>
</body>
</html>"##,
        ),
        (
            "/mixed-container-frame",
            r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; overflow: hidden; height: 220px; font-family: sans-serif; }
  .fitem { position: absolute; width: 80px; height: 40px; border: 1px solid #333; box-sizing: border-box; user-select: none; }
  #f-panel { position: relative; width: 280px; height: 220px; overflow: auto; }
  #f-src { left: 40px; top: 45px; height: 50px; background: #cde; }
  #f-dst { left: 40px; top: 650px; background: #dfd; }
</style></head>
<body>
  <div id="f-panel">
    <div id="f-src" class="fitem" draggable="true">FSrc</div>
    <div id="f-dst" class="fitem">FDst</div>
  </div>
  <script>
    window.addEventListener('mousedown', e => {
      const id = (e.target && e.target.id) ? e.target.id : (e.target ? e.target.tagName : 'none');
      window.top.__recordMouse('fdown=' + id);
    }, true);
    window.addEventListener('mouseup', e => {
      const id = (e.target && e.target.id) ? e.target.id : (e.target ? e.target.tagName : 'none');
      window.top.__recordMouse('fup=' + id);
    }, true);
    document.getElementById('f-src').addEventListener('dragstart', e => {
      e.dataTransfer.setData('text/plain', 'f-src');
    });
    const fdst = document.getElementById('f-dst');
    fdst.addEventListener('dragenter', e => e.preventDefault());
    fdst.addEventListener('dragover', e => e.preventDefault());
    fdst.addEventListener('drop', e => {
      e.preventDefault();
      window.top.__recordMouse('fdrop=' + fdst.id + '(' + e.dataTransfer.getData('text/plain') + ')');
    });
  </script>
</body>
</html>"##,
        ),
        (
            "/mixed-doc-frame",
            r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; height: 900px; font-family: sans-serif; position: relative; }
  .dfitem { position: absolute; width: 80px; height: 40px; border: 1px solid #333; box-sizing: border-box; user-select: none; }
  #df-src { left: 40px; top: 45px; height: 50px; background: #cde; }
  #df-dst { left: 40px; top: 650px; background: #dfd; }
</style></head>
<body>
  <div id="df-src" class="dfitem" draggable="true">DFSrc</div>
  <div id="df-dst" class="dfitem">DFDst</div>
  <script>
    window.addEventListener('mousedown', e => {
      const id = (e.target && e.target.id) ? e.target.id : (e.target ? e.target.tagName : 'none');
      window.top.__recordMouse('dfdown=' + id);
    }, true);
    window.addEventListener('mouseup', e => {
      const id = (e.target && e.target.id) ? e.target.id : (e.target ? e.target.tagName : 'none');
      window.top.__recordMouse('dfup=' + id);
    }, true);
  </script>
</body>
</html>"##,
        ),
    ]);

    let dir = TempDir::new("browser-mixed-container-drag");
    let probe = dir.write(
        "probe-vp.yaml",
        &format!(
            r##"name: probe viewport
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.hover:
      at: {{ x: 10, y: 10 }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let probe_out = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&probe), "--no-bundle"],
    );
    assert_eq!(exit_code(&probe_out), Some(0), "{}", stdout(&probe_out));
    let vp = &json_stdout(&probe_out)["cases"][0]["steps"][1]["result"]["viewport"];
    let vw = vp["width"].as_u64().expect("viewport width");
    let vh = vp["height"].as_u64().expect("viewport height");

    // 1. Exact F-3 repro: non-scrollable body, `#panel` (`overflow:auto`) has `#src` at (100,100)
    // and `#dst` at the bottom. `from.at` + `to.css: "#dst"` scrolls `#panel` and MUST fail closed
    // as Execution (exit code 1, `kind: "execution"`) with ZERO mouse events (no `down=DIV`).
    cu.clear_hits();
    let mixed_from_at = dir.write(
        "container-from-at.yaml",
        &format!(
            r##"name: mixed from.at to.css in inner overflow panel
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.drag:
      from: {{ at: {{ x: 100, y: 100, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#dst" }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let bundles = dir.path().join("container-bundles");
    let fail_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&mixed_from_at),
            "--bundle-dir",
            path_str(&bundles),
        ],
    );
    assert_eq!(
        exit_code(&fail_out),
        Some(1),
        "mixed at+locator inner container scroll must fail closed as TestFailure (1): stdout={}\nstderr={}",
        stdout(&fail_out),
        stderr(&fail_out)
    );
    let fail_json = json_stdout(&fail_out);
    assert_eq!(
        fail_json["cases"][0]["steps"][1]["error"]["kind"],
        json!("execution"),
        "mixed container scroll error kind must be execution: {fail_json}"
    );
    let err_msg = case_error(&fail_json);
    assert!(
        err_msg.contains("panel") && err_msg.contains("browser.scroll"),
        "error must name scroll container and remediation: {err_msg}"
    );
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire before inner container mixed drag fails closed, got: {:?}",
        cu.hits_matching("/mouse-event")
    );
    let bundle = PathBuf::from(fail_json["cases"][0]["bundle"].as_str().expect("bundle"));
    let dom_html = fs::read_to_string(bundle.join("browser/dom.html")).expect("bundle dom.html");
    assert!(
        dom_html.contains("<div id=\"mouse-log\"></div>"),
        "DOM mouse log must remain completely empty (no down=DIV!): {dom_html}"
    );

    // 2. Reverse mixed direction (`from: { css: "#dst" }`, `to: { at: { x: 100, y: 100 } }`)
    // also scrolls `#panel` and MUST fail closed with ZERO mouse events.
    cu.clear_hits();
    let mixed_to_at = dir.write(
        "container-to-at.yaml",
        &format!(
            r##"name: mixed from.css to.at in inner overflow panel
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.drag:
      from: {{ css: "#dst" }}
      to: {{ at: {{ x: 100, y: 100, viewport: {{ width: {vw}, height: {vh} }} }} }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let fail2_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&mixed_to_at),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&fail2_out), Some(1), "{}", stdout(&fail2_out));
    let fail2_json = json_stdout(&fail2_out);
    assert_eq!(
        fail2_json["cases"][0]["steps"][1]["error"]["kind"],
        json!("execution"),
        "{fail2_json}"
    );
    let err2 = case_error(&fail2_json);
    assert!(
        err2.contains("panel") && err2.contains("browser.scroll"),
        "reverse container error must name #panel and browser.scroll: {err2}"
    );
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire on reverse container mixed drag failure, got: {:?}",
        cu.hits_matching("/mouse-event")
    );

    // 2b. Horizontal scroll inside `#panel` (`from.at` + `to.css: "#h-dst"`) also fails closed
    // with ZERO mouse events.
    cu.clear_hits();
    let horiz_mixed = dir.write(
        "container-horiz-mixed.yaml",
        &format!(
            r##"name: mixed drag with horizontal inner overflow scroll
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.drag:
      from: {{ at: {{ x: 100, y: 100, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#h-dst" }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let horiz_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&horiz_mixed),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&horiz_out), Some(1), "{}", stdout(&horiz_out));
    let horiz_json = json_stdout(&horiz_out);
    assert_eq!(
        horiz_json["cases"][0]["steps"][1]["error"]["kind"],
        json!("execution"),
        "{horiz_json}"
    );
    assert!(
        case_error(&horiz_json).contains("panel"),
        "horizontal scroll failure must name #panel: {horiz_json}"
    );
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire on horizontal container mixed drag failure, got: {:?}",
        cu.hits_matching("/mouse-event")
    );

    // 3. Mixed `at` + locator inside an open Shadow DOM scroll container (`#shadow-panel`)
    // MUST fail closed with ZERO mouse events.
    cu.clear_hits();
    let shadow_mixed = dir.write(
        "container-shadow-mixed.yaml",
        &format!(
            r##"name: mixed drag inside open shadow scroll container
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.drag:
      from: {{ at: {{ x: 100, y: 315, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#s-dst" }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let shadow_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&shadow_mixed),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&shadow_out), Some(1), "{}", stdout(&shadow_out));
    let shadow_json = json_stdout(&shadow_out);
    assert_eq!(
        shadow_json["cases"][0]["steps"][1]["error"]["kind"],
        json!("execution"),
        "{shadow_json}"
    );
    assert!(
        case_error(&shadow_json).contains("shadow-panel"),
        "shadow scroll failure must name #shadow-panel: {shadow_json}"
    );
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire on shadow container mixed drag failure, got: {:?}",
        cu.hits_matching("/mouse-event")
    );

    // 4. Mixed `at` + locator inside a same-origin iframe scroll container (`#f-panel`)
    // MUST fail closed with ZERO mouse events.
    cu.clear_hits();
    let frame_mixed = dir.write(
        "container-frame-mixed.yaml",
        &format!(
            r##"name: mixed drag inside same-origin iframe scroll container
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.drag:
      from: {{ at: {{ x: 416, y: 91, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#f-dst", within: {{ css: "#drag-frame" }} }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let frame_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&frame_mixed),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&frame_out), Some(1), "{}", stdout(&frame_out));
    let frame_json = json_stdout(&frame_out);
    assert_eq!(
        frame_json["cases"][0]["steps"][1]["error"]["kind"],
        json!("execution"),
        "{frame_json}"
    );
    assert!(
        case_error(&frame_json).contains("f-panel"),
        "iframe scroll failure must name #f-panel: {frame_json}"
    );
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire on iframe container mixed drag failure, got: {:?}",
        cu.hits_matching("/mouse-event")
    );

    // 4b. Mixed `at` + locator scrolling a same-origin iframe's own document (`#doc-scroll-frame`)
    // MUST fail closed naming `#doc-scroll-frame` with ZERO mouse events.
    cu.clear_hits();
    let doc_frame_mixed = dir.write(
        "container-doc-frame-mixed.yaml",
        &format!(
            r##"name: mixed drag scrolling same-origin iframe document
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.drag:
      from: {{ at: {{ x: 416, y: 323, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#df-dst", within: {{ css: "#doc-scroll-frame" }} }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let doc_frame_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&doc_frame_mixed),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&doc_frame_out),
        Some(1),
        "{}",
        stdout(&doc_frame_out)
    );
    let doc_frame_json = json_stdout(&doc_frame_out);
    assert_eq!(
        doc_frame_json["cases"][0]["steps"][1]["error"]["kind"],
        json!("execution"),
        "{doc_frame_json}"
    );
    assert!(
        case_error(&doc_frame_json).contains("doc-scroll-frame"),
        "iframe document scroll failure must name #doc-scroll-frame: {doc_frame_json}"
    );
    assert!(
        cu.hits_matching("/mouse-event").is_empty(),
        "zero mouse events may fire on iframe document mixed drag failure, got: {:?}",
        cu.hits_matching("/mouse-event")
    );

    // 5. Locating both `#src` and `#dst` inside `#panel` succeeds cleanly (`down=src`, `drop=dst(src)`).
    cu.clear_hits();
    let both_located = dir.write(
        "container-both-located.yaml",
        &format!(
            r##"name: locate both src and dst inside inner overflow panel
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.drag:
      from: {{ css: "#src" }}
      to: {{ css: "#dst" }}
  - browser.read:
      css: "#mouse-log"
    expect:
      - path: $.text
        contains: "down=src"
      - path: $.text
        contains: "drop=dst(src)"
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let both_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&both_located),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&both_out),
        Some(0),
        "located drag inside inner container must succeed: stdout={}\nstderr={}",
        stdout(&both_out),
        stderr(&both_out)
    );

    // 6. Scrolling `#panel` first with `browser.scroll`, then running mixed `at` (#near-src) + `#dst` succeeds!
    let coord_probe = dir.write(
        "container-scroll-probe.yaml",
        &format!(
            r##"name: probe near-src coordinates after scrolling panel
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.scroll:
      css: "#dst"
      into_view: true
  - browser.drag:
      from: {{ css: "#near-src" }}
      to: {{ css: "#dst" }}
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let cp_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&coord_probe),
            "--no-bundle",
        ],
    );
    assert_eq!(exit_code(&cp_out), Some(0), "{}", stdout(&cp_out));
    let cp_json = json_stdout(&cp_out);
    let near_x = cp_json["cases"][0]["steps"][2]["result"]["from"]["x"]
        .as_f64()
        .expect("near_x")
        .round() as i64;
    let near_y = cp_json["cases"][0]["steps"][2]["result"]["from"]["y"]
        .as_f64()
        .expect("near_y")
        .round() as i64;

    cu.clear_hits();
    let scroll_then_mixed = dir.write(
        "container-scroll-then-mixed.yaml",
        &format!(
            r##"name: scroll container first then mixed drag
steps:
  - browser.open:
      url: "{}"
      width: 640
      height: 560
  - browser.scroll:
      css: "#dst"
      into_view: true
  - browser.drag:
      from: {{ at: {{ x: {near_x}, y: {near_y}, viewport: {{ width: {vw}, height: {vh} }} }} }}
      to: {{ css: "#dst" }}
  - browser.read:
      css: "#mouse-log"
    expect:
      - path: $.text
        contains: "down=near-src"
      - path: $.text
        contains: "drop=dst(near-src)"
"##,
            cu.url("/mixed-container-drag")
        ),
    );
    let stm_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&scroll_then_mixed),
            "--no-bundle",
        ],
    );
    assert_eq!(
        exit_code(&stm_out),
        Some(0),
        "scrolling container first then mixed drag must succeed: stdout={}\nstderr={}",
        stdout(&stm_out),
        stderr(&stm_out)
    );
}

#[test]
fn browser_clipboard_rejects_connected_and_headed_browsers_as_environment() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![(
        "/clip-guard",
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"><title>Clip Guard</title></head>
<body>
  <div id="ready">ready</div>
  <script>
    try {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        get() {
          fetch('/clip-touched').catch(() => {});
          throw new Error('navigator.clipboard must not be touched');
        }
      });
    } catch (_) {}
  </script>
</body>
</html>"##,
    )]);

    let dir = TempDir::new("browser-clipboard-isolation");

    // (a) `connect` mode rejects both `browser.set_clipboard` and `browser.read_clipboard`
    // with ExitCode::Environment (4) before touching `navigator.clipboard`.
    let external = UserBrowser::start();
    let conn_secret = "conn-clip-secret-778899";
    for (name, step_yaml) in [
        (
            "connect-set-clip",
            "  - browser.set_clipboard:\n      text: \"${env:DA_CONN_CLIP_SECRET}\"\n",
        ),
        ("connect-read-clip", "  - browser.read_clipboard: {}\n"),
    ] {
        cu.clear_hits();
        let case = dir.write(
            &format!("{name}.yaml"),
            &format!(
                "name: {name}\nsteps:\n  - browser.open:\n      connect: \"{}\"\n      url: \"{}\"\n{step_yaml}",
                external.connect_url(),
                cu.url("/clip-guard")
            ),
        );
        let out = run_da_with_env(
            dir.path(),
            &["--output", "json", "run", path_str(&case), "--no-bundle"],
            &[("DA_CONN_CLIP_SECRET", conn_secret.to_string())],
        );
        assert_eq!(
            exit_code(&out),
            Some(4),
            "{name} must fail with ExitCode::Environment (4): stdout={}\nstderr={}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            !stdout(&out).contains(conn_secret) && !stderr(&out).contains(conn_secret),
            "{name} leaked secret!\nstdout={}\nstderr={}",
            stdout(&out),
            stderr(&out)
        );
        let j = json_stdout(&out);
        assert_eq!(j["cases"][0]["steps"][0]["status"], json!("passed"), "{j}");
        assert_eq!(j["cases"][0]["steps"][1]["status"], json!("error"), "{j}");
        let msg = case_error(&j);
        assert!(
            msg.contains("connect") && msg.contains("headless: true"),
            "{name} error must explain connected clipboard isolation: {msg}"
        );
        assert!(
            cu.hits_matching("/clip-touched").is_empty(),
            "{name} must fail before touching navigator.clipboard"
        );
    }
    drop(external);

    // (b) `headless: false` rejects both `browser.set_clipboard` and `browser.read_clipboard`
    // with ExitCode::Environment (4) before touching `navigator.clipboard`.
    let has_display = !cfg!(target_os = "linux")
        || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
    let headed_secret = "headed-clip-secret-556677";

    if has_display {
        for (name, step_yaml) in [
            (
                "headed-set-clip",
                "  - browser.set_clipboard:\n      text: \"${env:DA_HEADED_CLIP_SECRET}\"\n",
            ),
            ("headed-read-clip", "  - browser.read_clipboard: {}\n"),
        ] {
            cu.clear_hits();
            let case = dir.write(
                &format!("{name}.yaml"),
                &format!(
                    "name: {name}\nsteps:\n  - browser.open:\n      headless: false\n      url: \"{}\"\n{step_yaml}",
                    cu.url("/clip-guard")
                ),
            );
            let out = run_da_with_env(
                dir.path(),
                &["--output", "json", "run", path_str(&case), "--no-bundle"],
                &[("DA_HEADED_CLIP_SECRET", headed_secret.to_string())],
            );
            assert_eq!(
                exit_code(&out),
                Some(4),
                "{name} must fail with ExitCode::Environment (4): stdout={}\nstderr={}",
                stdout(&out),
                stderr(&out)
            );
            assert!(
                !stdout(&out).contains(headed_secret) && !stderr(&out).contains(headed_secret),
                "{name} leaked secret!\nstdout={}\nstderr={}",
                stdout(&out),
                stderr(&out)
            );
            let j = json_stdout(&out);
            assert_eq!(j["cases"][0]["steps"][0]["status"], json!("passed"), "{j}");
            assert_eq!(j["cases"][0]["steps"][1]["status"], json!("error"), "{j}");
            let msg = case_error(&j);
            assert!(
                msg.contains("headless: false") && msg.contains("headless: true"),
                "{name} error must explain headed clipboard isolation: {msg}"
            );
            assert!(
                cu.hits_matching("/clip-touched").is_empty(),
                "{name} must reject before touching navigator.clipboard"
            );
        }
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for (name, step_yaml) in [
                (
                    "headed-stub-set-clip",
                    "  - browser.set_clipboard:\n      text: \"${env:DA_HEADED_CLIP_SECRET}\"\n",
                ),
                ("headed-stub-read-clip", "  - browser.read_clipboard: {}\n"),
            ] {
                let done_file = dir.path().join(format!("{name}.done"));
                let done_for_server = done_file.clone();
                let (port, handle) = spawn_mock_ws(move |mut socket| {
                    while let Some(cmd) = read_mock_cdp(&mut socket) {
                        let method = cmd["method"].as_str().unwrap_or_default();
                        match method {
                            "Target.createTarget" => send_mock_cdp(
                                &mut socket,
                                &json!({
                                    "id": cmd["id"],
                                    "result": { "targetId": "t-h1" }
                                }),
                            ),
                            "Target.getTargets" => send_mock_cdp(
                                &mut socket,
                                &json!({
                                    "id": cmd["id"],
                                    "result": { "targetInfos": [{ "targetId": "t-h1", "type": "page", "url": "about:blank" }] }
                                }),
                            ),
                            "Target.attachToTarget" => send_mock_cdp(
                                &mut socket,
                                &json!({ "id": cmd["id"], "result": { "sessionId": "s-h1" } }),
                            ),
                            "Page.enable"
                            | "Runtime.enable"
                            | "Network.enable"
                            | "Page.navigate"
                            | "Target.setDiscoverTargets"
                            | "Page.setLifecycleEventsEnabled"
                            | "Emulation.setDeviceMetricsOverride" => {
                                send_mock_cdp(
                                    &mut socket,
                                    &json!({ "id": cmd["id"], "result": {} }),
                                );
                            }
                            "Runtime.evaluate" => {
                                let expr = cmd["params"]["expression"].as_str().unwrap_or_default();
                                assert!(
                                    !expr.contains("clipboard"),
                                    "headed clipboard step must fail before evaluating clipboard JS: {expr}"
                                );
                                let val = if expr == "location.href" {
                                    "http://127.0.0.1/clip-guard"
                                } else if expr == "document.readyState" {
                                    "complete"
                                } else {
                                    "Clip Guard"
                                };
                                send_mock_cdp(
                                    &mut socket,
                                    &json!({ "id": cmd["id"], "result": { "result": { "type": "string", "value": val } } }),
                                );
                            }
                            "Browser.grantPermissions" | "Emulation.setFocusEmulationEnabled" => {
                                panic!("headed clipboard step must not send {method}");
                            }
                            "Browser.close" => {
                                send_mock_cdp(
                                    &mut socket,
                                    &json!({ "id": cmd["id"], "result": {} }),
                                );
                                let _ = fs::write(&done_for_server, b"done");
                                break;
                            }
                            _ => send_mock_cdp(
                                &mut socket,
                                &json!({ "id": cmd["id"], "result": {} }),
                            ),
                        }
                    }
                    let _ = fs::write(&done_for_server, b"done");
                });

                let stub = dir.path().join(format!("{name}-chrome.sh"));
                let script = format!(
                    "#!/bin/sh\ntrap 'exit 0' TERM INT\nprofile=\"\"\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    --headless*) exit 91 ;;\n    --user-data-dir=*) profile=\"${{arg#--user-data-dir=}}\" ;;\n  esac\ndone\nprintf '%s\\n/devtools/browser/mock\\n' '{port}' > \"$profile/DevToolsActivePort\"\nwhile [ ! -f '{}' ]; do sleep 0.02; done\n",
                    done_file.display()
                );
                fs::write(&stub, script).expect("write stub chrome");
                let mut perms = fs::metadata(&stub).expect("stub metadata").permissions();
                perms.set_mode(0o755);
                fs::set_permissions(&stub, perms).expect("chmod stub");

                let case = dir.write(
                    &format!("{name}.yaml"),
                    &format!(
                        "name: {name}\nsteps:\n  - browser.open:\n      headless: false\n      url: \"http://127.0.0.1/clip-guard\"\n{step_yaml}"
                    ),
                );
                let out = run_da_with_env(
                    dir.path(),
                    &["--output", "json", "run", path_str(&case), "--no-bundle"],
                    &[
                        ("DA_CHROME", stub.to_string_lossy().into_owned()),
                        ("DA_HEADED_CLIP_SECRET", headed_secret.to_string()),
                    ],
                );
                let _ = fs::write(&done_file, b"done");
                handle.join().expect("join mock server");

                assert_eq!(exit_code(&out), Some(4), "{}", stdout(&out));
                let j = json_stdout(&out);
                assert_eq!(j["cases"][0]["steps"][0]["status"], json!("passed"), "{j}");
                assert_eq!(j["cases"][0]["steps"][1]["status"], json!("error"), "{j}");
                let msg = case_error(&j);
                assert!(
                    msg.contains("headless: false") && msg.contains("headless: true"),
                    "{msg}"
                );
            }
        }
    }
}

#[test]
fn browser_clipboard_closed_tab_fails_fast_as_environment_error() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let cu = CuServer::start(vec![(
        "/clip-closed-tab",
        r##"<!doctype html>
<html>
<head><meta charset="utf-8"><title>Clip Closed Tab</title></head>
<body><div id="ready">alive</div></body>
</html>"##,
    )]);

    let dir = TempDir::new("browser-clipboard-closed-tab");
    let start_out = run_da_in(dir.path(), &["--output", "json", "session", "start"]);
    assert_eq!(exit_code(&start_out), Some(0), "{}", stdout(&start_out));
    let session_id = json_stdout(&start_out)["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let open_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &session_id,
            "browser.open",
            "--arg",
            &format!("url={}", cu.url("/clip-closed-tab")),
        ],
    );
    assert_eq!(exit_code(&open_out), Some(0), "{}", stdout(&open_out));

    // Sanity check: while the isolated headless tab is alive, `browser.set_clipboard` succeeds.
    let alive_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &session_id,
            "browser.set_clipboard",
            "--arg",
            "text=alive",
        ],
    );
    assert_eq!(exit_code(&alive_out), Some(0), "{}", stdout(&alive_out));

    // Read the DevTools port of the `da`-launched headless Chrome, open a keepalive tab so the
    // browser process stays alive, then close the owned tab via `/json/close/{id}`.
    let state_path = dir
        .path()
        .join(".da")
        .join("sessions")
        .join(&session_id)
        .join("state.json");
    let state: Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read session state.json"))
            .expect("parse session state.json");
    let port = state["handles"]["browser"]["port"]
        .as_u64()
        .expect("browser handle port") as u16;
    let _ = http_request(port, "PUT", "/json/new?about:blank").expect("open keepalive tab");

    let (_, list_body) = http_request(port, "GET", "/json/list").expect("list tabs");
    let tabs: Value = serde_json::from_str(&list_body).expect("parse /json/list");
    let owned_tab_id = tabs
        .as_array()
        .expect("tab list array")
        .iter()
        .find(|t| {
            t["url"]
                .as_str()
                .is_some_and(|u| u.contains("/clip-closed-tab"))
        })
        .and_then(|t| t["id"].as_str())
        .expect("find owned /clip-closed-tab target id")
        .to_string();
    let _ = http_request(port, "GET", &format!("/json/close/{owned_tab_id}"));

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let (_, cur_body) = http_request(port, "GET", "/json/list").unwrap_or((0, "[]".into()));
        let cur: Value = serde_json::from_str(&cur_body).unwrap_or_else(|_| json!([]));
        if !cur
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["id"].as_str() == Some(owned_tab_id.as_str()))
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    let secret = "closed-tab-clip-secret-424242";
    let set_closed = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &session_id,
            "browser.set_clipboard",
            "--arg",
            "text=${env:DA_CLOSED_CLIP_SECRET}",
        ],
        &[("DA_CLOSED_CLIP_SECRET", secret.to_string())],
    );
    assert_eq!(
        exit_code(&set_closed),
        Some(4),
        "browser.set_clipboard on closed real tab must fail with ExitCode::Environment (4): stdout={}\nstderr={}",
        stdout(&set_closed),
        stderr(&set_closed)
    );
    assert!(
        !stdout(&set_closed).contains(secret) && !stderr(&set_closed).contains(secret),
        "closed-tab set_clipboard must not leak secret!\nstdout={}\nstderr={}",
        stdout(&set_closed),
        stderr(&set_closed)
    );

    let read_closed = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "step",
            &session_id,
            "browser.read_clipboard",
        ],
    );
    assert_eq!(
        exit_code(&read_closed),
        Some(4),
        "browser.read_clipboard on closed real tab must fail with ExitCode::Environment (4): stdout={}\nstderr={}",
        stdout(&read_closed),
        stderr(&read_closed)
    );

    let _ = run_da_in(dir.path(), &["session", "stop", &session_id]);
}

#[test]
fn computer_use_mock_closed_tab_fails_fast_as_environment_error() {
    let dir = TempDir::new("browser-cu-dead-tab");
    for (name, step_yaml) in [
        ("scroll", "  - browser.scroll:\n      delta_y: 100\n"),
        ("hover", "  - browser.hover:\n      css: \"#btn\"\n"),
        (
            "drag",
            "  - browser.drag:\n      from: { css: \"#a\" }\n      to: { css: \"#b\" }\n",
        ),
    ] {
        let (port, handle) = spawn_mock_devtools(move |mut socket| {
            while let Some(cmd) = read_mock_cdp(&mut socket) {
                let method = cmd["method"].as_str().unwrap_or_default();
                match method {
                    "Target.createTarget" => send_mock_cdp(
                        &mut socket,
                        &json!({ "id": cmd["id"], "result": { "targetId": "t-cu-1" } }),
                    ),
                    "Target.attachToTarget" => send_mock_cdp(
                        &mut socket,
                        &json!({ "id": cmd["id"], "result": { "sessionId": "s-cu-1" } }),
                    ),
                    "Page.enable"
                    | "Runtime.enable"
                    | "Network.enable"
                    | "Page.navigate"
                    | "Target.setDiscoverTargets"
                    | "Page.setLifecycleEventsEnabled"
                    | "Emulation.setDeviceMetricsOverride" => {
                        send_mock_cdp(&mut socket, &json!({ "id": cmd["id"], "result": {} }));
                    }
                    "Runtime.evaluate" => {
                        let expr = cmd["params"]["expression"].as_str().unwrap_or_default();
                        if expr == "location.href"
                            || expr == "document.title"
                            || expr == "document.readyState"
                        {
                            let val = if expr == "location.href" {
                                "http://127.0.0.1/cu"
                            } else if expr == "document.readyState" {
                                "complete"
                            } else {
                                "CU"
                            };
                            send_mock_cdp(
                                &mut socket,
                                &json!({ "id": cmd["id"], "result": { "result": { "type": "string", "value": val } } }),
                            );
                        } else {
                            send_mock_cdp(
                                &mut socket,
                                &json!({
                                    "id": cmd["id"],
                                    "error": { "code": -32001, "message": "Session with given id not found." }
                                }),
                            );
                        }
                    }
                    "Target.closeTarget" => {
                        send_mock_cdp(
                            &mut socket,
                            &json!({ "id": cmd["id"], "result": { "success": true } }),
                        );
                        break;
                    }
                    _ => {
                        send_mock_cdp(
                            &mut socket,
                            &json!({
                                "id": cmd["id"],
                                "error": { "code": -32001, "message": "Session with given id not found." }
                            }),
                        );
                    }
                }
            }
        });

        let case = dir.write(
            &format!("dead-{name}.yaml"),
            &format!(
                "name: dead tab {name}\nsteps:\n  - browser.open:\n      connect: \"http://127.0.0.1:{port}\"\n      url: \"http://127.0.0.1/cu\"\n{step_yaml}"
            ),
        );
        let out = run_da_in(
            dir.path(),
            &["--output", "json", "run", path_str(&case), "--no-bundle"],
        );
        handle.join().expect("mock server join");
        assert_eq!(
            exit_code(&out),
            Some(4),
            "closed tab during {name} must return exit code 4 (Environment): stdout={}\nstderr={}",
            stdout(&out),
            stderr(&out)
        );
        let j = json_stdout(&out);
        assert_eq!(
            j["cases"][0]["steps"][0]["status"],
            json!("passed"),
            "browser.open must succeed before {name} hits closed tab: {j}"
        );
    }
}
