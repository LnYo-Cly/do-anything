//! End-to-end CLI tests for `da session` with non-browser steps, locking,
//! secrets hygiene, idle reaping, and YAML export.

mod support;

#[cfg(feature = "http")]
use std::fs;
use std::path::Path;
#[cfg(feature = "http")]
use std::path::PathBuf;
#[cfg(feature = "http")]
use std::process::Stdio;
use std::process::{Command, Output};

use serde_json::Value;
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(da())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run the da binary")
}

#[cfg(feature = "http")]
fn run_da_stdin(dir: &Path, args: &[&str], stdin_text: &str) -> Output {
    use std::io::Write;
    let mut child = Command::new(da())
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn da");
    child
        .stdin
        .as_mut()
        .expect("stdin pipe")
        .write_all(stdin_text.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for da")
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout must be one JSON object ({error})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[cfg(feature = "http")]
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if !dir.exists() {
        return;
    }
    for entry in fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn start_list_and_stop_work_even_without_driver_features() {
    let dir = TempDir::new("session-basic");

    let start = run_da_in(dir.path(), &["session", "start"]);
    assert_eq!(start.status.code(), Some(0), "{:?}", start);
    let started = json_stdout(&start);
    assert_eq!(started["ok"], true);
    let id = started["session_id"]
        .as_str()
        .expect("session_id must be a string")
        .to_string();
    let session_dir = dir
        .path()
        .join(started["dir"].as_str().expect("dir must be a string"));
    assert!(session_dir.exists(), "session directory must exist");

    let list = run_da_in(dir.path(), &["session", "list"]);
    assert_eq!(list.status.code(), Some(0));
    let listed = json_stdout(&list);
    let sessions = listed["sessions"].as_array().expect("sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], id);

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0));
    let stopped = json_stdout(&stop);
    assert_eq!(stopped["ok"], true);
    assert!(!session_dir.exists(), "stop must remove session directory");

    let list_after = run_da_in(dir.path(), &["session", "list"]);
    assert_eq!(
        json_stdout(&list_after)["sessions"]
            .as_array()
            .expect("sessions")
            .len(),
        0
    );
}

#[test]
fn idle_timeout_zero_reaps_sessions_on_list_and_start() {
    let dir = TempDir::new("session-reap");

    let s1 = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id1 = s1["session_id"].as_str().unwrap().to_string();
    let dir1 = dir.path().join(s1["dir"].as_str().unwrap());
    assert!(dir1.exists());

    let list = run_da_in(dir.path(), &["session", "list", "--idle-timeout-ms", "0"]);
    assert_eq!(list.status.code(), Some(0));
    let listed = json_stdout(&list);
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 0);
    assert!(
        listed["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &id1),
        "reaped list must include {id1}: {listed}"
    );
    assert!(!dir1.exists(), "reaped session directory must be deleted");

    let s2 = json_stdout(&run_da_in(
        dir.path(),
        &["session", "start", "--idle-timeout-ms", "0"],
    ));
    let id2 = s2["session_id"].as_str().unwrap().to_string();
    let dir2 = dir.path().join(s2["dir"].as_str().unwrap());
    assert!(dir2.exists());

    let s3 = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    assert!(
        s3["reaped"].as_array().unwrap().iter().any(|v| v == &id2),
        "start must reap expired session {id2}: {s3}"
    );
    assert!(!dir2.exists());
}

#[cfg(feature = "http")]
#[test]
fn session_steps_preserve_saved_vars_secrets_and_export_replayable_case() {
    let server = support::TestServer::start();
    let dir = TempDir::new("session-http-export");
    let secret = "s3cr3t-session-token-999";

    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    // Step 1 via stdin envelope format with ${env:...} secret and save.
    let step1_input = serde_json::json!({
        "use": "http.request",
        "with": {
            "method": "POST",
            "url": server.url("/echo"),
            "headers": { "X-Token": "${env:DA_SESSION_TEST_SECRET}" },
            "body": "hello"
        },
        "expect": [{ "path": "$.status", "equals": 200 }],
        "save": { "echo_path": "$.body" }
    })
    .to_string();

    let mut cmd1 = Command::new(da());
    cmd1.current_dir(dir.path())
        .args(["session", "step", &id])
        .env("DA_SESSION_TEST_SECRET", secret)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd1.spawn().expect("spawn step 1");
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(step1_input.as_bytes())
            .unwrap();
    }
    let out1 = child.wait_with_output().unwrap();
    assert_eq!(out1.status.code(), Some(0), "{:?}", out1);
    let res1 = json_stdout(&out1);
    assert_eq!(res1["ok"], true);
    assert_eq!(res1["step"]["status"], "passed");
    assert!(
        !String::from_utf8_lossy(&out1.stdout).contains(secret),
        "step stdout must redact secret"
    );

    // Step 2 (failing step) should NOT be exported, and should be listed in skipped_steps.
    let step2_input = format!(
        "http.request:\n  method: GET\n  url: {}\nexpect:\n  - path: $.status\n    equals: 200\n",
        server.url("/status/500")
    );
    let out2 = run_da_stdin(dir.path(), &["session", "step", &id], &step2_input);
    assert_eq!(out2.status.code(), Some(1));
    let res2 = json_stdout(&out2);
    assert_eq!(res2["ok"], false);
    assert_eq!(res2["step"]["status"], "failed");

    // Step 3 uses --arg CLI flags and saved variable from Step 1.
    let out3 = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "http.request",
            "--arg",
            "method=GET",
            "--arg",
            &format!("url={}", server.url("/text")),
            "--save",
            "body_text=$.body",
        ],
    );
    assert_eq!(out3.status.code(), Some(0), "{:?}", out3);

    // Verify NO file in session_dir contains the secret value!
    let mut files = Vec::new();
    collect_files(&session_dir, &mut files);
    assert!(!files.is_empty());
    for file in &files {
        let content = fs::read_to_string(file).expect("session file readable as UTF-8");
        assert!(
            !content.contains(secret),
            "session file {} leaked secret value!",
            file.display()
        );
    }

    // Export to case.yaml
    let exported_path = dir.path().join("exported.yaml");
    let export_out = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported_path.to_str().unwrap(),
            "--name",
            "Exported API flow",
        ],
    );
    assert_eq!(export_out.status.code(), Some(0), "{:?}", export_out);
    let export_json = json_stdout(&export_out);
    assert_eq!(export_json["exported_steps"], 2);
    assert_eq!(export_json["skipped_steps"].as_array().unwrap().len(), 1);

    let yaml_text = fs::read_to_string(&exported_path).expect("exported yaml must exist");
    assert!(
        yaml_text.contains("${env:DA_SESSION_TEST_SECRET}"),
        "exported YAML must keep ${{env:...}} reference: {yaml_text}"
    );
    assert!(!yaml_text.contains(secret));

    // Lint and replay exported case with DA_SESSION_TEST_SECRET set.
    let lint = Command::new(da())
        .current_dir(dir.path())
        .args(["lint", exported_path.to_str().unwrap()])
        .env("DA_SESSION_TEST_SECRET", secret)
        .output()
        .expect("lint run");
    assert_eq!(
        lint.status.code(),
        Some(0),
        "exported YAML must pass da lint: {}",
        String::from_utf8_lossy(&lint.stdout)
    );

    let replay = Command::new(da())
        .current_dir(dir.path())
        .args(["run", exported_path.to_str().unwrap(), "--no-bundle"])
        .env("DA_SESSION_TEST_SECRET", secret)
        .output()
        .expect("replay run");
    assert_eq!(
        replay.status.code(),
        Some(0),
        "exported YAML must replay cleanly: {}",
        String::from_utf8_lossy(&replay.stdout)
    );

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0));
}

#[cfg(feature = "http")]
#[test]
fn concurrent_step_on_same_session_is_rejected_without_interleaving() {
    let server = support::TestServer::start();
    let dir = TempDir::new("session-concurrent");
    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();

    // Start a slow step (/sleep takes 1000 ms).
    let step_json = serde_json::json!({
        "http.request": {
            "method": "GET",
            "url": server.url("/sleep")
        }
    })
    .to_string();
    let slow = Command::new(da())
        .current_dir(dir.path())
        .args(["session", "step", &id, "--step", &step_json])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn slow step");

    // Wait deterministically until the first process reaches /sleep and holds the session lock.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while server.request_count() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "first process did not reach /sleep in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let fast = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "http.request",
            "--arg",
            "method=GET",
            "--arg",
            &format!("url={}", server.url("/text")),
        ],
    );
    assert_ne!(
        fast.status.code(),
        Some(0),
        "concurrent step must be rejected while first step holds lock"
    );
    let fast_json = json_stdout(&fast);
    assert_eq!(fast_json["ok"], false);
    assert!(
        fast_json["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("busy") || m.contains("another command")),
        "got: {fast_json}"
    );

    let slow_out = slow.wait_with_output().expect("wait for slow step");
    assert_eq!(slow_out.status.code(), Some(0));

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0));
}

#[cfg(feature = "http")]
#[test]
fn optional_step_skips_and_exports_with_optional_modifier() {
    let server = support::TestServer::start();
    let dir = TempDir::new("session-optional");

    let start = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = start["session_id"].as_str().unwrap().to_string();

    let step_yaml = format!(
        "http.request:\n  method: GET\n  url: \"{}\"\noptional: true\nexpect:\n  - path: \"$.status\"\n    equals: 999\n",
        server.url("/text")
    );
    let out = run_da_stdin(dir.path(), &["session", "step", &id, "-f", "-"], &step_yaml);
    assert_eq!(out.status.code(), Some(0), "{:?}", out);
    let step_json = json_stdout(&out);
    assert_eq!(step_json["ok"], true);
    assert_eq!(step_json["step"]["status"], "skipped");

    let exported = dir.path().join("optional.yaml");
    let exp = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported.to_str().unwrap(),
            "--name",
            "Optional export",
        ],
    );
    assert_eq!(exp.status.code(), Some(0), "{:?}", exp);
    let exp_json = json_stdout(&exp);
    assert_eq!(exp_json["exported_steps"], 1);
    let yaml = fs::read_to_string(&exported).unwrap();
    assert!(yaml.contains("optional: true"), "got: {yaml}");

    assert_eq!(
        run_da_in(dir.path(), &["lint", exported.to_str().unwrap()])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        run_da_in(
            dir.path(),
            &["run", exported.to_str().unwrap(), "--no-bundle"]
        )
        .status
        .code(),
        Some(0)
    );

    assert_eq!(
        run_da_in(dir.path(), &["session", "stop", &id])
            .status
            .code(),
        Some(0)
    );
}
