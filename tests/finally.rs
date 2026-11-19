//! End-to-end tests for case-level `finally` cleanup steps.
//!
//! `finally` steps always run in order after `steps` finish (whether `steps`
//! passed, failed an assertion, or errored) and before sessions close. Failure
//! in one `finally` step does not skip later `finally` steps, and variables
//! saved in `steps` are available in `finally` (with `optional: true` skipping
//! cleanup if an earlier `save` step failed before writing the variable).

// Each feature combination compiles a different subset of the helpers below.
#![allow(dead_code)]

mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;
use support::TempDir;
#[cfg(feature = "http")]
use support::TestServer;

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da(args: &[&str]) -> Output {
    let work = TempDir::new("finally-cwd");
    Command::new(da())
        .current_dir(work.path())
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

#[cfg(feature = "http")]
#[test]
fn finally_runs_in_order_after_a_failed_main_step_even_when_a_finally_step_fails() {
    let server = TestServer::start();
    let dir = TempDir::new("finally-order");
    let case = dir.write(
        "finally-order.yaml",
        &format!(
            r#"
name: Cleanup after main failure
steps:
  - http.request:
      method: POST
      url: {base}/login
      json: {{ user: "alice" }}
    save:
      token: $.body.token
  - http.request:
      url: {base}/health
    expect:
      - path: $.status
        equals: 500
  - http.request:
      url: {base}/after-failure
finally:
  - http.request:
      method: DELETE
      url: {base}/cleanup/${{token}}
    expect:
      - path: $.status
        equals: 204
  - http.request:
      method: POST
      url: {base}/health
"#,
            base = server.url("")
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&output), Some(1), "stderr: {:?}", stderr(&output));

    let value = json_stdout(&output);
    let case_report = &value["cases"][0];
    assert_eq!(case_report["status"], "failed");
    assert_eq!(
        case_report["error"]["kind"], "assertion",
        "main step failure must stay the primary case error: {case_report}"
    );
    assert_eq!(case_report["steps"][0]["status"], "passed");
    assert_eq!(case_report["steps"][1]["status"], "failed");
    assert_eq!(case_report["steps"][2]["status"], "skipped");

    let finally_reports = case_report["finally"]
        .as_array()
        .expect("finally reports must appear in JSON");
    assert_eq!(finally_reports.len(), 2);
    assert_eq!(
        finally_reports[0]["status"], "failed",
        "first finally step failed its assertion"
    );
    assert_eq!(
        finally_reports[1]["status"], "passed",
        "second finally step must still execute after first finally step fails"
    );

    let targets: Vec<String> = server
        .requests()
        .into_iter()
        .map(|req| format!("{} {}", req.method, req.target))
        .collect();
    assert_eq!(
        targets,
        vec![
            "POST /login",
            "GET /health",
            "DELETE /cleanup/t-123",
            "POST /health",
        ]
    );
}

#[cfg(feature = "http")]
#[test]
fn finally_failure_turns_a_passing_case_into_a_failure_and_writes_bundle() {
    let server = TestServer::start();
    let dir = TempDir::new("finally-fail-bundle");
    let bundle_dir = dir.path().join("bundles");
    let case = dir.write(
        "finally-fails.yaml",
        &format!(
            r#"
name: Finally assertion fails
steps:
  - http.request:
      url: {base}/health
    expect:
      - path: $.status
        equals: 200
finally:
  - http.request:
      method: DELETE
      url: {base}/cleanup
    expect:
      - path: $.status
        equals: 204
  - http.request:
      url: {base}/health
    expect:
      - path: $.status
        equals: 200
"#,
            base = server.url("")
        ),
    );

    let output = run_da(&[
        "--output",
        "json",
        "run",
        path_str(&case),
        "--bundle-dir",
        path_str(&bundle_dir),
    ]);
    assert_eq!(exit_code(&output), Some(1), "stderr: {:?}", stderr(&output));

    let value = json_stdout(&output);
    let case_report = &value["cases"][0];
    assert_eq!(case_report["status"], "failed");
    assert_eq!(case_report["error"]["kind"], "assertion");
    assert_eq!(case_report["steps"][0]["status"], "passed");
    assert_eq!(case_report["finally"][0]["status"], "failed");
    assert_eq!(case_report["finally"][1]["status"], "passed");
    assert_eq!(value["summary"]["steps"], 3);
    assert_eq!(value["summary"]["steps_passed"], 2);
    assert_eq!(value["summary"]["steps_failed"], 1);

    let bundle_path = case_report["bundle"]
        .as_str()
        .expect("a failing finally must write a failure bundle");
    let report_json: Value = serde_json::from_str(
        &std::fs::read_to_string(Path::new(bundle_path).join("report.json"))
            .expect("bundle report.json must exist"),
    )
    .expect("bundle report.json must parse");
    assert_eq!(report_json["status"], "failed");
    assert_eq!(report_json["finally"].as_array().map(Vec::len), Some(2));
}

#[cfg(feature = "http")]
#[test]
fn optional_finally_skips_when_saved_variable_was_never_created() {
    let server = TestServer::start();
    let dir = TempDir::new("finally-optional-var");
    let case = dir.write(
        "finally-optional.yaml",
        &format!(
            r#"
name: Optional cleanup when create fails
steps:
  - http.request:
      method: POST
      url: {base}/login
      json: {{ user: "alice" }}
    expect:
      - path: $.status
        equals: 500
    save:
      item_id: $.body.token
finally:
  - http.request:
      method: DELETE
      url: {base}/cleanup/${{item_id}}
    optional: true
  - http.request:
      method: POST
      url: {base}/always-runs
"#,
            base = server.url("")
        ),
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(0), "stdout: {}", stdout(&lint));

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&output), Some(1), "stderr: {:?}", stderr(&output));

    let value = json_stdout(&output);
    let case_report = &value["cases"][0];
    assert_eq!(case_report["status"], "failed");
    assert_eq!(case_report["finally"][0]["status"], "skipped");
    assert!(
        case_report["finally"][0]["error"]["message"]
            .as_str()
            .is_some_and(|msg| msg.contains("item_id")),
        "skipped finally step should explain missing variable: {case_report}"
    );
    assert_eq!(case_report["finally"][1]["status"], "passed");

    let targets: Vec<String> = server
        .requests()
        .into_iter()
        .map(|req| format!("{} {}", req.method, req.target))
        .collect();
    assert_eq!(targets, vec!["POST /login", "POST /always-runs"]);
}

#[cfg(feature = "cli")]
#[test]
fn lint_and_schema_validate_finally_steps() {
    let dir = TempDir::new("finally-lint-schema");
    let valid = dir.write(
        "valid.yaml",
        &format!(
            "name: Valid finally\nsteps:\n  - cli.run:\n      command: '{}'\n      args: [\"--version\"]\n    save:\n      ver: $.stdout\nfinally:\n  - cli.run:\n      command: '{}'\n      args: [\"--version\"]\n    expect:\n      - path: $.exit_code\n        equals: 0\n",
            da(),
            da()
        ),
    );
    let lint_ok = run_da(&["lint", path_str(&valid)]);
    assert_eq!(exit_code(&lint_ok), Some(0), "stdout: {}", stdout(&lint_ok));

    let empty_finally = dir.write(
        "empty-finally.yaml",
        &format!(
            "name: Empty finally\nsteps:\n  - cli.run:\n      command: '{}'\nfinally: []\n",
            da()
        ),
    );
    let lint_empty = run_da(&["lint", path_str(&empty_finally)]);
    assert_eq!(exit_code(&lint_empty), Some(3));
    assert!(
        stdout(&lint_empty).contains("finally"),
        "got: {}",
        stdout(&lint_empty)
    );

    let undef_var = dir.write(
        "undef-var.yaml",
        &format!(
            "name: Undefined var in finally\nsteps:\n  - cli.run:\n      command: '{}'\nfinally:\n  - cli.run:\n      command: '{}'\n      args: [\"${{missing_var}}\"]\n",
            da(),
            da()
        ),
    );
    let lint_undef = run_da(&["lint", path_str(&undef_var)]);
    assert_eq!(exit_code(&lint_undef), Some(3));
    assert!(
        stdout(&lint_undef).contains("finally step 1")
            && stdout(&lint_undef).contains("missing_var"),
        "got: {}",
        stdout(&lint_undef)
    );

    let schema_out = run_da(&["--output", "json", "schema"]);
    assert_eq!(exit_code(&schema_out), Some(0));
    let schema = json_stdout(&schema_out);
    let validator = jsonschema::validator_for(&schema).expect("schema must compile");

    let good_instance = serde_json::json!({
        "name": "Schema ok",
        "steps": [{"cli.run": {"command": "da"}}],
        "finally": [{"cli.run": {"command": "da"}, "optional": true}]
    });
    assert!(validator.is_valid(&good_instance));

    let empty_instance = serde_json::json!({
        "name": "Schema empty finally",
        "steps": [{"cli.run": {"command": "da"}}],
        "finally": []
    });
    assert!(!validator.is_valid(&empty_instance));
}

#[cfg(feature = "cli")]
#[test]
fn text_output_renders_finally_failures_and_optional_skips() {
    let dir = TempDir::new("finally-text");
    let case = dir.write(
        "finally-text.yaml",
        &format!(
            r#"
name: Text finally output
steps:
  - cli.run:
      command: '{}'
      args: ["--version"]
finally:
  - cli.run:
      command: '{}'
      args: ["--version"]
    optional: true
    expect:
      - path: $.exit_code
        equals: 99
  - cli.run:
      command: '{}'
      args: ["--version"]
    expect:
      - path: $.exit_code
        equals: 42
"#,
            da(),
            da(),
            da()
        ),
    );

    let output = run_da(&["run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&output), Some(1), "stderr: {:?}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("FAIL "), "got: {text}");
    assert!(
        text.contains("finally cli.run (optional): 1 of 1 assertions failed"),
        "got: {text}"
    );
    assert!(
        text.contains("finally cli.run: 1 of 1 assertions failed"),
        "got: {text}"
    );
    assert!(
        text.contains("$.exit_code equals 42 (actual 0)"),
        "got: {text}"
    );
}
