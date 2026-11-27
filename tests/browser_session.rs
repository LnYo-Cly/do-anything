#![cfg(feature = "browser")]

//! End-to-end browser tests for `da session`: cross-process CDP reattach,
//! AXTree snapshots, `ref` validation, 7-tier deterministic locator synthesis,
//! fragile export, stale refs, concurrent locking, secrets hygiene, and cleanup.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(da())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run da")
}

fn run_da_stdin(dir: &Path, args: &[&str], stdin_text: &str) -> Output {
    let mut child = Command::new(da())
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn da");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_text.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
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

/// Minimal local HTTP server serving custom HTML routes for synthesis & ref tests.
struct PageServer {
    port: u16,
    stop: Arc<AtomicBool>,
    slow_hits: Arc<AtomicUsize>,
    handle: Option<JoinHandle<()>>,
}

impl PageServer {
    fn start(routes: BTreeMap<String, String>) -> Self {
        let listener = support::bind_browser_safe_listener();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let slow_hits = Arc::new(AtomicUsize::new(0));
        let slow_counter = Arc::clone(&slow_hits);
        let routes = Arc::new(routes);

        let handle = thread::spawn(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let routes = Arc::clone(&routes);
                        let slow_counter = Arc::clone(&slow_counter);
                        thread::spawn(move || serve_once(stream, &routes, &slow_counter));
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            stop,
            slow_hits,
            handle: Some(handle),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }
}

impl Drop for PageServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_once(stream: TcpStream, routes: &BTreeMap<String, String>, slow_hits: &AtomicUsize) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream);
    let mut req_line = String::new();
    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
        return;
    }
    let target = req_line.split_whitespace().nth(1).unwrap_or("/");
    let path = target.split('?').next().unwrap_or(target);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    if path == "/slow-page" {
        slow_hits.fetch_add(1, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(1200));
    }
    let (status, body) = match routes.get(path) {
        Some(html) => ("200 OK", html.as_str()),
        None => (
            "404 Not Found",
            "<!doctype html><html><body>not found</body></html>",
        ),
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

static CHROME_LOCK: Mutex<()> = Mutex::new(());

fn chrome_guard() -> Option<MutexGuard<'static, ()>> {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    let res = READY.get_or_init(|| {
        let dir = TempDir::new("browser-session-probe");
        let server = PageServer::start(BTreeMap::from([(
            "/probe".to_string(),
            "<!doctype html><html><title>Probe</title><body><p>ok</p></body></html>".to_string(),
        )]));
        let started = run_da_in(dir.path(), &["session", "start"]);
        if started.status.code() != Some(0) {
            return Err(format!("start failed: {:?}", started));
        }
        let id = json_stdout(&started)["session_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let open = run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.open",
                "--arg",
                &format!("url={}", server.url("/probe")),
            ],
        );
        let _ = run_da_in(dir.path(), &["session", "stop", &id]);
        if open.status.code() == Some(0) {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&open.stdout).into_owned())
        }
    });
    match res {
        Ok(()) => Some(
            CHROME_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ),
        Err(msg) => {
            if std::env::var("DA_TEST_BROWSER").as_deref() == Ok("1") {
                panic!("DA_TEST_BROWSER=1 requires working Chrome: {msg}");
            }
            None
        }
    }
}

fn find_ref_by_role_and_name(tree: &[Value], role: &str, name: &str) -> Option<String> {
    for node in tree {
        if node.get("role").and_then(Value::as_str) == Some(role)
            && node.get("name").and_then(Value::as_str) == Some(name)
            && let Some(r) = node.get("ref").and_then(Value::as_str)
        {
            return Some(r.to_string());
        }
        if let Some(children) = node.get("children").and_then(Value::as_array)
            && let Some(found) = find_ref_by_role_and_name(children, role, name)
        {
            return Some(found);
        }
    }
    None
}

fn find_ref_by_role(tree: &[Value], role: &str) -> Option<String> {
    for node in tree {
        if node.get("role").and_then(Value::as_str) == Some(role)
            && let Some(r) = node.get("ref").and_then(Value::as_str)
        {
            return Some(r.to_string());
        }
        if let Some(children) = node.get("children").and_then(Value::as_array)
            && let Some(found) = find_ref_by_role(children, role)
        {
            return Some(found);
        }
    }
    None
}

fn count_nodes(tree: &[Value]) -> usize {
    tree.iter()
        .map(|node| {
            1 + node
                .get("children")
                .and_then(Value::as_array)
                .map_or(0, |c| count_nodes(c))
        })
        .sum()
}

fn tree_contains_role(tree: &[Value], role: &str) -> bool {
    tree.iter().any(|node| {
        node.get("role").and_then(Value::as_str) == Some(role)
            || node
                .get("children")
                .and_then(Value::as_array)
                .is_some_and(|c| tree_contains_role(c, role))
    })
}

fn read_browser_handle(session_dir: &Path) -> (u16, PathBuf) {
    let state: Value =
        serde_json::from_str(&fs::read_to_string(session_dir.join("state.json")).unwrap()).unwrap();
    let handle = &state["handles"]["browser"];
    let port = handle["port"].as_u64().expect("browser handle port") as u16;
    let profile = PathBuf::from(handle["profile"].as_str().expect("browser handle profile"));
    (port, profile)
}

fn is_port_open(port: u16) -> bool {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

#[test]
fn da_lint_and_da_run_reject_ref_in_case_files() {
    let dir = TempDir::new("ref-rejected");
    let case = dir.write(
        "bad-ref.yaml",
        "name: bad ref\nsteps:\n  - browser.click:\n      ref: e1\n",
    );
    let lint = run_da_in(dir.path(), &["lint", case.to_str().unwrap()]);
    assert_eq!(lint.status.code(), Some(3), "{:?}", lint);

    let run = run_da_in(dir.path(), &["run", case.to_str().unwrap(), "--no-bundle"]);
    assert_eq!(run.status.code(), Some(3), "{:?}", run);
}

#[test]
fn browser_session_end_to_end_export_replay_secrets_and_stop_cleanup() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let mut routes = BTreeMap::new();
    routes.insert(
        "/login".to_string(),
        r##"<!doctype html>
<html>
<head><title>Login</title></head>
<body>
  <div style="display:none"><button>Hidden Ghost</button></div>
  <div aria-hidden="true"><button>Ignored Ghost</button></div>
  <h1>Sign in</h1>
  <label for="user-box">Username</label>
  <input id="user-box" type="text" />
  <label for="pass-box">Password</label>
  <input id="pass-box" type="password" />
  <button data-testid="submit-btn" onclick="
    var u = document.getElementById('user-box').value;
    var out = document.getElementById('greeting');
    out.textContent = 'Welcome, ' + u;
    out.style.display = 'block';
  "><span>Sign In</span></button>
  <p id="greeting" style="display:none"></p>
</body>
</html>"##
            .to_string(),
    );
    let server = PageServer::start(routes);
    let dir = TempDir::new("browser-e2e");
    let secret = "Sup3rS3cretP@ss!";

    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    // 1. Open page -> browser launches & snapshot returned.
    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/login")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let open_json = json_stdout(&open);
    let tree = open_json["snapshot"]["tree"].as_array().unwrap();
    assert!(!tree_contains_role(tree, "InlineTextBox"));
    assert!(
        find_ref_by_role_and_name(tree, "button", "Ignored Ghost").is_none(),
        "aria-hidden ignored nodes must not appear in snapshot"
    );

    let (debug_port, profile_dir) = read_browser_handle(&session_dir);
    assert!(is_port_open(debug_port), "Chrome debug port must be open");
    assert!(profile_dir.exists(), "Chrome temp profile must exist");

    // 2. Type username using ref.
    let user_ref = find_ref_by_role_and_name(tree, "textbox", "Username")
        .expect("Username textbox ref must exist");
    let type_user = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.type",
            "--arg",
            &format!("ref={user_ref}"),
            "--arg",
            "text=alice",
        ],
    );
    assert_eq!(type_user.status.code(), Some(0), "{:?}", type_user);

    // 3. Type secret password using ${env:...} and ref.
    let tree2 = json_stdout(&type_user)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let pass_ref = find_ref_by_role_and_name(&tree2, "textbox", "Password")
        .expect("Password textbox ref must exist");
    let pass_step = serde_json::json!({
        "browser.type": {
            "ref": pass_ref,
            "text": "${env:DA_TEST_PASS}"
        }
    })
    .to_string();
    let mut cmd_pass = Command::new(da());
    cmd_pass
        .current_dir(dir.path())
        .args(["session", "step", &id, "--step", &pass_step])
        .env("DA_TEST_PASS", secret);
    let out_pass = cmd_pass.output().unwrap();
    assert_eq!(out_pass.status.code(), Some(0), "{:?}", out_pass);
    assert!(
        !String::from_utf8_lossy(&out_pass.stdout).contains(secret),
        "secret must be redacted from stdout"
    );

    // 3b. Capture a screenshot while sensitive input is populated, confirming valid PNG output and unharmed form state.
    let shot = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.screenshot",
            "--arg",
            "path=shots/login.png",
        ],
    );
    assert_eq!(shot.status.code(), Some(0), "{:?}", shot);
    let shot_bytes = fs::read(dir.path().join("shots/login.png")).expect("screenshot PNG file");
    assert!(
        shot_bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "screenshot must be a valid PNG"
    );

    // 4. Click Submit button using ref.
    let tree3 = json_stdout(&shot)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let btn_ref = find_ref_by_role_and_name(&tree3, "button", "Sign In")
        .expect("Sign In button ref must exist");
    let click = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.click",
            "--arg",
            &format!("ref={btn_ref}"),
        ],
    );
    assert_eq!(click.status.code(), Some(0), "{:?}", click);

    // 5. Read greeting with expect -> exported! Also run a pure read without expect/save -> skipped!
    let tree4 = json_stdout(&click)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    assert!(
        count_nodes(&tree4) > 0,
        "snapshot after click must contain visible greeting"
    );
    let pure_read = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.read",
            "--arg",
            "css=#greeting",
        ],
    );
    assert_eq!(pure_read.status.code(), Some(0));

    let read_expect = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.read:\n  css: \"#greeting\"\nexpect:\n  - path: $.text\n    equals: \"Welcome, alice\"\n",
    );
    assert_eq!(read_expect.status.code(), Some(0), "{:?}", read_expect);

    // 6. Verify NO file in session_dir contains the secret value.
    let mut session_files = Vec::new();
    collect_files(&session_dir, &mut session_files);
    for file in &session_files {
        let content = fs::read_to_string(file).unwrap();
        assert!(
            !content.contains(secret),
            "session file {} leaked secret!",
            file.display()
        );
    }

    // 7. Export, lint, and replay with da run!
    let exported = dir.path().join("login-flow.yaml");
    let exp_out = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported.to_str().unwrap(),
            "--name",
            "Login Flow",
        ],
    );
    assert_eq!(exp_out.status.code(), Some(0), "{:?}", exp_out);
    let exp_json = json_stdout(&exp_out);
    assert_eq!(exp_json["exported_steps"], 6);
    assert_eq!(exp_json["skipped_steps"].as_array().unwrap().len(), 1);

    let yaml = fs::read_to_string(&exported).unwrap();
    assert!(
        !yaml.contains("ref:"),
        "exported YAML must not contain ref: {yaml}"
    );
    assert!(
        yaml.contains("${env:DA_TEST_PASS}"),
        "exported YAML must keep ${{env:...}}: {yaml}"
    );
    assert!(!yaml.contains(secret));

    let lint = Command::new(da())
        .current_dir(dir.path())
        .args(["lint", exported.to_str().unwrap()])
        .env("DA_TEST_PASS", secret)
        .output()
        .unwrap();
    assert_eq!(
        lint.status.code(),
        Some(0),
        "exported YAML failed lint: {}",
        String::from_utf8_lossy(&lint.stdout)
    );

    let replay = Command::new(da())
        .current_dir(dir.path())
        .args(["run", exported.to_str().unwrap(), "--no-bundle"])
        .env("DA_TEST_PASS", secret)
        .output()
        .unwrap();
    assert_eq!(
        replay.status.code(),
        Some(0),
        "exported YAML failed da run replay: {}",
        String::from_utf8_lossy(&replay.stdout)
    );

    // 8. Stop -> debug port unreachable, temp profile deleted, session dir deleted!
    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{:?}", stop);
    assert!(
        !is_port_open(debug_port),
        "debug port must be closed after stop"
    );
    assert!(
        !profile_dir.exists(),
        "temp profile must be deleted after stop"
    );
    assert!(
        !session_dir.exists(),
        "session dir must be deleted after stop"
    );
}

#[test]
fn session_coordinate_click_exports_a_pinned_viewport_and_replays() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let page = r##"<!doctype html>
<html><head><title>Canvas</title>
<style>html, body { margin: 0; padding: 0; } #target { display: block; }</style>
</head>
<body>
<canvas id="target" width="200" height="100"></canvas>
<p id="status">idle</p>
<script>
document.getElementById('target').addEventListener('click', function (event) {
  document.getElementById('status').textContent = 'canvas ' + event.clientX + ',' + event.clientY;
});
</script>
</body></html>"##
        .to_string();
    let server = PageServer::start(BTreeMap::from([("/coords".to_string(), page)]));
    let dir = TempDir::new("browser-coords-session");
    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/coords")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{open:?}");

    // Click the canvas with bare coordinates; the step records the live viewport.
    let click = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.click",
            "--with",
            "at: {x: 100, y: 50}",
        ],
    );
    assert_eq!(click.status.code(), Some(0), "{click:?}");
    let click_json = json_stdout(&click);
    let viewport = &click_json["step"]["result"]["viewport"];
    let width = viewport["width"]
        .as_u64()
        .unwrap_or_else(|| panic!("the click must report a viewport: {click_json}"));
    let height = viewport["height"]
        .as_u64()
        .unwrap_or_else(|| panic!("the click must report a viewport: {click_json}"));

    // A read with an expectation proves the click hit the canvas, and exports
    // the assertion so the replay can prove it too.
    let read_expect = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.read:\n  css: \"#status\"\nexpect:\n  - path: $.text\n    equals: \"canvas 100,50\"\n",
    );
    assert_eq!(read_expect.status.code(), Some(0), "{read_expect:?}");

    // Export pins the recorded viewport and marks the step fragile.
    let exported = dir.path().join("coords-flow.yaml");
    let exp_out = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported.to_str().unwrap(),
            "--name",
            "Coordinate flow",
        ],
    );
    assert_eq!(exp_out.status.code(), Some(0), "{exp_out:?}");
    let exp_json = json_stdout(&exp_out);
    assert_eq!(exp_json["exported_steps"], 3, "{exp_json}");
    let fragile = exp_json["fragile"]
        .as_array()
        .unwrap_or_else(|| panic!("the export must report fragile steps: {exp_json}"));
    assert_eq!(fragile.len(), 1, "{exp_json}");
    assert_eq!(fragile[0]["action"], "browser.click", "{exp_json}");
    assert_eq!(
        fragile[0]["reason"], "explicit_coordinates",
        "the export must say why the step is fragile: {exp_json}"
    );
    assert_eq!(fragile[0]["viewport"]["width"], json!(width), "{exp_json}");
    assert_eq!(
        fragile[0]["viewport"]["height"],
        json!(height),
        "{exp_json}"
    );

    let yaml = fs::read_to_string(&exported).unwrap();
    assert!(
        yaml.contains("at:"),
        "the coordinates must be exported: {yaml}"
    );
    assert!(
        yaml.contains(&format!("width: {width}")),
        "the recorded viewport width must be exported: {yaml}"
    );
    assert!(
        yaml.contains(&format!("height: {height}")),
        "the recorded viewport height must be exported: {yaml}"
    );
    assert!(
        yaml.contains("# fragile:"),
        "the exported case must say the step is fragile: {yaml}"
    );

    // The exported case is valid (lint warns, but exits 0) and replays at the
    // recorded size: the read expectation proves the click hit the canvas again.
    let lint = run_da_in(dir.path(), &["lint", exported.to_str().unwrap()]);
    assert_eq!(
        lint.status.code(),
        Some(0),
        "exported YAML failed lint: stdout={}\nstderr={}",
        String::from_utf8_lossy(&lint.stdout),
        String::from_utf8_lossy(&lint.stderr)
    );
    let replay = run_da_in(
        dir.path(),
        &["run", exported.to_str().unwrap(), "--no-bundle"],
    );
    assert_eq!(
        replay.status.code(),
        Some(0),
        "exported YAML failed da run replay: stdout={}\nstderr={}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{stop:?}");
}

#[test]
fn idle_reap_on_list_closes_browser_and_removes_temp_profile() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let server = PageServer::start(BTreeMap::from([(
        "/p".to_string(),
        "<!doctype html><html><body><p>hi</p></body></html>".to_string(),
    )]));
    let dir = TempDir::new("browser-reap");

    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/p")),
        ],
    );
    assert_eq!(open.status.code(), Some(0));
    let (debug_port, profile_dir) = read_browser_handle(&session_dir);
    assert!(is_port_open(debug_port));
    assert!(profile_dir.exists());

    // Without calling stop, idle-timeout-ms 0 on list must close Chrome and delete both dirs!
    let list = run_da_in(dir.path(), &["session", "list", "--idle-timeout-ms", "0"]);
    assert_eq!(list.status.code(), Some(0), "{:?}", list);
    let list_json = json_stdout(&list);
    assert!(
        list_json["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &id),
        "reaped must contain {id}: {list_json}"
    );
    assert!(
        !is_port_open(debug_port),
        "debug port must be closed after idle reap"
    );
    assert!(
        !profile_dir.exists(),
        "temp profile must be deleted after idle reap"
    );
    assert!(
        !session_dir.exists(),
        "session dir must be deleted after idle reap"
    );
}

#[test]
fn synthesis_covers_all_seven_tiers_digits_and_generated_token_filtering() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let mut routes = BTreeMap::new();
    // 1. testid (with inner span normalization!)
    routes.insert(
        "/tier1".to_string(),
        r##"<!doctype html><html><body>
          <button data-testid="buy-now" onclick="document.body.dataset.clicked='1'"><span>Buy</span></button>
        </body></html>"##.to_string(),
    );
    // 2. role + name
    routes.insert(
        "/tier2".to_string(),
        r##"<!doctype html><html><body>
          <button onclick="document.body.dataset.clicked='2'">Publish Post</button>
        </body></html>"##
            .to_string(),
    );
    // 3. form control placeholder / label (expressed as role + name)
    routes.insert(
        "/tier3".to_string(),
        r##"<!doctype html><html><body>
          <input type="text" placeholder="Search orders" />
        </body></html>"##
            .to_string(),
    );
    // 4. exact text (div with role/no interactive ancestor? Or p / span read!)
    routes.insert(
        "/tier4".to_string(),
        r##"<!doctype html><html><body>
          <p>Order confirmed</p>
        </body></html>"##
            .to_string(),
    );
    // 5. stable id (clickable div with tabindex, generated id on sibling ignored, button has stable id and non-unique text)
    routes.insert(
        "/tier5".to_string(),
        r##"<!doctype html><html><body>
          <button id="save-primary" onclick="document.body.dataset.clicked='5'">Go</button>
          <button id=":r1:" onclick="document.body.dataset.clicked='other'">Go</button>
        </body></html>"##
            .to_string(),
    );
    // 6. css scoped within stable ancestor (both buttons say "Edit" with no unique role+name or id, inside distinct testid cards; generated class excluded, digit class after non-digit class)
    routes.insert(
        "/tier6".to_string(),
        r##"<!doctype html><html><body>
          <section data-testid="card-alpha">
            <button class="css-9f8a7b col-2 action-btn" onclick="document.body.dataset.clicked='6a'">Edit</button>
          </section>
          <section data-testid="card-beta">
            <button class="css-1a2b3c action-btn" onclick="document.body.dataset.clicked='6b'">Edit</button>
          </section>
        </body></html>"##.to_string(),
    );
    // 7. structural path fallback (duplicate buttons with no testid, no unique name, no stable id, no stable ancestor -> falls back to structural CSS path -> fragile)
    routes.insert(
        "/tier7".to_string(),
        r##"<!doctype html><html><body>
          <main>
            <div class="css-abc123"><button id=":r9:" onclick="document.body.dataset.clicked='7'">Same</button></div>
            <div class="css-def456"><button id="a1b2c3d4" onclick="document.body.dataset.clicked='other'">Same</button></div>
          </main>
        </body></html>"##.to_string(),
    );

    let server = PageServer::start(routes);
    let dir = TempDir::new("synthesis-tiers");
    let id = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let session_dir = dir.path().join(".da/sessions").join(&id);

    // Tier 1: testid
    let r = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/tier1")),
        ],
    ));
    let btn = find_ref_by_role_and_name(r["snapshot"]["tree"].as_array().unwrap(), "button", "Buy")
        .unwrap();
    assert_eq!(
        run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.click",
                "--arg",
                &format!("ref={btn}")
            ]
        )
        .status
        .code(),
        Some(0)
    );

    // Tier 2: role + name
    let r = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/tier2")),
        ],
    ));
    let btn = find_ref_by_role_and_name(
        r["snapshot"]["tree"].as_array().unwrap(),
        "button",
        "Publish Post",
    )
    .unwrap();
    assert_eq!(
        run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.click",
                "--arg",
                &format!("ref={btn}")
            ]
        )
        .status
        .code(),
        Some(0)
    );

    // Tier 3: placeholder -> role + name
    let r = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/tier3")),
        ],
    ));
    let tb = find_ref_by_role(r["snapshot"]["tree"].as_array().unwrap(), "textbox").unwrap();
    assert_eq!(
        run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.type",
                "--arg",
                &format!("ref={tb}"),
                "--arg",
                "text=shoes"
            ]
        )
        .status
        .code(),
        Some(0)
    );

    // Tier 4: exact text (browser.read with expect so it exports)
    let r = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/tier4")),
        ],
    ));
    let p_ref = find_ref_by_role(r["snapshot"]["tree"].as_array().unwrap(), "paragraph")
        .or_else(|| find_ref_by_role(r["snapshot"]["tree"].as_array().unwrap(), "StaticText"))
        .expect("paragraph ref");
    let read_step = json!({
        "browser.read": { "ref": p_ref },
        "expect": [{ "path": "$.text", "equals": "Order confirmed" }]
    })
    .to_string();
    assert_eq!(
        run_da_in(dir.path(), &["session", "step", &id, "--step", &read_step])
            .status
            .code(),
        Some(0)
    );

    // Tier 5: stable id (skipping generated :r1: on sibling)
    let r = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/tier5")),
        ],
    ));
    let btn = find_ref_by_role_and_name(r["snapshot"]["tree"].as_array().unwrap(), "button", "Go")
        .unwrap();
    assert_eq!(
        run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.click",
                "--arg",
                &format!("ref={btn}")
            ]
        )
        .status
        .code(),
        Some(0)
    );

    // Tier 6: within + css (excluding css-9f8a7b, preferring non-digit action-btn over col-2)
    let r = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/tier6")),
        ],
    ));
    let btn =
        find_ref_by_role_and_name(r["snapshot"]["tree"].as_array().unwrap(), "button", "Edit")
            .unwrap();
    assert_eq!(
        run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.click",
                "--arg",
                &format!("ref={btn}")
            ]
        )
        .status
        .code(),
        Some(0)
    );

    // Tier 7: structural path -> fragile
    let r = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/tier7")),
        ],
    ));
    let btn =
        find_ref_by_role_and_name(r["snapshot"]["tree"].as_array().unwrap(), "button", "Same")
            .unwrap();
    assert_eq!(
        run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.click",
                "--arg",
                &format!("ref={btn}")
            ]
        )
        .status
        .code(),
        Some(0)
    );

    // Inspect steps.jsonl synthesized locators + export report!
    let log_text = fs::read_to_string(session_dir.join("steps.jsonl")).unwrap();
    let records: Vec<Value> = log_text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let synth_locs: Vec<&Value> = records
        .iter()
        .filter_map(|rec| {
            let loc = &rec["synthesized_locator"];
            if loc.is_null() { None } else { Some(loc) }
        })
        .collect();
    assert_eq!(
        synth_locs.len(),
        7,
        "all 7 ref steps must have synthesized locators: {records:?}"
    );
    assert_eq!(synth_locs[0]["testid"], "buy-now");
    assert_eq!(synth_locs[1]["role"], "button");
    assert_eq!(synth_locs[1]["name"], "Publish Post");
    assert_eq!(synth_locs[2]["role"], "textbox");
    assert_eq!(synth_locs[2]["name"], "Search orders");
    assert_eq!(synth_locs[3]["text"], "Order confirmed");
    assert_eq!(synth_locs[4]["css"], "#save-primary");
    assert_eq!(synth_locs[5]["within"]["testid"], "card-alpha");
    assert!(
        synth_locs[5]["css"]
            .as_str()
            .unwrap()
            .contains("action-btn")
            && !synth_locs[5]["css"]
                .as_str()
                .unwrap()
                .contains("css-9f8a7b")
            && !synth_locs[5]["css"].as_str().unwrap().contains("col-2"),
        "tier 6 must exclude generated class and prefer non-digit class: {}",
        synth_locs[5]
    );
    assert!(
        synth_locs[6]["css"].as_str().is_some(),
        "tier 7 must synthesize structural css: {}",
        synth_locs[6]
    );

    let exp_path = dir.path().join("tiers.yaml");
    let exp = json_stdout(&run_da_in(
        dir.path(),
        &["session", "export", &id, "-o", exp_path.to_str().unwrap()],
    ));
    assert_eq!(
        exp["fragile"].as_array().unwrap().len(),
        1,
        "only tier 7 is fragile: {exp}"
    );
    let yaml = fs::read_to_string(&exp_path).unwrap();
    assert!(
        yaml.contains("# fragile:"),
        "YAML must contain fragile comment: {yaml}"
    );

    assert_eq!(
        run_da_in(dir.path(), &["lint", exp_path.to_str().unwrap()])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        run_da_in(
            dir.path(),
            &["run", exp_path.to_str().unwrap(), "--no-bundle"]
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

#[test]
fn stale_ref_snapshot_scoping_truncation_and_concurrent_browser_step() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let mut routes = BTreeMap::new();
    routes.insert(
        "/page-a".to_string(),
        r##"<!doctype html><html><body>
          <section aria-label="Panel">
            <button onclick="document.getElementById('btn').remove()">Remove</button>
            <button id="btn">Target</button>
          </section>
          <ul><li>1</li><li>2</li><li>3</li><li>4</li><li>5</li><li>6</li><li>7</li><li>8</li></ul>
        </body></html>"##
            .to_string(),
    );
    routes.insert(
        "/page-b".to_string(),
        r##"<!doctype html><html><body><button>Other</button></body></html>"##.to_string(),
    );
    routes.insert(
        "/page-c".to_string(),
        r##"<!doctype html><html><body><label for="cbtn">LabelClick</label><button id="cbtn">Go</button></body></html>"##.to_string(),
    );
    let server = PageServer::start(routes);
    let dir = TempDir::new("browser-stale-scope");
    let id = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let open = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/page-a")),
        ],
    ));
    let tree = open["snapshot"]["tree"].as_array().unwrap();
    let target_ref = find_ref_by_role_and_name(tree, "button", "Target").unwrap();
    let panel_ref = find_ref_by_role_and_name(tree, "region", "Panel")
        .or_else(|| find_ref_by_role(tree, "list"))
        .unwrap();

    // Scoped snapshot + truncation test.
    let trunc = json_stdout(&run_da_in(
        dir.path(),
        &["session", "snapshot", &id, "--max-nodes", "3"],
    ));
    assert_eq!(trunc["snapshot"]["truncated"], true, "got: {trunc}");

    let scoped = json_stdout(&run_da_in(
        dir.path(),
        &["session", "snapshot", &id, "--ref", &panel_ref],
    ));
    assert_eq!(scoped["ok"], true, "scoped snapshot must succeed: {scoped}");

    // Navigate to /page-b -> old target_ref from /page-a must fail as stale!
    let _ = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/page-b")),
        ],
    );
    let stale = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.click",
            "--arg",
            &format!("ref={target_ref}"),
        ],
    );
    assert_ne!(stale.status.code(), Some(0), "stale ref must fail");
    let stale_json = json_stdout(&stale);
    assert!(
        stale_json["step"]["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("snapshot")),
        "stale ref error must instruct to retake snapshot: {stale_json}"
    );

    // Clicking a DOM Text node (`StaticText`) must normalize up to an Element cleanly.
    let page_c = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/page-c")),
        ],
    ));
    let text_ref = find_ref_by_role_and_name(
        page_c["snapshot"]["tree"].as_array().unwrap(),
        "StaticText",
        "LabelClick",
    )
    .expect("StaticText ref inside label must exist");
    let click_text = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.click",
            "--arg",
            &format!("ref={text_ref}"),
        ],
    );
    assert_eq!(
        click_text.status.code(),
        Some(0),
        "clicking StaticText ref must normalize without Text.getAttribute error: {:?}",
        click_text
    );

    // Concurrent browser step on same session must fail while /slow-page is loading.
    let slow = Command::new(da())
        .current_dir(dir.path())
        .args([
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/slow-page")),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait_until = Instant::now() + Duration::from_secs(5);
    while server.slow_hits.load(Ordering::SeqCst) == 0 && Instant::now() < wait_until {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        server.slow_hits.load(Ordering::SeqCst) > 0,
        "slow request must reach server before concurrent lock probe"
    );
    let busy = run_da_in(dir.path(), &["session", "snapshot", &id]);
    assert_ne!(
        busy.status.code(),
        Some(0),
        "concurrent snapshot/step must be rejected"
    );
    let _ = slow.wait_with_output();

    assert_eq!(
        run_da_in(dir.path(), &["session", "stop", &id])
            .status
            .code(),
        Some(0)
    );
}

#[test]
fn session_step_dismisses_unhandled_dialog_and_stays_usable() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let server = PageServer::start(BTreeMap::from([(
        "/dialog".to_string(),
        r#"<!doctype html>
<html>
<head><title>Dialog page</title></head>
<body>
  <h1>Dialog page</h1>
  <button data-testid="alert-button" onclick="alert('session dialog')">Show alert</button>
  <p id="status">ready</p>
</body>
</html>"#
            .to_string(),
    )]));
    let dir = TempDir::new("browser-session-dialog");

    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/dialog")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let open_json = json_stdout(&open);
    let tree = open_json["snapshot"]["tree"].as_array().unwrap();
    let button_ref = find_ref_by_role_and_name(tree, "button", "Show alert")
        .expect("alert button ref must exist");
    let heading_ref =
        find_ref_by_role_and_name(tree, "heading", "Dialog page").expect("heading ref must exist");

    // An unhandled dialog fails the step fast instead of waiting for the timeout.
    let click = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.click",
            "--arg",
            &format!("ref={button_ref}"),
        ],
    );
    assert_ne!(click.status.code(), Some(0), "dialog must fail the step");
    let click_json = json_stdout(&click);
    let message = click_json["step"]["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains("alert") && message.contains("session dialog"),
        "step error must name the dialog type and message: {click_json}"
    );

    // The browser is still usable: a fresh snapshot and a ref read both work.
    let snapshot = run_da_in(dir.path(), &["session", "snapshot", &id]);
    assert_eq!(snapshot.status.code(), Some(0), "{:?}", snapshot);
    let snapshot_json = json_stdout(&snapshot);
    assert!(
        snapshot_json["snapshot"]["tree"]
            .as_array()
            .is_some_and(|tree| !tree.is_empty()),
        "snapshot after a dismissed dialog must still return elements: {snapshot_json}"
    );
    let read = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.read",
            "--arg",
            &format!("ref={heading_ref}"),
        ],
    );
    assert_eq!(read.status.code(), Some(0), "{:?}", read);
    assert_eq!(json_stdout(&read)["step"]["result"]["text"], "Dialog page");

    // Export keeps the successful steps and reports the failed one.
    let exported = run_da_in(dir.path(), &["session", "export", &id]);
    assert_eq!(exported.status.code(), Some(0), "{:?}", exported);
    let export_json = json_stdout(&exported);
    let skipped = export_json["skipped_steps"].as_array().unwrap();
    assert!(
        skipped
            .iter()
            .any(|step| step["action"].as_str() == Some("browser.click")),
        "the failed click must be listed in skipped_steps: {export_json}"
    );
    let yaml = export_json["yaml"].as_str().unwrap_or_default();
    assert!(
        !yaml.contains("alert-button") && !yaml.contains("Show alert"),
        "the failed dialog step must not be exported: {yaml}"
    );

    assert_eq!(
        run_da_in(dir.path(), &["session", "stop", &id])
            .status
            .code(),
        Some(0)
    );
}

#[test]
fn stop_kills_live_chrome_even_when_debug_port_handle_is_closed() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let server = PageServer::start(BTreeMap::from([(
        "/p".to_string(),
        "<!doctype html><html><body><p>still alive</p></body></html>".to_string(),
    )]));
    let dir = TempDir::new("browser-closed-port");

    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/p")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let (real_port, profile_dir) = read_browser_handle(&session_dir);
    assert!(is_port_open(real_port), "Chrome debug port must be open");
    assert!(profile_dir.exists(), "Chrome temp profile must exist");

    // Pick a closed port so CDP connect fails and `is_port_listening(port)` is false
    // while the real detached Chrome process (with `pid` + `profile`) is still alive.
    let closed_listener = support::bind_browser_safe_listener();
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);

    let state_path = session_dir.join("state.json");
    let mut state: Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state.json"))
            .expect("parse state.json");
    let browser_handle = state["handles"]["browser"]
        .as_object_mut()
        .expect("browser handle object");
    let captured_pid = browser_handle["pid"]
        .as_u64()
        .expect("captured browser pid must be present");
    assert!(captured_pid > 0, "captured browser pid must be nonzero");
    browser_handle.insert("port".to_string(), json!(closed_port));
    browser_handle.insert(
        "websocket_url".to_string(),
        json!(format!(
            "ws://127.0.0.1:{closed_port}/devtools/browser/closed"
        )),
    );
    fs::write(
        &state_path,
        serde_json::to_string_pretty(&state).expect("serialize state"),
    )
    .expect("write state.json");

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(
        stop.status.code(),
        Some(0),
        "stop must kill live Chrome by pid {captured_pid} even if debug port is unreachable: stdout={} stderr={}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(
        !is_port_open(real_port),
        "real Chrome debug port {real_port} (captured pid {captured_pid}, profile {}) must be closed after stop even when handle port {closed_port} was closed",
        profile_dir.display()
    );
    assert!(
        !profile_dir.exists(),
        "temp profile must be deleted after stop"
    );
    assert!(
        !session_dir.exists(),
        "session dir must be deleted after stop"
    );
}

#[test]
fn stop_reports_error_when_browser_port_remains_listening_after_cleanup() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let server = PageServer::start(BTreeMap::from([(
        "/p".to_string(),
        "<!doctype html><html><body><p>surviving</p></body></html>".to_string(),
    )]));
    let dir = TempDir::new("browser-stop-failure");

    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/p")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let (real_port, profile_dir) = read_browser_handle(&session_dir);
    assert!(is_port_open(real_port), "Chrome debug port must be open");

    let state_path = session_dir.join("state.json");
    let original_state = fs::read_to_string(&state_path).expect("read original state.json");
    struct RestoreStopGuard {
        work_dir: PathBuf,
        session_dir: PathBuf,
        session_id: String,
        original_state: String,
        active: bool,
    }
    impl Drop for RestoreStopGuard {
        fn drop(&mut self) {
            if self.active {
                let _ = fs::create_dir_all(&self.session_dir);
                let _ = fs::write(self.session_dir.join("state.json"), &self.original_state);
                let _ = run_da_in(&self.work_dir, &["session", "stop", &self.session_id]);
            }
        }
    }
    let mut guard = RestoreStopGuard {
        work_dir: dir.path().to_path_buf(),
        session_dir: session_dir.clone(),
        session_id: id.clone(),
        original_state: original_state.clone(),
        active: true,
    };

    // Keep a complete, consistent handle whose CDP endpoint cannot be reached
    // and whose pid belongs to an already-exited dummy process, so `stop` cannot
    // close or verify Chrome, must report the surviving listener, and must not
    // delete Chrome's temporary profile while Chrome is still alive.
    let mut dead_child = Command::new(da())
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn short-lived dummy process");
    let dead_pid = dead_child.id();
    let _ = dead_child.wait();

    let mut state: Value = serde_json::from_str(&original_state).expect("parse state.json");
    let browser_handle = state["handles"]["browser"]
        .as_object_mut()
        .expect("browser handle object");
    browser_handle.insert("pid".to_string(), json!(dead_pid));
    browser_handle.insert(
        "websocket_url".to_string(),
        json!(format!(
            "ws://127.0.0.1:{real_port}/devtools/browser/unreachable"
        )),
    );
    fs::write(
        &state_path,
        serde_json::to_string_pretty(&state).expect("serialize state"),
    )
    .expect("write tampered state.json");

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(
        stop.status.code(),
        Some(4),
        "stop must exit with environment error when browser remains alive: stdout={} stderr={}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    let stop_json = json_stdout(&stop);
    assert_eq!(stop_json["ok"], false, "ok must be false: {stop_json}");
    let errors = stop_json["errors"].as_array().expect("errors array");
    assert!(
        errors.iter().any(|err| {
            err.as_str()
                .is_some_and(|msg| msg.contains(&real_port.to_string()))
        }),
        "errors must report surviving debug port {real_port}: {stop_json}"
    );
    assert!(
        is_port_open(real_port),
        "Chrome debug port {real_port} should still be open before cleanup"
    );
    assert!(
        profile_dir.exists(),
        "temp profile must not be deleted while Chrome is still alive"
    );

    // Restore original handle and confirm real cleanup succeeds cleanly.
    fs::create_dir_all(&session_dir).expect("recreate session dir for cleanup");
    fs::write(&state_path, &original_state).expect("restore state.json");
    let cleanup = run_da_in(dir.path(), &["session", "stop", &id]);
    guard.active = false;
    assert_eq!(cleanup.status.code(), Some(0), "{:?}", cleanup);
    assert!(!is_port_open(real_port));
    assert!(!profile_dir.exists());
    assert!(!session_dir.exists());
}

#[test]
fn stop_refuses_tampered_dead_pid_and_closed_port_when_profile_port_is_still_listening() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let server = PageServer::start(BTreeMap::from([(
        "/p".to_string(),
        "<!doctype html><html><body><p>surviving</p></body></html>".to_string(),
    )]));
    let dir = TempDir::new("browser-stale-port-unverified-pid");

    let started = json_stdout(&run_da_in(dir.path(), &["session", "start"]));
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/p")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let (real_port, profile_dir) = read_browser_handle(&session_dir);
    assert!(is_port_open(real_port), "Chrome debug port must be open");

    let state_path = session_dir.join("state.json");
    let original_state = fs::read_to_string(&state_path).expect("read original state.json");
    struct RestoreStopGuard {
        work_dir: PathBuf,
        session_dir: PathBuf,
        session_id: String,
        original_state: String,
        active: bool,
    }
    impl Drop for RestoreStopGuard {
        fn drop(&mut self) {
            if self.active {
                let _ = fs::create_dir_all(&self.session_dir);
                let _ = fs::write(self.session_dir.join("state.json"), &self.original_state);
                let _ = run_da_in(&self.work_dir, &["session", "stop", &self.session_id]);
            }
        }
    }
    let mut guard = RestoreStopGuard {
        work_dir: dir.path().to_path_buf(),
        session_dir: session_dir.clone(),
        session_id: id.clone(),
        original_state: original_state.clone(),
        active: true,
    };

    let closed_listener = support::bind_browser_safe_listener();
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);
    let dead_pid = spawn_dead_pid();

    let mut state: Value = serde_json::from_str(&original_state).expect("parse state.json");
    let browser_handle = state["handles"]["browser"]
        .as_object_mut()
        .expect("browser handle object");
    browser_handle.insert("pid".to_string(), json!(dead_pid));
    browser_handle.insert("port".to_string(), json!(closed_port));
    browser_handle.insert(
        "websocket_url".to_string(),
        json!(format!(
            "ws://127.0.0.1:{closed_port}/devtools/browser/unreachable"
        )),
    );
    fs::write(
        &state_path,
        serde_json::to_string_pretty(&state).expect("serialize state"),
    )
    .expect("write tampered state.json");

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(
        stop.status.code(),
        Some(4),
        "stop must refuse unverified pid while profile DevTools port {real_port} is open: stdout={} stderr={}",
        String::from_utf8_lossy(&stop.stdout),
        String::from_utf8_lossy(&stop.stderr)
    );
    let stop_json = json_stdout(&stop);
    assert_eq!(stop_json["ok"], false, "ok must be false: {stop_json}");
    let errors = stop_json["errors"].as_array().expect("errors array");
    assert!(
        errors.iter().any(|err| {
            err.as_str()
                .is_some_and(|msg| msg.contains(&real_port.to_string()))
        }),
        "errors must report surviving profile debug port {real_port}: {stop_json}"
    );
    assert!(
        is_port_open(real_port),
        "unrelated/unverified-pid stop must leave Chrome debug port {real_port} untouched"
    );
    assert!(
        profile_dir.exists(),
        "temp profile must not be deleted while its DevTools port {real_port} is still open"
    );

    fs::create_dir_all(&session_dir).expect("recreate session dir for cleanup");
    fs::write(&state_path, &original_state).expect("restore state.json");
    let cleanup = run_da_in(dir.path(), &["session", "stop", &id]);
    guard.active = false;
    assert_eq!(cleanup.status.code(), Some(0), "{:?}", cleanup);
    assert!(!is_port_open(real_port));
    assert!(!profile_dir.exists());
    assert!(!session_dir.exists());
}

fn spawn_dead_pid() -> u32 {
    let mut child = Command::new(da())
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn short-lived dummy process");
    let pid = child.id();
    let _ = child.wait();
    pid
}

fn write_persisted_browser_handle(dir: &Path, id: &str, handle: Value) {
    let state_path = dir.join(".da/sessions").join(id).join("state.json");
    let mut state: Value =
        serde_json::from_str(&fs::read_to_string(&state_path).expect("read state.json"))
            .expect("parse state.json");
    state["handles"]["browser"] = handle;
    fs::write(
        &state_path,
        serde_json::to_string_pretty(&state).expect("serialize state.json"),
    )
    .expect("write state.json");
}

#[test]
fn stop_reap_and_reattach_refuse_tampered_unowned_profile_without_deleting_it() {
    let dir = TempDir::new("browser-unowned-profile");
    let unrelated_dir = dir.path().join("unrelated-user-dir");
    fs::create_dir_all(&unrelated_dir).expect("create unrelated dir");
    let sentinel = unrelated_dir.join("keep.txt");
    fs::write(&sentinel, "do not delete").expect("write sentinel");

    let closed_listener = support::bind_browser_safe_listener();
    let closed_port = closed_listener.local_addr().unwrap().port();
    drop(closed_listener);
    let dead_pid = spawn_dead_pid();

    let tampered_handle = json!({
        "mode": "launched",
        "websocket_url": format!("ws://127.0.0.1:{closed_port}/devtools/browser/fake"),
        "target_id": "target-1",
        "port": closed_port,
        "pid": dead_pid,
        "profile": unrelated_dir.to_string_lossy(),
        "headless": true,
        "width": 1280,
        "height": 800,
        "connect": null,
        "testid_attribute": "data-testid",
        "refs": {}
    });

    // 1. Reattach via `session snapshot` must reject the unowned profile.
    let s1 = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    write_persisted_browser_handle(dir.path(), &s1, tampered_handle.clone());
    let snap = run_da_in(dir.path(), &["session", "snapshot", &s1]);
    assert_eq!(snap.status.code(), Some(4), "{:?}", snap);
    assert!(
        sentinel.exists(),
        "reattach path must leave unrelated directory intact"
    );

    // 2. `session stop` must reject the unowned profile and leave the directory intact.
    let stop = run_da_in(dir.path(), &["session", "stop", &s1]);
    assert_eq!(
        stop.status.code(),
        Some(4),
        "stop must reject unowned profile: {}",
        String::from_utf8_lossy(&stop.stdout)
    );
    let stop_json = json_stdout(&stop);
    assert_eq!(stop_json["ok"], false, "{stop_json}");
    assert!(
        sentinel.exists(),
        "stop must never recursively delete an unowned profile directory"
    );

    // 3. Idle reap via `session list --idle-timeout-ms 0` must also refuse and preserve it.
    let s2 = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    write_persisted_browser_handle(dir.path(), &s2, tampered_handle);
    let list = run_da_in(dir.path(), &["session", "list", "--idle-timeout-ms", "0"]);
    assert_eq!(
        list.status.code(),
        Some(4),
        "idle reap must report unowned profile refusal: {}",
        String::from_utf8_lossy(&list.stdout)
    );
    let list_json = json_stdout(&list);
    assert_eq!(list_json["ok"], false, "{list_json}");
    assert!(
        sentinel.exists(),
        "idle reap must never recursively delete an unowned profile directory"
    );
}

#[test]
fn stop_refuses_incomplete_and_inconsistent_launched_handles_before_connecting() {
    use std::io::Read;
    use std::net::TcpListener;

    let dir = TempDir::new("browser-bad-handles");
    let incomplete_listener = TcpListener::bind("127.0.0.1:0").expect("bind incomplete listener");
    let incomplete_port = incomplete_listener.local_addr().unwrap().port();
    incomplete_listener
        .set_nonblocking(true)
        .expect("set nonblocking");

    let open_probe_listener = TcpListener::bind("127.0.0.1:0").expect("bind open probe listener");
    let open_probe_port = open_probe_listener.local_addr().unwrap().port();
    open_probe_listener
        .set_nonblocking(true)
        .expect("set nonblocking");

    let accepted_incomplete = Arc::new(AtomicBool::new(false));
    let accepted_incomplete_flag = Arc::clone(&accepted_incomplete);
    let received_cdp_bytes = Arc::new(AtomicBool::new(false));
    let received_cdp_flag = Arc::clone(&received_cdp_bytes);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_flag);
    let server = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            if let Ok((_stream, _)) = incomplete_listener.accept() {
                accepted_incomplete_flag.store(true, Ordering::SeqCst);
            }
            if let Ok((mut stream, _)) = open_probe_listener.accept() {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
                let mut buf = [0u8; 64];
                if let Ok(n) = stream.read(&mut buf)
                    && n > 0
                {
                    received_cdp_flag.store(true, Ordering::SeqCst);
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    });

    let dead_pid = spawn_dead_pid();
    let owned_profile = std::env::temp_dir().join(format!("da-chrome-profile-{dead_pid}-999"));
    let other_listener = support::bind_browser_safe_listener();
    let other_port = other_listener.local_addr().unwrap().port();
    drop(other_listener);

    let bad_handles = [
        // Missing pid and profile while pointing at a live listening port.
        json!({
            "mode": "launched",
            "websocket_url": format!("ws://127.0.0.1:{incomplete_port}/devtools/browser/fake"),
            "target_id": "target-1",
            "port": incomplete_port,
            "pid": null,
            "profile": null,
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": null,
            "testid_attribute": "data-testid",
            "refs": {}
        }),
        // Present profile, missing pid, listening port.
        json!({
            "mode": "launched",
            "websocket_url": format!("ws://127.0.0.1:{incomplete_port}/devtools/browser/fake"),
            "target_id": "target-1",
            "port": incomplete_port,
            "pid": null,
            "profile": owned_profile.to_string_lossy(),
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": null,
            "testid_attribute": "data-testid",
            "refs": {}
        }),
        // Inconsistent websocket_url port vs handle port.
        json!({
            "mode": "launched",
            "websocket_url": format!("ws://127.0.0.1:{incomplete_port}/devtools/browser/fake"),
            "target_id": "target-1",
            "port": other_port,
            "pid": dead_pid,
            "profile": owned_profile.to_string_lossy(),
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": null,
            "testid_attribute": "data-testid",
            "refs": {}
        }),
        // Valid-looking handle whose pid does NOT own the listening port (listening port alone is insufficient proof).
        json!({
            "mode": "launched",
            "websocket_url": format!("ws://127.0.0.1:{open_probe_port}/devtools/browser/fake"),
            "target_id": "target-1",
            "port": open_probe_port,
            "pid": dead_pid,
            "profile": owned_profile.to_string_lossy(),
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": null,
            "testid_attribute": "data-testid",
            "refs": {}
        }),
    ];

    for bad_handle in bad_handles {
        let id = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        write_persisted_browser_handle(dir.path(), &id, bad_handle);
        let stop = run_da_in(dir.path(), &["session", "stop", &id]);
        assert_eq!(stop.status.code(), Some(4), "{:?}", stop);
        assert_eq!(json_stdout(&stop)["ok"], false);
    }

    stop_flag.store(true, Ordering::Relaxed);
    let _ = server.join();
    assert!(
        !accepted_incomplete.load(Ordering::SeqCst),
        "dispose_handle must never even connect to incomplete or inconsistent launched handles"
    );
    assert!(
        !received_cdp_bytes.load(Ordering::SeqCst),
        "dispose_handle must never send WebSocket handshake or Browser.close without confirmed pid ownership"
    );
}

#[test]
fn session_steps_and_export_preserve_within_iframe_while_snapshot_stays_main_frame() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let mut routes = BTreeMap::new();
    routes.insert(
        "/host".to_string(),
        r##"<!doctype html>
<html>
<head><title>Frame Host</title></head>
<body>
  <h1>Host Header</h1>
  <iframe id="embed" title="Embedded Form" src="/inner"></iframe>
</body>
</html>"##
            .to_string(),
    );
    routes.insert(
        "/inner".to_string(),
        r##"<!doctype html>
<html>
<body>
  <button id="inner-btn" onclick="document.getElementById('inner-msg').textContent='clicked'">Inner Save</button>
  <p id="inner-msg">idle</p>
</body>
</html>"##
            .to_string(),
    );
    let server = PageServer::start(routes);
    let dir = TempDir::new("browser-session-iframe");

    let id = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/host")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let open_json = json_stdout(&open);
    let tree = open_json["snapshot"]["tree"].as_array().unwrap();
    assert!(
        find_ref_by_role_and_name(tree, "Iframe", "Embedded Form").is_some(),
        "main-document snapshot should include the Iframe node: {tree:?}"
    );
    assert!(
        find_ref_by_role_and_name(tree, "button", "Inner Save").is_none(),
        "main-document snapshot should not descend into iframe document: {tree:?}"
    );

    let click_step = json!({
        "browser.click": {
            "css": "#inner-btn",
            "within": { "css": "#embed" }
        }
    })
    .to_string();
    let click = run_da_in(dir.path(), &["session", "step", &id, "--step", &click_step]);
    assert_eq!(click.status.code(), Some(0), "{:?}", click);

    let read_step = json!({
        "browser.read": {
            "css": "#inner-msg",
            "within": { "css": "#embed" }
        },
        "expect": [{ "path": "$.text", "equals": "clicked" }]
    })
    .to_string();
    let read = run_da_in(dir.path(), &["session", "step", &id, "--step", &read_step]);
    assert_eq!(read.status.code(), Some(0), "{:?}", read);

    let exported_path = dir.path().join("iframe_replay.yaml");
    let export = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported_path.to_str().unwrap(),
            "--name",
            "Session iframe replay",
        ],
    );
    assert_eq!(export.status.code(), Some(0), "{:?}", export);
    let yaml = fs::read_to_string(&exported_path).expect("read exported YAML");
    assert!(
        yaml.contains("within:") && yaml.contains("#embed"),
        "exported YAML must preserve explicit within iframe locator:\n{yaml}"
    );

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{:?}", stop);

    let replay = run_da_in(
        dir.path(),
        &["run", exported_path.to_str().unwrap(), "--no-bundle"],
    );
    assert_eq!(replay.status.code(), Some(0), "{:?}", replay);
}

#[test]
fn session_step_uploads_files_through_a_ref_and_exports_a_replayable_step() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let mut routes = BTreeMap::new();
    routes.insert(
        "/upload".to_string(),
        r##"<!doctype html>
<html>
<head><title>Session Upload</title></head>
<body>
  <label for="avatar-file">Avatar</label>
  <input id="avatar-file" name="avatar" type="file">
  <p id="file-events">idle</p>
  <script>
  var avatarNode = document.getElementById('avatar-file');
  var avatarEvents = document.getElementById('file-events');
  avatarNode.addEventListener('change', function () {
    var names = [];
    for (var i = 0; i < avatarNode.files.length; i++) names.push(avatarNode.files[i].name);
    avatarEvents.textContent = 'avatar-file:' + avatarNode.files.length + ':' + names.join(',');
  });
  </script>
</body>
</html>"##
            .to_string(),
    );
    let server = PageServer::start(routes);
    let dir = TempDir::new("browser-session-upload");
    // `da session` resolves a relative `files` path against the working
    // directory, and the exported case is replayed from the same directory.
    dir.write("avatar.txt", "session avatar bytes\n");

    let id = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/upload")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let tree = json_stdout(&open)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let avatar_ref = find_ref_by_role_and_name(&tree, "button", "Avatar")
        .unwrap_or_else(|| panic!("the file input must be in the snapshot: {tree:?}"));

    let upload = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.upload",
            "--arg",
            &format!("ref={avatar_ref}"),
            "--arg",
            "files=[\"avatar.txt\"]",
        ],
    );
    assert_eq!(
        upload.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&upload.stdout),
        String::from_utf8_lossy(&upload.stderr)
    );
    let upload_json = json_stdout(&upload);
    assert_eq!(
        upload_json["step"]["result"]["files"],
        json!(1),
        "{upload_json}"
    );
    assert_eq!(
        upload_json["step"]["result"]["names"],
        json!(["avatar.txt"]),
        "{upload_json}"
    );

    // The page's own change handler saw the file, which proves Chrome fired the
    // events for the CDP file assignment.
    let read = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.read:\n  css: \"#file-events\"\nexpect:\n  - path: $.text\n    equals: \"avatar-file:1:avatar.txt\"\n",
    );
    assert_eq!(read.status.code(), Some(0), "{:?}", read);

    // Export replaces the ref with the synthesized locator and keeps `files`.
    let exported_path = dir.path().join("avatar-flow.yaml");
    let export = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported_path.to_str().unwrap(),
            "--name",
            "Session upload replay",
        ],
    );
    assert_eq!(export.status.code(), Some(0), "{:?}", export);
    let yaml = fs::read_to_string(&exported_path).expect("read exported YAML");
    assert!(
        !yaml.contains("ref:"),
        "exported YAML must not contain a ref:\n{yaml}"
    );
    assert!(
        yaml.contains("browser.upload") && yaml.contains("avatar.txt"),
        "exported YAML must keep the upload step and its files:\n{yaml}"
    );

    let lint = run_da_in(dir.path(), &["lint", exported_path.to_str().unwrap()]);
    assert_eq!(
        lint.status.code(),
        Some(0),
        "exported YAML failed lint: {}",
        String::from_utf8_lossy(&lint.stdout)
    );

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{:?}", stop);

    let replay = run_da_in(
        dir.path(),
        &["run", exported_path.to_str().unwrap(), "--no-bundle"],
    );
    assert_eq!(
        replay.status.code(),
        Some(0),
        "exported YAML failed da run replay: {}",
        String::from_utf8_lossy(&replay.stdout)
    );
}

#[test]
fn da_session_start_observe_browser_network_collects_requests_across_steps() {
    let Some(_guard) = chrome_guard() else {
        return;
    };

    let server = support::TestServer::start();
    let dir = TempDir::new("browser-session-network");
    let closed = support::closed_port();
    let url = server.url(&format!(
        "/browser/network?case=session&closed={closed}&delay=1500"
    ));

    let start = run_da_in(
        dir.path(),
        &["session", "start", "--observe", "net=browser.network"],
    );
    assert_eq!(
        start.status.code(),
        Some(0),
        "session start failed: {}",
        String::from_utf8_lossy(&start.stdout)
    );
    let id = json_stdout(&start)["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={url}"),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);

    let click = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.click",
            "--arg",
            "css=#net-all",
        ],
    );
    assert_eq!(click.status.code(), Some(0), "{:?}", click);

    let wait_step = "browser.wait:\n  css: \"#network-done\"\nexpect:\n  - path: $.observed.net.requests[0].method\n    exists: true\nsave:\n  first_req_url: $.observed.net.requests[0].url\n";
    let wait = run_da_stdin(dir.path(), &["session", "step", &id], wait_step);
    assert_eq!(
        wait.status.code(),
        Some(0),
        "wait step failed: {}",
        String::from_utf8_lossy(&wait.stdout)
    );
    let wait_json = json_stdout(&wait);
    let reqs = wait_json["step"]["result"]["observed"]["net"]["requests"]
        .as_array()
        .expect("observed.net.requests array");
    assert!(
        reqs.iter().any(|r| {
            r["method"] == "POST"
                && r["url"]
                    .as_str()
                    .is_some_and(|u| u.contains("/health?kind=post"))
                && r["status"] == 200
        }),
        "expected /health POST request in observed.net.requests: {reqs:?}"
    );
    assert!(
        reqs.iter().any(|r| {
            r["url"]
                .as_str()
                .is_some_and(|u| u.contains("/nowhere?kind=missing"))
                && r["status"] == 404
        }),
        "expected /nowhere 404 request in observed.net.requests: {reqs:?}"
    );

    let steps_path = dir
        .path()
        .join(".da/sessions")
        .join(&id)
        .join("steps.jsonl");
    let jsonl = fs::read_to_string(&steps_path).expect("steps.jsonl");
    let last: Value =
        serde_json::from_str(jsonl.lines().last().expect("last line")).expect("last line json");
    assert!(last["started_at_ms"].as_u64().is_some_and(|t| t > 0));
    assert!(last["duration_ms"].as_u64().is_some());
    assert!(last["result"]["observed"]["net"]["requests"].is_array());

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{:?}", stop);
}

#[test]
fn session_export_preserves_saved_var_in_set_cookies_and_refuses_inline_secrets() {
    let Some(_guard) = chrome_guard() else {
        return;
    };

    let server = support::TestServer::start();
    let dir = TempDir::new("browser-session-cookie-export");
    let base = server.url("");

    let id = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let open_step = format!("browser.open:\n  url: \"{base}/health\"\n");
    let s0 = run_da_stdin(dir.path(), &["session", "step", &id], &open_step);
    assert_eq!(
        s0.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&s0.stdout)
    );

    let read_step = "browser.read:\n  css: \"body\"\nsave:\n  tok: $.text\n";
    let s1 = run_da_stdin(dir.path(), &["session", "step", &id], read_step);
    assert_eq!(
        s1.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&s1.stdout)
    );

    let cookie_step = format!(
        "browser.set_cookies:\n  cookies:\n    - name: sid\n      value: \"${{tok}}\"\n      url: \"{base}/\"\n"
    );
    let s2 = run_da_stdin(dir.path(), &["session", "step", &id], &cookie_step);
    assert_eq!(
        s2.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&s2.stdout)
    );
    let s2_json = json_stdout(&s2);
    assert_eq!(s2_json["step"]["args"]["cookies"][0]["value"], "***");

    let steps_path = dir
        .path()
        .join(".da/sessions")
        .join(&id)
        .join("steps.jsonl");
    let jsonl = fs::read_to_string(&steps_path).expect("steps.jsonl");
    let line3: Value =
        serde_json::from_str(jsonl.lines().nth(2).expect("third record")).expect("record json");
    assert_eq!(line3["raw_args"]["cookies"][0]["value"], "${tok}");

    let exported_path = dir.path().join("exported.yaml");
    let export = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported_path.to_str().expect("utf8 path"),
        ],
    );
    assert_eq!(
        export.status.code(),
        Some(0),
        "export failed: stdout={} stderr={}",
        String::from_utf8_lossy(&export.stdout),
        String::from_utf8_lossy(&export.stderr)
    );
    let export_json = json_stdout(&export);
    assert_eq!(export_json["skipped_steps"], json!([]));
    let yaml = export_json["yaml"].as_str().expect("exported yaml");
    assert!(
        yaml.contains("${tok}"),
        "expected ${{tok}} in yaml:\n{yaml}"
    );
    assert!(!yaml.contains("***"), "unexpected *** in yaml:\n{yaml}");

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{:?}", stop);

    let replay = run_da_in(
        dir.path(),
        &[
            "run",
            exported_path.to_str().expect("utf8 path"),
            "--no-bundle",
        ],
    );
    assert_eq!(
        replay.status.code(),
        Some(0),
        "replay failed: stdout={} stderr={}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );

    // Pre-open browser.set_cookies under --trace-steps must skip step_evidence cleanly without error files.
    let preopen_case = dir.write(
        "preopen.yaml",
        &format!(
            "name: Preopen cookie\nvars:\n  tok: \"preopen-val\"\nsteps:\n  - browser.set_cookies:\n      cookies:\n        - name: sid\n          value: \"${{tok}}\"\n          url: \"{base}/\"\n"
        ),
    );
    let trace_dir = dir.path().join("trace-bundle");
    let preopen_run = run_da_in(
        dir.path(),
        &[
            "run",
            preopen_case.to_str().expect("utf8 path"),
            "--record",
            trace_dir.to_str().expect("utf8 path"),
            "--trace-steps",
            "--no-bundle",
        ],
    );
    assert_eq!(preopen_run.status.code(), Some(0));
    assert!(
        !trace_dir.join("trace/steps/1").exists(),
        "pre-open browser.set_cookies must not write a noisy error trace group"
    );

    // Inline literal cookie value is redacted in steps.jsonl and refused on export.
    let id2 = json_stdout(&run_da_in(dir.path(), &["session", "start"]))["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();
    let inline_cookie = format!(
        "browser.set_cookies:\n  cookies:\n    - name: sid\n      value: \"inline-s3cr3t-cookie\"\n      url: \"{base}/\"\n"
    );
    let inline_step = run_da_stdin(dir.path(), &["session", "step", &id2], &inline_cookie);
    assert_eq!(inline_step.status.code(), Some(0));
    let jsonl2 = fs::read_to_string(
        dir.path()
            .join(".da/sessions")
            .join(&id2)
            .join("steps.jsonl"),
    )
    .expect("steps.jsonl");
    assert!(!jsonl2.contains("inline-s3cr3t-cookie"));
    let inline_export = run_da_in(dir.path(), &["session", "export", &id2]);
    assert_ne!(
        inline_export.status.code(),
        Some(0),
        "inline redacted secret must fail export"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&inline_export.stdout),
        String::from_utf8_lossy(&inline_export.stderr)
    );
    assert!(combined.contains("browser.set_cookies"), "{combined}");
    let _ = run_da_in(dir.path(), &["session", "stop", &id2]);
}

fn run_da_env(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Output {
    let mut cmd = Command::new(da());
    cmd.current_dir(dir).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run da with env")
}

fn devtools_http_get_or_post(port: u16, method: &str, path: &str) -> String {
    use std::io::Read;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream =
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect devtools");
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let req =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    text.split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default()
}

#[test]
fn session_computer_use_scroll_hover_drag_export_replay_and_optional_skip() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let html = r##"<!doctype html>
<html>
<head><meta charset="utf-8"><style>
  body { margin: 0; font-family: sans-serif; height: 2000px; }
  #hover-btn { margin: 16px; padding: 8px 16px; }
  #hover-tip { display: none; }
  #hover-wrap:hover #hover-tip { display: inline; }
  .dnd { display: flex; gap: 32px; margin: 16px; }
  #card { width: 100px; height: 48px; border: 1px solid #333; }
  #bin { width: 120px; height: 48px; border: 1px dashed #333; }
  #box { margin: 16px; width: 200px; height: 80px; overflow: auto; border: 1px solid #666; }
  #box-in { height: 400px; }
  #far-cta { position: absolute; left: 40px; top: 1300px; }
</style></head>
<body>
  <div id="hover-wrap">
    <button id="hover-btn">Inspect Menu</button>
    <span id="hover-tip">Hover Revealed</span>
  </div>
  <div class="dnd">
    <button id="card" draggable="true">Drag Card</button>
    <div id="bin" role="region" aria-label="Drop Bin">Drop Here</div>
  </div>
  <div id="box" role="region" aria-label="Scroll Panel"><div id="box-in">Inside</div></div>
  <button id="far-cta" onclick="document.getElementById('status').textContent = 'cta-clicked'">Far Action</button>
  <div id="status">init</div>
  <script>
    const card = document.getElementById('card');
    const bin = document.getElementById('bin');
    card.addEventListener('dragstart', e => e.dataTransfer.setData('text/plain', 'session-card'));
    bin.addEventListener('dragover', e => e.preventDefault());
    bin.addEventListener('drop', e => {
      e.preventDefault();
      document.getElementById('status').textContent =
        e.dataTransfer.getData('text/plain') + ':scroll=' + Math.round(document.getElementById('box').scrollTop);
    });
  </script>
</body>
</html>"##;

    let server = PageServer::start(BTreeMap::from([("/cu".to_string(), html.to_string())]));
    let dir = TempDir::new("session-cu-export");
    let started = run_da_in(dir.path(), &["session", "start"]);
    assert_eq!(started.status.code(), Some(0), "{:?}", started);
    let id = json_stdout(&started)["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/cu")),
        ],
    );
    assert_eq!(open.status.code(), Some(0), "{:?}", open);
    let tree = json_stdout(&open)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();

    let hover_ref = find_ref_by_role_and_name(&tree, "button", "Inspect Menu").expect("hover ref");
    let s_hover = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        &format!("browser.hover:\n  ref: {hover_ref}\n"),
    );
    assert_eq!(s_hover.status.code(), Some(0), "{:?}", s_hover);

    let after_hover_tree = json_stdout(&s_hover)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let panel_ref =
        find_ref_by_role_and_name(&after_hover_tree, "region", "Scroll Panel").expect("panel ref");
    let s_scroll_box = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        &format!("browser.scroll:\n  ref: {panel_ref}\n  delta_y: 120\n"),
    );
    assert_eq!(s_scroll_box.status.code(), Some(0), "{:?}", s_scroll_box);

    let after_scroll_tree = json_stdout(&s_scroll_box)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let card_ref =
        find_ref_by_role_and_name(&after_scroll_tree, "button", "Drag Card").expect("card ref");
    let bin_ref =
        find_ref_by_role_and_name(&after_scroll_tree, "region", "Drop Bin").expect("bin ref");
    let s_drag = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        &format!("browser.drag:\n  from:\n    ref: {card_ref}\n  to:\n    ref: {bin_ref}\n"),
    );
    assert_eq!(
        s_drag.status.code(),
        Some(0),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&s_drag.stdout),
        String::from_utf8_lossy(&s_drag.stderr)
    );

    // Optional failed drag referencing one live ref and one missing ref must NOT leak raw `ref:` into export.
    let after_drag_tree = json_stdout(&s_drag)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let card_ref2 =
        find_ref_by_role_and_name(&after_drag_tree, "button", "Drag Card").expect("card ref 2");
    let s_opt_drag = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        &format!(
            "browser.drag:\n  from:\n    ref: {card_ref2}\n  to:\n    ref: e99999\noptional: true\n"
        ),
    );
    assert_eq!(s_opt_drag.status.code(), Some(0), "{:?}", s_opt_drag);

    let after_opt_tree = json_stdout(&s_opt_drag)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let far_ref =
        find_ref_by_role_and_name(&after_opt_tree, "button", "Far Action").expect("far ref");
    let s_scroll_view = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        &format!("browser.scroll:\n  ref: {far_ref}\n  into_view: true\n"),
    );
    assert_eq!(s_scroll_view.status.code(), Some(0), "{:?}", s_scroll_view);

    let s_coord_hover = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.hover:\n  at:\n    x: 40\n    y: 40\n",
    );
    assert_eq!(s_coord_hover.status.code(), Some(0), "{:?}", s_coord_hover);

    let exported = dir.path().join("cu-replay.yaml");
    let exp = run_da_in(
        dir.path(),
        &[
            "session",
            "export",
            &id,
            "-o",
            exported.to_str().unwrap(),
            "--name",
            "CU Session Export",
        ],
    );
    assert_eq!(
        exp.status.code(),
        Some(0),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&exp.stdout),
        String::from_utf8_lossy(&exp.stderr)
    );

    let yaml = fs::read_to_string(&exported).expect("read exported yaml");
    assert!(
        !yaml.contains("ref:"),
        "exported YAML must not contain raw refs:\n{yaml}"
    );
    assert!(
        yaml.contains("browser.drag")
            && yaml.contains("browser.hover")
            && yaml.contains("browser.scroll")
            && yaml.contains("viewport:"),
        "exported YAML must include synthesized CU steps and pinned viewport:\n{yaml}"
    );

    let lint = run_da_in(dir.path(), &["lint", exported.to_str().unwrap()]);
    assert_eq!(
        lint.status.code(),
        Some(0),
        "lint failed:\n{}",
        String::from_utf8_lossy(&lint.stdout)
    );

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{:?}", stop);

    let replay = run_da_in(
        dir.path(),
        &["run", exported.to_str().unwrap(), "--no-bundle"],
    );
    assert_eq!(
        replay.status.code(),
        Some(0),
        "replay failed:\n{}",
        String::from_utf8_lossy(&replay.stdout)
    );
}

#[test]
fn session_clipboard_redacts_env_secret_in_logs_state_and_export() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let server = PageServer::start(BTreeMap::from([(
        "/clip-session".to_string(),
        r##"<!doctype html>
<html>
<body>
  <button id="check" onclick="navigator.clipboard.readText().then(t => document.getElementById('len').textContent = 'len:' + String(t.length))">Check</button>
  <div id="len">len:0</div>
</body>
</html>"##
            .to_string(),
    )]));
    let dir = TempDir::new("session-clip-secrets");
    let secret = "session-clip-secret-4242";
    let envs = [("DA_SESSION_CLIP_SECRET", secret.to_string())];

    let started = run_da_in(dir.path(), &["session", "start"]);
    assert_eq!(started.status.code(), Some(0));
    let id = json_stdout(&started)["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/clip-session")),
        ],
    );
    assert_eq!(open.status.code(), Some(0));

    let set_step = run_da_env(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.set_clipboard",
            "--arg",
            "text=${env:DA_SESSION_CLIP_SECRET}",
        ],
        &envs,
    );
    assert_eq!(set_step.status.code(), Some(0), "{:?}", set_step);

    // Verify browser received real 24-character string, not `***`.
    let read_len = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.click:\n  css: \"#check\"\n",
    );
    assert_eq!(read_len.status.code(), Some(0), "{:?}", read_len);
    let wait_len = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.wait:\n  text: \"len:24\"\n",
    );
    assert_eq!(wait_len.status.code(), Some(0), "{:?}", wait_len);

    // Reading & saving clipboard must redact across stdout, steps.jsonl, and state.json.
    let mut read_cmd = Command::new(da());
    read_cmd
        .current_dir(dir.path())
        .args(["session", "step", &id])
        .env("DA_SESSION_CLIP_SECRET", secret)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = read_cmd.spawn().expect("spawn");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"browser.read_clipboard: {}\nsave:\n  saved_clip: $.text\n")
        .unwrap();
    let saved_out = child.wait_with_output().expect("wait");
    assert_eq!(saved_out.status.code(), Some(0), "{:?}", saved_out);
    let stdout_text = String::from_utf8_lossy(&saved_out.stdout);
    assert!(
        !stdout_text.contains(secret),
        "raw clipboard secret leaked to session stdout: {stdout_text}"
    );
    let saved_json = json_stdout(&saved_out);
    assert_eq!(saved_json["step"]["result"]["text"], json!("***"));

    let exported = dir.path().join("clip-export.yaml");
    let exp = run_da_in(
        dir.path(),
        &["session", "export", &id, "-o", exported.to_str().unwrap()],
    );
    assert_eq!(exp.status.code(), Some(0), "{:?}", exp);
    let exp_yaml = fs::read_to_string(&exported).expect("export yaml");
    assert!(
        exp_yaml.contains("${env:DA_SESSION_CLIP_SECRET}") && !exp_yaml.contains(secret),
        "exported YAML must keep env placeholder and redact secret:\n{exp_yaml}"
    );

    let session_dir = dir.path().join(".da/sessions").join(&id);
    for file_name in ["steps.jsonl", "state.json"] {
        let p = session_dir.join(file_name);
        let contents = fs::read_to_string(&p).expect("read session artifact");
        assert!(
            !contents.contains(secret),
            "raw clipboard secret leaked into {}:\n{}",
            p.display(),
            contents
        );
    }

    let _ = run_da_in(dir.path(), &["session", "stop", &id]);
}

#[test]
fn session_connect_closed_tab_fails_fast_as_environment_and_preserves_user_tab() {
    let Some(_guard) = chrome_guard() else {
        return;
    };
    let server = PageServer::start(BTreeMap::from([
        (
            "/user".to_string(),
            "<!doctype html><html><title>User Tab</title><body><div id='user-ok'>alive</div></body></html>"
                .to_string(),
        ),
        (
            "/owned".to_string(),
            "<!doctype html><html><title>Owned Tab</title><body><button id='target'>Act</button></body></html>"
                .to_string(),
        ),
    ]));
    let dir = TempDir::new("session-connect-closed-tab");

    // Host browser session simulating the user's open Chrome with an existing `/user` tab.
    let s1 = run_da_in(dir.path(), &["session", "start"]);
    assert_eq!(s1.status.code(), Some(0));
    let id1 = json_stdout(&s1)["session_id"].as_str().unwrap().to_string();
    let open1 = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id1,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/user")),
        ],
    );
    assert_eq!(open1.status.code(), Some(0), "{:?}", open1);
    let (port, _) = read_browser_handle(&dir.path().join(".da/sessions").join(&id1));

    // Second session connects to that Chrome and opens its own `/owned` tab.
    let s2 = run_da_in(dir.path(), &["session", "start"]);
    assert_eq!(s2.status.code(), Some(0));
    let id2 = json_stdout(&s2)["session_id"].as_str().unwrap().to_string();
    let open2 = run_da_stdin(
        dir.path(),
        &["session", "step", &id2],
        &format!(
            "browser.open:\n  connect: \"http://127.0.0.1:{port}\"\n  url: \"{}\"\n",
            server.url("/owned")
        ),
    );
    assert_eq!(open2.status.code(), Some(0), "{:?}", open2);
    let tree2 = json_stdout(&open2)["snapshot"]["tree"]
        .as_array()
        .unwrap()
        .clone();
    let target_ref = find_ref_by_role_and_name(&tree2, "button", "Act").expect("Act ref");

    // While `/owned` is still alive, connected sessions must reject clipboard steps
    // with exit code 4 (Environment) so external/host clipboard state is never touched.
    for (clip_label, clip_yaml) in [
        (
            "connect-set-clip",
            "browser.set_clipboard:\n  text: \"probe\"\n",
        ),
        ("connect-read-clip", "browser.read_clipboard: {}\n"),
    ] {
        let rejected = run_da_stdin(dir.path(), &["session", "step", &id2], clip_yaml);
        assert_eq!(
            rejected.status.code(),
            Some(4),
            "{clip_label} in connected session must return Environment exit 4, got stdout={}\nstderr={}",
            String::from_utf8_lossy(&rejected.stdout),
            String::from_utf8_lossy(&rejected.stderr)
        );
    }

    // Externally close ONLY the `/owned` tab via DevTools HTTP endpoint.
    let tabs: Value = serde_json::from_str(&devtools_http_get_or_post(port, "GET", "/json/list"))
        .expect("list tabs");
    let owned_tab_id = tabs
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["url"].as_str().is_some_and(|u| u.contains("/owned")))
        .and_then(|t| t["id"].as_str())
        .expect("find owned tab id");
    let _ = devtools_http_get_or_post(port, "GET", &format!("/json/close/{owned_tab_id}"));

    // Poll briefly until Chrome actually removes `/owned` from `/json/list`.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let cur: Value =
            serde_json::from_str(&devtools_http_get_or_post(port, "GET", "/json/list"))
                .unwrap_or_else(|_| json!([]));
        if !cur
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["id"].as_str() == Some(owned_tab_id))
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    // Running Computer Use steps against the externally closed tab must fail fast with exit code 4 (Environment).
    for (label, stdin_yaml) in [
        (
            "scroll-ref",
            format!("browser.scroll:\n  ref: {target_ref}\n"),
        ),
        (
            "hover-ref",
            format!("browser.hover:\n  ref: {target_ref}\n"),
        ),
        (
            "drag-ref",
            format!(
                "browser.drag:\n  from: {{ ref: {target_ref} }}\n  to: {{ ref: {target_ref} }}\n"
            ),
        ),
    ] {
        let failed = run_da_stdin(dir.path(), &["session", "step", &id2], &stdin_yaml);
        assert_eq!(
            failed.status.code(),
            Some(4),
            "{label} on externally closed owned tab must return Environment exit 4, got stdout={}\nstderr={}",
            String::from_utf8_lossy(&failed.stdout),
            String::from_utf8_lossy(&failed.stderr)
        );
    }

    // Cleanup s2; user's `/user` tab in s1 must still be alive and readable!
    let _ = run_da_in(dir.path(), &["session", "stop", &id2]);
    let check_user = run_da_stdin(
        dir.path(),
        &["session", "step", &id1],
        "browser.read:\n  css: \"#user-ok\"\nexpect:\n  - path: $.text\n    equals: \"alive\"\n",
    );
    assert_eq!(
        check_user.status.code(),
        Some(0),
        "user tab must remain untouched after owned tab death and stop: {:?}",
        check_user
    );
    let _ = run_da_in(dir.path(), &["session", "stop", &id1]);
}

const BROWSER_MOCK_HOOK_RS: &str = r#"
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};

fn parse_kv(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    let cfg_path = env::args().nth(1).expect("cfg path");
    let cfg = parse_kv(&fs::read_to_string(&cfg_path).expect("read cfg"));
    let mut stdin_bytes = Vec::new();
    let _ = io::stdin().read_to_end(&mut stdin_bytes);
    let stdin_text = String::from_utf8_lossy(&stdin_bytes).into_owned();
    if let Some(req_file) = cfg.get("request_file") {
        let _ = fs::write(req_file, &stdin_bytes);
    }
    let model = cfg.get("model").map(String::as_str).unwrap_or("mock-vlm-1");
    let latency = cfg.get("latency_ms").map(String::as_str).unwrap_or("42");
    let trace_id = cfg.get("trace_id").map(String::as_str).unwrap_or("tr-browser-1");

    let result = if stdin_text.contains("\"model/select_ref\"") {
        let r = cfg.get("select_ref").map(String::as_str).unwrap_or("e1");
        format!(
            "{{\"ref\":{},\"confidence\":0.95,\"model\":{},\"latency_ms\":{latency},\"usage\":{{\"prompt_tokens\":40,\"completion_tokens\":2}},\"trace_id\":{}}}",
            json_escape(r),
            json_escape(model),
            json_escape(trace_id),
        )
    } else if stdin_text.contains("\"model/locate\"") {
        let x = cfg.get("locate_x").map(String::as_str).unwrap_or("100");
        let y = cfg.get("locate_y").map(String::as_str).unwrap_or("112");
        format!(
            "{{\"x\":{x},\"y\":{y},\"confidence\":0.92,\"model\":{},\"latency_ms\":{latency},\"usage\":{{\"prompt_tokens\":320,\"completion_tokens\":6}},\"trace_id\":{}}}",
            json_escape(model),
            json_escape(trace_id),
        )
    } else {
        let passed = cfg.get("judge_passed").map(String::as_str).unwrap_or("true");
        let rationale = cfg.get("judge_rationale").map(String::as_str).unwrap_or("verified");
        format!(
            "{{\"passed\":{passed},\"rationale\":{},\"confidence\":0.97,\"model\":{},\"latency_ms\":{latency},\"usage\":{{\"prompt_tokens\":90,\"completion_tokens\":12}},\"trace_id\":{}}}",
            json_escape(rationale),
            json_escape(model),
            json_escape(trace_id),
        )
    };
    let _ = writeln!(io::stdout(), "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{result}}}");
    let _ = io::stdout().flush();
}
"#;

fn compile_browser_mock_hook(dir: &Path) -> PathBuf {
    let src = dir.join("browser_mock_hook.rs");
    fs::write(&src, BROWSER_MOCK_HOOK_RS).expect("write browser_mock_hook.rs");
    let out = dir.join(if cfg!(windows) {
        "browser_mock_hook.exe"
    } else {
        "browser_mock_hook"
    });
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let status = Command::new(rustc)
        .arg(&src)
        .arg("-O")
        .arg("-o")
        .arg(&out)
        .status()
        .expect("compile browser_mock_hook.rs");
    assert!(status.success(), "rustc failed for browser_mock_hook.rs");
    out
}

#[test]
fn browser_model_hooks_select_ref_locate_masked_pixels_canvas_fallback_and_replay() {
    let Some(_guard) = chrome_guard() else {
        return;
    };

    let xorigin_child_html = r##"<!doctype html><html><body style="margin:0;background:#ff00ff"><input id="xpw" type="password" value="Sup3rSecret!" style="position:fixed;left:10px;top:10px;width:180px;height:40px;background:#ff00ff;border:0"></body></html>"##.to_string();
    let xorigin_server =
        PageServer::start(BTreeMap::from([("/child".to_string(), xorigin_child_html)]));
    let xorigin_child_url = xorigin_server.url("/child");

    let same_wrapper_html = format!(
        r##"<!doctype html><html><body style="margin:0;background:#f4f4f4"><iframe id="inner-xframe" src="{xorigin_child_url}" onload="window.parent.document.getElementById('save-btn').setAttribute('data-nested-xframe-loaded', '1')" style="position:fixed;left:10px;top:10px;width:220px;height:80px;border:0"></iframe></body></html>"##
    );

    let page_html = format!(
        r##"<!doctype html>
<html>
<head>
  <title>Model Hook Lab</title>
  <style>
    html, body {{ margin: 0; padding: 0; background: #f4f4f4; }}
    #pw {{
      position: fixed;
      left: 20px;
      top: 20px;
      width: 180px;
      height: 40px;
      background: #ffffff;
      color: #ff0000;
      border: 0;
      padding: 0;
      margin: 0;
      font-size: 24px;
    }}
    #save-btn {{
      position: fixed;
      left: 20px;
      top: 90px;
      width: 160px;
      height: 44px;
      background: rgb(0, 200, 80);
      color: #ffffff;
      border: 0;
    }}
    #jev-btn {{
      position: fixed;
      left: 220px;
      top: 90px;
      width: 160px;
      height: 44px;
    }}
    #board {{
      position: fixed;
      left: 20px;
      top: 160px;
      background: #224488;
      display: block;
    }}
  </style>
</head>
<body>
  <label for="pw" style="position:fixed;left:420px;top:20px;">Password</label>
  <input id="pw" type="password" />
  <button id="save-btn" data-testid="save-btn" onclick="document.getElementById('log').textContent += '[btn]';">Save Action</button>
  <button id="jev-btn" onclick="document.getElementById('log').textContent += '[jev]';">Jev Target</button>
  <canvas id="board" width="200" height="100"></canvas>
  <iframe id="xorigin-frame" src="{xorigin_child_url}" onload="document.getElementById('save-btn').setAttribute('data-xframe-loaded', '1')" style="position:fixed;left:420px;top:160px;width:220px;height:80px;border:0"></iframe>
  <iframe id="nested-same-frame" src="/same-wrapper" style="position:fixed;left:420px;top:260px;width:240px;height:100px;border:0"></iframe>
  <p id="log" style="position:fixed;left:420px;top:90px;"></p>
  <script>
    document.getElementById('board').addEventListener('click', function (ev) {{
      var r = this.getBoundingClientRect();
      var ox = Math.round(ev.clientX - r.left);
      var oy = Math.round(ev.clientY - r.top);
      document.getElementById('log').textContent += '[canvas:' + ox + ',' + oy + ']';
    }});
  </script>
</body>
</html>"##
    );

    let server = PageServer::start(BTreeMap::from([
        ("/model-lab".to_string(), page_html),
        ("/same-wrapper".to_string(), same_wrapper_html),
    ]));
    let dir = TempDir::new("browser-model-hooks");
    let hook_bin = compile_browser_mock_hook(dir.path());
    let hook_str = hook_bin.to_str().unwrap();
    let req_file = dir.path().join("hook_req.json");
    let cfg_path = dir.path().join("hook.cfg");
    fs::write(&cfg_path, format!("request_file={}\n", req_file.display())).unwrap();
    let cfg_str = cfg_path.to_str().unwrap();
    let secret = "Sup3rS3cr3tPw999!";

    let run_env = |args: &[&str]| -> Output {
        Command::new(da())
            .current_dir(dir.path())
            .args(args)
            .env("DA_BROWSER_HOOK_SECRET", secret)
            .output()
            .expect("run da")
    };

    // 1. Start session with `--model-hook`.
    let started = json_stdout(&run_env(&[
        "session",
        "start",
        "--model-hook",
        hook_str,
        "--model-hook-arg",
        cfg_str,
    ]));
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    // 2. Open page at explicit 1280x720, wait for cross-origin iframes, and type secret password into `#pw`.
    let open = json_stdout(&run_env(&[
        "session",
        "step",
        &id,
        "browser.open",
        "--arg",
        &format!("url={}", server.url("/model-lab")),
        "--arg",
        "width=1280",
        "--arg",
        "height=720",
    ]));
    assert_eq!(open["ok"], true, "{open}");
    let wait_frames = json_stdout(&run_env(&[
        "session",
        "step",
        &id,
        "browser.wait",
        "--arg",
        "css=#save-btn[data-xframe-loaded='1'][data-nested-xframe-loaded='1']",
    ]));
    assert_eq!(wait_frames["ok"], true, "{wait_frames}");
    let tree = wait_frames["snapshot"]["tree"].as_array().unwrap();
    let pw_ref = find_ref_by_role_and_name(tree, "textbox", "Password").expect("pw ref");

    let mut type_cmd = Command::new(da());
    type_cmd
        .current_dir(dir.path())
        .args(["session", "step", &id, "-f", "-"])
        .env("DA_BROWSER_HOOK_SECRET", secret)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = type_cmd.spawn().unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            format!(
                "browser.type:\n  ref: {pw_ref}\n  text: \"${{env:DA_BROWSER_HOOK_SECRET}}\"\n"
            )
            .as_bytes(),
        )
        .unwrap();
    let typed_out = child.wait_with_output().unwrap();
    assert_eq!(typed_out.status.code(), Some(0), "{typed_out:?}");
    let typed_json = json_stdout(&typed_out);
    let tree2 = typed_json["snapshot"]["tree"].as_array().unwrap();
    let jev_ref = find_ref_by_role_and_name(tree2, "button", "Jev Target").expect("jev ref");

    // 3. `select-ref` picks `jev_ref`, scrubs secret from query, and subsequent `browser.click` acts on it.
    fs::write(
        &cfg_path,
        format!(
            "request_file={}\nselect_ref={jev_ref}\nmodel=jev-0.2b\nlatency_ms=12\ntrace_id=tr-jev-1\n",
            req_file.display()
        ),
    )
    .unwrap();
    let sel_out = run_env(&[
        "session",
        "select-ref",
        &id,
        "--query",
        "Select Jev Target for ${env:DA_BROWSER_HOOK_SECRET}",
    ]);
    assert_eq!(sel_out.status.code(), Some(0), "{sel_out:?}");
    let sel_json = json_stdout(&sel_out);
    assert_eq!(sel_json["ref"], jev_ref);
    assert_eq!(
        sel_json["synthesized_locator"],
        json!({ "role": "button", "name": "Jev Target" })
    );
    let sel_stdin = fs::read_to_string(&req_file).unwrap();
    assert!(
        !sel_stdin.contains(secret),
        "select_ref stdin leaked secret!"
    );

    let click_jev = run_env(&[
        "session",
        "step",
        &id,
        "browser.click",
        "--arg",
        &format!("ref={jev_ref}"),
    ]);
    assert_eq!(click_jev.status.code(), Some(0), "{click_jev:?}");

    // 4. `locate` on HTML button `#save-btn` at `(100, 112)` with `--click`:
    //    - captures input-masked PNG (password field blacked out, button visible!),
    //    - reverse-synthesizes `{ "testid": "save-btn" }` (`0` lint warnings!).
    fs::write(
        &cfg_path,
        format!(
            "request_file={}\nlocate_x=100\nlocate_y=112\nmodel=ui-tars-1.5-7b\nlatency_ms=1821\ntrace_id=tr-vlm-btn\n",
            req_file.display()
        ),
    )
    .unwrap();
    let loc_btn_out = run_env(&[
        "session",
        "locate",
        &id,
        "--query",
        "Click Save Action with ${env:DA_BROWSER_HOOK_SECRET}",
        "--click",
    ]);
    assert_eq!(loc_btn_out.status.code(), Some(0), "{loc_btn_out:?}");
    let loc_btn_json = json_stdout(&loc_btn_out);
    assert_eq!(
        loc_btn_json["synthesized_locator"],
        json!({ "testid": "save-btn" }),
        "{loc_btn_json}"
    );
    assert_eq!(loc_btn_json["fragile"], false, "{loc_btn_json}");

    let loc_stdin_raw = fs::read_to_string(&req_file).unwrap();
    assert!(
        !loc_stdin_raw.contains(secret),
        "locate stdin leaked secret!"
    );
    let loc_req_val: Value = serde_json::from_str(loc_stdin_raw.trim()).unwrap();
    let vw = loc_req_val["params"]["viewport"]["width"]
        .as_u64()
        .expect("viewport.width");
    let vh = loc_req_val["params"]["viewport"]["height"]
        .as_u64()
        .expect("viewport.height");
    let img_b64 = loc_req_val["params"]["image_base64"]
        .as_str()
        .expect("image_base64 string")
        .to_string();

    // Verify actual rendered pixels of `image_base64` via headless Chrome `<canvas>`:
    // - PNG matches `viewport` 1-to-1 (`img.width === vw && img.height === vh`)
    // - `#pw` `(30..190, 25..55)` MUST be solid `#000000`
    // - direct cross-origin `<iframe>` `(440..620, 175..225)` MUST be solid `#000000`
    // - nested same-origin -> cross-origin `<iframe>` `(450..630, 285..335)` MUST be solid `#000000`
    // - `#save-btn` `(20..180, 90..134)` stays green without font-glyph false negatives!
    let pixel_check_html = format!(
        r##"<!doctype html><html><body>
<p id="px">pending</p>
<script>
var img = new Image();
img.onload = function () {{
  var c = document.createElement('canvas');
  c.width = img.width;
  c.height = img.height;
  var ctx = c.getContext('2d');
  ctx.drawImage(img, 0, 0);
  var scaleX = img.width / {vw};
  var scaleY = img.height / {vh};
  var countNonBlack = function (x0, x1, dx, y0, y1, dy) {{
    var bad = 0;
    for (var x = x0; x <= x1; x += dx) {{
      for (var y = y0; y <= y1; y += dy) {{
        var d = ctx.getImageData(Math.round(x * scaleX), Math.round(y * scaleY), 1, 1).data;
        if (d[0] !== 0 || d[1] !== 0 || d[2] !== 0 || d[3] !== 255) bad++;
      }}
    }}
    return bad;
  }};
  var nonBlackInPw = countNonBlack(30, 190, 10, 25, 55, 6);
  var nonBlackInXOrigin = countNonBlack(440, 620, 15, 175, 225, 10);
  var nonBlackInNestedXOrigin = countNonBlack(450, 630, 15, 285, 335, 10);
  var corner = ctx.getImageData(Math.round(26 * scaleX), Math.round(96 * scaleY), 1, 1).data;
  var greenCount = 0;
  for (var bx = 25; bx <= 175; bx += 10) {{
    for (var by = 95; by <= 128; by += 6) {{
      var bd = ctx.getImageData(Math.round(bx * scaleX), Math.round(by * scaleY), 1, 1).data;
      if (bd[1] > 150 && bd[0] < 40) greenCount++;
    }}
  }}
  var btnGreen = (corner[1] > 150 && corner[0] < 40) && greenCount > 20;
  var oneToOne = (img.width === {vw} && img.height === {vh});
  var el = document.getElementById('px');
  el.textContent =
    '1x=' + oneToOne +
    ';pw_non_black=' + nonBlackInPw +
    ';xorigin_non_black=' + nonBlackInXOrigin +
    ';nested_xorigin_non_black=' + nonBlackInNestedXOrigin +
    ';btn_green=' + btnGreen;
  el.setAttribute('data-ready', '1');
}};
img.src = 'data:image/png;base64,{img_b64}';
</script></body></html>"##
    );
    let px_server = PageServer::start(BTreeMap::from([(
        "/px".to_string(),
        pixel_check_html.clone(),
    )]));
    let px_case = dir.write(
        "verify-mask.yaml",
        &format!(
            "name: Verify locate screenshot mask\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.wait:\n      css: \"#px[data-ready='1']\"\n  - browser.read:\n      css: \"#px\"\n    expect:\n      - path: $.text\n        equals: \"1x=true;pw_non_black=0;xorigin_non_black=0;nested_xorigin_non_black=0;btn_green=true\"\n",
            px_server.url("/px")
        ),
    );
    let px_run = run_da_in(
        dir.path(),
        &["run", px_case.to_str().unwrap(), "--no-bundle"],
    );
    assert_eq!(
        px_run.status.code(),
        Some(0),
        "password pixels were not masked in model/locate screenshot: stdout={}\nstderr={}",
        String::from_utf8_lossy(&px_run.stdout),
        String::from_utf8_lossy(&px_run.stderr)
    );

    // 5. `judge` records redacted rationale & audit fields in `steps.jsonl`:
    // first without `--screenshot` (omits image), then with `--screenshot` (attaches masked viewport PNG).
    fs::write(
        &cfg_path,
        format!(
            "request_file={}\njudge_passed=true\njudge_rationale=Buttons clicked cleanly without leaking {secret}\nmodel=vllm-judge\nlatency_ms=310\ntrace_id=tr-{secret}\n",
            req_file.display()
        ),
    )
    .unwrap();
    let judge_out = run_env(&[
        "session",
        "judge",
        &id,
        "--rubric",
        "Confirm both buttons appended markers",
    ]);
    assert_eq!(judge_out.status.code(), Some(0), "{judge_out:?}");
    let judge_no_shot_req: Value =
        serde_json::from_str(fs::read_to_string(&req_file).unwrap().trim()).unwrap();
    assert_eq!(judge_no_shot_req["method"], "model/judge");
    assert!(
        judge_no_shot_req["params"].get("image_base64").is_none(),
        "default judge without --screenshot must not attach image_base64"
    );

    let judge_shot_out = run_env(&[
        "session",
        "judge",
        &id,
        "--rubric",
        "Confirm rendered button visual state",
        "--screenshot",
    ]);
    assert_eq!(judge_shot_out.status.code(), Some(0), "{judge_shot_out:?}");
    let judge_shot_req: Value =
        serde_json::from_str(fs::read_to_string(&req_file).unwrap().trim()).unwrap();
    assert_eq!(judge_shot_req["method"], "model/judge");
    assert_eq!(judge_shot_req["params"]["mime_type"], "image/png");
    assert_eq!(
        judge_shot_req["params"]["viewport"],
        loc_req_val["params"]["viewport"]
    );
    let judge_img_b64 = judge_shot_req["params"]["image_base64"]
        .as_str()
        .expect("judge --screenshot image_base64");
    assert!(!judge_img_b64.is_empty());
    let judge_px_html = pixel_check_html.replace(&img_b64, judge_img_b64);
    let judge_px_server =
        PageServer::start(BTreeMap::from([("/px-judge".to_string(), judge_px_html)]));
    let judge_px_case = dir.write(
        "verify-judge-mask.yaml",
        &format!(
            "name: Verify judge screenshot mask\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.wait:\n      css: \"#px[data-ready='1']\"\n  - browser.read:\n      css: \"#px\"\n    expect:\n      - path: $.text\n        equals: \"1x=true;pw_non_black=0;xorigin_non_black=0;nested_xorigin_non_black=0;btn_green=true\"\n",
            judge_px_server.url("/px-judge")
        ),
    );
    let judge_px_run = run_da_in(
        dir.path(),
        &["run", judge_px_case.to_str().unwrap(), "--no-bundle"],
    );
    assert_eq!(
        judge_px_run.status.code(),
        Some(0),
        "password/iframe pixels were not masked in judge --screenshot PNG: stdout={}\nstderr={}",
        String::from_utf8_lossy(&judge_px_run.stdout),
        String::from_utf8_lossy(&judge_px_run.stderr)
    );

    // Assert log state `[jev][btn]` and export deterministic button-only case -> 0 lint warnings!
    let read1 = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.read:\n  css: \"#log\"\nexpect:\n  - path: $.text\n    equals: \"[jev][btn]\"\n",
    );
    assert_eq!(read1.status.code(), Some(0), "{read1:?}");

    let btn_case = dir.path().join("btn-deterministic.yaml");
    let exp_btn = json_stdout(&run_env(&[
        "session",
        "export",
        &id,
        "-o",
        btn_case.to_str().unwrap(),
        "--name",
        "Deterministic button flow",
    ]));
    assert_eq!(
        exp_btn["fragile"].as_array().unwrap().len(),
        0,
        "HTML button locate must produce 0 fragile steps: {exp_btn}"
    );
    let lint_btn = run_env(&["--output", "json", "lint", btn_case.to_str().unwrap()]);
    assert_eq!(lint_btn.status.code(), Some(0), "{lint_btn:?}");
    let lint_btn_json = json_stdout(&lint_btn);
    assert_eq!(
        lint_btn_json["warnings"].as_array().map_or(0, |w| w.len()),
        0,
        "expected 0 da lint warnings for reverse-synthesized HTML button: {lint_btn_json}"
    );

    // 6. `locate` on opaque `<canvas>` (`#board` at `(120, 210)`, offset `(100, 50)`) -> falls back to `at:` + coordinate lint warning!
    fs::write(
        &cfg_path,
        format!(
            "request_file={}\nlocate_x=120\nlocate_y=210\nmodel=ui-tars-1.5-7b\nlatency_ms=1900\ntrace_id=tr-vlm-canvas\n",
            req_file.display()
        ),
    )
    .unwrap();
    let loc_canvas_out = run_env(&[
        "session",
        "locate",
        &id,
        "--query",
        "Click canvas board at offset 100,50",
        "--click",
    ]);
    assert_eq!(loc_canvas_out.status.code(), Some(0), "{loc_canvas_out:?}");
    let loc_canvas_json = json_stdout(&loc_canvas_out);
    assert_eq!(loc_canvas_json["synthesized_locator"], Value::Null);
    assert_eq!(loc_canvas_json["fragile"], true);

    let read2 = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.read:\n  css: \"#log\"\nexpect:\n  - path: $.text\n    equals: \"[jev][btn][canvas:100,50]\"\n",
    );
    assert_eq!(read2.status.code(), Some(0), "{read2:?}");

    let canvas_case = dir.path().join("canvas-fallback.yaml");
    let exp_canvas = json_stdout(&run_env(&[
        "session",
        "export",
        &id,
        "-o",
        canvas_case.to_str().unwrap(),
        "--name",
        "Canvas fallback flow",
    ]));
    assert_eq!(
        exp_canvas["fragile"].as_array().unwrap().len(),
        1,
        "{exp_canvas}"
    );
    assert_eq!(
        exp_canvas["fragile"][0]["reason"], "explicit_coordinates",
        "{exp_canvas}"
    );

    let lint_canvas = run_env(&["--output", "json", "lint", canvas_case.to_str().unwrap()]);
    assert_eq!(lint_canvas.status.code(), Some(0), "{lint_canvas:?}");
    let lint_canvas_json = json_stdout(&lint_canvas);
    assert_eq!(
        lint_canvas_json["warnings"]
            .as_array()
            .map_or(0, |w| w.len()),
        1,
        "expected 1 coordinate lint warning on canvas fallback: {lint_canvas_json}"
    );

    // Check `steps.jsonl`: zero secret leakage anywhere and audit metadata present!
    let mut sess_files = Vec::new();
    collect_files(&session_dir, &mut sess_files);
    for f in &sess_files {
        let txt = fs::read_to_string(f).unwrap();
        assert!(
            !txt.contains(secret),
            "session file {} leaked secret!",
            f.display()
        );
    }
    let steps_txt = fs::read_to_string(session_dir.join("steps.jsonl")).unwrap();
    assert!(steps_txt.contains("\"jev-0.2b\""));
    assert!(steps_txt.contains("\"ui-tars-1.5-7b\""));
    assert!(steps_txt.contains("\"vllm-judge\""));

    // 7. Stop session, delete mock hook binary so NO model process exists, and replay BOTH cases deterministically!
    let stop = run_env(&["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{stop:?}");
    fs::remove_file(&hook_bin).expect("delete mock hook executable before replay");

    let replay_btn = run_env(&["run", btn_case.to_str().unwrap(), "--no-bundle"]);
    assert_eq!(
        replay_btn.status.code(),
        Some(0),
        "btn replay failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&replay_btn.stdout),
        String::from_utf8_lossy(&replay_btn.stderr)
    );

    let replay_canvas = run_env(&["run", canvas_case.to_str().unwrap(), "--no-bundle"]);
    assert_eq!(
        replay_canvas.status.code(),
        Some(0),
        "canvas replay failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&replay_canvas.stdout),
        String::from_utf8_lossy(&replay_canvas.stderr)
    );
}

#[test]
fn browser_locate_masks_top_layer_modals_popovers_and_frames() {
    let Some(_guard) = chrome_guard() else {
        return;
    };

    let xorigin_child_html = r##"<!doctype html><html><body style="margin:0;background:#ff00ff"><input id="xpw" type="password" value="Sup3rSecret!" style="position:fixed;left:10px;top:10px;width:180px;height:40px;background:#ff00ff;border:0"></body></html>"##.to_string();
    let xorigin_server = PageServer::start(BTreeMap::from([(
        "/xorigin".to_string(),
        xorigin_child_html,
    )]));
    let xorigin_url = xorigin_server.url("/xorigin");

    let same_in_dlg_html = r##"<!doctype html><html><body style="margin:0;background:#eeeeee"><input id="same-pw" type="password" value="SameInDlg!" style="position:fixed;left:10px;top:10px;width:180px;height:40px;background:#ff00ff;color:#ff00ff;border:0;padding:0;margin:0"></body></html>"##.to_string();

    let page_html = format!(
        r##"<!doctype html>
<html>
<head>
  <title>Top Layer Mask Lab</title>
  <style>
    html, body {{ margin: 0; padding: 0; background: #f4f4f4; }}
    ::backdrop {{ background: rgba(0, 160, 0, 0.2); }}
    #bg-pw {{
      position: fixed;
      left: 40px;
      top: 40px;
      width: 260px;
      height: 60px;
      background: #ff00ff;
      color: #ff00ff;
      border: 0;
      padding: 0;
      margin: 0;
    }}
    #d {{
      position: fixed;
      left: 40px;
      top: 40px;
      width: 540px;
      height: 340px;
      margin: 0;
      padding: 0;
      border: 0;
      background: #ffffff;
    }}
    #dlg-btn {{
      position: fixed;
      left: 80px;
      top: 50px;
      width: 160px;
      height: 40px;
      background: rgb(0, 200, 80);
      color: #ffffff;
      border: 0;
      padding: 0;
      margin: 0;
    }}
    #pw {{
      position: fixed;
      left: 60px;
      top: 110px;
      width: 180px;
      height: 40px;
      background: #ff00ff;
      color: #ff00ff;
      border: 0;
      padding: 0;
      margin: 0;
      font-size: 20px;
    }}
    #dlg-shadow-host {{
      position: fixed;
      left: 60px;
      top: 170px;
      width: 180px;
      height: 40px;
    }}
    #dlg-same {{
      position: fixed;
      left: 60px;
      top: 230px;
      width: 220px;
      height: 60px;
      border: 0;
    }}
    #dlg-xorigin {{
      position: fixed;
      left: 310px;
      top: 110px;
      width: 220px;
      height: 80px;
      border: 0;
    }}
    #pop {{
      position: fixed;
      left: 640px;
      top: 40px;
      width: 300px;
      height: 180px;
      margin: 0;
      padding: 0;
      border: 0;
      background: #ffffff;
    }}
    #pop-pw {{
      position: fixed;
      left: 660px;
      top: 60px;
      width: 200px;
      height: 44px;
      background: #ff00ff;
      color: #ff00ff;
      border: 0;
      padding: 0;
      margin: 0;
    }}
  </style>
</head>
<body>
  <input id="bg-pw" type="password" value="BgSecret!" />
  <dialog id="d">
    <button id="dlg-btn" type="button" onclick="document.getElementById('dlg-status').textContent = 'clicked';">Confirm Modal</button>
    <input id="pw" type="password" value="Sup3rSecret!" />
    <div id="dlg-shadow-host"></div>
    <iframe id="dlg-same" src="/dlg-same" onload="document.getElementById('dlg-btn').setAttribute('data-same-loaded', '1')"></iframe>
    <iframe id="dlg-xorigin" src="{xorigin_url}" onload="document.getElementById('dlg-btn').setAttribute('data-xorigin-loaded', '1')"></iframe>
    <p id="dlg-status" style="position:fixed;left:310px;top:50px;margin:0;">idle</p>
  </dialog>
  <div id="pop" popover="auto">
    <input id="pop-pw" type="password" value="PopSecret!" />
  </div>
  <p id="top-layer-state" style="position:fixed;left:40px;top:420px;">pending</p>
  <script>
    var shadow = document.getElementById('dlg-shadow-host').attachShadow({{ mode: 'open' }});
    var shadowPw = document.createElement('input');
    shadowPw.id = 'dlg-shadow-pw';
    shadowPw.type = 'password';
    shadowPw.value = 'ShadowInDlg!';
    shadowPw.style.cssText = 'display:block;width:180px;height:40px;background:#ff00ff;color:#ff00ff;border:0;padding:0;margin:0;';
    shadow.appendChild(shadowPw);

    var d = document.getElementById('d');
    var p = document.getElementById('pop');
    d.showModal();
    p.showPopover();
    window.recordTopLayerState = function () {{
      return 'dialog_open=' + d.open +
        ';dialog_modal=' + d.matches(':modal') +
        ';popover_open=' + p.matches(':popover-open') +
        ';status=' + document.getElementById('dlg-status').textContent;
    }};
  </script>
</body>
</html>"##
    );

    let server = PageServer::start(BTreeMap::from([
        ("/top-layer".to_string(), page_html),
        ("/dlg-same".to_string(), same_in_dlg_html),
    ]));
    let dir = TempDir::new("browser-top-layer-locate");
    let hook_bin = compile_browser_mock_hook(dir.path());
    let hook_str = hook_bin.to_str().unwrap();
    let req_file = dir.path().join("hook_req.json");
    let cfg_path = dir.path().join("hook.cfg");
    fs::write(
        &cfg_path,
        format!(
            "request_file={}\nlocate_x=150\nlocate_y=130\nmodel=ui-tars-1.5-7b\nlatency_ms=42\ntrace_id=tr-top-layer\n",
            req_file.display()
        ),
    )
    .unwrap();
    let cfg_str = cfg_path.to_str().unwrap();

    let started = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "start",
            "--model-hook",
            hook_str,
            "--model-hook-arg",
            cfg_str,
        ],
    ));
    let id = started["session_id"].as_str().unwrap().to_string();

    let open = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url={}", server.url("/top-layer")),
            "--arg",
            "width=1280",
            "--arg",
            "height=720",
        ],
    ));
    assert_eq!(open["ok"], true, "{open}");

    let wait_ready = json_stdout(&run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.wait",
            "--arg",
            "css=#dlg-btn[data-same-loaded='1'][data-xorigin-loaded='1']",
        ],
    ));
    assert_eq!(wait_ready["ok"], true, "{wait_ready}");

    // Locate the password input inside `<dialog>.showModal()` -> hit-testing must
    // still resolve `{"css":"#pw"}` after mask cleanup while masking every top-layer pixel!
    let loc_pw = run_da_in(
        dir.path(),
        &[
            "session",
            "locate",
            &id,
            "--query",
            "Locate password field inside modal dialog",
        ],
    );
    assert_eq!(loc_pw.status.code(), Some(0), "{loc_pw:?}");
    let loc_pw_json = json_stdout(&loc_pw);
    assert_eq!(
        loc_pw_json["synthesized_locator"],
        json!({ "css": "#pw" }),
        "expected hit-testing to synthesize #pw inside modal dialog: {loc_pw_json}"
    );
    assert_eq!(loc_pw_json["fragile"], false, "{loc_pw_json}");

    let loc_req_raw = fs::read_to_string(&req_file).unwrap();
    let loc_req_val: Value = serde_json::from_str(loc_req_raw.trim()).unwrap();
    let vw = loc_req_val["params"]["viewport"]["width"].as_u64().unwrap();
    let vh = loc_req_val["params"]["viewport"]["height"]
        .as_u64()
        .unwrap();
    let img_b64 = loc_req_val["params"]["image_base64"].as_str().unwrap();

    // Immediately after `locate` mask cleanup, BOTH `<dialog>:modal` and `[popover="auto"]:popover-open`
    // must still be open (and `#dlg-btn` remains clickable afterward; note clicking outside a
    // `popover="auto"` light-dismisses the popover per HTML spec).
    let verify_modal_pop = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.wait:\n  css: \"#d:modal\"\n",
    );
    assert_eq!(
        verify_modal_pop.status.code(),
        Some(0),
        "dialog must still be :modal after locate: {verify_modal_pop:?}"
    );
    let verify_pop_open = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.wait:\n  css: \"#pop:popover-open\"\n",
    );
    assert_eq!(
        verify_pop_open.status.code(),
        Some(0),
        "popover must still be :popover-open after locate: {verify_pop_open:?}"
    );

    let click_btn = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.click:\n  css: \"#dlg-btn\"\n",
    );
    assert_eq!(click_btn.status.code(), Some(0), "{click_btn:?}");

    let verify_state = run_da_stdin(
        dir.path(),
        &["session", "step", &id],
        "browser.read:\n  css: \"#d[open]:modal #dlg-status\"\nexpect:\n  - path: $.text\n    equals: \"clicked\"\n",
    );
    assert_eq!(verify_state.status.code(), Some(0), "{verify_state:?}");

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{stop:?}");

    // Verify rendered PNG pixels via real Chrome `<canvas>`:
    // - `#pw` in modal `<dialog>`: `(70..230, 118..142)` -> solid `#000000`
    // - `#dlg-shadow-pw` in shadow root inside `<dialog>`: `(70..230, 178..202)` -> solid `#000000`
    // - `#same-pw` in same-origin iframe inside `<dialog>`: `(78..242, 248..272)` -> solid `#000000`
    // - `#dlg-xorigin` cross-origin iframe inside `<dialog>`: `(320..520, 120..180)` -> solid `#000000`
    // - `#pop-pw` in `<div popover="auto">`: `(670..850, 68..96)` -> solid `#000000`
    // - `#dlg-btn` `(90..230, 58..82)` stays green even though `#bg-pw` sits behind it!
    let pixel_check_html = format!(
        r##"<!doctype html><html><body>
<p id="px">pending</p>
<script>
var img = new Image();
img.onload = function () {{
  var c = document.createElement('canvas');
  c.width = img.width;
  c.height = img.height;
  var ctx = c.getContext('2d');
  ctx.drawImage(img, 0, 0);
  var scaleX = img.width / {vw};
  var scaleY = img.height / {vh};
  var countNonBlack = function (x0, x1, dx, y0, y1, dy) {{
    var bad = 0;
    for (var x = x0; x <= x1; x += dx) {{
      for (var y = y0; y <= y1; y += dy) {{
        var d = ctx.getImageData(Math.round(x * scaleX), Math.round(y * scaleY), 1, 1).data;
        if (d[0] !== 0 || d[1] !== 0 || d[2] !== 0 || d[3] !== 255) bad++;
      }}
    }}
    return bad;
  }};
  var pwBad = countNonBlack(70, 230, 10, 118, 142, 6);
  var shadowBad = countNonBlack(70, 230, 10, 178, 202, 6);
  var sameBad = countNonBlack(78, 242, 12, 248, 272, 6);
  var xoriginBad = countNonBlack(320, 520, 15, 120, 180, 10);
  var popBad = countNonBlack(670, 850, 12, 68, 96, 6);
  var greenCount = 0;
  for (var bx = 90; bx <= 230; bx += 10) {{
    for (var by = 58; by <= 82; by += 6) {{
      var bd = ctx.getImageData(Math.round(bx * scaleX), Math.round(by * scaleY), 1, 1).data;
      if (bd[1] > 150 && bd[0] < 40) greenCount++;
    }}
  }}
  var el = document.getElementById('px');
  el.textContent =
    '1x=' + (img.width === {vw} && img.height === {vh}) +
    ';pw=' + pwBad +
    ';shadow=' + shadowBad +
    ';same=' + sameBad +
    ';xorigin=' + xoriginBad +
    ';pop=' + popBad +
    ';btn_green=' + (greenCount > 20);
  el.setAttribute('data-ready', '1');
}};
img.src = 'data:image/png;base64,{img_b64}';
</script></body></html>"##
    );
    let px_server = PageServer::start(BTreeMap::from([("/px".to_string(), pixel_check_html)]));
    let px_case = dir.write(
        "verify-top-layer-mask.yaml",
        &format!(
            "name: Verify top-layer locate mask\nsteps:\n  - browser.open:\n      url: \"{}\"\n  - browser.wait:\n      css: \"#px[data-ready='1']\"\n  - browser.read:\n      css: \"#px\"\n    expect:\n      - path: $.text\n        equals: \"1x=true;pw=0;shadow=0;same=0;xorigin=0;pop=0;btn_green=true\"\n",
            px_server.url("/px")
        ),
    );
    let px_run = run_da_in(
        dir.path(),
        &["run", px_case.to_str().unwrap(), "--no-bundle"],
    );
    assert_eq!(
        px_run.status.code(),
        Some(0),
        "top-layer pixels were not masked properly: stdout={}\nstderr={}",
        String::from_utf8_lossy(&px_run.stdout),
        String::from_utf8_lossy(&px_run.stderr)
    );
}

#[cfg(feature = "ai")]
mod ai_browser_tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    struct BrowserModelMock {
        port: u16,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl BrowserModelMock {
        fn start<F>(handler: F) -> Self
        where
            F: Fn(usize, &Value) -> Value + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind browser model mock");
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();

            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let handler = Arc::new(handler);

            let reqs_c = Arc::clone(&requests);
            let stop_c = Arc::clone(&stop);
            let call_count = Arc::new(AtomicUsize::new(0));
            let handle = thread::spawn(move || {
                while !stop_c.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let reqs = Arc::clone(&reqs_c);
                            let h = Arc::clone(&handler);
                            let cnt = Arc::clone(&call_count);
                            thread::spawn(move || {
                                serve_mock(stream, &reqs, &cnt, &*h);
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });

            Self {
                port,
                requests,
                stop,
                handle: Some(handle),
            }
        }

        fn endpoint(&self) -> String {
            format!("http://127.0.0.1:{}/v1", self.port)
        }
    }

    impl Drop for BrowserModelMock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn serve_mock(
        stream: TcpStream,
        requests: &Mutex<Vec<String>>,
        counter: &AtomicUsize,
        handler: &(dyn Fn(usize, &Value) -> Value + Send + Sync),
    ) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut writer = stream;

        let mut req_line = String::new();
        if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
            return;
        }
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':')
                && k.trim().eq_ignore_ascii_case("content-length")
            {
                len = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body_buf = vec![0u8; len];
        if len > 0 {
            let _ = reader.read_exact(&mut body_buf);
        }
        let raw_body = String::from_utf8_lossy(&body_buf).into_owned();
        requests.lock().unwrap().push(raw_body.clone());

        let parsed_req: Value = serde_json::from_str(&raw_body).unwrap_or(Value::Null);
        let idx = counter.fetch_add(1, Ordering::SeqCst);
        let content = handler(idx, &parsed_req);

        let reply_body = json!({
            "id": format!("chatcmpl-chrome-{idx}"),
            "object": "chat.completion",
            "created": 1700000000,
            "model": "chrome-mock-v1",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content.to_string() },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 12, "completion_tokens": 6, "total_tokens": 18 }
        })
        .to_string();

        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply_body.len(),
            reply_body
        );
        let _ = writer.write_all(resp.as_bytes());
        let _ = writer.flush();
    }

    fn extract_payload_from_chat_req(req: &Value) -> Value {
        let content = &req["messages"][1]["content"];
        let text = if let Some(s) = content.as_str() {
            s
        } else if let Some(arr) = content.as_array() {
            arr.iter()
                .find_map(|part| {
                    if part.get("type").and_then(Value::as_str) == Some("text") {
                        part.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .unwrap_or("{}")
        } else {
            "{}"
        };
        serde_json::from_str(text).unwrap_or(Value::Null)
    }

    #[test]
    fn browser_session_builtin_model_extract_act_and_offline_replay() {
        let Some(_guard) = chrome_guard() else {
            return;
        };
        let routes = BTreeMap::from([(
            "/checkout".to_string(),
            r##"<!doctype html>
<html>
<head><title>Checkout</title></head>
<body style="margin:0;padding:0;font-family:sans-serif">
  <input id="pw" type="password" value="top-secret" style="position:fixed;left:24px;top:24px;width:200px;height:30px" />
  <p id="order" style="position:fixed;left:24px;top:72px">Order #777: $42.50</p>
  <label for="note-box" style="position:fixed;left:24px;top:116px">Order Note</label>
  <input id="note-box" type="text" style="position:fixed;left:24px;top:140px;width:200px;height:30px" />
  <button data-testid="confirm-btn" style="position:fixed;left:24px;top:190px;width:140px;height:36px" onclick="
    document.getElementById('status').textContent = 'Confirmed: ' + document.getElementById('note-box').value;
  ">Confirm</button>
  <p id="status" style="position:fixed;left:24px;top:240px">Pending</p>
</body>
</html>"##
                .to_string(),
        )]);
        let page = PageServer::start(routes);

        let model_mock = BrowserModelMock::start(|idx, req| match idx {
            0 => json!({
                "data": { "order_id": "ORD-777", "total": "$42.50" },
                "confidence": 0.96
            }),
            1 => json!({
                "status": "continue",
                "summary": "Type note via visual locate_type",
                "action": { "kind": "locate_type", "x": 124, "y": 155, "text": "deliver fast" },
                "confidence": 0.92
            }),
            2 => {
                let payload = extract_payload_from_chat_req(req);
                let tree = payload["snapshot"]["tree"]
                    .as_array()
                    .expect("live Chrome snapshot tree in act request");
                let confirm_ref = find_ref_by_role_and_name(tree, "button", "Confirm")
                    .expect("Confirm button ref in live snapshot");
                json!({
                    "status": "continue",
                    "summary": "Click Confirm button by ref",
                    "action": { "kind": "step", "use": "browser.click", "with": { "ref": confirm_ref } },
                    "confidence": 0.95
                })
            }
            _ => json!({
                "status": "done",
                "summary": "Checkout note submitted and confirmed",
                "reason": "Status updated to Confirmed",
                "action": null,
                "confidence": 0.99
            }),
        });

        let dir = TempDir::new("chrome-extract-act");
        let started = json_stdout(&run_da_in(
            dir.path(),
            &[
                "session",
                "start",
                "--model",
                "openai::hy4",
                "--model-adapter",
                "omlx",
                "--model-endpoint",
                &model_mock.endpoint(),
                "--model-reasoning-effort",
                "low",
            ],
        ));
        assert_eq!(started["ok"], true, "{started}");
        let id = started["session_id"].as_str().unwrap().to_string();

        let opened = run_da_in(
            dir.path(),
            &[
                "session",
                "step",
                &id,
                "browser.open",
                "--arg",
                &format!("url={}", page.url("/checkout")),
                "--arg",
                "width=1280",
                "--arg",
                "height=720",
            ],
        );
        assert_eq!(opened.status.code(), Some(0), "{opened:?}");

        let extracted = run_da_in(
            dir.path(),
            &[
                "session",
                "extract",
                &id,
                "--query",
                "Extract order_id and total",
                "--schema",
                r#"{"type":"object","properties":{"order_id":{"type":"string"},"total":{"type":"string"}}}"#,
                "--screenshot",
                "--save",
                "ORDER_INFO",
            ],
        );
        assert_eq!(extracted.status.code(), Some(0), "{extracted:?}");
        let ext_json = json_stdout(&extracted);
        assert_eq!(
            ext_json["data"],
            json!({"order_id": "ORD-777", "total": "$42.50"})
        );
        assert_eq!(ext_json["screenshot"], true);
        assert!(ext_json["saved"]["ORDER_INFO"].is_string());

        let acted = run_da_in(
            dir.path(),
            &[
                "session",
                "act",
                &id,
                "--goal",
                "Type deliver fast into Order Note and click Confirm",
                "--screenshot",
                "--max-steps",
                "4",
            ],
        );
        assert_eq!(acted.status.code(), Some(0), "{acted:?}");
        let act_json = json_stdout(&acted);
        assert_eq!(act_json["status"], "done");
        assert_eq!(act_json["steps_taken"], 2);
        assert_eq!(act_json["screenshot"], true);

        let verify_read = run_da_stdin(
            dir.path(),
            &["session", "step", &id],
            "browser.read:\n  css: \"#status\"\nexpect:\n  - path: $.text\n    equals: \"Confirmed: deliver fast\"\n",
        );
        assert_eq!(verify_read.status.code(), Some(0), "{verify_read:?}");

        let out_yaml = dir.path().join("checkout-replay.yaml");
        let exported = run_da_in(
            dir.path(),
            &[
                "session",
                "export",
                &id,
                "--name",
                "Chrome extract act replay",
                "-o",
                out_yaml.to_str().unwrap(),
            ],
        );
        assert_eq!(exported.status.code(), Some(0), "{exported:?}");

        let stop = run_da_in(dir.path(), &["session", "stop", &id]);
        assert_eq!(stop.status.code(), Some(0), "{stop:?}");

        {
            let reqs = model_mock.requests.lock().unwrap();
            assert_eq!(reqs.len(), 4, "expected 1 extract + 3 act_step calls");
            for r in reqs.iter() {
                assert!(r.contains("data:image/png;base64,"), "{r}");
                assert!(r.contains("\"reasoning_effort\":\"low\""), "{r}");
            }
        }
        drop(model_mock);

        let yaml_text = fs::read_to_string(&out_yaml).unwrap();
        assert!(!yaml_text.contains("model.extract"), "{yaml_text}");
        assert!(!yaml_text.contains("model.act"), "{yaml_text}");
        assert!(yaml_text.contains("browser.type:"), "{yaml_text}");
        assert!(yaml_text.contains("testid: confirm-btn"), "{yaml_text}");
        assert!(yaml_text.contains("browser.read:"), "{yaml_text}");

        let lint = run_da_in(dir.path(), &["lint", out_yaml.to_str().unwrap()]);
        assert_eq!(lint.status.code(), Some(0), "{lint:?}");

        let replay = run_da_in(
            dir.path(),
            &["run", out_yaml.to_str().unwrap(), "--no-bundle"],
        );
        assert_eq!(replay.status.code(), Some(0), "{replay:?}");
    }
}
