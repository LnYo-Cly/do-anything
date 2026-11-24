//! End-to-end integration tests for `da run --repeat <N>`.

mod support;

#[cfg(feature = "http")]
use std::io::{Read, Write};
#[cfg(feature = "http")]
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
#[cfg(feature = "http")]
use std::sync::Arc;
#[cfg(feature = "http")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(feature = "http")]
use std::time::Duration;

#[cfg(feature = "http")]
use serde_json::Value;
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da(args: &[&str]) -> Output {
    let work = TempDir::new("repeat-cwd");
    Command::new(da())
        .current_dir(work.path())
        .args(args)
        .output()
        .expect("failed to run the da binary")
}

fn exit_code(output: &Output) -> Option<i32> {
    output.status.code()
}

#[cfg(feature = "http")]
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("test paths must be valid UTF-8")
}

#[test]
fn repeat_zero_or_non_numeric_is_rejected_with_usage_exit_two() {
    let dir = TempDir::new("repeat-usage");
    let case = dir.write("a.yaml", "name: A\nsteps:\n  - unknown.step: {}\n");

    let zero = run_da(&["run", path_str(&case), "--repeat", "0"]);
    assert_eq!(exit_code(&zero), Some(2), "stderr: {}", stderr(&zero));
    assert!(
        stderr(&zero).contains("--repeat"),
        "stderr: {}",
        stderr(&zero)
    );

    let bad = run_da(&["run", path_str(&case), "--repeat", "abc"]);
    assert_eq!(exit_code(&bad), Some(2), "stderr: {}", stderr(&bad));
}

#[test]
fn invalid_case_under_repeat_exits_three_immediately() {
    let dir = TempDir::new("repeat-invalid");
    let case = dir.write(
        "broken.yaml",
        "name: Broken\nsteps:\n  - unregistered.step: {}\n",
    );

    let out = run_da(&[
        "--output",
        "json",
        "run",
        path_str(&case),
        "--repeat",
        "3",
        "--no-bundle",
    ]);
    assert_eq!(exit_code(&out), Some(3), "stderr: {}", stderr(&out));
}

#[cfg(feature = "http")]
struct StatefulRepeatServer {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "http")]
impl StatefulRepeatServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let flaky_hits = Arc::new(AtomicUsize::new(0));
        let timeout_hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let flaky = Arc::clone(&flaky_hits);
                        let slow = Arc::clone(&timeout_hits);
                        std::thread::spawn(move || {
                            // Accepting from a non-blocking listener yields a
                            // non-blocking socket on Windows; restore blocking
                            // mode before reading the request.
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                            let mut buf = Vec::new();
                            let mut chunk = [0u8; 2048];
                            loop {
                                match stream.read(&mut chunk) {
                                    Ok(0) => break,
                                    Ok(read) => {
                                        buf.extend_from_slice(&chunk[..read]);
                                        if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                                            break;
                                        }
                                    }
                                    Err(error)
                                        if error.kind() == std::io::ErrorKind::WouldBlock
                                            || error.kind() == std::io::ErrorKind::TimedOut =>
                                    {
                                        break;
                                    }
                                    Err(_) => break,
                                }
                            }
                            let req = String::from_utf8_lossy(&buf);
                            let first_line = req.lines().next().unwrap_or("");
                            let path = first_line.split_whitespace().nth(1).unwrap_or("/");

                            let (status_line, body) = match path {
                                "/pass" => ("200 OK", r#"{"ok":true}"#.to_string()),
                                "/fail" => {
                                    ("500 Internal Server Error", r#"{"ok":false}"#.to_string())
                                }
                                "/flaky" => {
                                    // Passes on attempt 1, fails on attempts 2 and 3.
                                    let prev = flaky.fetch_add(1, Ordering::SeqCst);
                                    if prev == 0 {
                                        ("200 OK", r#"{"attempt":1,"ok":true}"#.to_string())
                                    } else {
                                        (
                                            "500 Internal Server Error",
                                            format!(r#"{{"attempt":{},"ok":false}}"#, prev + 1),
                                        )
                                    }
                                }
                                "/timeout-second" => {
                                    let prev = slow.fetch_add(1, Ordering::SeqCst);
                                    if prev == 0 {
                                        ("200 OK", r#"{"ok":true}"#.to_string())
                                    } else {
                                        std::thread::sleep(Duration::from_millis(1500));
                                        ("200 OK", r#"{"ok":true}"#.to_string())
                                    }
                                }
                                "/echo-auth" => {
                                    let auth = req
                                        .lines()
                                        .find_map(|line| {
                                            let (k, v) = line.split_once(':')?;
                                            k.eq_ignore_ascii_case("authorization")
                                                .then(|| v.trim().to_string())
                                        })
                                        .unwrap_or_default();
                                    (
                                        "500 Internal Server Error",
                                        serde_json::json!({ "seen_auth": auth }).to_string(),
                                    )
                                }
                                _ => ("404 Not Found", r#"{"ok":false}"#.to_string()),
                            };

                            let response = format!(
                                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                            let _ = stream.flush();
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[cfg(feature = "http")]
impl Drop for StatefulRepeatServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(feature = "http")]
#[test]
fn repeat_one_preserves_backward_compatible_json_text_and_record_layout() {
    let server = StatefulRepeatServer::start();
    let dir = TempDir::new("repeat-one-compat");
    let case = dir.write(
        "pass.yaml",
        &format!(
            "name: Stable single\nsteps:\n  - http.request:\n      url: {}/pass\n    expect:\n      - path: $.status\n        equals: 200\n",
            server.base_url()
        ),
    );

    let rec_default = dir.path().join("rec-default");
    let rec_one = dir.path().join("rec-one");

    let out_default = run_da(&[
        "--output",
        "json",
        "run",
        path_str(&case),
        "--no-bundle",
        "--record",
        path_str(&rec_default),
    ]);
    let out_one = run_da(&[
        "--output",
        "json",
        "run",
        path_str(&case),
        "--repeat",
        "1",
        "--no-bundle",
        "--record",
        path_str(&rec_one),
    ]);

    assert_eq!(exit_code(&out_default), Some(0));
    assert_eq!(exit_code(&out_one), Some(0));

    let v_default: Value = serde_json::from_slice(&out_default.stdout).expect("default JSON");
    let v_one: Value = serde_json::from_slice(&out_one.stdout).expect("repeat 1 JSON");
    assert!(
        v_one.get("repeat_count").is_none(),
        "--repeat 1 must preserve legacy JSON schema: {v_one}"
    );
    assert_eq!(v_default["ok"], v_one["ok"]);
    assert_eq!(v_default["cases"][0]["status"], "passed");
    assert_eq!(v_one["cases"][0]["status"], "passed");

    // `--record` with `--repeat 1` writes directly into `<dir>/0-...`, not `attempt-1/`.
    assert!(!rec_one.join("attempt-1").exists());
    assert!(rec_one.join("0-Stable-single/meta.json").is_file());
}

#[cfg(feature = "http")]
#[test]
fn repeat_three_classifies_stable_pass_flaky_and_stable_fail_and_records_attempts() {
    let server = StatefulRepeatServer::start();
    let dir = TempDir::new("repeat-three");
    let cases = dir.path().join("cases");
    std::fs::create_dir_all(&cases).expect("cases dir");

    std::fs::write(
        cases.join("01_pass.yaml"),
        format!(
            "name: Always passes\nsteps:\n  - http.request:\n      url: {}/pass\n    expect:\n      - path: $.status\n        equals: 200\n",
            server.base_url()
        ),
    )
    .expect("pass case");
    std::fs::write(
        cases.join("02_flaky.yaml"),
        format!(
            "name: Passes once then fails\nsteps:\n  - http.request:\n      url: {}/flaky\n    expect:\n      - path: $.status\n        equals: 200\n",
            server.base_url()
        ),
    )
    .expect("flaky case");
    std::fs::write(
        cases.join("03_fail.yaml"),
        format!(
            "name: Always fails\nsteps:\n  - http.request:\n      url: {}/fail\n    expect:\n      - path: $.status\n        equals: 200\n",
            server.base_url()
        ),
    )
    .expect("fail case");

    let record_dir = dir.path().join("recorded-repeat");
    let out = run_da(&[
        "--output",
        "json",
        "run",
        path_str(&cases),
        "--repeat",
        "3",
        "--no-bundle",
        "--record",
        path_str(&record_dir),
    ]);
    assert_eq!(exit_code(&out), Some(1), "stderr: {}", stderr(&out));

    let val: Value = serde_json::from_slice(&out.stdout).expect("repeat JSON");
    assert_eq!(val["ok"], false);
    assert_eq!(val["repeat_count"], 3);
    assert_eq!(val["summary"]["cases"], 3);
    assert_eq!(val["summary"]["stable_pass"], 1);
    assert_eq!(val["summary"]["flaky"], 1);
    assert_eq!(val["summary"]["stable_fail"], 1);
    assert_eq!(val["summary"]["total_attempts"], 9);
    assert_eq!(val["summary"]["passed_attempts"], 4);
    assert_eq!(val["summary"]["failed_attempts"], 5);

    let arr = val["cases"].as_array().expect("cases array");
    assert_eq!(arr[0]["classification"], "stable_pass");
    assert_eq!(arr[0]["passed_runs"], 3);
    assert_eq!(arr[0]["failed_runs"], 0);

    assert_eq!(arr[1]["classification"], "flaky");
    assert_eq!(arr[1]["passed_runs"], 1);
    assert_eq!(arr[1]["failed_runs"], 2);
    assert_eq!(arr[1]["attempts"][0]["status"], "passed");
    assert_eq!(arr[1]["attempts"][1]["status"], "failed");
    assert_eq!(arr[1]["attempts"][2]["status"], "failed");

    assert_eq!(arr[2]["classification"], "stable_fail");
    assert_eq!(arr[2]["passed_runs"], 0);
    assert_eq!(arr[2]["failed_runs"], 3);

    // Every attempt bundle and top-level repeat-summary.json are written cleanly.
    assert!(record_dir.join("repeat-summary.json").is_file());
    for k in 1..=3 {
        assert!(
            record_dir
                .join(format!("attempt-{k}/1-Passes-once-then-fails/meta.json"))
                .is_file(),
            "missing attempt-{k} bundle"
        );
    }

    // `da compare` works directly between `attempt-1` and `attempt-2`!
    let cmp = run_da(&[
        "--output",
        "json",
        "compare",
        path_str(&record_dir.join("attempt-1")),
        path_str(&record_dir.join("attempt-2")),
    ]);
    assert_eq!(exit_code(&cmp), Some(1));
    let cmp_val: Value = serde_json::from_slice(&cmp.stdout).expect("compare JSON");
    assert_eq!(cmp_val["summary"]["regressed"], 1);
    assert_eq!(cmp_val["summary"]["still_passing"], 1);
    assert_eq!(cmp_val["summary"]["still_failing"], 1);
}

#[cfg(feature = "http")]
#[test]
fn repeat_exits_zero_when_all_stable_pass_and_preserves_timeout_exit_five_and_redacts_secrets() {
    let server = StatefulRepeatServer::start();
    let dir = TempDir::new("repeat-pass-timeout-secret");

    let pass_case = dir.write(
        "pass.yaml",
        &format!(
            "name: All pass\nsteps:\n  - http.request:\n      url: {}/pass\n    expect:\n      - path: $.status\n        equals: 200\n",
            server.base_url()
        ),
    );
    let text_pass = run_da(&["run", path_str(&pass_case), "--repeat", "2", "--no-bundle"]);
    assert_eq!(
        exit_code(&text_pass),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&text_pass),
        stderr(&text_pass)
    );
    assert!(stdout(&text_pass).contains("STABLE_PASS"));
    assert!(stdout(&text_pass).contains("Summary (repeat 2): 1 case (1 stable_pass"));

    // Flaky case whose failure is a step timeout must exit 5 (`ExitCode::Timeout`).
    let timeout_case = dir.write(
        "timeout.yaml",
        &format!(
            "name: Second attempt times out\nsteps:\n  - http.request:\n      url: {}/timeout-second\n      timeout_ms: 500\n    expect:\n      - path: $.status\n        equals: 200\n",
            server.base_url()
        ),
    );
    let timeout_out = run_da(&[
        "run",
        path_str(&timeout_case),
        "--repeat",
        "2",
        "--no-bundle",
    ]);
    assert_eq!(
        exit_code(&timeout_out),
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&timeout_out),
        stderr(&timeout_out)
    );
    assert!(stdout(&timeout_out).contains("FLAKY"));

    // Secret redaction across repeat text, JSON, and repeat-summary.json.
    let secret_case = dir.write(
        "secret.yaml",
        &format!(
            "name: Secret header\nsteps:\n  - http.request:\n      url: {}/echo-auth\n      headers:\n        Authorization: \"Bearer ${{env:DA_REPEAT_SECRET_VAL}}\"\n    expect:\n      - path: $.status\n        equals: 200\n",
            server.base_url()
        ),
    );
    let rec_secret = dir.path().join("rec-secret");
    let secret_json_out = Command::new(da())
        .current_dir(dir.path())
        .env("DA_REPEAT_SECRET_VAL", "top-secret-repeat-777")
        .args([
            "--output",
            "json",
            "run",
            path_str(&secret_case),
            "--repeat",
            "2",
            "--no-bundle",
            "--record",
            path_str(&rec_secret),
        ])
        .output()
        .expect("run secret repeat");
    assert_eq!(exit_code(&secret_json_out), Some(1));
    assert!(
        !stdout(&secret_json_out).contains("top-secret-repeat-777"),
        "secret leaked in repeat JSON"
    );
    let summary_disk =
        std::fs::read_to_string(rec_secret.join("repeat-summary.json")).expect("summary file");
    assert!(
        !summary_disk.contains("top-secret-repeat-777"),
        "secret leaked in repeat-summary.json"
    );
    assert!(summary_disk.contains("***"));
}
