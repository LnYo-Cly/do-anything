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

    let login_step = format!(
        "http.request:\n  method: POST\n  url: \"{base}/login\"\n  json:\n    user: \"alice\"\nsave:\n  tok: $.body.token\n"
    );
    let s1 = run_da_stdin(dir.path(), &["session", "step", &id], &login_step);
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
    let line2: Value =
        serde_json::from_str(jsonl.lines().nth(1).expect("second record")).expect("record json");
    assert_eq!(line2["raw_args"]["cookies"][0]["value"], "${tok}");

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

    let trace_dir = dir.path().join("trace-bundle");
    let replay = run_da_in(
        dir.path(),
        &[
            "run",
            exported_path.to_str().expect("utf8 path"),
            "--record",
            trace_dir.to_str().expect("utf8 path"),
            "--trace-steps",
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
