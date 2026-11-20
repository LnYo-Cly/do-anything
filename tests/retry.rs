//! End-to-end tests for read-only step `retry` polling and static rejection
//! of `retry` on state-changing steps.

// Each feature combination compiles a different subset of the helpers below.
#![allow(dead_code)]

mod support;

use std::path::Path;
use std::process::{Command, Output};
#[cfg(any(feature = "http", feature = "db"))]
use std::time::Duration;

use serde_json::Value;
use support::TempDir;
#[cfg(any(feature = "http", feature = "browser"))]
use support::TestServer;

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da(args: &[&str]) -> Output {
    let work = TempDir::new("retry-cwd");
    run_da_in(work.path(), args)
}

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

/// Asserts that `da lint` and `da run` both refuse `retry` on one case, and that
/// the refusal names `retry` instead of running the step.
#[cfg(any(feature = "http", feature = "cli", feature = "browser"))]
fn assert_retry_rejected(dir: &TempDir, name: &str, yaml: &str) {
    let case = dir.write(&format!("{name}.yaml"), yaml);

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(
        exit_code(&lint),
        Some(3),
        "{name}: lint must refuse retry, stdout: {:?}",
        stdout(&lint)
    );
    assert!(
        stdout(&lint).contains("cannot use `retry`"),
        "{name}: lint output must explain why retry was refused, got: {:?}",
        stdout(&lint)
    );

    let run = run_da(&["run", path_str(&case), "--no-bundle"]);
    assert_eq!(
        exit_code(&run),
        Some(3),
        "{name}: run must refuse retry before any step runs, stderr: {:?}",
        stderr(&run)
    );
}

#[cfg(feature = "http")]
#[test]
fn rejects_retry_on_state_changing_http_methods_before_running() {
    for method in ["POST", "PUT", "PATCH", "DELETE", "get", "head"] {
        let server = TestServer::start();
        let dir = TempDir::new("retry-http-write");
        assert_retry_rejected(
            &dir,
            method,
            &format!(
                "name: Reject {method} retry\nsteps:\n  - http.request:\n      method: {method}\n      url: {base}/poll\n    retry:\n      until:\n        - path: $.status\n          equals: 200\n      timeout_ms: 500\n",
                base = server.url("")
            ),
        );
        assert_eq!(
            server.request_count(),
            0,
            "{method}: target server must receive 0 requests when retry is rejected"
        );
    }
}

#[cfg(feature = "http")]
#[test]
fn rejects_retry_when_http_method_comes_from_a_variable() {
    let server = TestServer::start();
    let dir = TempDir::new("retry-http-var-method");
    let case = dir.write(
        "var-method.yaml",
        &format!(
            "name: Reject variable POST retry\nvars:\n  method: POST\nsteps:\n  - http.request:\n      method: \"${{method}}\"\n      url: {base}/poll\n    retry:\n      until:\n        - path: $.status\n          equals: 200\n      timeout_ms: 500\n",
            base = server.url("")
        ),
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(3), "stdout: {:?}", stdout(&lint));
    assert!(stdout(&lint).contains("POST"), "got: {:?}", stdout(&lint));
    assert!(
        stdout(&lint).contains("cannot use `retry`"),
        "got: {:?}",
        stdout(&lint)
    );

    let run = run_da(&["run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&run), Some(3), "stderr: {:?}", stderr(&run));
    assert_eq!(server.request_count(), 0);
}

#[cfg(feature = "cli")]
#[test]
fn rejects_retry_on_cli_run_before_running() {
    let dir = TempDir::new("retry-cli-reject");
    let marker = dir.path().join("ran.txt");
    let case = dir.write(
        "cli-retry.yaml",
        &format!(
            "name: Reject cli retry\nsteps:\n  - cli.run:\n      command: '{}'\n      args: [\"--version\"]\n    retry:\n      until:\n        - path: $.exit_code\n          equals: 0\n      timeout_ms: 500\n",
            da()
        ),
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(3), "stdout: {:?}", stdout(&lint));
    assert!(
        stdout(&lint).contains("cannot use `retry`"),
        "got: {:?}",
        stdout(&lint)
    );

    let run = run_da(&["run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&run), Some(3), "stderr: {:?}", stderr(&run));
    assert!(!marker.exists());
}

#[cfg(feature = "browser")]
#[test]
fn rejects_retry_on_state_changing_browser_steps_and_accepts_read_and_wait() {
    let server = TestServer::start();
    let dir = TempDir::new("retry-browser-lint");

    let mut rejected = Vec::new();
    rejected.push((
        "browser.open",
        format!(
            "name: Open\nsteps:\n  - browser.open:\n      url: {}/health\n    retry:\n      until:\n        - path: $.url\n          exists: true\n      timeout_ms: 500\n",
            server.url("")
        ),
    ));
    rejected.push((
        "browser.click",
        "name: Click\nsteps:\n  - browser.click:\n      css: \"#btn\"\n    retry:\n      until:\n        - path: $.text\n          equals: ok\n      timeout_ms: 500\n".to_string(),
    ));
    rejected.push((
        "browser.type",
        "name: Type\nsteps:\n  - browser.type:\n      css: \"#in\"\n      text: hi\n    retry:\n      until:\n        - path: $.text\n          exists: false\n      timeout_ms: 500\n".to_string(),
    ));
    rejected.push((
        "browser.upload",
        "name: Upload\nsteps:\n  - browser.upload:\n      css: \"#file\"\n      files: [\"a.txt\"]\n    retry:\n      until:\n        - path: $.files\n          equals: 1\n      timeout_ms: 500\n".to_string(),
    ));
    rejected.push((
        "browser.press",
        "name: Press\nsteps:\n  - browser.press:\n      key: Enter\n    retry:\n      until:\n        - path: $.key\n          equals: Enter\n      timeout_ms: 500\n".to_string(),
    ));
    rejected.push((
        "browser.screenshot",
        "name: Shot\nsteps:\n  - browser.screenshot:\n      path: shot.png\n    retry:\n      until:\n        - path: $.bytes\n          exists: true\n      timeout_ms: 500\n".to_string(),
    ));
    rejected.push((
        "browser.set_cookies",
        "name: Cookies\nsteps:\n  - browser.set_cookies:\n      cookies:\n        - name: sid\n          value: tok\n          url: http://127.0.0.1/\n    retry:\n      until:\n        - path: $.cookies_set\n          equals: 1\n      timeout_ms: 500\n".to_string(),
    ));
    rejected.push((
        "browser.save_state",
        "name: Save\nsteps:\n  - browser.save_state:\n      path: state.json\n    retry:\n      until:\n        - path: $.path\n          exists: true\n      timeout_ms: 500\n".to_string(),
    ));
    rejected.push((
        "browser.switch_tab",
        "name: Switch tab\nsteps:\n  - browser.switch_tab: {}\n    retry:\n      until:\n        - path: $.title\n          exists: true\n      timeout_ms: 500\n".to_string(),
    ));

    for (name, yaml) in rejected {
        assert_retry_rejected(&dir, name, &yaml);
    }
    assert_eq!(
        server.request_count(),
        0,
        "browser.open must never contact the server when retry is rejected"
    );

    let read_case = dir.write(
        "browser-read.yaml",
        "name: Read retry\nsteps:\n  - browser.read:\n      css: \"#status\"\n    retry:\n      until:\n        - path: $.text\n          equals: done\n      timeout_ms: 500\n",
    );
    let read_lint = run_da(&["lint", path_str(&read_case)]);
    assert_eq!(
        exit_code(&read_lint),
        Some(0),
        "browser.read retry must pass lint, stdout: {:?}",
        stdout(&read_lint)
    );

    let wait_case = dir.write(
        "browser-wait.yaml",
        "name: Wait retry\nsteps:\n  - browser.wait:\n      css: \"#status\"\n    retry:\n      until:\n        - path: $.url\n          contains: done\n      timeout_ms: 500\n",
    );
    let wait_lint = run_da(&["lint", path_str(&wait_case)]);
    assert_eq!(
        exit_code(&wait_lint),
        Some(0),
        "browser.wait retry must pass lint, stdout: {:?}",
        stdout(&wait_lint)
    );
}

#[cfg(feature = "browser")]
#[test]
fn browser_retry_lint_respects_dialog_rules() {
    let server = TestServer::start();
    let dir = TempDir::new("retry-browser-dialog-lint");

    for (name, yaml) in [
        (
            "browser.open",
            format!(
                "name: Open with dialog and retry\nsteps:\n  - browser.open:\n      url: {}/health\n      dialog: accept\n    retry:\n      until:\n        - path: $.url\n          exists: true\n      timeout_ms: 500\n",
                server.url("")
            ),
        ),
        (
            "browser.click",
            "name: Click with dialog and retry\nsteps:\n  - browser.click:\n      css: \"#btn\"\n      dialog: accept\n    retry:\n      until:\n        - path: $.dialog.accepted\n          equals: true\n      timeout_ms: 500\n".to_string(),
        ),
        (
            "browser.type",
            "name: Type with dialog and retry\nsteps:\n  - browser.type:\n      css: \"#in\"\n      text: hi\n      dialog: dismiss\n    retry:\n      until:\n        - path: $.dialog.type\n          equals: confirm\n      timeout_ms: 500\n".to_string(),
        ),
        (
            "browser.press",
            "name: Press with dialog and retry\nsteps:\n  - browser.press:\n      key: Enter\n      dialog:\n        action: accept\n        prompt_text: ok\n    retry:\n      until:\n        - path: $.dialog.accepted\n          equals: true\n      timeout_ms: 500\n".to_string(),
        ),
    ] {
        assert_retry_rejected(&dir, name, &yaml);
    }
    assert_eq!(
        server.request_count(),
        0,
        "browser.open with dialog+retry must never contact the server"
    );

    let read_with_dialog = dir.write(
        "browser-read-dialog.yaml",
        "name: Read with dialog and retry\nsteps:\n  - browser.read:\n      css: \"#status\"\n      dialog: accept\n    retry:\n      until:\n        - path: $.text\n          equals: done\n      timeout_ms: 500\n",
    );
    let read_lint = run_da(&["lint", path_str(&read_with_dialog)]);
    assert_eq!(
        exit_code(&read_lint),
        Some(3),
        "browser.read does not accept dialog, stdout: {:?}",
        stdout(&read_lint)
    );
    assert!(
        stdout(&read_lint).contains("dialog"),
        "expected unknown field `dialog` error, got: {:?}",
        stdout(&read_lint)
    );
    let read_run = run_da(&["run", path_str(&read_with_dialog), "--no-bundle"]);
    assert_eq!(
        exit_code(&read_run),
        Some(3),
        "browser.read with dialog must fail preflight with exit 3"
    );

    for (file, yaml) in [
        (
            "browser-wait-dialog-string.yaml",
            "name: Wait retry with string dialog\nsteps:\n  - browser.wait:\n      css: \"#status\"\n      dialog: accept\n    retry:\n      until:\n        - path: $.dialog.type\n          equals: confirm\n      timeout_ms: 500\n",
        ),
        (
            "browser-wait-dialog-object.yaml",
            "name: Wait retry with prompt dialog\nsteps:\n  - browser.wait:\n      url_contains: /ready\n      dialog:\n        action: accept\n        prompt_text: token\n    retry:\n      until:\n        - path: $.url\n          contains: /ready\n      timeout_ms: 500\n",
        ),
    ] {
        let case = dir.write(file, yaml);
        let lint = run_da(&["lint", path_str(&case)]);
        assert_eq!(
            exit_code(&lint),
            Some(0),
            "{file}: browser.wait with dialog and retry must pass lint, stdout: {:?}",
            stdout(&lint)
        );
    }
}

#[cfg(feature = "http")]
#[test]
fn http_get_and_head_poll_until_ready_then_evaluate_expect_and_save() {
    let server = TestServer::start();
    let dir = TempDir::new("retry-http-pass");
    let case = dir.write(
        "poll-pass.yaml",
        &format!(
            r#"
name: Poll GET and HEAD
steps:
  - http.request:
      url: {base}/poll?ready_at=3
    retry:
      until:
        - path: $.body.ready
          equals: true
      timeout_ms: 2000
      interval_ms: 20
    expect:
      - path: $.status
        equals: 200
      - path: $.body.state
        equals: done
    save:
      attempt: $.body.attempt
  - http.request:
      method: HEAD
      url: {base}/poll?ready_at=5
    retry:
      until:
        - path: $.status
          equals: 200
      timeout_ms: 2000
      interval_ms: 20
  - http.request:
      url: {base}/echo?attempt=${{attempt}}
    expect:
      - path: $.body.query
        equals: "attempt=3"
"#,
            base = server.url("")
        ),
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(0), "stdout: {:?}", stdout(&lint));

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));

    let value = json_stdout(&output);
    assert_eq!(value["ok"], true);
    let step0 = &value["cases"][0]["steps"][0];
    assert_eq!(step0["status"], "passed");
    assert_eq!(step0["result"]["body"]["attempt"], 3);
    assert_eq!(step0["assertions"].as_array().map(Vec::len), Some(3));
    assert_eq!(value["cases"][0]["steps"][1]["status"], "passed");
    assert_eq!(value["cases"][0]["steps"][2]["status"], "passed");
    assert_eq!(server.request_count(), 6);
}

#[cfg(feature = "http")]
#[test]
fn http_get_reports_last_attempt_failure_when_retry_budget_expires() {
    let server = TestServer::start();
    let dir = TempDir::new("retry-http-timeout");
    let case = dir.write(
        "poll-fail.yaml",
        &format!(
            r#"
name: Poll expires
steps:
  - http.request:
      url: {base}/poll?ready_at=999
    retry:
      until:
        - path: $.body.ready
          equals: true
      timeout_ms: 500
      interval_ms: 50
"#,
            base = server.url("")
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&output), Some(1), "stderr: {:?}", stderr(&output));

    let value = json_stdout(&output);
    assert_eq!(value["ok"], false);
    let step = &value["cases"][0]["steps"][0];
    assert_eq!(step["status"], "failed");
    assert_eq!(step["error"]["kind"], "assertion");
    assert_eq!(step["assertions"][0]["passed"], false);
    assert_eq!(step["result"]["body"]["ready"], false);
    let attempts = server.request_count();
    assert!(
        attempts >= 2,
        "expected multiple polling attempts before deadline, got {attempts}"
    );
    assert_eq!(step["result"]["body"]["attempt"], attempts);
}

#[cfg(feature = "http")]
#[test]
fn retry_aborts_immediately_on_environment_error_with_exit_four() {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("probe listener must bind");
    let port = listener
        .local_addr()
        .expect("listener address must exist")
        .port();
    listener
        .set_nonblocking(true)
        .expect("listener must be non-blocking");

    let stop = Arc::new(AtomicBool::new(false));
    let attempts = Arc::new(AtomicUsize::new(0));
    let server_stop = Arc::clone(&stop);
    let server_attempts = Arc::clone(&attempts);
    let handle = std::thread::spawn(move || {
        while !server_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    server_attempts.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let dir = TempDir::new("retry-env-abort");
    let case = dir.write(
        "env-error.yaml",
        &format!(
            r#"
name: Environment error aborts retry
steps:
  - http.request:
      url: http://127.0.0.1:{port}/poll
    retry:
      until:
        - path: $.status
          equals: 200
      timeout_ms: 5000
      interval_ms: 50
"#,
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();

    assert_eq!(exit_code(&output), Some(4), "stderr: {:?}", stderr(&output));
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "environment error must abort after the first attempt"
    );
    let value = json_stdout(&output);
    let step_ms = value["cases"][0]["steps"][0]["duration_ms"]
        .as_u64()
        .expect("step duration_ms must be present");
    assert!(
        step_ms < 3000,
        "environment error must abort immediately instead of polling until 5000ms, step duration_ms={step_ms}"
    );
    assert_eq!(value["cases"][0]["steps"][0]["status"], "error");
    assert_eq!(
        value["cases"][0]["steps"][0]["error"]["kind"],
        "environment"
    );
}

#[cfg(feature = "http")]
#[test]
fn retry_with_optional_true_skips_when_until_exhausts_budget() {
    let server = TestServer::start();
    let dir = TempDir::new("retry-optional");
    let case = dir.write(
        "retry-optional.yaml",
        &format!(
            r#"
name: Optional retry
steps:
  - http.request:
      url: {base}/poll?ready_at=999
    optional: true
    retry:
      until:
        - path: $.body.ready
          equals: true
      timeout_ms: 80
      interval_ms: 20
  - http.request:
      url: {base}/health
    expect:
      - path: $.status
        equals: 200
"#,
            base = server.url("")
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));
    let value = json_stdout(&output);
    assert_eq!(value["cases"][0]["status"], "passed");
    assert_eq!(value["cases"][0]["steps"][0]["status"], "skipped");
    assert_eq!(value["cases"][0]["steps"][0]["error"]["kind"], "assertion");
    assert_eq!(value["cases"][0]["steps"][1]["status"], "passed");
}

#[cfg(feature = "http")]
#[test]
fn retry_stops_polling_once_until_passes_even_if_expect_fails() {
    let server = TestServer::start();
    let dir = TempDir::new("retry-expect-fail");
    let case = dir.write(
        "until-pass-expect-fail.yaml",
        &format!(
            r#"
name: Expect fails after until passes
steps:
  - http.request:
      url: {base}/poll?ready_at=2
    retry:
      until:
        - path: $.body.ready
          equals: true
      timeout_ms: 2000
      interval_ms: 20
    expect:
      - path: $.status
        equals: 201
"#,
            base = server.url("")
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    assert_eq!(exit_code(&output), Some(1), "stderr: {:?}", stderr(&output));
    assert_eq!(
        server.request_count(),
        2,
        "polling must stop immediately after attempt 2 satisfies `until`"
    );
    let value = json_stdout(&output);
    let step = &value["cases"][0]["steps"][0];
    assert_eq!(step["status"], "failed");
    assert_eq!(step["assertions"][0]["passed"], true);
    assert_eq!(step["assertions"][1]["passed"], false);
}

#[cfg(feature = "db")]
#[test]
fn db_query_polls_sqlite_until_row_becomes_ready() {
    let dir = TempDir::new("retry-db-poll");
    let db_path = dir.path().join("jobs.db");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("sqlite fixture must open");
        conn.execute_batch(
            "CREATE TABLE jobs (id INTEGER PRIMARY KEY, status TEXT);
             INSERT INTO jobs (id, status) VALUES (1, 'pending');",
        )
        .expect("sqlite fixture must initialize");
    }

    let writer_path = db_path.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        let conn = rusqlite::Connection::open(&writer_path).expect("writer connection must open");
        conn.execute("UPDATE jobs SET status = 'done' WHERE id = 1", [])
            .expect("writer update must succeed");
    });

    let case = dir.write(
        "db-poll.yaml",
        &format!(
            r#"
name: Poll database
steps:
  - db.query:
      url: "sqlite:{db}"
      sql: SELECT status FROM jobs WHERE id = 1
    retry:
      until:
        - path: $.rows[0].status
          equals: done
      timeout_ms: 2000
      interval_ms: 25
    expect:
      - path: $.row_count
        equals: 1
    save:
      final_status: $.rows[0].status
  - db.query:
      url: "sqlite:{db}"
      sql: SELECT status FROM jobs WHERE status = ?1
      params: ["${{final_status}}"]
    expect:
      - path: $.row_count
        equals: 1
"#,
            db = path_str(&db_path).replace('\\', "/")
        ),
    );

    let lint = run_da(&["lint", path_str(&case)]);
    assert_eq!(exit_code(&lint), Some(0), "stdout: {:?}", stdout(&lint));

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    writer.join().expect("writer thread must succeed");

    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));
    let value = json_stdout(&output);
    assert_eq!(value["ok"], true);
    assert_eq!(value["cases"][0]["steps"][0]["status"], "passed");
    assert_eq!(
        value["cases"][0]["steps"][0]["result"]["rows"][0]["status"],
        "done"
    );
    assert_eq!(value["cases"][0]["steps"][1]["status"], "passed");
}

#[cfg(all(feature = "http", feature = "log"))]
#[test]
fn retry_isolates_log_observer_lines_to_the_winning_attempt() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    let dir = TempDir::new("retry-log-observer");
    let log_path = dir.path().join("app.log");
    std::fs::write(&log_path, "startup\n").expect("seed log file must succeed");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind poll-log listener");
    let port = listener.local_addr().expect("listener addr").port();
    let server_log = log_path.clone();
    let server = std::thread::spawn(move || {
        for attempt in 1..=2 {
            let (mut stream, _) = listener.accept().expect("accept poll request");
            let mut req = [0_u8; 1024];
            let _ = stream.read(&mut req);
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&server_log)
                .expect("open log file");
            writeln!(file, "attempt-{attempt}").expect("append log line");
            let body = format!(r#"{{"ready":{},"attempt":{attempt}}}"#, attempt == 2);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write HTTP response");
        }
    });

    let case = dir.write(
        "retry-log.yaml",
        &format!(
            r#"
name: Retry isolates log observer
observe:
  app_log:
    log.file:
      path: '{log}'
steps:
  - http.request:
      url: http://127.0.0.1:{port}/status
    retry:
      until:
        - path: $.body.ready
          equals: true
      timeout_ms: 2000
      interval_ms: 20
    expect:
      - path: $.observed.app_log.lines
        equals: ["attempt-2"]
"#,
            log = path_str(&log_path).replace('\\', "/")
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    server.join().expect("poll-log server must finish");

    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));
    let value = json_stdout(&output);
    assert_eq!(value["ok"], true);
    assert_eq!(
        value["cases"][0]["steps"][0]["result"]["observed"]["app_log"]["lines"],
        serde_json::json!(["attempt-2"])
    );
}

#[cfg(feature = "http")]
#[test]
fn retry_succeeds_within_budget_and_fails_when_interval_exceeds_remaining_budget() {
    let within_budget = TestServer::start();
    let dir = TempDir::new("retry-budget-boundary");
    let pass_case = dir.write(
        "within-budget.yaml",
        &format!(
            r#"
name: Poll succeeds within budget
steps:
  - http.request:
      url: {base}/poll?ready_at=2
    retry:
      until:
        - path: $.body.ready
          equals: true
      timeout_ms: 2000
      interval_ms: 20
"#,
            base = within_budget.url("")
        ),
    );
    let pass_output = run_da(&[
        "--output",
        "json",
        "run",
        path_str(&pass_case),
        "--no-bundle",
    ]);
    assert_eq!(
        exit_code(&pass_output),
        Some(0),
        "stderr: {:?}",
        stderr(&pass_output)
    );
    let pass_json = json_stdout(&pass_output);
    assert_eq!(pass_json["ok"], true);
    assert_eq!(pass_json["cases"][0]["steps"][0]["status"], "passed");
    assert_eq!(
        pass_json["cases"][0]["steps"][0]["result"]["body"]["attempt"],
        2
    );
    assert_eq!(within_budget.request_count(), 2);

    let over_interval = TestServer::start();
    let fail_case = dir.write(
        "interval-exceeds-budget.yaml",
        &format!(
            r#"
name: Poll stops when interval exceeds remaining budget
steps:
  - http.request:
      url: {base}/poll?ready_at=2
    retry:
      until:
        - path: $.body.ready
          equals: true
      timeout_ms: 150
      interval_ms: 1000
"#,
            base = over_interval.url("")
        ),
    );
    let fail_output = run_da(&[
        "--output",
        "json",
        "run",
        path_str(&fail_case),
        "--no-bundle",
    ]);
    assert_eq!(
        exit_code(&fail_output),
        Some(1),
        "must fail when interval_ms exceeds remaining budget: stdout={:?} stderr={:?}",
        stdout(&fail_output),
        stderr(&fail_output)
    );
    assert_eq!(
        over_interval.request_count(),
        1,
        "must not start a second attempt when interval_ms exceeds remaining budget"
    );
    let fail_json = json_stdout(&fail_output);
    assert_eq!(fail_json["ok"], false);
    let step = &fail_json["cases"][0]["steps"][0];
    assert_eq!(step["status"], "failed");
    assert_eq!(step["error"]["kind"], "assertion");
    assert_eq!(step["assertions"][0]["passed"], false);
    assert_eq!(step["result"]["body"]["ready"], false);
    assert_eq!(step["result"]["body"]["attempt"], 1);
}

#[cfg(all(feature = "http", feature = "log"))]
#[test]
fn retry_interval_longer_than_budget_preserves_observer_isolation() {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;

    let dir = TempDir::new("retry-interval-observer");
    let log_path = dir.path().join("app.log");
    std::fs::write(&log_path, "startup\n").expect("seed log file must succeed");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind listener");
    let port = listener.local_addr().expect("listener addr").port();
    let server_log = log_path.clone();
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_requests = std::sync::Arc::clone(&requests);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_stop = std::sync::Arc::clone(&stop);
    listener
        .set_nonblocking(true)
        .expect("listener must be non-blocking");

    let server = std::thread::spawn(move || {
        while !server_stop.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut req_line = String::new();
                    {
                        let mut reader = BufReader::new(&stream);
                        let _ = reader.read_line(&mut req_line);
                    }
                    let (status, body) = if req_line.starts_with("GET /poll ") {
                        let attempt =
                            server_requests.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        let mut file = std::fs::OpenOptions::new()
                            .append(true)
                            .open(&server_log)
                            .expect("open log file");
                        writeln!(file, "poll-{attempt}").expect("append log line");
                        (
                            "202 Accepted",
                            format!(r#"{{"ready":{},"attempt":{attempt}}}"#, attempt >= 2),
                        )
                    } else {
                        let mut file = std::fs::OpenOptions::new()
                            .append(true)
                            .open(&server_log)
                            .expect("open log file");
                        writeln!(file, "after-step").expect("append log line");
                        ("200 OK", r#"{"ok":true}"#.to_string())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    let case = dir.write(
        "retry-interval-log.yaml",
        &format!(
            r#"
name: Interval exceeding budget isolates observer
observe:
  app_log:
    log.file:
      path: '{log}'
steps:
  - http.request:
      url: http://127.0.0.1:{port}/poll
    optional: true
    retry:
      until:
        - path: $.status
          equals: 200
      timeout_ms: 150
      interval_ms: 1000
    expect:
      - path: $.observed.app_log.lines
        equals: ["poll-1"]
  - http.request:
      url: http://127.0.0.1:{port}/after
    expect:
      - path: $.observed.app_log.lines
        equals: ["after-step"]
"#,
            log = path_str(&log_path).replace('\\', "/")
        ),
    );

    let output = run_da(&["--output", "json", "run", path_str(&case), "--no-bundle"]);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    server.join().expect("server thread must finish");

    assert_eq!(exit_code(&output), Some(0), "stderr: {:?}", stderr(&output));
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "poll step must make only 1 attempt when interval exceeds remaining budget"
    );
    let value = json_stdout(&output);
    assert_eq!(value["cases"][0]["steps"][0]["status"], "skipped");
    assert_eq!(value["cases"][0]["steps"][0]["result"]["status"], 202);
    assert_eq!(
        value["cases"][0]["steps"][0]["result"]["observed"]["app_log"]["lines"],
        serde_json::json!(["poll-1"])
    );
    assert_eq!(value["cases"][0]["steps"][1]["status"], "passed");
    assert_eq!(
        value["cases"][0]["steps"][1]["result"]["observed"]["app_log"]["lines"],
        serde_json::json!(["after-step"])
    );
}
