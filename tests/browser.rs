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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;
use support::{TempDir, TestServer};

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

    for (name, step, timeout) in [
        (
            "click.yaml",
            "  - browser.click:\n      css: \"#never\"\n      timeout_ms: 300\n",
            "300",
        ),
        (
            "wait.yaml",
            "  - browser.wait:\n      css: \"#never\"\n      timeout_ms: 200\n",
            "200",
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
            message.contains("#never"),
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
fn an_assertion_failure_closes_the_browser() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("browser-close");
    // `tempfile`, which headless_chrome uses for the profile, reads TEMP/TMP on
    // Windows and TMPDIR elsewhere. Putting them under the test's own directory
    // makes the profile it creates observable and keeps the check independent of
    // any other browser on the machine.
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

    // headless_chrome kills the child and waits for it before it removes the
    // profile directory, so a leftover directory means the process outlived the
    // case.
    let leftovers: Vec<PathBuf> = fs::read_dir(&profile_home)
        .expect("the profile directory must be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rust-headless-chrome-profile"))
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
