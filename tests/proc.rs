//! End-to-end tests for the `proc.*` long-lived process session driver.

#![cfg(feature = "proc")]
#![allow(dead_code, unused_imports)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    run_da_with_env(dir, args, &[])
}

fn run_da_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(da());
    cmd.current_dir(dir).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run the da binary")
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("test paths must be valid UTF-8")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout must contain exactly one JSON value")
}

fn next_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!(
        "da-proc-tok-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Pure-stdlib helper binary compiled once per test process, supporting:
/// - `server`: local HTTP server + optional grandchild process + secret logging
/// - `repl`: interactive stdin/stdout/stderr command loop
/// - `hang`: spawns a grandchild holding pipes, optionally silent (timeout tests)
/// - `grandchild`: sleeps while holding pidfile marker
/// - `crash`: writes stdout/stderr and exits non-zero
/// - `flood`: writes >2 MiB stdout and >1 MiB stderr to exercise bounded pipe draining
fn helper_bin() -> &'static Path {
    static BIN: LazyLock<(TempDir, PathBuf)> = LazyLock::new(|| {
        let dir = TempDir::new("proc-test-helper");
        let src = dir.write(
            "proc_helper.rs",
            r##"
use std::env;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn write_pid(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, format!("{}\n", std::process::id())).expect("pid file must write");
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(Instant::now() < deadline, "grandchild did not write {}", path.display());
        thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_grandchild(pid_file: &Path, token: &str, hold_pipes: bool) {
    let exe = env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.args(["grandchild", &pid_file.to_string_lossy(), token])
        .stdin(Stdio::null());
    if hold_pipes {
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let _child = cmd.spawn().expect("grandchild must spawn");
    wait_for_file(pid_file);
}

fn run_server(args: &[String]) {
    let pid_dir = PathBuf::from(&args[0]);
    let token = &args[1];
    let with_grandchild = args.iter().any(|a| a == "--grandchild");
    let hold_pipes = args.iter().any(|a| a == "--hold-pipes");
    let port: u16 = args
        .windows(2)
        .find(|w| w[0] == "--port")
        .map(|w| w[1].parse().expect("valid port"))
        .unwrap_or(0);

    write_pid(&pid_dir.join("parent.pid"));
    if with_grandchild {
        spawn_grandchild(&pid_dir.join("grandchild.pid"), token, hold_pipes);
    }

    if let Ok(cwd) = env::current_dir() {
        println!("CWD={}", cwd.display());
    }
    if let Ok(val) = env::var("DA_TEST_PROC_ENV") {
        println!("ENV_VAL={val}");
    }
    if let Ok(secret) = env::var("DA_TEST_PROC_SECRET") {
        println!("SECRET_OUT={secret}");
        eprintln!("SECRET_ERR={secret}");
    }

    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind loopback");
    let bound_port = listener.local_addr().expect("local addr").port();
    println!("READY http://127.0.0.1:{bound_port}");
    eprintln!("SERVER_STDERR_STARTED port={bound_port}");
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();

    for stream in listener.incoming().flatten() {
        thread::spawn(move || {
            let mut stream = stream;
            let mut buf = [0_u8; 4096];
            let read = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..read]);
            let path = req
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            let (status, body) = if path == "/health" {
                ("200 OK", r#"{"ok":true}"#.to_string())
            } else if path == "/secret" {
                let secret = env::var("DA_TEST_PROC_SECRET").unwrap_or_default();
                ("200 OK", format!(r#"{{"secret":"{secret}"}}"#))
            } else {
                ("404 Not Found", r#"{"ok":false}"#.to_string())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
    }
}

fn run_repl() {
    println!("REPL_READY");
    let _ = io::stdout().flush();
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line == "PING" {
            println!("PONG");
            let _ = io::stdout().flush();
        } else if let Some(msg) = line.strip_prefix("ERR ") {
            eprintln!("{msg}");
            let _ = io::stderr().flush();
        } else if let Some(msg) = line.strip_prefix("ECHO ") {
            println!("{msg}");
            let _ = io::stdout().flush();
        } else if let Some(code) = line.strip_prefix("EXIT ") {
            let code: i32 = code.trim().parse().unwrap_or(1);
            std::process::exit(code);
        } else {
            println!("UNKNOWN:{line}");
            let _ = io::stdout().flush();
        }
    }
    println!("BYE");
    let _ = io::stdout().flush();
}

fn run_hang(args: &[String]) {
    let pid_dir = PathBuf::from(&args[0]);
    let token = &args[1];
    let silent = args.iter().any(|a| a == "--silent");
    let hold_pipes = args.iter().any(|a| a == "--hold-pipes");
    write_pid(&pid_dir.join("parent.pid"));
    spawn_grandchild(&pid_dir.join("grandchild.pid"), token, hold_pipes);
    println!("HANG_BOOTED");
    eprintln!("HANG_ERR_BOOTED");
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    if !silent {
        println!("READY");
        let _ = io::stdout().flush();
    }
    loop {
        thread::sleep(Duration::from_secs(10));
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => run_server(&args[2..]),
        Some("grandchild") => {
            write_pid(Path::new(&args[2]));
            loop {
                thread::sleep(Duration::from_secs(10));
            }
        }
        Some("repl") => run_repl(),
        Some("hang") => run_hang(&args[2..]),
        Some("crash") => {
            let code: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2);
            println!("BEFORE_CRASH_OUT");
            eprintln!("BEFORE_CRASH_ERR");
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            std::process::exit(code);
        }
        Some("flood") => {
            let chunk_out = vec![b'A'; 65_536];
            let chunk_err = vec![b'E'; 65_536];
            let mut out = io::stdout().lock();
            let mut err = io::stderr().lock();
            for _ in 0..34 {
                out.write_all(&chunk_out).expect("stdout write");
            }
            for _ in 0..20 {
                err.write_all(&chunk_err).expect("stderr write");
            }
            out.write_all(b"\nFLOOD_DONE\n").expect("stdout marker");
            out.flush().expect("stdout flush");
            err.flush().expect("stderr flush");
            drop(out);
            drop(err);
            loop {
                thread::sleep(Duration::from_secs(10));
            }
        }
        other => panic!("unknown helper mode: {other:?}"),
    }
}
"##,
        );
        let exe = dir.path().join(if cfg!(windows) {
            "proc_helper.exe"
        } else {
            "proc_helper"
        });
        let status = Command::new("rustc")
            .arg("--edition=2024")
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .status()
            .expect("rustc must compile proc helper");
        assert!(status.success(), "rustc failed compiling proc helper");
        (dir, exe)
    });
    &BIN.1
}

/// Picks a currently free loopback port so YAML can interpolate it when needed.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn read_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "pid file {} was never written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let text = fs::read_to_string(path).expect("pid file must be readable");
    text.trim()
        .parse::<u32>()
        .unwrap_or_else(|e| panic!("invalid pid in {}: {text:?}: {e}", path.display()))
}

fn is_token_pid_alive(pid: u32, token: &str) -> bool {
    #[cfg(unix)]
    {
        let _ = token;
        let Ok(out) = Command::new("ps")
            .args(["-ww", "-p", &pid.to_string(), "-o", "stat=,args="])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        let state = trimmed.chars().next().unwrap_or(' ');
        if matches!(state, 'Z' | 'X' | 'x') {
            return false;
        }
        trimmed.contains(token)
    }
    #[cfg(windows)]
    {
        let tasklist = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .map(|r| r.join("System32").join("tasklist.exe"))
            .unwrap_or_else(|| PathBuf::from("tasklist.exe"));
        let Ok(out) = Command::new(tasklist)
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        let pid_str = pid.to_string();
        let exists = String::from_utf8_lossy(&out.stdout).lines().any(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with('"') {
                return false;
            }
            let mut fields = trimmed.split("\",\"");
            let _ = fields.next();
            fields
                .next()
                .is_some_and(|field| field.trim_matches('"') == pid_str)
        });
        if !exists {
            return false;
        }
        // Guard against immediate PID reuse by checking command line token.
        let ps = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .map(|r| {
                r.join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe")
            })
            .unwrap_or_else(|| PathBuf::from("powershell.exe"));
        let script = format!(
            "$p = Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}'; if ($p -and $p.CommandLine) {{ $p.CommandLine }}"
        );
        let Ok(cmd_out) = Command::new(ps)
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            return true;
        };
        String::from_utf8_lossy(&cmd_out.stdout).contains(token)
    }
}

fn assert_process_tree_dead(pid_dir: &Path, token: &str) {
    let parent_pid = read_pid(&pid_dir.join("parent.pid"));
    let grandchild_pid = read_pid(&pid_dir.join("grandchild.pid"));

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !is_token_pid_alive(parent_pid, token) && !is_token_pid_alive(grandchild_pid, token) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !is_token_pid_alive(parent_pid, token),
        "parent process {parent_pid} leaked after da exited"
    );
    assert!(
        !is_token_pid_alive(grandchild_pid, token),
        "grandchild process {grandchild_pid} leaked after da exited"
    );
}

#[cfg(feature = "http")]
#[test]
fn proc_start_local_server_with_http_assertions_and_zero_orphan_grandchildren_on_pass() {
    let helper = helper_bin();
    let dir = TempDir::new("proc-http-pass");
    let pid_dir = dir.path().join("pids");
    let port = free_port();
    let token = next_token();

    let case = dir.write(
        "pass.yaml",
        &format!(
            r#"
name: Server with grandchild passes and cleans up
steps:
  - proc.start:
      command: '{helper}'
      args: ["server", '{pid_dir}', "{token}", "--grandchild", "--hold-pipes", "--port", "{port}"]
      ready_pattern: "READY http://"
      ready_timeout_ms: 5000
    expect:
      - path: $.ready
        equals: true
  - http.request:
      url: "http://127.0.0.1:{port}/health"
    expect:
      - path: $.status
        equals: 200
      - path: $.body.ok
        equals: true
"#,
            helper = helper.display(),
            pid_dir = pid_dir.display(),
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    // Session::close() must have killed BOTH parent and grandchild even without proc.stop.
    assert_process_tree_dead(&pid_dir, &token);
}

#[cfg(feature = "http")]
#[test]
fn proc_kills_process_tree_on_assertion_failure_and_writes_proc_evidence() {
    let helper = helper_bin();
    let dir = TempDir::new("proc-http-fail");
    let pid_dir = dir.path().join("pids");
    let bundles = dir.path().join("bundles");
    let port = free_port();
    let token = next_token();

    let case = dir.write(
        "fail.yaml",
        &format!(
            r#"
name: Server cleanup on assertion failure
steps:
  - proc.start:
      command: '{helper}'
      args: ["server", '{pid_dir}', "{token}", "--grandchild", "--hold-pipes", "--port", "{port}"]
      ready_pattern: "READY http://"
      ready_timeout_ms: 5000
  - http.request:
      url: "http://127.0.0.1:{port}/health"
    expect:
      - path: $.status
        equals: 500
"#,
            helper = helper.display(),
            pid_dir = pid_dir.display(),
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
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_process_tree_dead(&pid_dir, &token);

    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["status"], "failed", "{report}");
    assert_eq!(report["cases"][0]["error"]["kind"], "assertion", "{report}");
    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("failed case must have bundle"),
    );
    let stdout_log = fs::read_to_string(bundle_dir.join("proc/stdout.log"))
        .expect("proc/stdout.log must be written");
    let stderr_log = fs::read_to_string(bundle_dir.join("proc/stderr.log"))
        .expect("proc/stderr.log must be written");
    assert!(stdout_log.contains("READY http://"), "got: {stdout_log:?}");
    assert!(
        stderr_log.contains("SERVER_STDERR_STARTED"),
        "got: {stderr_log:?}"
    );
}

#[test]
fn proc_kills_process_tree_on_start_and_wait_timeouts() {
    let helper = helper_bin();
    let dir = TempDir::new("proc-timeouts");

    // 1) Timeout inside `proc.start` waiting for `ready_pattern`
    let pid_dir_start = dir.path().join("pids-start");
    let token_start = next_token();
    let bundles_start = dir.path().join("bundles-start");
    let start_timeout_case = dir.write(
        "start-timeout.yaml",
        &format!(
            r#"
name: Start ready timeout
steps:
  - proc.start:
      command: '{helper}'
      args: ["hang", '{pid_dir}', "{token}", "--silent", "--hold-pipes"]
      ready_pattern: "NEVER_PRINTED"
      ready_timeout_ms: 400
"#,
            helper = helper.display(),
            pid_dir = pid_dir_start.display(),
            token = token_start,
        ),
    );
    let out_start = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&start_timeout_case),
            "--bundle-dir",
            path_str(&bundles_start),
        ],
    );
    assert_eq!(
        out_start.status.code(),
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&out_start),
        stderr(&out_start)
    );
    assert_process_tree_dead(&pid_dir_start, &token_start);
    let start_report = json_stdout(&out_start);
    let start_bundle = PathBuf::from(
        start_report["cases"][0]["bundle"]
            .as_str()
            .expect("timed out start must write bundle"),
    );
    assert!(
        fs::read_to_string(start_bundle.join("proc/stdout.log"))
            .expect("proc/stdout.log must exist on ready timeout")
            .contains("HANG_BOOTED")
    );

    // 2) Timeout inside `proc.wait_output`
    let pid_dir_wait = dir.path().join("pids-wait");
    let token_wait = next_token();
    let bundles_wait = dir.path().join("bundles-wait");
    let wait_timeout_case = dir.write(
        "wait-timeout.yaml",
        &format!(
            r#"
name: Wait output timeout
steps:
  - proc.start:
      command: '{helper}'
      args: ["hang", '{pid_dir}', "{token}", "--hold-pipes"]
      ready_pattern: "READY"
      ready_timeout_ms: 5000
  - proc.wait_output:
      pattern: "STILL_NEVER_PRINTED"
      timeout_ms: 250
"#,
            helper = helper.display(),
            pid_dir = pid_dir_wait.display(),
            token = token_wait,
        ),
    );
    let out_wait = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&wait_timeout_case),
            "--bundle-dir",
            path_str(&bundles_wait),
        ],
    );
    assert_eq!(
        out_wait.status.code(),
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&out_wait),
        stderr(&out_wait)
    );
    assert_process_tree_dead(&pid_dir_wait, &token_wait);
    let wait_report = json_stdout(&out_wait);
    let wait_bundle = PathBuf::from(
        wait_report["cases"][0]["bundle"]
            .as_str()
            .expect("timed out wait_output must write bundle"),
    );
    assert!(
        fs::read_to_string(wait_bundle.join("proc/stderr.log"))
            .expect("proc/stderr.log must exist on wait timeout")
            .contains("HANG_ERR_BOOTED")
    );
}

#[test]
fn interactive_proc_send_and_wait_output_across_repeated_patterns_and_eof() {
    let helper = helper_bin();
    let dir = TempDir::new("proc-interactive");

    let case = dir.write(
        "repl.yaml",
        &format!(
            r#"
name: Interactive REPL
steps:
  - proc.start:
      command: '{helper}'
      args: ["repl"]
      ready_pattern: "REPL_READY"
      ready_timeout_ms: 5000
  - proc.send:
      input: "PING\n"
    expect:
      - path: $.bytes_written
        equals: 5
  - proc.wait_output:
      pattern: "PONG"
      stream: stdout
      timeout_ms: 5000
    expect:
      - path: $.matched
        equals: true
      - path: $.stream
        equals: stdout
  - proc.send:
      input: "PING\n"
  - proc.wait_output:
      pattern: "PONG"
      stream: stdout
      timeout_ms: 5000
  - proc.send:
      input: "ERR warning-42\n"
  - proc.wait_output:
      pattern: "warning-42"
      stream: stderr
      timeout_ms: 5000
    expect:
      - path: $.stream
        equals: stderr
  - proc.send:
      eof: true
  - proc.wait_output:
      pattern: "BYE"
      timeout_ms: 5000
  - proc.stop: {{}}
    expect:
      - path: $.exit_code
        equals: 0
  - proc.stop: {{}}
    expect:
      - path: $.was_running
        equals: false
      - path: $.exit_code
        equals: 0
"#,
            helper = helper.display(),
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let report = json_stdout(&output);
    let stop_result = &report["cases"][0]["steps"][9]["result"];
    assert_eq!(stop_result["exit_code"], 0, "{stop_result}");
    assert!(
        stop_result["stdout"]
            .as_str()
            .is_some_and(|s| s.contains("BYE")),
        "{stop_result}"
    );
}

#[test]
fn multiple_named_processes_cwd_env_and_redacted_secrets_in_bundle() {
    let helper = helper_bin();
    let dir = TempDir::new("proc-multi-secret");
    let work_sub = dir.path().join("workdir");
    fs::create_dir_all(&work_sub).expect("workdir must exist");
    let pid_dir = dir.path().join("pids");
    let bundles = dir.path().join("bundles");
    let token = next_token();
    let secret = "super-s3cr3t-proc-val-987";

    let case = dir.write(
        "multi-secret.yaml",
        &format!(
            r#"
name: Named processes with redacted secrets
steps:
  - proc.start:
      command: '{helper}'
      args: ["server", '{pid_dir}', "{token}", "--grandchild", "--port", "0"]
      cwd: '{work_sub}'
      env:
        DA_TEST_PROC_ENV: "hello-env"
        DA_TEST_PROC_SECRET: "${{env:DA_PROC_TEST_SECRET}}"
      ready_pattern: "READY http://"
      ready_timeout_ms: 5000
  - proc.start:
      name: repl_worker
      command: '{helper}'
      args: ["repl"]
      ready_pattern: "REPL_READY"
      ready_timeout_ms: 5000
  - proc.send:
      name: repl_worker
      input: "ECHO tok=${{env:DA_PROC_TEST_SECRET}}\n"
  - proc.wait_output:
      name: repl_worker
      pattern: "tok="
      timeout_ms: 5000
    expect:
      - path: $.stdout
        equals: "force-failure-to-inspect-bundle"
"#,
            helper = helper.display(),
            pid_dir = pid_dir.display(),
            work_sub = work_sub.display(),
        ),
    );

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
        &[("DA_PROC_TEST_SECRET", secret)],
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_process_tree_dead(&pid_dir, &token);

    let out_str = stdout(&output);
    assert!(
        !out_str.contains(secret),
        "stdout JSON must never leak secret: {out_str}"
    );
    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["status"], "failed", "{report}");
    assert_eq!(report["cases"][0]["error"]["kind"], "assertion", "{report}");
    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("failed case must write bundle"),
    );

    let default_out = fs::read_to_string(bundle_dir.join("proc/default-stdout.log"))
        .expect("proc/default-stdout.log must exist when multiple processes are active");
    let default_err = fs::read_to_string(bundle_dir.join("proc/default-stderr.log"))
        .expect("proc/default-stderr.log must exist");
    let worker_out = fs::read_to_string(bundle_dir.join("proc/repl_worker-stdout.log"))
        .expect("proc/repl_worker-stdout.log must exist");

    assert!(
        default_out.contains("ENV_VAL=hello-env"),
        "got: {default_out:?}"
    );
    assert!(
        default_out.contains("workdir"),
        "cwd must apply to child, got: {default_out:?}"
    );
    assert!(
        default_out.contains("SECRET_OUT=***"),
        "got: {default_out:?}"
    );
    assert!(
        default_err.contains("SECRET_ERR=***"),
        "got: {default_err:?}"
    );
    assert!(worker_out.contains("tok=***"), "got: {worker_out:?}");

    for path in [
        bundle_dir.join("proc/default-stdout.log"),
        bundle_dir.join("proc/default-stderr.log"),
        bundle_dir.join("proc/repl_worker-stdout.log"),
        bundle_dir.join("proc/repl_worker-stderr.log"),
        bundle_dir.join("report.json"),
        bundle_dir.join("meta.json"),
    ] {
        let text = fs::read_to_string(&path).expect("bundle file must be readable");
        assert!(
            !text.contains(secret),
            "secret leaked into {}: {text:?}",
            path.display()
        );
    }
}

#[test]
fn bounded_pipe_draining_never_deadlocks_on_large_output() {
    let helper = helper_bin();
    let dir = TempDir::new("proc-flood");

    let case = dir.write(
        "flood.yaml",
        &format!(
            r#"
name: Flood output stays bounded and finishes
steps:
  - proc.start:
      command: '{helper}'
      args: ["flood"]
      ready_pattern: "FLOOD_DONE"
      ready_timeout_ms: 10000
    expect:
      - path: $.ready
        equals: true
  - proc.stop: {{}}
"#,
            helper = helper.display(),
        ),
    );

    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn proc_error_paths_and_lint_rules() {
    let helper = helper_bin();
    let dir = TempDir::new("proc-errors");

    // 1) Missing executable -> Environment error (exit 4)
    let missing = dir.write(
        "missing.yaml",
        "name: Missing binary\nsteps:\n  - proc.start:\n      command: da-no-such-proc-bin-999\n",
    );
    let missing_out = run_da_in(dir.path(), &["run", path_str(&missing), "--no-bundle"]);
    assert_eq!(
        missing_out.status.code(),
        Some(4),
        "stderr: {}",
        stderr(&missing_out)
    );

    // 2) Early crash before ready_pattern -> fast Execution error (exit 1) + bundle logs
    let bundles = dir.path().join("bundles");
    let crash = dir.write(
        "crash.yaml",
        &format!(
            r#"
name: Early crash before ready
steps:
  - proc.start:
      command: '{helper}'
      args: ["crash", "7"]
      ready_pattern: "READY"
      ready_timeout_ms: 10000
"#,
            helper = helper.display(),
        ),
    );
    let started = Instant::now();
    let crash_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&crash),
            "--bundle-dir",
            path_str(&bundles),
        ],
    );
    assert_eq!(crash_out.status.code(), Some(1));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "early crash must fail fast without waiting for ready_timeout_ms"
    );
    let crash_report = json_stdout(&crash_out);
    let crash_bundle = PathBuf::from(
        crash_report["cases"][0]["bundle"]
            .as_str()
            .expect("crash case must write bundle"),
    );
    assert!(
        fs::read_to_string(crash_bundle.join("proc/stdout.log"))
            .expect("proc/stdout.log")
            .contains("BEFORE_CRASH_OUT")
    );
    assert!(
        fs::read_to_string(crash_bundle.join("proc/stderr.log"))
            .expect("proc/stderr.log")
            .contains("BEFORE_CRASH_ERR")
    );

    // 3) Duplicate running process name -> Execution error (exit 1)
    let dup = dir.write(
        "dup.yaml",
        &format!(
            r#"
name: Duplicate running name
steps:
  - proc.start:
      name: api
      command: '{helper}'
      args: ["repl"]
      ready_pattern: "REPL_READY"
  - proc.start:
      name: api
      command: '{helper}'
      args: ["repl"]
"#,
            helper = helper.display(),
        ),
    );
    let dup_out = run_da_in(dir.path(), &["run", path_str(&dup), "--no-bundle"]);
    assert_eq!(dup_out.status.code(), Some(1));

    // 4) Retry on proc.* must be refused by lint & run (exit 3)
    let retry_case = dir.write(
        "retry.yaml",
        &format!(
            r#"
name: Refused retry
steps:
  - proc.start:
      command: '{helper}'
      args: ["repl"]
    retry:
      until:
        - path: $.ready
          equals: true
      timeout_ms: 500
"#,
            helper = helper.display(),
        ),
    );
    let retry_lint = run_da_in(dir.path(), &["lint", path_str(&retry_case)]);
    assert_eq!(retry_lint.status.code(), Some(3));
}
