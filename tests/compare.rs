//! End-to-end integration tests for `da compare`.

mod support;

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da(args: &[&str]) -> Output {
    let work = TempDir::new("compare-cwd");
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

fn write_bundle(root: &Path, folder: &str, meta: &Value, report: &Value) {
    let dir = root.join(folder);
    fs::create_dir_all(&dir).expect("case bundle directory must be created");
    fs::write(dir.join("case.yaml"), "name: synthetic\nsteps: []\n").expect("case.yaml");
    fs::write(
        dir.join("report.json"),
        serde_json::to_vec_pretty(report).expect("serialize report"),
    )
    .expect("report.json");
    fs::write(
        dir.join("meta.json"),
        serde_json::to_vec_pretty(meta).expect("serialize meta"),
    )
    .expect("meta.json");
}

fn meta_obj(case_file: &str, case_name: &str, sha: &str, status: &str) -> Value {
    json!({
        "schema_version": 1,
        "da_version": "0.1.0",
        "case_file": case_file,
        "case_sha256": sha,
        "case_name": case_name,
        "project": null,
        "suite": null,
        "layer": null,
        "tags": [],
        "overridden_vars": [],
        "status": status,
        "local_only": false,
        "created_at": 1
    })
}

fn report_obj(
    case_file: &str,
    case_name: &str,
    status: &str,
    duration_ms: u64,
    steps: Value,
    error: Value,
) -> Value {
    json!({
        "file": case_file,
        "name": case_name,
        "project": null,
        "suite": null,
        "layer": null,
        "tags": [],
        "status": status,
        "duration_ms": duration_ms,
        "bundle": null,
        "steps": steps,
        "error": error,
        "close_errors": []
    })
}

#[test]
fn missing_empty_or_non_directory_paths_return_usage_exit_two() {
    let dir = TempDir::new("compare-usage");
    let valid = dir.path().join("valid");
    write_bundle(
        &valid,
        "0-ok",
        &meta_obj("cases/ok.yaml", "Ok", "aaa111", "passed"),
        &report_obj(
            "cases/ok.yaml",
            "Ok",
            "passed",
            5,
            json!([{ "name": "sample.step", "status": "passed", "duration_ms": 5, "assertions": [] }]),
            Value::Null,
        ),
    );

    let missing = dir.path().join("missing");
    let out = run_da(&["compare", path_str(&missing), path_str(&valid)]);
    assert_eq!(exit_code(&out), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("does not exist"));

    let json_missing = run_da(&[
        "--output",
        "json",
        "compare",
        path_str(&missing),
        path_str(&valid),
    ]);
    assert_eq!(exit_code(&json_missing), Some(2));
    let err_val: Value = serde_json::from_slice(&json_missing.stdout).expect("valid error JSON");
    assert_eq!(err_val["ok"], false);
    assert_eq!(err_val["error"]["kind"], "usage");

    let file_path = dir.write("not-a-dir.txt", "hello");
    let out = run_da(&["compare", path_str(&valid), path_str(&file_path)]);
    assert_eq!(exit_code(&out), Some(2));
    assert!(stderr(&out).contains("not a directory"));

    let empty = dir.path().join("empty");
    fs::create_dir_all(empty.join("0-incomplete")).expect("create incomplete dir");
    fs::write(empty.join("0-incomplete/report.json"), "{}").expect("write partial report");
    let out = run_da(&["compare", path_str(&valid), path_str(&empty)]);
    assert_eq!(exit_code(&out), Some(2));
    assert!(stderr(&out).contains("meta.json"));

    let repeat_root = dir.path().join("repeat-record");
    fs::create_dir_all(repeat_root.join("attempt-1")).expect("create attempt-1");
    let out = run_da(&["compare", path_str(&repeat_root), path_str(&valid)]);
    assert_eq!(exit_code(&out), Some(2));
    assert!(
        stderr(&out).contains("attempt-1"),
        "stderr: {}",
        stderr(&out)
    );
}

#[test]
fn partial_case_directory_alongside_complete_case_fails_closed_with_exit_two() {
    let dir = TempDir::new("compare-partial-record");

    // 1. Candidate has 1 complete passing case + 1 incomplete failed case (`report.json` without `meta.json`).
    // Baseline has both cases complete and passing, so silently omitting `1-login` would misclassify
    // a real regression as `removed` and exit 0.
    let base_full = dir.path().join("base-full");
    write_bundle(
        &base_full,
        "0-health",
        &meta_obj("cases/health.yaml", "Health", "sha-health", "passed"),
        &report_obj(
            "cases/health.yaml",
            "Health",
            "passed",
            3,
            json!([]),
            Value::Null,
        ),
    );
    write_bundle(
        &base_full,
        "1-login",
        &meta_obj("cases/login.yaml", "Login", "sha-login", "passed"),
        &report_obj(
            "cases/login.yaml",
            "Login",
            "passed",
            5,
            json!([]),
            Value::Null,
        ),
    );

    let cand_partial = dir.path().join("cand-partial");
    write_bundle(
        &cand_partial,
        "0-health",
        &meta_obj("cases/health.yaml", "Health", "sha-health", "passed"),
        &report_obj(
            "cases/health.yaml",
            "Health",
            "passed",
            3,
            json!([]),
            Value::Null,
        ),
    );
    let cand_incomplete = cand_partial.join("1-login");
    fs::create_dir_all(&cand_incomplete).expect("create candidate incomplete dir");
    fs::write(
        cand_incomplete.join("report.json"),
        serde_json::to_vec_pretty(&report_obj(
            "cases/login.yaml",
            "Login",
            "failed",
            9,
            json!([]),
            json!({ "kind": "assertion", "message": "broken" }),
        ))
        .expect("serialize partial report"),
    )
    .expect("write partial report.json");

    let cand_text = run_da(&["compare", path_str(&base_full), path_str(&cand_partial)]);
    assert_eq!(
        exit_code(&cand_text),
        Some(2),
        "stdout: {}\nstderr: {}",
        stdout(&cand_text),
        stderr(&cand_text)
    );
    assert!(
        stdout(&cand_text).is_empty(),
        "stdout must be empty on text error: {}",
        stdout(&cand_text)
    );
    assert!(
        stderr(&cand_text).contains("1-login") && stderr(&cand_text).contains("meta.json"),
        "stderr: {}",
        stderr(&cand_text)
    );

    let cand_json = run_da(&[
        "--output",
        "json",
        "compare",
        path_str(&base_full),
        path_str(&cand_partial),
    ]);
    assert_eq!(exit_code(&cand_json), Some(2));
    let cand_err: Value = serde_json::from_slice(&cand_json.stdout).expect("valid error JSON");
    assert_eq!(cand_err["ok"], false);
    assert_eq!(cand_err["error"]["kind"], "usage");
    let cand_msg = cand_err["error"]["message"].as_str().expect("message");
    assert!(
        cand_msg.contains("1-login") && cand_msg.contains("meta.json"),
        "message: {cand_msg}"
    );

    // 2. Baseline has 1 complete case + 1 incomplete case (`case.yaml` present, `meta.json` absent).
    // Silently omitting `1-login` in baseline would misclassify it as `added` and exit 0.
    let base_partial = dir.path().join("base-partial");
    write_bundle(
        &base_partial,
        "0-health",
        &meta_obj("cases/health.yaml", "Health", "sha-health", "passed"),
        &report_obj(
            "cases/health.yaml",
            "Health",
            "passed",
            3,
            json!([]),
            Value::Null,
        ),
    );
    let base_incomplete = base_partial.join("1-login");
    fs::create_dir_all(&base_incomplete).expect("create baseline incomplete dir");
    fs::write(
        base_incomplete.join("case.yaml"),
        "name: Login\nsteps: []\n",
    )
    .expect("write partial case.yaml");

    let base_text = run_da(&["compare", path_str(&base_partial), path_str(&base_full)]);
    assert_eq!(
        exit_code(&base_text),
        Some(2),
        "stdout: {}\nstderr: {}",
        stdout(&base_text),
        stderr(&base_text)
    );
    assert!(
        stderr(&base_text).contains("1-login") && stderr(&base_text).contains("meta.json"),
        "stderr: {}",
        stderr(&base_text)
    );

    let base_json = run_da(&[
        "--output",
        "json",
        "compare",
        path_str(&base_partial),
        path_str(&base_full),
    ]);
    assert_eq!(exit_code(&base_json), Some(2));
    let base_err: Value = serde_json::from_slice(&base_json.stdout).expect("valid error JSON");
    assert_eq!(base_err["ok"], false);
    assert_eq!(base_err["error"]["kind"], "usage");
    let base_msg = base_err["error"]["message"].as_str().expect("message");
    assert!(
        base_msg.contains("1-login") && base_msg.contains("meta.json"),
        "message: {base_msg}"
    );

    // 3. Leftover `.tmp-*` staging directory alongside a complete case also fails closed.
    let staged_partial = dir.path().join("staged-partial");
    write_bundle(
        &staged_partial,
        "0-health",
        &meta_obj("cases/health.yaml", "Health", "sha-health", "passed"),
        &report_obj(
            "cases/health.yaml",
            "Health",
            "passed",
            3,
            json!([]),
            Value::Null,
        ),
    );
    let staging_dir = staged_partial.join(".tmp-9999-1");
    fs::create_dir_all(&staging_dir).expect("create staging dir");
    fs::write(staging_dir.join("case.yaml"), "name: Login\nsteps: []\n").expect("staging case");

    let staged_out = run_da(&["compare", path_str(&base_full), path_str(&staged_partial)]);
    assert_eq!(
        exit_code(&staged_out),
        Some(2),
        "stdout: {}\nstderr: {}",
        stdout(&staged_out),
        stderr(&staged_out)
    );
    assert!(
        stderr(&staged_out).contains(".tmp-9999-1"),
        "stderr: {}",
        stderr(&staged_out)
    );
}

#[test]
fn malformed_meta_or_report_returns_invalid_case_exit_three() {
    let dir = TempDir::new("compare-corrupt");
    let valid = dir.path().join("valid");
    write_bundle(
        &valid,
        "0-ok",
        &meta_obj("cases/ok.yaml", "Ok", "aaa111", "passed"),
        &report_obj(
            "cases/ok.yaml",
            "Ok",
            "passed",
            5,
            json!([{ "name": "sample.step", "status": "passed", "duration_ms": 5, "assertions": [] }]),
            Value::Null,
        ),
    );

    let bad_meta = dir.path().join("bad-meta");
    fs::create_dir_all(bad_meta.join("0-broken")).expect("dir");
    fs::write(bad_meta.join("0-broken/meta.json"), "{not-json").expect("corrupt meta");
    let out = run_da(&["compare", path_str(&bad_meta), path_str(&valid)]);
    assert_eq!(exit_code(&out), Some(3), "stderr: {}", stderr(&out));

    let missing_report = dir.path().join("missing-report");
    fs::create_dir_all(missing_report.join("0-broken")).expect("dir");
    fs::write(
        missing_report.join("0-broken/meta.json"),
        serde_json::to_vec(&meta_obj("cases/ok.yaml", "Ok", "aaa111", "passed")).unwrap(),
    )
    .expect("meta");
    let out = run_da(&[
        "--output",
        "json",
        "compare",
        path_str(&valid),
        path_str(&missing_report),
    ]);
    assert_eq!(exit_code(&out), Some(3));
    let val: Value = serde_json::from_slice(&out.stdout).expect("JSON error");
    assert_eq!(val["ok"], false);
    assert_eq!(val["error"]["kind"], "invalid_case");
}

#[test]
fn classifies_all_outcomes_across_shifted_positions_and_cross_os_paths() {
    let dir = TempDir::new("compare-shifted");
    let baseline = dir.path().join("baseline");
    let candidate = dir.path().join("candidate");

    // Baseline uses Windows backslashes and positions 0..4.
    write_bundle(
        &baseline,
        "0-login",
        &meta_obj(
            r".\cases\auth\login.yaml",
            "Login",
            "sha-login-v1",
            "passed",
        ),
        &report_obj(
            r"cases\auth\login.yaml",
            "Login",
            "passed",
            10,
            json!([
                {
                    "name": "sample.call",
                    "status": "passed",
                    "duration_ms": 10,
                    "assertions": [
                        { "path": "$.status", "comparison": "equals", "expected": 200, "actual": 200, "passed": true },
                        { "path": "$.ok", "comparison": "equals", "expected": true, "actual": true, "passed": true }
                    ]
                }
            ]),
            Value::Null,
        ),
    );
    write_bundle(
        &baseline,
        "1-signup",
        &meta_obj(
            r"cases\auth\signup.yaml",
            "Signup",
            "sha-signup-v1",
            "failed",
        ),
        &report_obj(
            r"cases\auth\signup.yaml",
            "Signup",
            "failed",
            8,
            json!([
                {
                    "name": "sample.call",
                    "status": "failed",
                    "duration_ms": 8,
                    "assertions": [
                        { "path": "$.status", "comparison": "equals", "expected": 201, "actual": 500, "passed": false }
                    ],
                    "error": { "kind": "assertion", "message": "1 of 1 assertions failed" }
                }
            ]),
            json!({ "kind": "assertion", "message": "1 of 1 assertions failed" }),
        ),
    );
    write_bundle(
        &baseline,
        "2-legacy",
        &meta_obj(r"cases\legacy.yaml", "Legacy", "sha-legacy", "passed"),
        &report_obj(
            "cases/legacy.yaml",
            "Legacy",
            "passed",
            4,
            json!([]),
            Value::Null,
        ),
    );
    write_bundle(
        &baseline,
        "3-health",
        &meta_obj(r"cases\health.yaml", "Health", "sha-health", "passed"),
        &report_obj(
            "cases/health.yaml",
            "Health",
            "passed",
            3,
            json!([]),
            Value::Null,
        ),
    );
    write_bundle(
        &baseline,
        "4-flaky-timeout",
        &meta_obj(r"cases\slow.yaml", "Slow", "sha-slow", "error"),
        &report_obj(
            "cases/slow.yaml",
            "Slow",
            "error",
            50,
            json!([{ "name": "sample.wait", "status": "error", "duration_ms": 50, "assertions": [] }]),
            json!({ "kind": "timeout", "message": "exceeded budget" }),
        ),
    );

    // Candidate inserts a brand-new case at position 0, shifting every later directory index,
    // and uses Unix forward slashes.
    write_bundle(
        &candidate,
        "0-brand-new",
        &meta_obj("./cases/aaa_new.yaml", "Brand new", "sha-new", "passed"),
        &report_obj(
            "cases/aaa_new.yaml",
            "Brand new",
            "passed",
            6,
            json!([]),
            Value::Null,
        ),
    );
    write_bundle(
        &candidate,
        "1-login",
        &meta_obj("cases/auth/login.yaml", "Login", "sha-login-v2", "failed"),
        &report_obj(
            "cases/auth/login.yaml",
            "Login",
            "failed",
            15,
            json!([
                {
                    "name": "sample.call",
                    "status": "failed",
                    "duration_ms": 15,
                    "assertions": [
                        { "path": "$.status", "comparison": "equals", "expected": 200, "actual": 500, "passed": false },
                        { "path": "$.ok", "comparison": "equals", "expected": true, "actual": true, "passed": true }
                    ],
                    "error": { "kind": "assertion", "message": "1 of 2 assertions failed" }
                }
            ]),
            json!({ "kind": "assertion", "message": "1 of 2 assertions failed" }),
        ),
    );
    write_bundle(
        &candidate,
        "2-signup",
        &meta_obj(
            "cases/auth/signup.yaml",
            "Signup",
            "sha-signup-v1",
            "passed",
        ),
        &report_obj(
            "cases/auth/signup.yaml",
            "Signup",
            "passed",
            5,
            json!([
                {
                    "name": "sample.call",
                    "status": "passed",
                    "duration_ms": 5,
                    "assertions": [
                        { "path": "$.status", "comparison": "equals", "expected": 201, "actual": 201, "passed": true }
                    ]
                }
            ]),
            Value::Null,
        ),
    );
    write_bundle(
        &candidate,
        "3-health",
        &meta_obj("cases/health.yaml", "Health", "sha-health", "passed"),
        &report_obj(
            "cases/health.yaml",
            "Health",
            "passed",
            4,
            json!([]),
            Value::Null,
        ),
    );
    write_bundle(
        &candidate,
        "4-slow",
        &meta_obj("cases/slow.yaml", "Slow", "sha-slow", "failed"),
        &report_obj(
            "cases/slow.yaml",
            "Slow",
            "failed",
            20,
            json!([{ "name": "sample.wait", "status": "failed", "duration_ms": 20, "assertions": [] }]),
            json!({ "kind": "assertion", "message": "still broken" }),
        ),
    );

    let text_out = run_da(&["compare", path_str(&baseline), path_str(&candidate)]);
    assert_eq!(
        exit_code(&text_out),
        Some(1),
        "stderr: {}",
        stderr(&text_out)
    );
    let text = stdout(&text_out);
    assert!(
        text.contains(
            "REGRESSED cases/auth/login.yaml [Login] passed -> failed (+5 ms, case_changed)"
        ),
        "got:\n{text}"
    );
    assert!(
        text.contains("FIXED cases/auth/signup.yaml [Signup] failed -> passed (-3 ms)"),
        "got:\n{text}"
    );
    assert!(
        text.contains("STILL_PASSING cases/health.yaml [Health] passed -> passed (+1 ms)"),
        "got:\n{text}"
    );
    assert!(
        text.contains("STILL_FAILING cases/slow.yaml [Slow] timeout -> failed (-30 ms)"),
        "got:\n{text}"
    );
    assert!(
        text.contains("ADDED cases/aaa_new.yaml [Brand new] none -> passed (6 ms)"),
        "got:\n{text}"
    );
    assert!(
        text.contains("REMOVED cases/legacy.yaml [Legacy] passed -> none (4 ms)"),
        "got:\n{text}"
    );
    assert!(text.contains("Summary: 6 cases (1 regressed, 1 fixed, 1 still_failing, 1 still_passing, 1 added, 1 removed, 1 case_changed)"), "got:\n{text}");

    let json_out = run_da(&[
        "--output",
        "json",
        "compare",
        path_str(&baseline),
        path_str(&candidate),
    ]);
    assert_eq!(exit_code(&json_out), Some(1));
    let val: Value = serde_json::from_slice(&json_out.stdout).expect("compare JSON");
    assert_eq!(val["ok"], false);
    assert_eq!(val["summary"]["total"], 6);
    assert_eq!(val["summary"]["regressed"], 1);
    assert_eq!(val["summary"]["fixed"], 1);
    assert_eq!(val["summary"]["still_failing"], 1);
    assert_eq!(val["summary"]["still_passing"], 1);
    assert_eq!(val["summary"]["added"], 1);
    assert_eq!(val["summary"]["removed"], 1);
    assert_eq!(val["summary"]["case_changed"], 1);

    let login = val["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["case_file"] == "cases/auth/login.yaml")
        .expect("login entry");
    assert_eq!(login["classification"], "regressed");
    assert_eq!(login["case_changed"], true);
    assert_eq!(login["duration_delta_ms"], 5);
    assert_eq!(login["assertions"]["delta"]["passed"], -1);
    assert_eq!(login["assertions"]["delta"]["failed"], 1);
    assert_eq!(login["steps"]["diffs"][0]["baseline_status"], "passed");
    assert_eq!(login["steps"]["diffs"][0]["candidate_status"], "failed");
}

#[test]
fn exits_zero_when_no_case_regressed_and_handles_duplicate_identities() {
    let dir = TempDir::new("compare-ok-and-dups");
    let baseline = dir.path().join("baseline");
    let candidate = dir.path().join("candidate");

    // Two occurrences of the same (case_file, case_name) in baseline; one fixed in candidate, second removed.
    write_bundle(
        &baseline,
        "0-dup",
        &meta_obj("cases/dup.yaml", "Dup", "sha-1", "failed"),
        &report_obj("cases/dup.yaml", "Dup", "failed", 5, json!([]), Value::Null),
    );
    write_bundle(
        &baseline,
        "1-dup",
        &meta_obj("cases/dup.yaml", "Dup", "sha-1", "failed"),
        &report_obj("cases/dup.yaml", "Dup", "failed", 5, json!([]), Value::Null),
    );

    write_bundle(
        &candidate,
        "0-dup",
        &meta_obj("cases/dup.yaml", "Dup", "sha-1", "passed"),
        &report_obj("cases/dup.yaml", "Dup", "passed", 4, json!([]), Value::Null),
    );

    let out = run_da(&[
        "--output",
        "json",
        "compare",
        path_str(&baseline),
        path_str(&candidate),
    ]);
    assert_eq!(exit_code(&out), Some(0), "stderr: {}", stderr(&out));
    let val: Value = serde_json::from_slice(&out.stdout).expect("compare JSON");
    assert_eq!(val["ok"], true);
    assert_eq!(val["summary"]["regressed"], 0);
    assert_eq!(val["summary"]["fixed"], 1);
    assert_eq!(val["summary"]["removed"], 1);
}

#[cfg(feature = "cli")]
#[test]
fn end_to_end_recorded_runs_detect_regression_and_keep_secrets_redacted() {
    let dir = TempDir::new("compare-e2e-secrets");
    let cases_dir = dir.path().join("cases");
    fs::create_dir_all(&cases_dir).expect("cases dir");

    let da_escaped = da().replace('\\', "\\\\");
    let target_case = cases_dir.join("b_check.yaml");
    fs::write(
        &target_case,
        format!(
            "name: Secret CLI check\nvars:\n  tok: ${{env:DA_COMPARE_SECRET_TOKEN}}\nsteps:\n  - cli.run:\n      command: \"{da_escaped}\"\n      args: [\"--version\"]\n      env:\n        SECRET_ECHO: ${{tok}}\n    expect:\n      - path: $.exit_code\n        equals: 0\n"
        ),
    )
    .expect("write baseline case");

    let baseline_record = dir.path().join("rec-base");
    let base_run = Command::new(da())
        .current_dir(dir.path())
        .env("DA_COMPARE_SECRET_TOKEN", "ultra-secret-compare-token-99")
        .args([
            "run",
            "cases",
            "--no-bundle",
            "--record",
            path_str(&baseline_record),
        ])
        .output()
        .expect("run baseline");
    assert_eq!(
        exit_code(&base_run),
        Some(0),
        "stderr: {}",
        stderr(&base_run)
    );

    // Insert an earlier case `a_first.yaml` so `b_check.yaml` shifts from `0-...` to `1-...`,
    // and edit `b_check.yaml` so it fails while mentioning the secret.
    fs::write(
        cases_dir.join("a_first.yaml"),
        format!(
            "name: Added earlier case\nsteps:\n  - cli.run:\n      command: \"{da_escaped}\"\n      args: [\"--version\"]\n    expect:\n      - path: $.exit_code\n        equals: 0\n"
        ),
    )
    .expect("write added case");
    fs::write(
        &target_case,
        format!(
            "name: Secret CLI check\nvars:\n  tok: ${{env:DA_COMPARE_SECRET_TOKEN}}\nsteps:\n  - cli.run:\n      command: \"{da_escaped}\"\n      args: [\"--version\"]\n      env:\n        SECRET_ECHO: ${{tok}}\n    expect:\n      - path: $.stdout\n        equals: ${{tok}}\n"
        ),
    )
    .expect("write regressed case");

    let candidate_record = dir.path().join("rec-cand");
    let cand_run = Command::new(da())
        .current_dir(dir.path())
        .env("DA_COMPARE_SECRET_TOKEN", "ultra-secret-compare-token-99")
        .args([
            "run",
            "cases",
            "--no-bundle",
            "--record",
            path_str(&candidate_record),
        ])
        .output()
        .expect("run candidate");
    assert_eq!(
        exit_code(&cand_run),
        Some(1),
        "stderr: {}",
        stderr(&cand_run)
    );

    let cmp_json = Command::new(da())
        .current_dir(dir.path())
        .args([
            "--output",
            "json",
            "compare",
            path_str(&baseline_record),
            path_str(&candidate_record),
        ])
        .output()
        .expect("compare json");
    assert_eq!(exit_code(&cmp_json), Some(1));
    let raw_json = stdout(&cmp_json);
    assert!(
        !raw_json.contains("ultra-secret-compare-token-99"),
        "secret leaked in compare JSON: {raw_json}"
    );
    let val: Value = serde_json::from_str(&raw_json).expect("valid JSON");
    assert_eq!(val["summary"]["regressed"], 1);
    assert_eq!(val["summary"]["added"], 1);
    assert_eq!(val["summary"]["case_changed"], 1);

    let cmp_text = Command::new(da())
        .current_dir(dir.path())
        .args([
            "compare",
            path_str(&baseline_record),
            path_str(&candidate_record),
        ])
        .output()
        .expect("compare text");
    assert_eq!(exit_code(&cmp_text), Some(1));
    assert!(
        !stdout(&cmp_text).contains("ultra-secret-compare-token-99"),
        "secret leaked in compare text"
    );
}
