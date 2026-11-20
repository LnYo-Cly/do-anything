#![cfg(feature = "browser")]

//! Tests for open shadow DOM piercing.
//!
//! `css`, `text`, `testid` and `role` locators reach elements inside any number
//! of nested open shadow roots, `within` and `nth` work inside a root, and
//! actionability no longer reports a shadow child as occluded by its own host.
//! A closed root cannot be pierced, so its error has to say so; `xpath` stays
//! shadow-blind by design and that limit is pinned here too.
//!
//! Every page comes from `tests/support`, so no test reaches the network. The
//! tests launch Chrome and are gated on `DA_TEST_BROWSER`: when Chrome cannot be
//! started they print why and skip, and with `DA_TEST_BROWSER=1` they fail
//! instead, which is what CI sets.

mod support;

use std::fs;
use std::path::Path;
use std::process::Output;
use std::sync::{Mutex, MutexGuard, OnceLock};

use serde_json::Value;
use support::{TempDir, TestServer};

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

/// Runs the binary with `dir` as its working directory.
fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    std::process::Command::new(da())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run the da binary")
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
    use std::io::Write;
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}

/// Serializes the tests that launch Chrome in this binary.
static CHROME_TESTS: Mutex<()> = Mutex::new(());

/// Takes the Chrome-test lock, or `None` when Chrome is unavailable.
fn chrome_test(server: &TestServer) -> Option<MutexGuard<'static, ()>> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    let ready = READY.get_or_init(|| {
        let dir = TempDir::new("shadow-dom-probe");
        let case = dir.write(
            "probe.yaml",
            &format!(
                "name: shadow probe\nsteps:\n  - browser.open:\n      url: \"{}\"\n",
                server.url("/browser/shadow")
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
    });
    match ready {
        Ok(()) => Some(
            CHROME_TESTS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ),
        Err(message) => {
            if std::env::var("DA_TEST_BROWSER").as_deref() == Ok("1") {
                panic!("DA_TEST_BROWSER=1 requires a working Chrome: {message}");
            }
            note(&format!(
                "skipping shadow DOM test: {message}; set DA_TEST_BROWSER=1 to fail instead of skipping"
            ));
            None
        }
    }
}

/// Runs one case file and asserts it succeeded.
fn run_ok(dir: &Path, case: &Path) -> Value {
    let output = run_da_in(
        dir,
        &["--output", "json", "run", path_str(case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&output),
        Some(0),
        "the case must succeed: stdout={}\nstderr={}",
        stdout(&output),
        stderr(&output)
    );
    json_stdout(&output)
}

/// Runs one case file, asserts it failed, and returns the reported message.
fn run_failing(dir: &Path, case: &Path) -> String {
    let output = run_da_in(
        dir,
        &["--output", "json", "run", path_str(case), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&output),
        Some(1),
        "the case must fail: stdout={}\nstderr={}",
        stdout(&output),
        stderr(&output)
    );
    case_error(&json_stdout(&output)).to_string()
}

#[test]
fn locators_pierce_any_depth_of_open_shadow_roots() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("shadow-locators");
    let case = dir.write(
        "locators.yaml",
        &format!(
            r##"name: shadow locators
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#single-button"
  - browser.click:
      testid: shadow-field
  - browser.type:
      testid: shadow-field
      text: shadow typed
  - browser.read:
      css: "#shadow-typed"
    expect:
      - path: $.text
        equals: shadow typed
  - browser.read:
      testid: nested-marker
    expect:
      - path: $.text
        equals: nested marker
  - browser.read:
      testid: nested-marker
      within:
        css: "#depth-host"
    expect:
      - path: $.text
        equals: nested marker
  - browser.click:
      text: Composed Text
  - browser.click:
      css: "#nested-button"
  - browser.click:
      text: Nested Shadow
  - browser.click:
      css: ".row"
      nth: 1
  - browser.click:
      css: ".row"
      nth: 2
      within:
        css: "#nth-host"
  - browser.click:
      role: button
      name: Nested Shadow
  - browser.click:
      css: "#within-button"
      within:
        css: "#within-host"
  - browser.click:
      role: button
      name: Scoped Shadow
      within:
        css: "#within-host"
  - browser.wait:
      text: Scoped Shadow
      within:
        css: "#within-host"
  - browser.click:
      css: "#inside-frame"
      within:
        css: "#shadow-frame"
  - browser.read:
      css: "#probe-status"
      within:
        css: "#shadow-frame"
    expect:
      - path: $.text
        equals: clicked
  - browser.click:
      role: button
      name: Overlay Target
  - browser.read:
      css: "#shadow-status"
    expect:
      - path: $.text
        equals: "single,text,nested,nested,row1,row2,nested,within,within,overlay"
"##,
            url = server.url("/browser/shadow")
        ),
    );

    run_ok(dir.path(), &case);
}

#[test]
fn a_shadow_target_is_not_occluded_by_its_own_host() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("shadow-host-occlusion");
    // The control sits inside an open root whose host would be the only element
    // `document.elementFromPoint` used to report, so the step must now succeed
    // at the same point it used to fail at.
    let case = dir.write(
        "host-occlusion.yaml",
        &format!(
            r##"name: shadow host does not occlude its own root
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#overlay-target"
  - browser.read:
      css: "#shadow-status"
    expect:
      - path: $.text
        equals: overlay
"##,
            url = server.url("/browser/shadow")
        ),
    );

    run_ok(dir.path(), &case);
}

#[test]
fn a_light_overlay_still_occludes_a_shadow_target() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("shadow-real-overlay");
    let case = dir.write(
        "real-overlay.yaml",
        &format!(
            r##"name: a real overlay still occludes
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#overlay-target"
      timeout_ms: 800
"##,
            url = server.url("/browser/shadow?overlay=1")
        ),
    );

    let error = run_failing(dir.path(), &case);
    assert!(
        error.contains("was occluded by <div#real-overlay>"),
        "a light-DOM overlay must still be reported as occluding a shadow target: {error}"
    );
}

#[test]
fn a_closed_shadow_root_reports_that_it_cannot_be_pierced() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("shadow-closed");
    let url = server.url("/browser/shadow-closed");

    // A CSS locator cannot see into a closed root at all.
    let css_case = dir.write(
        "closed-css.yaml",
        &format!(
            r##"name: closed root css
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      css: "#closed-button"
      timeout_ms: 400
"##
        ),
    );
    let error = run_failing(dir.path(), &css_case);
    assert!(
        error.contains("could not find CSS selector `#closed-button`"),
        "the reported error must still name the locator: {error}"
    );
    assert!(
        error.contains("closed shadow root"),
        "a closed shadow root must be named as the reason: {error}"
    );
    assert!(
        error.contains("`attachShadow({ mode: 'open' })`"),
        "the error must say how to make the root reachable: {error}"
    );

    // The accessibility tree does expose a closed root, so `role` finds the node
    // and only the hit test can explain why the click cannot land.
    let role_case = dir.write(
        "closed-role.yaml",
        &format!(
            r##"name: closed root role
steps:
  - browser.open:
      url: "{url}"
  - browser.click:
      role: button
      name: Closed Shadow
      timeout_ms: 800
"##
        ),
    );
    let error = run_failing(dir.path(), &role_case);
    assert!(
        error.contains("was occluded by <div#closed-host>"),
        "the closed host is the only element the hit test can see: {error}"
    );
    assert!(
        error.contains("closed shadow root"),
        "a closed shadow root must be named as the reason: {error}"
    );
}

#[test]
fn xpath_stays_shadow_blind() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("shadow-xpath");
    // `document.evaluate` cannot enter a shadow root; the limit is documented and
    // pinned here so a later change cannot silently promise more than it does.
    let case = dir.write(
        "xpath.yaml",
        &format!(
            r##"name: xpath stays in the light DOM
steps:
  - browser.open:
      url: "{url}"
  - browser.wait:
      xpath: "//*[@id='single-button']"
      timeout_ms: 400
"##,
            url = server.url("/browser/shadow")
        ),
    );

    let error = run_failing(dir.path(), &case);
    assert!(
        error.contains("could not find XPath `//*[@id='single-button']`"),
        "xpath must keep missing shadow content: {error}"
    );
    assert!(
        !error.contains("closed shadow root"),
        "this page has no closed root, so no closed-root hint belongs here: {error}"
    );
}

#[test]
fn screenshot_masks_still_cover_fields_inside_a_shadow_root() {
    let server = TestServer::start();
    let Some(_guard) = chrome_test(&server) else {
        return;
    };
    let dir = TempDir::new("shadow-mask");
    let case = dir.write(
        "mask.yaml",
        &format!(
            r##"name: screenshot masks shadow fields
steps:
  - browser.open:
      url: "{url}"
  - browser.screenshot:
      path: shots/shadow-masked.png
  - browser.read:
      css: "#mask-report"
    expect:
      - path: $.text
        equals: "shadow-pw=true,shadow-note=true"
"##,
            url = server.url("/browser/shadow-mask")
        ),
    );

    run_ok(dir.path(), &case);
    let shot = dir.path().join("shots/shadow-masked.png");
    let bytes = fs::read(&shot).expect("the screenshot must exist");
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "the screenshot must be a PNG"
    );
}
