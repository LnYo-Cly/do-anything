#![cfg(feature = "cli")]

//! End-to-end tests for the step modifier `optional: true`.
//!
//! The modifier is read and judged by the engine, so these tests run the
//! compiled binary and check the exit code together with the report a user
//! reads: a step whose work is allowed to be absent is `skipped`, while a step
//! that could not run at all still ends the case.

mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use support::TempDir;

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

/// Runs the binary with an empty working directory of its own.
fn run_da(args: &[&str]) -> Output {
    let work = TempDir::new("optional-cwd");
    run_da_in(work.path(), args)
}

/// Runs the binary with `dir` as its working directory.
fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(da())
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

/// A `cli.run` step that always succeeds, quoted for a case file.
fn succeeding_step() -> String {
    format!(
        "  - cli.run:\n      command: '{}'\n      args: [\"--version\"]\n",
        da()
    )
}

/// A case that runs the binary and asserts that it fails.
fn assertion_failure_case() -> String {
    format!(
        "name: Optional assertion\nsteps:\n  - cli.run:\n      command: '{}'\n      args: [\"--version\"]\n    optional: true\n    expect:\n      - path: $.exit_code\n        equals: 42\n{}",
        da(),
        succeeding_step()
    )
}

#[test]
fn a_failed_assertion_on_an_optional_step_leaves_the_case_passing() {
    let dir = TempDir::new("optional-assertion");
    let case = dir.write("optional.yaml", &assertion_failure_case());

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);

    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));
    let value = json_stdout(&output);
    assert_eq!(value["ok"], true);
    let case_report = &value["cases"][0];
    assert_eq!(case_report["status"], "passed");
    assert_eq!(case_report["error"], Value::Null);

    let skipped = &case_report["steps"][0];
    assert_eq!(skipped["status"], "skipped");
    assert_eq!(
        skipped["error"]["kind"], "assertion",
        "the reason must stay in the report, got: {skipped}"
    );
    assert_eq!(skipped["assertions"][0]["passed"], false);
    assert!(
        skipped["result"]["exit_code"].is_number(),
        "the result must stay in the report, got: {skipped}"
    );

    assert_eq!(
        case_report["steps"][1]["status"], "passed",
        "the step after a skipped optional step must run"
    );
    assert_eq!(value["summary"]["steps_skipped"], 1);
    assert_eq!(value["summary"]["cases_passed"], 1);
}

#[test]
fn text_output_names_the_optional_step_and_keeps_the_failure() {
    let dir = TempDir::new("optional-text");
    let case = dir.write("optional.yaml", &assertion_failure_case());

    let output = run_da(&["run", path_str(&case), "--no-bundle"]);

    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("PASS"), "got: {text}");
    assert!(
        text.contains("cli.run (optional): 1 of 1 assertions failed"),
        "the skipped step must say why, got: {text}"
    );
    assert!(
        text.contains("$.exit_code equals 42 (actual 0)"),
        "the failed assertion must stay visible, got: {text}"
    );
    assert!(text.contains("1 skipped"), "got: {text}");
}

#[test]
fn an_environment_error_on_an_optional_step_still_ends_the_run() {
    let dir = TempDir::new("optional-environment");
    let case = dir.write(
        "environment.yaml",
        &format!(
            "name: Optional program that cannot start\nsteps:\n  - cli.run:\n      command: da-no-such-program\n    optional: true\n{}",
            succeeding_step()
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);

    assert_eq!(
        exit_code(&output),
        Some(4),
        "an environment error must not be skipped, stderr: {:?}",
        stderr(&output)
    );
    let value = json_stdout(&output);
    assert_eq!(value["ok"], false);
    let case_report = &value["cases"][0];
    assert_eq!(case_report["status"], "error");
    assert_eq!(case_report["error"]["kind"], "environment");
    assert_eq!(case_report["steps"][0]["status"], "error");
    assert_eq!(case_report["steps"][0]["error"]["kind"], "environment");
    assert_eq!(
        case_report["steps"][1]["status"], "skipped",
        "the case must stop after an environment error"
    );
    assert_eq!(case_report["steps"][1]["result"], Value::Null);
}

#[cfg(feature = "http")]
#[test]
fn an_optional_step_that_times_out_is_skipped() {
    let server = support::TestServer::start();
    let dir = TempDir::new("optional-timeout");
    let case = dir.write(
        "timeout.yaml",
        &format!(
            "name: Optional slow call\nsteps:\n  - http.request:\n      url: {base}/slow\n      timeout_ms: 50\n    optional: true\n  - http.request:\n      url: {base}/health\n    expect:\n      - path: $.status\n        equals: 200\n",
            base = server.url("")
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);

    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));
    let value = json_stdout(&output);
    let case_report = &value["cases"][0];
    assert_eq!(case_report["status"], "passed");
    assert_eq!(case_report["steps"][0]["status"], "skipped");
    assert_eq!(case_report["steps"][0]["error"]["kind"], "timeout");
    assert_eq!(case_report["steps"][1]["status"], "passed");
}

#[test]
fn an_optional_step_may_not_save_a_variable() {
    let dir = TempDir::new("optional-save");
    let case = dir.write(
        "optional-save.yaml",
        &format!(
            "name: Optional save\nsteps:\n  - cli.run:\n      command: '{}'\n    optional: true\n    save:\n      version: $.stdout\n",
            da()
        ),
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(3), "stderr: {:?}", stderr(&lint));
    // `da lint` reports what it found on standard output.
    let lint_error = stdout(&lint);
    assert!(lint_error.contains("`optional: true`"), "got: {lint_error}");
    assert!(lint_error.contains("`save`"), "got: {lint_error}");
    assert!(
        lint_error.contains("line 6"),
        "the error must name the `save` line, got: {lint_error}"
    );
    assert!(
        lint_error.lines().any(|line| line.contains('^')),
        "the parser snippet must keep its caret line, got: {lint_error}"
    );

    let run = run_da(&["run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&run), Some(3), "stderr: {:?}", stderr(&run));
    assert!(
        stdout(&run).is_empty(),
        "no case may run, got: {:?}",
        stdout(&run)
    );
    assert!(
        stderr(&run).contains("`optional: true` cannot be combined with `save`"),
        "got: {:?}",
        stderr(&run)
    );
}

#[test]
fn an_optional_step_needs_a_boolean() {
    let dir = TempDir::new("optional-boolean");
    let case = dir.write(
        "optional-string.yaml",
        "name: Wrong optional\nsteps:\n  - cli.run:\n      command: da\n    optional: \"true\"\n",
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(3), "stderr: {:?}", stderr(&lint));
    let error = stdout(&lint);
    assert!(
        error.contains("`optional` must be true or false"),
        "got: {error}"
    );
    assert!(error.contains("a string"), "got: {error}");
    assert!(
        error.contains("line 5"),
        "the error must name the value's line, got: {error}"
    );

    let run = run_da(&["run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&run), Some(3), "stderr: {:?}", stderr(&run));
}

#[test]
fn an_explicit_false_optional_step_still_saves_and_runs() {
    let dir = TempDir::new("optional-explicit-false");
    let case = dir.write(
        "explicit-false.yaml",
        &format!(
            "name: Explicit false\nsteps:\n  - cli.run:\n      command: '{}'\n      args: [\"--version\"]\n    optional: false\n    save:\n      version: $.stdout\n    expect:\n      - path: $.exit_code\n        equals: 0\n{}",
            da(),
            succeeding_step()
        ),
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(0), "stderr: {:?}", stderr(&lint));

    let run = run_da(&["run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&run), Some(0), "stderr: {:?}", stderr(&run));
}
