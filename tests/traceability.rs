//! End-to-end tests for #80 step-level traceability:
//! resolved step `args` and `saved` in `StepReport`, `case_sha256` and
//! `overridden_vars` in `meta.json`, `da session` audit/observers, and
//! `--trace-steps` + `da report`.

#![cfg_attr(
    not(all(feature = "http", feature = "log")),
    allow(dead_code, unused_imports)
)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(da());
    command.current_dir(dir).args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("failed to run da")
}

fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    run_da_with_env(dir, args, &[])
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be JSON ({error})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("test path must be valid UTF-8")
}

fn assert_no_secret_leaks(haystack: &str, secret: &str, context: &str) {
    assert!(
        !haystack.contains(secret),
        "{context}: raw secret leaked into output"
    );
    let pct = support::percent_encode(secret.as_bytes());
    assert!(
        !haystack.contains(&pct),
        "{context}: percent-encoded secret `{pct}` leaked into output"
    );
    let b64 = support::base64_encode(secret.as_bytes());
    assert!(
        !haystack.contains(&b64),
        "{context}: base64 secret `{b64}` leaked into output"
    );
}

#[cfg(feature = "http")]
#[test]
fn step_report_includes_resolved_args_and_saved_with_secret_redaction() {
    const SECRET: &str = "trace-s3cr3t-Val/99";
    const ENV_NAME: &str = "DA_TRACE_TEST_SECRET";

    let server = support::TestServer::start();
    let dir = TempDir::new("trace-args-saved");
    let vars_file = dir.write(
        "vars.yaml",
        &format!(
            "zeta: \"tail\"\nalpha: \"${{env:{ENV_NAME}}}\"\nsecret_b64: \"{}\"\n",
            support::base64_encode(SECRET.as_bytes())
        ),
    );
    let case_yaml = format!(
        r#"name: Multi-step traceability
vars:
  beta: default_beta
steps:
  - http.request:
      method: POST
      url: "{base}/login"
      json:
        user: "alice"
    expect:
      - path: $.status
        equals: 200
    save:
      session_token: $.body.token
  - http.request:
      method: POST
      url: "{base}/encoded-secret"
      json:
        token: "${{session_token}}"
        secret: "${{alpha}}"
        extra: "${{beta}}"
    save:
      echoed_url: $.body.url
      echoed_auth: $.body.authorization
      echoed_b64: $.body.json
  - http.request:
      method: POST
      url: "{base}/echo"
      json:
        carried: "${{echoed_url}}"
        carried_auth: "${{echoed_auth}}"
        carried_b64: "${{echoed_b64}}"
        pct: "{pct}"
        b64: "${{secret_b64}}"
    expect:
      - path: $.status
        equals: 999
  - http.request:
      method: GET
      url: "{base}/health"
finally:
  - http.request:
      method: GET
      url: "{base}/echo?final=${{beta}}"
"#,
        base = server.url(""),
        pct = support::percent_encode(SECRET.as_bytes()),
    );
    let case_path = dir.write("case.yaml", &case_yaml);
    let expected_sha = do_anything::core::sha256::hex(case_yaml.as_bytes());
    let bundles_dir = dir.path().join("bundles");
    let record_dir = dir.path().join("record");

    let out = run_da_with_env(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&case_path),
            "--vars-file",
            path_str(&vars_file),
            "--var",
            &format!("beta={SECRET}"),
            "--var",
            "gamma=non-env-cli-literal-val",
            "--bundle-dir",
            path_str(&bundles_dir),
            "--record",
            path_str(&record_dir),
        ],
        &[(ENV_NAME, SECRET)],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout_text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_no_secret_leaks(&stdout_text, SECRET, "--output json stdout");

    let json = json_stdout(&out);
    let case_json = &json["cases"][0];
    let steps = case_json["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 4);

    // Step 0: passed and saved `session_token`
    assert_eq!(steps[0]["args"]["method"], "POST");
    assert_eq!(steps[0]["args"]["json"]["user"], "alice");
    assert_eq!(steps[0]["saved"]["session_token"], "t-123");

    // Step 1: received `${session_token}` resolved + redacted secret in args and saved
    assert_eq!(steps[1]["args"]["json"]["token"], "t-123");
    assert_eq!(steps[1]["args"]["json"]["secret"], "***");
    assert_eq!(steps[1]["args"]["json"]["extra"], "***");
    let saved_url = steps[1]["saved"]["echoed_url"]
        .as_str()
        .expect("saved echoed_url must be a string");
    let saved_auth = steps[1]["saved"]["echoed_auth"]
        .as_str()
        .expect("saved echoed_auth must be a string");
    let saved_b64 = steps[1]["saved"]["echoed_b64"]
        .as_str()
        .expect("saved echoed_b64 must be a string");
    for (label, val) in [
        ("echoed_url", saved_url),
        ("echoed_auth", saved_auth),
        ("echoed_b64", saved_b64),
    ] {
        assert!(
            val.contains("***"),
            "saved `{label}` carrying encoded secret must be redacted: {val}"
        );
        assert_no_secret_leaks(val, SECRET, "StepReport.saved");
    }

    // Step 2: failed assertion, still carries resolved redacted args
    assert_eq!(steps[2]["status"], "failed");
    assert_eq!(steps[2]["args"]["json"]["pct"], "***");
    assert_eq!(steps[2]["args"]["json"]["b64"], "***");
    assert_eq!(steps[2]["saved"], serde_json::json!({}));

    // Step 3: skipped after failure, never executed -> args null, saved {}
    assert_eq!(steps[3]["status"], "skipped");
    assert_eq!(steps[3]["args"], Value::Null);
    assert_eq!(steps[3]["saved"], serde_json::json!({}));

    // Finally step: also records resolved redacted args
    let finally = case_json["finally"].as_array().expect("finally array");
    assert_eq!(finally.len(), 1);
    assert!(
        finally[0]["args"]["url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/echo?final=***")),
        "finally step args must be resolved and redacted: {}",
        finally[0]["args"]
    );

    // Check failure bundle and record bundle `meta.json` and `report.json`
    let bundle_path = PathBuf::from(case_json["bundle"].as_str().expect("bundle path"));
    let rec_case = record_dir.join("0-Multi-step-traceability");
    for root in [&bundle_path, &rec_case] {
        let meta_text = fs::read_to_string(root.join("meta.json")).expect("read meta.json");
        assert_no_secret_leaks(&meta_text, SECRET, "meta.json");
        assert!(
            !meta_text.contains("non-env-cli-literal-val"),
            "meta.json must record overridden variable names only, never values: {meta_text}"
        );
        let meta: Value = serde_json::from_str(&meta_text).expect("parse meta.json");
        assert_eq!(meta["case_sha256"], expected_sha);
        assert_eq!(
            meta["overridden_vars"],
            serde_json::json!(["alpha", "beta", "gamma", "secret_b64", "zeta"])
        );

        let report_text = fs::read_to_string(root.join("report.json")).expect("read report.json");
        assert_no_secret_leaks(&report_text, SECRET, "report.json");
        let report: Value = serde_json::from_str(&report_text).expect("parse report.json");
        assert_eq!(report["steps"], case_json["steps"]);
        assert_eq!(report["finally"], case_json["finally"]);
    }
}

#[cfg(feature = "cli")]
#[test]
fn meta_json_records_empty_overridden_vars_when_none_supplied() {
    let dir = TempDir::new("trace-meta-empty-vars");
    let case_yaml = format!(
        "name: No overrides\nsteps:\n  - cli.run:\n      command: '{}'\n      args: [\"--version\"]\n",
        da().replace('\\', "\\\\")
    );
    let case_path = dir.write("case.yaml", &case_yaml);
    let expected_sha = do_anything::core::sha256::hex(case_yaml.as_bytes());
    let record_dir = dir.path().join("record");

    let out = run_da_in(
        dir.path(),
        &[
            "run",
            path_str(&case_path),
            "--record",
            path_str(&record_dir),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let meta: Value = serde_json::from_str(
        &fs::read_to_string(record_dir.join("0-No-overrides/meta.json")).expect("read meta.json"),
    )
    .expect("parse meta.json");
    assert_eq!(meta["case_sha256"], expected_sha);
    assert_eq!(meta["overridden_vars"], serde_json::json!([]));
}

#[cfg(feature = "http")]
#[test]
fn da_session_steps_jsonl_records_timing_redacted_result_and_reads_legacy_records() {
    const SECRET: &str = "sess-s3cr3t-Val/42";
    const ENV_NAME: &str = "DA_SESS_AUDIT_SECRET";

    let server = support::TestServer::start();
    let dir = TempDir::new("trace-sess-audit");

    let start = run_da_with_env(dir.path(), &["session", "start"], &[(ENV_NAME, SECRET)]);
    assert_eq!(start.status.code(), Some(0));
    let id = json_stdout(&start)["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let step_yaml = format!(
        "http.request:\n  method: POST\n  url: \"{}/encoded-secret\"\n  json:\n    secret: \"${{env:{ENV_NAME}}}\"\nsave:\n  saved_url: $.body.url\n",
        server.url("")
    );
    let step1 = run_da_with_env(
        dir.path(),
        &["session", "step", &id, "--step", &step_yaml],
        &[(ENV_NAME, SECRET)],
    );
    assert_eq!(
        step1.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&step1.stdout)
    );

    // Also run a failing step (invalid args -> no result) to check error record keeps timing and null result
    let step2 = run_da_with_env(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "http.request",
            "--arg",
            "url=not-a-valid-url",
        ],
        &[(ENV_NAME, SECRET)],
    );
    assert_ne!(step2.status.code(), Some(0));

    let session_dir = dir.path().join(".da/sessions").join(&id);
    let steps_path = session_dir.join("steps.jsonl");
    let raw_jsonl = fs::read_to_string(&steps_path).expect("steps.jsonl must exist");
    assert_no_secret_leaks(&raw_jsonl, SECRET, "steps.jsonl");

    let lines: Vec<Value> = raw_jsonl
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid jsonl line"))
        .collect();
    assert_eq!(lines.len(), 2);
    assert!(
        lines[0]["started_at_ms"].as_u64().is_some_and(|t| t > 0),
        "started_at_ms missing: {}",
        lines[0]
    );
    assert!(
        lines[0]["duration_ms"].as_u64().is_some(),
        "duration_ms missing: {}",
        lines[0]
    );
    assert_eq!(lines[0]["result"]["status"], 200);
    assert!(
        lines[0]["result"]["body"]["url"]
            .as_str()
            .is_some_and(|u| u.contains("***")),
        "result must be redacted: {}",
        lines[0]
    );
    assert!(lines[1]["started_at_ms"].as_u64().is_some_and(|t| t > 0));
    assert_eq!(lines[1]["result"], Value::Null);

    // Append a legacy StepRecord line without started_at_ms, duration_ms, or result to verify backward compatibility.
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&steps_path)
        .expect("open steps.jsonl");
    writeln!(
        file,
        "{}",
        serde_json::json!({
            "index": 3,
            "action": "http.request",
            "raw_args": { "url": format!("{}/health", server.url("")) },
            "expect": [],
            "save": {},
            "optional": false,
            "status": "passed",
            "error": null
        })
    )
    .expect("append legacy line");
    drop(file);

    let loaded = do_anything::session::store::load_steps(&session_dir)
        .expect("load_steps must accept legacy StepRecord lines");
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[2].started_at_ms, 0);
    assert_eq!(loaded[2].duration_ms, 0);
    assert_eq!(loaded[2].result, None);

    let export = run_da_in(dir.path(), &["session", "export", &id]);
    assert_eq!(export.status.code(), Some(0));
    assert_eq!(json_stdout(&export)["exported_steps"], 2);

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0));
}

#[cfg(all(feature = "http", feature = "log"))]
#[test]
fn da_session_start_observe_log_file_collects_across_steps_and_redacts_secrets() {
    const SECRET: &str = "obs-s3cr3t-Val/77";
    const ENV_NAME: &str = "DA_SESS_OBS_SECRET";

    let server = support::TestServer::start();
    let dir = TempDir::new("trace-sess-observe");
    let log_path = dir.path().join("app.log");
    fs::write(&log_path, "preexisting startup line\n").expect("write initial log");

    // Bad syntax -> exit 2, no session directory created.
    let bad_syntax = run_da_in(dir.path(), &["session", "start", "--observe", "bad-spec"]);
    assert_eq!(bad_syntax.status.code(), Some(2));
    assert!(!dir.path().join(".da/sessions").exists());

    // Unknown observer -> exit 3, no session directory created.
    let unknown = run_da_in(
        dir.path(),
        &["session", "start", "--observe", "probe=no.such.observer"],
    );
    assert_eq!(unknown.status.code(), Some(3));
    assert!(
        !dir.path().join(".da/sessions").exists()
            || fs::read_dir(dir.path().join(".da/sessions"))
                .map(|mut d| d.next().is_none())
                .unwrap_or(true)
    );

    // Invalid observer arguments -> exit 3.
    let bad_args = run_da_in(
        dir.path(),
        &["session", "start", "--observe", "app_log=log.file:path="],
    );
    assert_eq!(bad_args.status.code(), Some(3));

    // Start valid session with attached log.file observer.
    let spec = format!("app_log=log.file:path={}", log_path.display());
    let start = run_da_with_env(
        dir.path(),
        &["session", "start", "--observe", &spec],
        &[(ENV_NAME, SECRET)],
    );
    assert_eq!(
        start.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&start.stdout)
    );
    let id = json_stdout(&start)["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    // Step 1 writes a line via /write-log and asserts on $.observed.app_log.lines while saving $.observed.app_log.text.
    let step1_yaml = format!(
        r#"http.request:
  method: POST
  url: "{base}/write-log"
  json:
    path: '{log}'
    text: "step-1-line\n"
    append: true
expect:
  - path: $.observed.app_log.lines
    equals: ["step-1-line"]
save:
  logged_one: $.observed.app_log.text
"#,
        base = server.url(""),
        log = log_path.display(),
    );
    let step1 = run_da_in(dir.path(), &["session", "step", &id, "--step", &step1_yaml]);
    assert_eq!(
        step1.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&step1.stdout)
    );
    let s1_json = json_stdout(&step1);
    assert_eq!(
        s1_json["step"]["result"]["observed"]["app_log"]["lines"],
        serde_json::json!(["step-1-line"])
    );

    // Step 2 writes raw + percent + base64 secret into the log file via server-side expansion;
    // observer output and steps.jsonl must redact all forms without embedding encoded literals in raw_args.
    let step2_yaml = format!(
        r#"http.request:
  method: POST
  url: "{base}/write-log"
  json:
    path: '{log}'
    text: "prev=${{logged_one}} "
    secret_forms: "${{env:{env}}}"
    append: true
expect:
  - path: $.observed.app_log.text
    contains: "prev=step-1-line"
"#,
        base = server.url(""),
        log = log_path.display(),
        env = ENV_NAME,
    );
    let step2 = run_da_with_env(
        dir.path(),
        &["session", "step", &id, "--step", &step2_yaml],
        &[(ENV_NAME, SECRET)],
    );
    assert_eq!(
        step2.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&step2.stdout)
    );
    let s2_stdout = String::from_utf8_lossy(&step2.stdout).into_owned();
    assert_no_secret_leaks(&s2_stdout, SECRET, "da session step stdout observed");
    let s2_json = json_stdout(&step2);
    let observed_text = s2_json["step"]["result"]["observed"]["app_log"]["text"]
        .as_str()
        .expect("observed text");
    assert!(
        observed_text.contains("raw=*** pct=*** b64=***"),
        "{observed_text}"
    );

    let session_dir = dir.path().join(".da/sessions").join(&id);
    let jsonl = fs::read_to_string(session_dir.join("steps.jsonl")).expect("read steps.jsonl");
    assert_no_secret_leaks(&jsonl, SECRET, "steps.jsonl observed");
    let state_json = fs::read_to_string(session_dir.join("state.json")).expect("read state.json");
    assert_no_secret_leaks(&state_json, SECRET, "state.json");

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0));
}

#[allow(dead_code)]
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[cfg(feature = "http")]
#[test]
fn trace_steps_without_record_is_rejected_with_usage_exit_code() {
    let server = support::TestServer::start();
    let dir = TempDir::new("trace-steps-no-record");
    let case_path = dir.write(
        "ok.yaml",
        &format!(
            "name: No record rejection\nsteps:\n  - http.request:\n      url: \"{}/health\"\n",
            server.url("")
        ),
    );
    let out = run_da_in(
        dir.path(),
        &["run", path_str(&case_path), "--trace-steps", "--no-bundle"],
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--trace-steps") && stderr.contains("--record"),
        "expected usage explanation naming both flags, got: {stderr}"
    );
    assert!(!dir.path().join(".da/failures").exists());
}
