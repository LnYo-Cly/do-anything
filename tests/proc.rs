//! End-to-end tests for the `proc.*` long-lived process session driver.

#![cfg(feature = "proc")]
#![allow(dead_code, unused_imports)]

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
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

/// `proc.*` tests spawn process trees and the Windows teardown inspects the
/// machine-wide process table. Concurrent tests make those WMI queries
/// contend and can exceed the bounded teardown budget, so serialize them even
/// when the harness runs tests in parallel.
static PROCESS_TABLE_LOCK: Mutex<()> = Mutex::new(());

fn lock_process_table() -> MutexGuard<'static, ()> {
    PROCESS_TABLE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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
/// - `boundary-flood`: writes a secret straddling the 1 MiB eviction boundary
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

fn run_launcher(args: &[String]) {
    let pid_dir = PathBuf::from(&args[0]);
    let token = &args[1];
    let hold_pipes = args.iter().any(|a| a == "--hold-pipes");
    write_pid(&pid_dir.join("parent.pid"));
    spawn_grandchild(&pid_dir.join("grandchild.pid"), token, hold_pipes);
    println!("LAUNCHED");
    let _ = io::stdout().flush();
}

fn run_intermediate(args: &[String]) {
    let pid_dir = PathBuf::from(&args[0]);
    let token = &args[1];
    let hold_pipes = args.iter().any(|a| a == "--hold-pipes");
    write_pid(&pid_dir.join("intermediate.pid"));
    spawn_grandchild(&pid_dir.join("grandchild.pid"), token, hold_pipes);
}

fn run_launcher_three_level(args: &[String]) {
    let pid_dir = PathBuf::from(&args[0]);
    let token = &args[1];
    let hold_pipes = args.iter().any(|a| a == "--hold-pipes");
    let exit_root = args.iter().any(|a| a == "--exit-root");
    let silent = args.iter().any(|a| a == "--silent");
    write_pid(&pid_dir.join("parent.pid"));

    let exe = env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.arg("intermediate")
        .arg(&pid_dir.to_string_lossy().into_owned())
        .arg(token)
        .stdin(Stdio::null());
    if hold_pipes {
        cmd.arg("--hold-pipes")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    let mut intermediate = cmd.spawn().expect("intermediate must spawn");
    let status = intermediate.wait().expect("intermediate must wait");
    assert!(status.success(), "intermediate failed: {status}");
    wait_for_file(&pid_dir.join("intermediate.pid"));
    wait_for_file(&pid_dir.join("grandchild.pid"));

    if silent {
        println!("THREE_LEVEL_BOOTED");
    } else {
        println!("THREE_LEVEL_LAUNCHED");
    }
    let _ = io::stdout().flush();

    if !exit_root {
        loop {
            thread::sleep(Duration::from_secs(10));
        }
    }
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
        Some("launcher") => run_launcher(&args[2..]),
        Some("intermediate") => run_intermediate(&args[2..]),
        Some("launcher-three-level") => run_launcher_three_level(&args[2..]),
        Some("crash") => {
            let code: i32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2);
            println!("BEFORE_CRASH_OUT");
            eprintln!("BEFORE_CRASH_ERR");
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            std::process::exit(code);
        }
        Some("boundary") => {
            let secret = env::var("DA_TEST_PROC_SECRET").unwrap_or_default();
            let filler = "a".repeat(290);
            println!("{filler}{secret}");
            eprintln!("{filler}{secret}");
            let _ = io::stdout().flush();
            let _ = io::stderr().flush();
            std::process::exit(1);
        }
        Some("boundary-flood") => {
            let secret = env::var("DA_PROC_TEST_SECRET")
                .or_else(|_| env::var("DA_TEST_PROC_SECRET"))
                .expect("DA_PROC_TEST_SECRET or DA_TEST_PROC_SECRET must be set");
            assert!(!secret.is_empty(), "test secret must not be empty");
            let hang = args.iter().any(|a| a == "--hang");
            const TARGET_BYTES: usize = 1_048_577;

            let mut err_buf = Vec::with_capacity(TARGET_BYTES);
            err_buf.extend_from_slice(secret.as_bytes());
            err_buf.resize(TARGET_BYTES, b'e');
            let mut err = io::stderr().lock();
            err.write_all(&err_buf).expect("stderr write");
            err.flush().expect("stderr flush");
            drop(err);

            let marker = b"\nEND_MARKER";
            let mut out_buf = Vec::with_capacity(TARGET_BYTES);
            out_buf.extend_from_slice(secret.as_bytes());
            let filler_len = TARGET_BYTES
                .checked_sub(secret.len() + marker.len())
                .expect("secret and marker must fit within TARGET_BYTES");
            out_buf.resize(secret.len() + filler_len, b'a');
            out_buf.extend_from_slice(marker);
            assert_eq!(out_buf.len(), TARGET_BYTES);
            let mut out = io::stdout().lock();
            out.write_all(&out_buf).expect("stdout write");
            out.flush().expect("stdout flush");
            drop(out);

            if hang {
                loop {
                    thread::sleep(Duration::from_secs(10));
                }
            }
            std::process::exit(1);
        }
        Some("cursor-secret") => {
            let secret = env::var("DA_TEST_PROC_SECRET")
                .expect("DA_TEST_PROC_SECRET must be set for cursor-secret");
            println!("token={secret}");
            println!("CURSOR_READY");
            let _ = io::stdout().flush();
            loop {
                thread::sleep(Duration::from_secs(10));
            }
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

fn assert_pids_dead(pids: &[(&str, u32)], token: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if pids.iter().all(|&(_, pid)| !is_token_pid_alive(pid, token)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    for &(role, pid) in pids {
        assert!(
            !is_token_pid_alive(pid, token),
            "{role} process {pid} leaked after da exited"
        );
    }
}

fn assert_process_tree_dead(pid_dir: &Path, token: &str) {
    let parent_pid = read_pid(&pid_dir.join("parent.pid"));
    let grandchild_pid = read_pid(&pid_dir.join("grandchild.pid"));
    assert_pids_dead(
        &[("parent", parent_pid), ("grandchild", grandchild_pid)],
        token,
    );
}

fn assert_three_level_tree_dead(pid_dir: &Path, token: &str) {
    let parent_pid = read_pid(&pid_dir.join("parent.pid"));
    let intermediate_pid = read_pid(&pid_dir.join("intermediate.pid"));
    let grandchild_pid = read_pid(&pid_dir.join("grandchild.pid"));
    assert_pids_dead(
        &[
            ("parent", parent_pid),
            ("intermediate", intermediate_pid),
            ("grandchild", grandchild_pid),
        ],
        token,
    );
}

#[cfg(feature = "http")]
#[test]
fn proc_start_local_server_with_http_assertions_and_zero_orphan_grandchildren_on_pass() {
    let _process_table = lock_process_table();
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
    let _process_table = lock_process_table();
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
    let _process_table = lock_process_table();
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
    let _process_table = lock_process_table();
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
    let _process_table = lock_process_table();
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
    let _process_table = lock_process_table();
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
    let _process_table = lock_process_table();
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
      ready_timeout_ms: 60000
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
        started.elapsed() < Duration::from_secs(30),
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

#[test]
fn reusing_an_exited_name_stops_the_old_tree_and_preserves_its_logs() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-reuse-exited");
    let pid_dir = dir.path().join("pids-old");
    let bundles = dir.path().join("bundles");
    let token = next_token();

    let case = dir.write(
        "reuse-exited.yaml",
        &format!(
            r#"
name: Reusing an exited process name retires its tree
steps:
  - proc.start:
      name: reused
      command: '{helper}'
      args: ["launcher", '{pid_dir}', "{token}", "--hold-pipes"]
      ready_pattern: "LAUNCHED"
      ready_timeout_ms: 5000
  - proc.wait_output:
      name: reused
      pattern: "PARENT_EXIT"
      timeout_ms: 300
    optional: true
  - proc.start:
      name: reused
      command: '{helper}'
      args: ["repl"]
      ready_pattern: "REPL_READY"
      ready_timeout_ms: 5000
  - proc.wait_output:
      name: reused
      pattern: "NEVER_AFTER_RESTART"
      timeout_ms: 200
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
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert_process_tree_dead(&pid_dir, &token);

    let report = json_stdout(&output);
    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("the timed-out case must write a bundle"),
    );
    let retired_log = fs::read_to_string(bundle_dir.join("proc/reused-stdout.log"))
        .expect("retired process stdout must be preserved");
    assert!(
        retired_log.contains("LAUNCHED"),
        "retired process output was dropped: {retired_log:?}"
    );
    assert!(
        retired_log.contains("REPL_READY"),
        "current process output must share the same evidence file: {retired_log:?}"
    );
}

#[test]
fn stop_kills_an_exited_leaders_descendants_without_pipe_eof() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-stop-orphan");
    let pid_dir = dir.path().join("pids-orphan");
    let token = next_token();

    let case = dir.write(
        "stop-orphan.yaml",
        &format!(
            r#"
name: Stop kills descendants even when output pipes are closed
steps:
  - proc.start:
      name: orphan
      command: '{helper}'
      args: ["launcher", '{pid_dir}', "{token}"]
      ready_pattern: "LAUNCHED"
      ready_timeout_ms: 5000
  - proc.wait_output:
      name: orphan
      pattern: "PARENT_EXIT"
      timeout_ms: 300
    optional: true
  - proc.stop:
      name: orphan
    expect:
      - path: $.was_running
        equals: false
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
    assert_process_tree_dead(&pid_dir, &token);
}

#[test]
fn stop_kills_sleeping_grandchild_after_intermediate_parent_exited_with_live_root() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-three-level-live-root");
    let pid_dir = dir.path().join("pids");
    let token = next_token();

    let case = dir.write(
        "three-level-live-root.yaml",
        &format!(
            r#"
name: Stop kills grandchild holding stdout after intermediate parent exited
steps:
  - proc.start:
      name: tree
      command: '{helper}'
      args: ["launcher-three-level", '{pid_dir}', "{token}", "--hold-pipes"]
      ready_pattern: "THREE_LEVEL_LAUNCHED"
      ready_timeout_ms: 5000
  - proc.stop:
      name: tree
    expect:
      - path: $.was_running
        equals: true
"#,
            helper = helper.display(),
            pid_dir = pid_dir.display(),
        ),
    );

    let started = Instant::now();
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
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "proc.stop must not stall on orphaned grandchild holding stdout open"
    );
    let report = json_stdout(&output);
    assert_eq!(
        report["cases"][0]["steps"][1]["result"]["was_running"], true,
        "{report}"
    );
    assert_three_level_tree_dead(&pid_dir, &token);
}

#[test]
fn stop_kills_sleeping_grandchild_after_intermediate_and_root_exited() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-three-level-exited-root");
    let pid_dir = dir.path().join("pids");
    let token = next_token();

    let case = dir.write(
        "three-level-exited-root.yaml",
        &format!(
            r#"
name: Stop reports was_running false and kills grandchild when root and intermediate exited
steps:
  - proc.start:
      name: tree
      command: '{helper}'
      args: ["launcher-three-level", '{pid_dir}', "{token}", "--hold-pipes", "--exit-root"]
      ready_pattern: "THREE_LEVEL_LAUNCHED"
      ready_timeout_ms: 5000
  - proc.wait_output:
      name: tree
      pattern: "PARENT_EXIT"
      timeout_ms: 500
    optional: true
  - proc.stop:
      name: tree
    expect:
      - path: $.was_running
        equals: false
"#,
            helper = helper.display(),
            pid_dir = pid_dir.display(),
        ),
    );

    let started = Instant::now();
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
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "proc.stop must not stall on orphaned grandchild holding stdout open"
    );
    let report = json_stdout(&output);
    assert_eq!(
        report["cases"][0]["steps"][2]["result"]["was_running"], false,
        "{report}"
    );
    assert_three_level_tree_dead(&pid_dir, &token);
}

#[test]
fn proc_start_ready_timeout_kills_sleeping_grandchild_after_intermediate_parent_exited() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-three-level-ready-timeout");
    let pid_dir = dir.path().join("pids");
    let bundles = dir.path().join("bundles");
    let token = next_token();

    let case = dir.write(
        "three-level-ready-timeout.yaml",
        &format!(
            r#"
name: Start ready_timeout kills grandchild after intermediate parent exited
steps:
  - proc.start:
      command: '{helper}'
      args: ["launcher-three-level", '{pid_dir}', "{token}", "--hold-pipes", "--silent"]
      ready_pattern: "NEVER_PRINTED"
      ready_timeout_ms: 1000
"#,
            helper = helper.display(),
            pid_dir = pid_dir.display(),
        ),
    );

    let started = Instant::now();
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
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "ready_timeout teardown must not stall on orphaned grandchild holding stdout open"
    );
    assert_three_level_tree_dead(&pid_dir, &token);
    let report = json_stdout(&output);
    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("timed out start must write bundle"),
    );
    assert!(
        fs::read_to_string(bundle_dir.join("proc/stdout.log"))
            .expect("proc/stdout.log must exist on ready timeout")
            .contains("THREE_LEVEL_BOOTED")
    );
}

#[test]
fn proc_send_times_out_and_stops_a_process_that_does_not_read_stdin() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-send-timeout");
    let pid_dir = dir.path().join("pids-send");
    let token = next_token();
    let payload = "x".repeat(1024 * 1024);

    let case = dir.write(
        "send-timeout.yaml",
        &format!(
            r#"
name: Send deadline stops a non-reading process
steps:
  - proc.start:
      name: stdin_hold
      command: '{helper}'
      args: ["hang", '{pid_dir}', "{token}", "--silent", "--hold-pipes"]
      ready_pattern: "HANG_BOOTED"
      ready_timeout_ms: 5000
  - proc.send:
      name: stdin_hold
      input: "{payload}"
      timeout_ms: 500
"#,
            helper = helper.display(),
            pid_dir = pid_dir.display(),
        ),
    );

    let started = Instant::now();
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the send deadline must not block the case"
    );
    assert_process_tree_dead(&pid_dir, &token);

    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["error"]["kind"], "timeout", "{report}");
    let message = report["cases"][0]["error"]["message"]
        .as_str()
        .expect("timeout error must carry a message");
    assert!(
        message.contains("process tree was stopped"),
        "timeout must report cleanup: {message}"
    );
}

#[test]
fn ready_failure_never_exposes_a_secret_prefix_at_the_excerpt_boundary() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-secret-boundary");
    let bundles = dir.path().join("bundles");
    let secret = "boundary-s3cr3t-value-987654";

    let case = dir.write(
        "secret-boundary.yaml",
        &format!(
            r#"
name: Secret at the old excerpt boundary
steps:
  - proc.start:
      command: '{helper}'
      args: ["boundary"]
      env:
        DA_TEST_PROC_SECRET: "${{env:DA_PROC_TEST_SECRET}}"
      ready_pattern: "NEVER_PRINTED"
      ready_timeout_ms: 10000
"#,
            helper = helper.display(),
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

    let prefix = &secret[..10];
    let report = json_stdout(&output);
    let message = report["cases"][0]["error"]["message"]
        .as_str()
        .expect("execution error must carry a message");
    assert!(
        !message.contains(prefix),
        "error message leaked a secret prefix: {message}"
    );
    assert!(
        !stdout(&output).contains(prefix),
        "JSON output leaked a secret prefix: {}",
        stdout(&output)
    );

    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("the failed case must write a bundle"),
    );
    for name in ["proc/stdout.log", "proc/stderr.log"] {
        let log = fs::read_to_string(bundle_dir.join(name))
            .unwrap_or_else(|error| panic!("{name} must be readable: {error}"));
        assert!(log.contains("***"), "{name} must be redacted: {log:?}");
        assert!(
            !log.contains(prefix),
            "{name} leaked a secret prefix: {log:?}"
        );
        assert!(
            !log.contains(secret),
            "{name} leaked the complete secret: {log:?}"
        );
    }
    let report_json =
        fs::read_to_string(bundle_dir.join("report.json")).expect("report.json must be readable");
    assert!(
        !report_json.contains(prefix),
        "report.json leaked a secret prefix: {report_json:?}"
    );
}

#[test]
fn failure_bundle_keeps_distinct_logs_for_case_colliding_process_names() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-case-collision");
    let bundles = dir.path().join("bundles");

    let case = dir.write(
        "case-collision.yaml",
        &format!(
            r#"
name: Case-colliding process names preserve separate logs in failure bundle
steps:
  - proc.start:
      name: A
      command: '{helper}'
      args: ["repl"]
      ready_pattern: "REPL_READY"
      ready_timeout_ms: 5000
  - proc.send:
      name: A
      input: "ECHO PROC_UPPER_A_OUT\nERR PROC_UPPER_A_ERR\n"
  - proc.wait_output:
      name: A
      pattern: "PROC_UPPER_A_OUT"
      stream: stdout
      timeout_ms: 5000
  - proc.wait_output:
      name: A
      pattern: "PROC_UPPER_A_ERR"
      stream: stderr
      timeout_ms: 5000
  - proc.start:
      name: a
      command: '{helper}'
      args: ["repl"]
      ready_pattern: "REPL_READY"
      ready_timeout_ms: 5000
  - proc.send:
      name: a
      input: "ECHO PROC_LOWER_a_OUT\nERR PROC_LOWER_a_ERR\n"
  - proc.wait_output:
      name: a
      pattern: "PROC_LOWER_a_OUT"
      stream: stdout
      timeout_ms: 5000
  - proc.wait_output:
      name: a
      pattern: "PROC_LOWER_a_ERR"
      stream: stderr
      timeout_ms: 5000
  - proc.wait_output:
      name: a
      pattern: "NEVER_EMITTED_TOKEN"
      timeout_ms: 200
"#,
            helper = helper.display(),
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
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["error"]["kind"], "timeout", "{report}");
    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("timed out step must write a failure bundle"),
    );

    let proc_dir = bundle_dir.join("proc");
    let mut entries: Vec<String> = fs::read_dir(&proc_dir)
        .expect("proc/ evidence directory must exist")
        .map(|entry| {
            entry
                .expect("dir entry must read")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            "A-stderr.log",
            "A-stdout.log",
            "a-2-stderr.log",
            "a-2-stdout.log",
        ],
        "expected distinct case-collision-safe evidence files, got {entries:?}"
    );

    let upper_out =
        fs::read_to_string(proc_dir.join("A-stdout.log")).expect("A-stdout.log must exist");
    let upper_err =
        fs::read_to_string(proc_dir.join("A-stderr.log")).expect("A-stderr.log must exist");
    let lower_out =
        fs::read_to_string(proc_dir.join("a-2-stdout.log")).expect("a-2-stdout.log must exist");
    let lower_err =
        fs::read_to_string(proc_dir.join("a-2-stderr.log")).expect("a-2-stderr.log must exist");

    assert!(
        upper_out.contains("PROC_UPPER_A_OUT") && !upper_out.contains("PROC_LOWER_a_OUT"),
        "A-stdout.log unexpected: {upper_out:?}"
    );
    assert!(
        upper_err.contains("PROC_UPPER_A_ERR") && !upper_err.contains("PROC_LOWER_a_ERR"),
        "A-stderr.log unexpected: {upper_err:?}"
    );
    assert!(
        lower_out.contains("PROC_LOWER_a_OUT") && !lower_out.contains("PROC_UPPER_A_OUT"),
        "a-2-stdout.log unexpected: {lower_out:?}"
    );
    assert!(
        lower_err.contains("PROC_LOWER_a_ERR") && !lower_err.contains("PROC_UPPER_A_ERR"),
        "a-2-stderr.log unexpected: {lower_err:?}"
    );
}

fn assert_no_secret_fragments(label: &str, text: &str, secret: &str) {
    let prefix = &secret[..10];
    let suffix = &secret[1..];
    assert!(
        !text.contains(secret),
        "{label} leaked complete secret: {text:?}"
    );
    assert!(
        !text.contains(suffix),
        "{label} leaked post-eviction secret suffix: {text:?}"
    );
    assert!(
        !text.contains(prefix),
        "{label} leaked secret prefix: {text:?}"
    );
}

#[test]
fn post_eviction_failure_bundle_never_leaks_boundary_secret() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-r8-bundle");
    let bundles = dir.path().join("bundles");
    let secret = "BOUNDARY-s3cr3t-value-987654";

    let case = dir.write(
        "post-eviction-bundle.yaml",
        &format!(
            r#"
name: Post-eviction failure bundle omits straddling secret
steps:
  - proc.start:
      command: '{helper}'
      args: ["boundary-flood", "--hang"]
      env:
        DA_TEST_PROC_SECRET: "${{env:DA_PROC_TEST_SECRET}}"
      ready_pattern: "END_MARKER"
      ready_timeout_ms: 10000
  - proc.wait_output:
      pattern: "NEVER_PRINTED"
      timeout_ms: 250
"#,
            helper = helper.display(),
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
        Some(5),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let out_str = stdout(&output);
    assert_no_secret_fragments("cli json", &out_str, secret);
    let report = json_stdout(&output);
    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("timed out case must write a failure bundle"),
    );

    for rel in [
        "proc/stdout.log",
        "proc/stderr.log",
        "report.json",
        "meta.json",
    ] {
        let content = fs::read_to_string(bundle_dir.join(rel))
            .unwrap_or_else(|error| panic!("{rel} must be readable: {error}"));
        assert_no_secret_fragments(rel, &content, secret);
        if rel != "meta.json" {
            assert!(
                content.contains("omitted"),
                "{rel} must include post-eviction omission notice: {content:?}"
            );
        }
    }
}

#[test]
fn post_eviction_step_results_never_leak_boundary_secret() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-r8-steps");
    let secret = "BOUNDARY-s3cr3t-value-987654";

    let case = dir.write(
        "post-eviction-steps.yaml",
        &format!(
            r#"
name: Post-eviction step results omit straddling secret
steps:
  - proc.start:
      command: '{helper}'
      args: ["boundary-flood", "--hang"]
      env:
        DA_TEST_PROC_SECRET: "${{env:DA_PROC_TEST_SECRET}}"
  - proc.wait_output:
      pattern: "END_MARKER"
      timeout_ms: 10000
  - proc.stop: {{}}
"#,
            helper = helper.display(),
        ),
    );

    let output = run_da_with_env(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
        &[("DA_PROC_TEST_SECRET", secret)],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let out_str = stdout(&output);
    assert_no_secret_fragments("cli json", &out_str, secret);
    assert!(
        out_str.contains("omitted"),
        "cli json must contain omission notice: {out_str:?}"
    );

    let report = json_stdout(&output);
    let wait_result = &report["cases"][0]["steps"][1]["result"];
    assert_eq!(wait_result["matched"], true, "{wait_result}");
    let wait_stdout = wait_result["stdout"]
        .as_str()
        .expect("proc.wait_output must return stdout string");
    assert!(
        wait_stdout.contains("omitted"),
        "wait_output stdout must include omission notice: {wait_stdout:?}"
    );
    assert_no_secret_fragments("wait_output stdout", wait_stdout, secret);
    if let Some(wait_stderr) = wait_result["stderr"].as_str() {
        assert_no_secret_fragments("wait_output stderr", wait_stderr, secret);
    }

    let stop_result = &report["cases"][0]["steps"][2]["result"];
    let stop_stdout = stop_result["stdout"]
        .as_str()
        .expect("proc.stop must return stdout string");
    assert!(
        stop_stdout.contains("omitted"),
        "proc.stop stdout must include omission notice: {stop_stdout:?}"
    );
    assert_no_secret_fragments("proc.stop stdout", stop_stdout, secret);
    let stop_stderr = stop_result["stderr"]
        .as_str()
        .expect("proc.stop must return stderr string");
    assert!(
        stop_stderr.contains("omitted"),
        "proc.stop stderr must include omission notice: {stop_stderr:?}"
    );
    assert_no_secret_fragments("proc.stop stderr", stop_stderr, secret);
}

#[test]
fn cursor_split_secret_suffixes_never_leak_from_wait_or_ready_paths() {
    let _process_table = lock_process_table();
    let helper = helper_bin();
    let dir = TempDir::new("proc-r9-cursor-secret");
    let bundles = dir.path().join("bundles");
    let secret = "supersecretvalue123";

    let case = dir.write(
        "cursor-split-secret.yaml",
        &format!(
            r#"
name: Cursor split secret stays redacted
steps:
  - proc.start:
      name: wait_path
      command: '{helper}'
      args: ["cursor-secret"]
      env:
        DA_TEST_PROC_SECRET: "${{env:DA_PROC_TEST_SECRET}}"
      ready_pattern: "token="
      ready_timeout_ms: 5000
  - proc.wait_output:
      name: wait_path
      pattern: "ersec"
      stream: stdout
      timeout_ms: 5000
  - proc.wait_output:
      name: wait_path
      pattern: "value"
      stream: stdout
      timeout_ms: 5000
  - proc.start:
      name: ready_path
      command: '{helper}'
      args: ["cursor-secret"]
      env:
        DA_TEST_PROC_SECRET: "${{env:DA_PROC_TEST_SECRET}}"
      ready_pattern: "ersec"
      ready_timeout_ms: 5000
  - proc.wait_output:
      name: ready_path
      pattern: "value"
      stream: stdout
      timeout_ms: 5000
    expect:
      - path: $.stdout
        equals: "force-failure-to-inspect-bundle"
"#,
            helper = helper.display(),
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

    let out_str = stdout(&output);
    assert_no_secret_fragments("cli json", &out_str, secret);
    assert!(
        !out_str.contains("retvalue123"),
        "cli json leaked the post-match secret suffix: {out_str:?}"
    );

    let report = json_stdout(&output);
    assert_eq!(report["cases"][0]["status"], "failed", "{report}");
    assert_eq!(report["cases"][0]["error"]["kind"], "assertion", "{report}");
    for index in [1, 2, 3, 4] {
        let result = &report["cases"][0]["steps"][index]["result"];
        let result_text = result["stdout"].as_str().unwrap_or_default();
        assert!(
            result_text.contains("***"),
            "step {index} must return the redacted line: {result_text:?}"
        );
        assert!(
            !result_text.contains("retvalue123"),
            "step {index} leaked the post-match secret suffix: {result_text:?}"
        );
        assert_no_secret_fragments("step result", result_text, secret);
    }

    let bundle_dir = PathBuf::from(
        report["cases"][0]["bundle"]
            .as_str()
            .expect("the failed case must write a bundle"),
    );
    for rel in [
        "report.json",
        "proc/wait_path-stdout.log",
        "proc/ready_path-stdout.log",
    ] {
        let content = fs::read_to_string(bundle_dir.join(rel))
            .unwrap_or_else(|error| panic!("{rel} must be readable: {error}"));
        assert_no_secret_fragments(rel, &content, secret);
        assert!(
            !content.contains("retvalue123"),
            "{rel} leaked the post-match secret suffix: {content:?}"
        );
    }
}
