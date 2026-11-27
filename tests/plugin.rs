//! End-to-end tests for synchronous stdio JSON-RPC 2.0 plugins.
//!
//! Every test drives the real `da` binary and the real fixture executable; no
//! protocol call is made in-process. The fixture is the `da-plugin-fixture`
//! binary target declared in `Cargo.toml`, so no binary is checked in.

#![cfg(feature = "plugin")]

mod support;

use std::fs;
#[cfg(feature = "mcp")]
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "mcp")]
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TempDir;

/// The environment variable the fixture echoes as a secret.
const SECRET_VARIABLE: &str = "DA_PLUGIN_TEST_SECRET";
/// A value long enough that encoded forms are worth searching for too.
const SECRET: &str = "plugin-secret-9f3a-DA";

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn fixture() -> &'static str {
    env!("CARGO_BIN_EXE_da-plugin-fixture")
}

/// Runs `da` in `dir`, clearing plugin-related ambient state first.
fn run_da(dir: &Path, args: &[&str], environment: &[(&str, &str)]) -> Output {
    let mut command = Command::new(da());
    command
        .current_dir(dir)
        .args(args)
        .env_remove("DA_FIXTURE_MODE")
        .env_remove("DA_FIXTURE_PID_FILE")
        .env_remove("DA_FIXTURE_CLOSE_LOG")
        .env_remove("DA_PLUGIN_TIMEOUT_MS")
        .env_remove("DA_PLUGIN_EXTRA_SECRET")
        .env_remove(SECRET_VARIABLE);
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("failed to run the da binary")
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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("the test path must be UTF-8")
}

/// Every regular file under `root`.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}

/// The one file named `name` under `root`, failing when there is no such file.
fn find_named(root: &Path, name: &str) -> PathBuf {
    files_under(root)
        .into_iter()
        .find(|path| path.file_name().is_some_and(|file| file == name))
        .unwrap_or_else(|| panic!("no `{name}` under {}", root.display()))
}

/// Asserts a value did not escape in any form `Secrets` redacts.
fn assert_no_secret(text: &str, secret: &str) {
    for form in [
        secret.to_string(),
        support::percent_encode(secret.as_bytes()),
        support::base64_encode(secret.as_bytes()),
    ] {
        assert!(
            !text.contains(&form),
            "the secret escaped as `{form}`:\n{text}"
        );
    }
}

/// Whether a process with `pid` is still running.
fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .expect("`tasklist` must be available on Windows");
        let text = String::from_utf8_lossy(&output.stdout);
        text.split_whitespace()
            .any(|field| field.trim_matches(',') == pid.to_string())
    }
    #[cfg(unix)]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// Waits for the process to disappear, then asserts it did.
fn assert_process_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("plugin process {pid} is still running after its command exited");
}

/// Reads the pid the fixture wrote when it started.
fn fixture_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && let Ok(pid) = contents.trim().parse()
        {
            return pid;
        }
        if Instant::now() >= deadline {
            panic!(
                "the fixture did not write a pid to {} (contents: {:?})",
                path.display(),
                fs::read_to_string(path).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The plugin case used by the schema and lint checks.
const SIMPLE_CASE: &str = "name: Plugin step\nsteps:\n  - custom.echo:\n      value: hello\n";

#[test]
fn schema_and_lint_see_plugin_declarations_and_reject_bad_ones() {
    let dir = TempDir::new("plugin-schema-lint");

    let schema = run_da(
        dir.path(),
        &["--plugin", fixture(), "schema", "--output", "json"],
        &[],
    );
    assert_eq!(schema.status.code(), Some(0), "{}", stderr(&schema));
    let schema = json_stdout(&schema);
    for name in ["custom.echo", "custom.act", "custom.metrics"] {
        assert!(
            schema["$defs"].get(name).is_some(),
            "`da schema` must declare `{name}`: {schema}"
        );
    }
    assert_eq!(
        schema["$defs"]["custom.echo"]["properties"]["value"]["minLength"], 1,
        "the plugin's JSON Schema bound must reach `da schema`: {schema}"
    );
    assert_eq!(
        schema["$defs"]["custom.echo"]["description"],
        "Echoes the `value` argument in several result shapes."
    );

    let valid = dir.write("valid.yaml", SIMPLE_CASE);
    let lint = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "lint",
            path_str(&valid),
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(lint.status.code(), Some(0), "{}", stderr(&lint));
    assert_eq!(json_stdout(&lint)["ok"], true);

    let bad_bound = dir.write(
        "bad-bound.yaml",
        "name: Plugin bad bound\nsteps:\n  - custom.echo:\n      value: hello\n      count: 9\n",
    );
    let lint = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "lint",
            path_str(&bad_bound),
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(lint.status.code(), Some(3), "{}", stderr(&lint));
    assert_eq!(json_stdout(&lint)["ok"], false);

    // The fixture's `step/validate` rejects a value its JSON Schema accepts,
    // so a refusal here proves lint called the plugin's own validator.
    let lint = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "lint",
            path_str(&valid),
            "--output",
            "json",
        ],
        &[("DA_FIXTURE_MODE", "validate-reject")],
    );
    assert_eq!(lint.status.code(), Some(3), "{}", stderr(&lint));
    assert!(
        stdout(&lint).contains("rejected by the fixture's step/validate"),
        "the plugin validator's message must reach lint: {}",
        stdout(&lint)
    );

    let duplicate = run_da(
        dir.path(),
        &["--plugin", fixture(), "schema", "--output", "json"],
        &[("DA_FIXTURE_MODE", "duplicate")],
    );
    assert_eq!(duplicate.status.code(), Some(2), "{}", stderr(&duplicate));
    assert!(
        stderr(&duplicate).contains("already registered"),
        "a duplicate declaration must be refused cleanly: {}",
        stderr(&duplicate)
    );

    #[cfg(feature = "http")]
    {
        let collision = run_da(
            dir.path(),
            &["--plugin", fixture(), "schema", "--output", "json"],
            &[("DA_FIXTURE_MODE", "collide-builtin")],
        );
        assert_eq!(collision.status.code(), Some(2), "{}", stderr(&collision));
        assert!(
            stderr(&collision).contains("already registered"),
            "a plugin name colliding with a built-in must be refused cleanly: {}",
            stderr(&collision)
        );
    }
}

#[test]
fn run_record_report_and_failure_bundle_redact_every_plugin_path() {
    let dir = TempDir::new("plugin-redaction");
    let case = dir.write(
        "case.yaml",
        &format!(
            r#"
name: Plugin secret paths
observe:
  metrics:
    custom.metrics:
      value: ${{env:{SECRET_VARIABLE}}}
steps:
  - custom.echo:
      value: ${{env:{SECRET_VARIABLE}}}
  - custom.act:
      value: ${{env:{SECRET_VARIABLE}}}
  - custom.act:
      value: ${{env:{SECRET_VARIABLE}}}
      fail: true
"#
        ),
    );
    let record = dir.path().join("record");
    let bundles = dir.path().join("bundles");

    let run = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "run",
            path_str(&case),
            "--record",
            path_str(&record),
            "--bundle-dir",
            path_str(&bundles),
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(
        run.status.code(),
        Some(1),
        "{}\n{}",
        stdout(&run),
        stderr(&run)
    );
    assert_no_secret(&stdout(&run), SECRET);
    assert_no_secret(&stderr(&run), SECRET);
    assert!(stdout(&run).contains("***"), "{}", stdout(&run));

    let report = json_stdout(&run);
    let steps = report["cases"][0]["steps"]
        .as_array()
        .expect("the case must report its steps");
    assert_eq!(steps.len(), 3, "{report}");
    assert_eq!(steps[0]["status"], "passed", "{report}");
    assert_eq!(
        steps[0]["result"]["echo"], "***",
        "a step result must be redacted: {report}"
    );
    assert_eq!(
        steps[0]["result"]["observed"]["metrics"]["value"], "***",
        "an observer value must be redacted: {report}"
    );
    assert_eq!(steps[1]["status"], "passed", "{report}");
    assert_eq!(
        steps[1]["result"]["last"], "***",
        "a session result must be redacted: {report}"
    );
    assert_eq!(steps[2]["status"], "error", "{report}");
    assert!(
        steps[2]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("***")),
        "an error string must be redacted: {report}"
    );

    let recorded = find_named(&record, "report.json");
    assert_no_secret(
        &fs::read_to_string(&recorded).expect("recorded report"),
        SECRET,
    );
    let recorded_report: Value =
        serde_json::from_str(&fs::read_to_string(&recorded).expect("recorded report"))
            .expect("recorded report must be JSON");
    assert_eq!(recorded_report["steps"][0]["result"]["echo"], "***");

    let evidence = find_named(&record, "session.txt");
    let text = fs::read_to_string(&evidence).expect("text evidence");
    assert_no_secret(&text, SECRET);
    assert!(
        text.contains("***"),
        "text evidence must be redacted: {text}"
    );
    let opaque = find_named(&record, "opaque.bin");
    assert_eq!(
        fs::read(&opaque).expect("binary evidence"),
        vec![0, 1, 2, 255],
        "opaque bytes cross the documented redaction boundary unchanged"
    );

    let html = run_da(
        dir.path(),
        &[
            "report",
            path_str(&record),
            "--shareable",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(html.status.code(), Some(0), "{}", stderr(&html));
    let html = fs::read_to_string(record.join("report.html")).expect("report.html");
    assert_no_secret(&html, SECRET);
    assert!(html.contains("***"), "the HTML report must show redaction");

    let bundle_report = find_named(&bundles, "report.json");
    assert_no_secret(
        &fs::read_to_string(&bundle_report).expect("bundle report"),
        SECRET,
    );
    let bundle_evidence = find_named(&bundles, "session.txt");
    assert_no_secret(
        &fs::read_to_string(&bundle_evidence).expect("bundle evidence"),
        SECRET,
    );
    let bundle_meta = find_named(&bundles, "meta.json");
    assert_no_secret(
        &fs::read_to_string(&bundle_meta).expect("bundle meta"),
        SECRET,
    );
}

#[test]
fn session_lifecycle_uses_the_persisted_plugin_path_and_redacts_the_step_log() {
    let dir = TempDir::new("plugin-session");
    let sessions = dir.path().join("sessions");
    let start = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--observe",
            &format!("metrics=custom.metrics:value=${{env:{SECRET_VARIABLE}}}"),
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(start.status.code(), Some(0), "{}", stderr(&start));
    let started = json_stdout(&start);
    let id = started["session_id"]
        .as_str()
        .expect("session_id must be a string")
        .to_string();
    assert!(
        started["plugins"]
            .as_array()
            .is_some_and(|plugins| !plugins.is_empty()),
        "the explicit plugin path must be persisted: {started}"
    );

    // No `--plugin` here on purpose: the path comes from `state.json`.
    let step = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &id,
            "custom.act",
            "--with",
            &json!({"value": format!("${{env:{SECRET_VARIABLE}}}")}).to_string(),
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(
        step.status.code(),
        Some(0),
        "{}\n{}",
        stdout(&step),
        stderr(&step)
    );
    assert_no_secret(&stdout(&step), SECRET);
    let payload = json_stdout(&step);
    assert_eq!(payload["step"]["result"]["count"], 1, "{payload}");
    assert_eq!(payload["step"]["result"]["last"], "***", "{payload}");
    assert_eq!(
        payload["step"]["result"]["observed"]["metrics"]["value"], "***",
        "a plugin observer must run in `da session`: {payload}"
    );
    assert_eq!(payload["snapshot"]["count"], 1, "{payload}");
    assert_eq!(payload["snapshot"]["last"], "***", "{payload}");

    let snapshot = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "snapshot",
            &id,
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(snapshot.status.code(), Some(0), "{}", stderr(&snapshot));
    assert_no_secret(&stdout(&snapshot), SECRET);
    let snapshot = json_stdout(&snapshot);
    assert_eq!(snapshot["snapshot"]["count"], 1, "{snapshot}");
    assert_eq!(snapshot["snapshot"]["last"], "***", "{snapshot}");

    let export_path = dir.path().join("exported.yaml");
    let export = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "export",
            &id,
            "--out",
            path_str(&export_path),
            "--name",
            "Plugin export",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(export.status.code(), Some(0), "{}", stderr(&export));
    let exported = fs::read_to_string(&export_path).expect("exported case");
    assert!(exported.contains("custom.act:"), "{exported}");
    assert!(
        exported.contains(SECRET_VARIABLE),
        "export must keep the environment placeholder: {exported}"
    );
    assert_no_secret(&exported, SECRET);

    let steps =
        fs::read_to_string(sessions.join(&id).join("steps.jsonl")).expect("steps.jsonl must exist");
    assert!(steps.contains("custom.act"), "{steps}");
    assert_no_secret(&steps, SECRET);
    assert!(
        steps.contains("***"),
        "the step log must show redaction: {steps}"
    );
    let state =
        fs::read_to_string(sessions.join(&id).join("state.json")).expect("state.json must exist");
    assert_no_secret(&state, SECRET);

    let stop = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "stop",
            &id,
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(stop.status.code(), Some(0), "{}", stderr(&stop));
    assert_eq!(json_stdout(&stop)["ok"], true);
    assert!(
        !sessions.join(&id).exists(),
        "stop must remove the session directory"
    );
}

#[test]
fn session_start_reaps_an_expired_plugin_handle_without_adopting_its_path() {
    let dir = TempDir::new("plugin-reap");
    let sessions = dir.path().join("sessions");
    let start = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "--idle-timeout-ms",
            "1",
            "start",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(start.status.code(), Some(0), "{}", stderr(&start));
    let first = json_stdout(&start);
    let first_id = first["session_id"]
        .as_str()
        .expect("session_id must be a string")
        .to_string();

    let step = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &first_id,
            "custom.act",
            "--with",
            r#"{"value":"kept"}"#,
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(
        step.status.code(),
        Some(0),
        "{}\n{}",
        stdout(&step),
        stderr(&step)
    );

    std::thread::sleep(Duration::from_millis(50));

    // No `--plugin` here: the fixture is registered only because the expired
    // session persisted it, and reaping must dispose its opaque handle.
    let second = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    let second = json_stdout(&second);
    assert_eq!(second["ok"], true, "{second}");
    assert!(
        second["reaped"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item == &json!(first_id))),
        "the expired plugin session must be reaped: {second}"
    );
    assert_eq!(
        second["reap_errors"].as_array().map(Vec::len),
        Some(0),
        "reaping must close the plugin handle without errors: {second}"
    );
    assert_eq!(
        second["plugins"].as_array().map(Vec::len),
        Some(0),
        "a plugin loaded only for reaping must not enter the new session: {second}"
    );
    assert!(
        !sessions.join(&first_id).exists(),
        "the expired session directory must be removed"
    );

    let second_id = second["session_id"]
        .as_str()
        .expect("session_id must be a string")
        .to_string();
    let stop = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "stop",
            &second_id,
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(stop.status.code(), Some(0), "{}", stderr(&stop));
}

/// A `da mcp` child process driven over its real standard input and output.
#[cfg(feature = "mcp")]
struct McpServer {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    seen: String,
}

#[cfg(feature = "mcp")]
impl McpServer {
    fn start(dir: &Path, sessions: &Path) -> Self {
        Self::start_with_args(dir, sessions, &[])
    }

    fn start_with_args(dir: &Path, sessions: &Path, extra_args: &[&str]) -> Self {
        let mut child = Command::new(da())
            .args([
                "--plugin",
                fixture(),
                "mcp",
                "--sessions-dir",
                path_str(sessions),
            ])
            .args(extra_args)
            .current_dir(dir)
            .env(SECRET_VARIABLE, SECRET)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start `da mcp`");
        let stdin = child.stdin.take().expect("mcp stdin");
        let stdout = BufReader::new(child.stdout.take().expect("mcp stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            seen: String::new(),
        }
    }

    fn request(&mut self, message: Value) -> Value {
        let text = message.to_string();
        let stdin = self.stdin.as_mut().expect("mcp stdin is open");
        writeln!(stdin, "{text}").expect("write mcp request");
        stdin.flush().expect("flush mcp request");
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read mcp answer");
        assert!(read > 0, "da mcp closed its stdout");
        self.seen.push_str(&line);
        serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("da mcp must answer JSON ({error}): {line:?}"))
    }

    fn initialize(&mut self) {
        let answer = self.request(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "plug-test", "version": "0"},
            },
        }));
        assert_eq!(answer["result"]["serverInfo"]["name"], "da", "{answer}");
        let stdin = self.stdin.as_mut().expect("mcp stdin is open");
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .expect("write initialized notification");
        stdin.flush().expect("flush initialized notification");
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }))
    }
}

#[cfg(feature = "mcp")]
impl Drop for McpServer {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(feature = "mcp")]
fn payload(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("a tool result must carry text: {response}"));
    serde_json::from_str(text)
        .unwrap_or_else(|error| panic!("the tool text must be JSON ({error}): {text:?}"))
}

#[test]
#[cfg(feature = "mcp")]
fn mcp_drives_a_plugin_session_without_leaking_secrets() {
    let dir = TempDir::new("plugin-mcp");
    let sessions = dir.path().join("mcp-sessions");
    let mut server = McpServer::start(dir.path(), &sessions);
    server.initialize();

    let started = payload(&server.call(1, "session_start", json!({})));
    assert_eq!(started["ok"], true, "{started}");
    let id = started["session_id"]
        .as_str()
        .expect("session_id must be a string")
        .to_string();

    let step = payload(&server.call(
        2,
        "step",
        json!({
            "session_id": id,
            "use": "custom.act",
            "with": {"value": format!("${{env:{SECRET_VARIABLE}}}")},
        }),
    ));
    assert_eq!(step["ok"], true, "{step}");
    assert_eq!(step["step"]["result"]["count"], 1, "{step}");
    assert_eq!(step["step"]["result"]["last"], "***", "{step}");

    let snapshot = payload(&server.call(3, "snapshot", json!({"session_id": id})));
    assert_eq!(snapshot["ok"], true, "{snapshot}");
    assert_eq!(snapshot["snapshot"]["count"], 1, "{snapshot}");
    assert_eq!(snapshot["snapshot"]["last"], "***", "{snapshot}");

    let exported =
        payload(&server.call(4, "export", json!({"session_id": id, "name": "MCP plugin"})));
    assert_eq!(exported["ok"], true, "{exported}");
    let yaml = exported["yaml"].as_str().expect("exported yaml");
    assert!(yaml.contains("custom.act:"), "{yaml}");
    assert!(yaml.contains(SECRET_VARIABLE), "{yaml}");

    let stopped = payload(&server.call(5, "session_stop", json!({"session_id": id})));
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(
        !sessions.join(&id).exists(),
        "mcp stop must remove the session"
    );

    assert_no_secret(&server.seen, SECRET);
    assert!(server.seen.contains("***"), "{}", server.seen);

    server.stdin = None;
    let mut rest = String::new();
    let _ = server.stdout.read_to_string(&mut rest);
    assert!(
        rest.trim().is_empty(),
        "da mcp wrote unsolicited output: {rest:?}"
    );
    server.seen.push_str(&rest);
    let status = server.child.wait().expect("wait for da mcp");
    assert!(status.success(), "da mcp exited with {status}");
}

#[test]
fn hung_and_broken_plugin_rpcs_exit_with_documented_codes_and_no_orphans() {
    let dir = TempDir::new("plugin-failures");
    let case = dir.write("case.yaml", SIMPLE_CASE);

    // `hang-manifest` proves the initial manifest call has its own deadline.
    let manifest_pid = dir.path().join("hang-manifest.pid");
    let schema = run_da(
        dir.path(),
        &["--plugin", fixture(), "schema", "--output", "json"],
        &[
            ("DA_FIXTURE_MODE", "hang-manifest"),
            ("DA_FIXTURE_PID_FILE", path_str(&manifest_pid)),
            // The deadline starts when the request is written, before the
            // fixture process has necessarily finished starting. Leave enough
            // budget for startup so the pid record is never cut mid-write.
            ("DA_PLUGIN_TIMEOUT_MS", "2000"),
        ],
    );
    assert_eq!(schema.status.code(), Some(5), "{}", stderr(&schema));
    assert_process_gone(fixture_pid(&manifest_pid));

    for (mode, expected) in [("hang", 5), ("crash", 4), ("bad-json", 4)] {
        let pid_file = dir.path().join(format!("{mode}.pid"));
        let mut environment = vec![
            ("DA_FIXTURE_MODE", mode),
            ("DA_FIXTURE_PID_FILE", path_str(&pid_file)),
        ];
        if mode == "hang" {
            environment.push(("DA_PLUGIN_TIMEOUT_MS", "2000"));
        }
        let run = run_da(
            dir.path(),
            &[
                "--plugin",
                fixture(),
                "run",
                path_str(&case),
                "--no-bundle",
                "--output",
                "json",
            ],
            &environment,
        );
        assert_eq!(
            run.status.code(),
            Some(expected),
            "{mode}: stdout={}\nstderr={}",
            stdout(&run),
            stderr(&run)
        );
        assert_process_gone(fixture_pid(&pid_file));
    }
}

fn run_hang_stdin_timeout(dir: &Path, label: &str, args: &[&str]) -> Value {
    let pid_file = dir.join(format!("hang-stdin-{label}.pid"));
    let started = Instant::now();
    let output = run_da(
        dir,
        args,
        &[
            ("DA_FIXTURE_MODE", "hang-stdin"),
            ("DA_FIXTURE_PID_FILE", path_str(&pid_file)),
            ("DA_PLUGIN_TIMEOUT_MS", "2000"),
        ],
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{label} blocked past DA_PLUGIN_TIMEOUT_MS: {:?}",
        started.elapsed()
    );
    assert_eq!(
        output.status.code(),
        Some(5),
        "hang-stdin {label}: stdout={}\nstderr={}",
        stdout(&output),
        stderr(&output)
    );
    let json = json_stdout(&output);
    assert_eq!(json["ok"], false, "{json}");
    assert_process_gone(fixture_pid(&pid_file));
    json
}

#[test]
fn large_request_against_unreading_plugin_times_out_and_reaps_child() {
    let dir = TempDir::new("plugin-hang-stdin");
    // Larger than default and resized OS pipe buffers (>1 MiB) so writing the
    // JSON-RPC request line blocks in the OS pipe unless the writer enforces
    // the request deadline.
    let large_value = "x".repeat(2 * 1024 * 1024);
    let large_case = dir.write(
        "large-case.yaml",
        &format!("name: Large plugin step\nsteps:\n  - custom.echo:\n      value: {large_value}\n"),
    );

    // 1. Normal mode succeeds end-to-end with the >1 MiB payload.
    let ok_run = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "run",
            path_str(&large_case),
            "--no-bundle",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(
        ok_run.status.code(),
        Some(0),
        "stdout={}\nstderr={}",
        stdout(&ok_run),
        stderr(&ok_run)
    );
    assert_eq!(
        json_stdout(&ok_run)["cases"][0]["steps"][0]["result"]["echo"]
            .as_str()
            .map(str::len),
        Some(large_value.len())
    );

    // 2. `da run` against `hang-stdin` times out within DA_PLUGIN_TIMEOUT_MS,
    // exits with code 5, and leaves no surviving child process.
    let run_json = run_hang_stdin_timeout(
        dir.path(),
        "run",
        &[
            "--plugin",
            fixture(),
            "run",
            path_str(&large_case),
            "--no-bundle",
            "--output",
            "json",
        ],
    );
    assert_eq!(run_json["errors"][0]["kind"], "timeout", "{run_json}");

    // 3. `da lint` against `hang-stdin` also exits with code 5 and reaps the child.
    let _lint_json = run_hang_stdin_timeout(
        dir.path(),
        "lint",
        &[
            "--plugin",
            fixture(),
            "lint",
            path_str(&large_case),
            "--output",
            "json",
        ],
    );

    // 4. `da session step` against `hang-stdin` also exits with code 5 and reaps the child.
    let sessions = dir.path().join("sessions");
    let session_start = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(
        session_start.status.code(),
        Some(0),
        "{}",
        stderr(&session_start)
    );
    let session_id = json_stdout(&session_start)["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let large_step_file = dir.write(
        "large-step.json",
        &json!({"use": "custom.echo", "with": {"value": large_value}}).to_string(),
    );
    let step_json = run_hang_stdin_timeout(
        dir.path(),
        "step",
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &session_id,
            "--file",
            path_str(&large_step_file),
            "--output",
            "json",
        ],
    );
    assert_eq!(step_json["step"]["error"]["kind"], "timeout", "{step_json}");

    let stop = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "stop",
            &session_id,
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(stop.status.code(), Some(0), "{}", stderr(&stop));
}

#[test]
fn unrelated_or_handleless_plugins_are_not_spawned_by_list_or_start_while_expired_handles_reap() {
    let dir = TempDir::new("plugin-explicit-load");
    let sessions = dir.path().join("sessions");

    // 1. Active session with an active plugin handle (not expired).
    let active_start = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    ));
    let active_id = active_start["session_id"].as_str().unwrap().to_string();
    let active_step = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &active_id,
            "custom.act",
            "--with",
            r#"{"value":"still-active"}"#,
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(
        active_step.status.code(),
        Some(0),
        "{}",
        stderr(&active_step)
    );

    // 2. Expired session with NO handles (never ran a step).
    let empty_start = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "--idle-timeout-ms",
            "1",
            "start",
            "--output",
            "json",
        ],
        &[],
    ));
    let empty_id = empty_start["session_id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(50));

    // Neither `list` nor `start` should spawn the plugin for `active_id` or `empty_id`.
    // Setting `hang-manifest` + PID file ensures any accidental spawn is caught immediately.
    let list_pid = dir.path().join("list-should-not-spawn.pid");
    let listed = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "list",
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_MODE", "hang-manifest"),
            ("DA_FIXTURE_PID_FILE", path_str(&list_pid)),
            ("DA_PLUGIN_TIMEOUT_MS", "500"),
        ],
    );
    assert_eq!(
        listed.status.code(),
        Some(0),
        "stdout={}\nstderr={}",
        stdout(&listed),
        stderr(&listed)
    );
    assert!(
        !list_pid.exists(),
        "list must not spawn plugins for active or handle-less sessions"
    );
    let listed_json = json_stdout(&listed);
    assert!(
        listed_json["reaped"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item == &json!(empty_id))),
        "handle-less expired session must still be reaped: {listed_json}"
    );
    assert!(
        listed_json["sessions"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["id"] == json!(active_id))),
        "active session must remain listed: {listed_json}"
    );

    let start_pid = dir.path().join("start-should-not-spawn.pid");
    let started_other = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_MODE", "hang-manifest"),
            ("DA_FIXTURE_PID_FILE", path_str(&start_pid)),
            ("DA_PLUGIN_TIMEOUT_MS", "500"),
        ],
    );
    assert_eq!(
        started_other.status.code(),
        Some(0),
        "{}",
        stderr(&started_other)
    );
    assert!(
        !start_pid.exists(),
        "start must not spawn plugins for unrelated active sessions"
    );
    let started_other_id = json_stdout(&started_other)["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 3. Expired session WITH an active handle IS reaped using its persisted absolute path.
    let exp_start = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "--idle-timeout-ms",
            "1",
            "start",
            "--output",
            "json",
        ],
        &[],
    ));
    let exp_id = exp_start["session_id"].as_str().unwrap().to_string();
    let exp_step = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &exp_id,
            "custom.act",
            "--with",
            r#"{"value":"to-reap"}"#,
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(exp_step.status.code(), Some(0), "{}", stderr(&exp_step));
    let exp_state: Value = serde_json::from_str(
        &fs::read_to_string(sessions.join(&exp_id).join("state.json")).unwrap(),
    )
    .unwrap();
    let exp_token = exp_state["handles"]["custom"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    std::thread::sleep(Duration::from_millis(50));

    let close_log = dir.path().join("reap-close.log");
    let list_reap = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "list",
            "--output",
            "json",
        ],
        &[("DA_FIXTURE_CLOSE_LOG", path_str(&close_log))],
    );
    assert_eq!(list_reap.status.code(), Some(0), "{}", stderr(&list_reap));
    let list_reap_json = json_stdout(&list_reap);
    assert!(
        list_reap_json["reaped"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item == &json!(exp_id))),
        "expired plugin session with handle must be reaped by list: {list_reap_json}"
    );
    assert!(
        !sessions.join(&exp_id).exists(),
        "reaped session directory must be deleted"
    );
    let closed_tokens = fs::read_to_string(&close_log).expect("close log must be written");
    assert!(
        closed_tokens.lines().any(|line| line == exp_token),
        "session/close must receive the persisted handle token `{exp_token}`: {closed_tokens}"
    );

    // Clean up remaining active sessions.
    for id in [active_id, started_other_id] {
        let stop = run_da(
            dir.path(),
            &[
                "session",
                "--sessions-dir",
                path_str(&sessions),
                "stop",
                &id,
                "--output",
                "json",
            ],
            &[],
        );
        assert_eq!(stop.status.code(), Some(0), "{}", stderr(&stop));
    }
}

#[test]
fn relative_and_non_canonical_persisted_plugin_paths_are_rejected_without_spawning() {
    let dir = TempDir::new("plugin-reject-relative");
    let sessions = dir.path().join("sessions");
    let bin_name = format!("cwd-plugin{}", std::env::consts::EXE_SUFFIX);
    let cwd_bin = dir.path().join(&bin_name);
    fs::copy(fixture(), &cwd_bin).expect("copy fixture binary to cwd");

    // Explicit `--plugin` CLI flag still resolves relative paths against cwd.
    let explicit_rel = format!("./{bin_name}");
    let schema = run_da(dir.path(), &["--plugin", &explicit_rel, "schema"], &[]);
    assert_eq!(schema.status.code(), Some(0), "{}", stderr(&schema));

    let started = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    ));
    let id = started["session_id"].as_str().unwrap().to_string();
    let state_path = sessions.join(&id).join("state.json");
    let mut state: Value = serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    state["plugins"] = json!([explicit_rel]);
    state["handles"] = json!({"custom": {"token": "evil-tok-1", "count": 1, "last": "x"}});
    fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    let evil_pid = dir.path().join("evil-spawned.pid");

    let step = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &id,
            "custom.act",
            "--with",
            r#"{"value":"hi"}"#,
            "--output",
            "json",
        ],
        &[("DA_FIXTURE_PID_FILE", path_str(&evil_pid))],
    );
    assert_ne!(step.status.code(), Some(0), "{}", stdout(&step));
    assert_eq!(json_stdout(&step)["ok"], false);
    assert!(!evil_pid.exists(), "step must not execute cwd binary");

    let snap = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "snapshot",
            &id,
            "--output",
            "json",
        ],
        &[("DA_FIXTURE_PID_FILE", path_str(&evil_pid))],
    );
    assert_ne!(snap.status.code(), Some(0), "{}", stdout(&snap));
    assert_eq!(json_stdout(&snap)["ok"], false);
    assert!(!evil_pid.exists(), "snapshot must not execute cwd binary");

    let stop = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "stop",
            &id,
            "--output",
            "json",
        ],
        &[("DA_FIXTURE_PID_FILE", path_str(&evil_pid))],
    );
    assert_eq!(stop.status.code(), Some(4), "{}", stdout(&stop));
    assert_eq!(json_stdout(&stop)["ok"], false);
    assert!(
        state_path.exists(),
        "state.json must be retained on rejection"
    );
    assert!(!evil_pid.exists(), "stop must not execute cwd binary");

    let list = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "--idle-timeout-ms",
            "0",
            "list",
            "--output",
            "json",
        ],
        &[("DA_FIXTURE_PID_FILE", path_str(&evil_pid))],
    );
    assert_eq!(list.status.code(), Some(4), "{}", stdout(&list));
    let list_json = json_stdout(&list);
    assert_eq!(list_json["ok"], false, "{list_json}");
    assert!(
        !list_json["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &json!(id)),
        "rejected session must not be reported as reaped: {list_json}"
    );
    assert!(!evil_pid.exists(), "list must not execute cwd binary");

    // Also test a non-canonical absolute path containing `..` during `start` reap.
    let sub = dir.path().join("sub");
    fs::create_dir_all(&sub).unwrap();
    let canonical_sub = fs::canonicalize(&sub).unwrap();
    let non_canonical_abs = format!("{}/../{}", canonical_sub.display(), bin_name);
    state["plugins"] = json!([non_canonical_abs]);
    state["last_active_ms"] = json!(1);
    state["idle_timeout_ms"] = json!(1);
    fs::write(&state_path, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    let start_reap = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[("DA_FIXTURE_PID_FILE", path_str(&evil_pid))],
    );
    assert_eq!(start_reap.status.code(), Some(0), "{}", stderr(&start_reap));
    let start_json = json_stdout(&start_reap);
    assert_eq!(start_json["ok"], true, "{start_json}");
    assert!(
        !start_json["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &json!(id)),
        "{start_json}"
    );
    assert!(
        !start_json["reap_errors"].as_array().unwrap().is_empty(),
        "{start_json}"
    );
    assert!(
        !evil_pid.exists(),
        "start reap must not execute non-canonical persisted binary"
    );
}

#[test]
fn missing_plugin_on_unrelated_session_does_not_block_list_or_start() {
    let dir = TempDir::new("plugin-missing-isolation");
    let sessions = dir.path().join("sessions");

    let temp_active_bin = dir
        .path()
        .join(format!("ephemeral-active{}", std::env::consts::EXE_SUFFIX));
    let temp_exp_bin = dir
        .path()
        .join(format!("ephemeral-expired{}", std::env::consts::EXE_SUFFIX));
    fs::copy(fixture(), &temp_active_bin).expect("copy active fixture binary");
    fs::copy(fixture(), &temp_exp_bin).expect("copy expired fixture binary");

    // Active session whose plugin binary disappears later.
    let broken_active_id = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            path_str(&temp_active_bin),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    ))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        run_da(
            dir.path(),
            &[
                "session",
                "--sessions-dir",
                path_str(&sessions),
                "step",
                &broken_active_id,
                "custom.act",
                "--with",
                r#"{"value":"active-missing"}"#,
            ],
            &[],
        )
        .status
        .code(),
        Some(0)
    );

    // Expired session whose plugin binary also disappears later.
    let broken_exp_id = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            path_str(&temp_exp_bin),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "--idle-timeout-ms",
            "1",
            "start",
            "--output",
            "json",
        ],
        &[],
    ))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        run_da(
            dir.path(),
            &[
                "session",
                "--sessions-dir",
                path_str(&sessions),
                "step",
                &broken_exp_id,
                "custom.act",
                "--with",
                r#"{"value":"exp-missing"}"#,
            ],
            &[],
        )
        .status
        .code(),
        Some(0)
    );
    fs::remove_file(&temp_active_bin).expect("delete active ephemeral plugin");
    fs::remove_file(&temp_exp_bin).expect("delete expired ephemeral plugin");

    // Healthy active and expired sessions using `fixture()`.
    let ok_active_id = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    ))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        run_da(
            dir.path(),
            &[
                "session",
                "--sessions-dir",
                path_str(&sessions),
                "step",
                &ok_active_id,
                "custom.act",
                "--with",
                r#"{"value":"ok-active"}"#,
            ],
            &[],
        )
        .status
        .code(),
        Some(0)
    );

    let ok_exp_id = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "--idle-timeout-ms",
            "1",
            "start",
            "--output",
            "json",
        ],
        &[],
    ))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        run_da(
            dir.path(),
            &[
                "session",
                "--sessions-dir",
                path_str(&sessions),
                "step",
                &ok_exp_id,
                "custom.act",
                "--with",
                r#"{"value":"ok-expired"}"#,
            ],
            &[],
        )
        .status
        .code(),
        Some(0)
    );
    std::thread::sleep(Duration::from_millis(50));

    // `list` must report `broken_exp_id` per-session without aborting listing
    // `broken_active_id` / `ok_active_id` or reaping `ok_exp_id`.
    let listed = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "list",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(listed.status.code(), Some(4), "{}", stdout(&listed));
    let listed_json = json_stdout(&listed);
    let session_ids: Vec<&str> = listed_json["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(
        session_ids.contains(&ok_active_id.as_str()),
        "{listed_json}"
    );
    assert!(
        session_ids.contains(&broken_active_id.as_str()),
        "{listed_json}"
    );
    let reaped_ids: Vec<&str> = listed_json["reaped"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(reaped_ids.contains(&ok_exp_id.as_str()), "{listed_json}");
    assert!(
        !reaped_ids.contains(&broken_exp_id.as_str()),
        "{listed_json}"
    );
    assert!(!sessions.join(&ok_exp_id).exists());
    assert!(sessions.join(&broken_exp_id).exists());
    let list_errors = listed_json["errors"].to_string();
    assert!(
        list_errors.contains("ephemeral-expired"),
        "expected expired plugin error in `errors`: {listed_json}"
    );
    assert!(
        !list_errors.contains("ephemeral-active"),
        "active session plugin must not be loaded by `list`: {listed_json}"
    );

    // `start` must still succeed despite `broken_active_id` and `broken_exp_id`.
    let started = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(started.status.code(), Some(0), "{}", stderr(&started));
    let started_json = json_stdout(&started);
    assert_eq!(started_json["ok"], true, "{started_json}");
    let reap_errors = started_json["reap_errors"].to_string();
    assert!(
        reap_errors.contains("ephemeral-expired"),
        "expected expired plugin error in `reap_errors`: {started_json}"
    );
    assert!(
        !reap_errors.contains("ephemeral-active"),
        "active session plugin must not be loaded by `start`: {started_json}"
    );
    assert!(
        !started_json["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &json!(broken_exp_id)),
        "{started_json}"
    );
    let new_id = started_json["session_id"].as_str().unwrap().to_string();
    for id in [ok_active_id, new_id] {
        assert_eq!(
            run_da(
                dir.path(),
                &[
                    "session",
                    "--sessions-dir",
                    path_str(&sessions),
                    "stop",
                    &id,
                ],
                &[],
            )
            .status
            .code(),
            Some(0)
        );
    }
}

#[test]
fn close_failure_retains_handle_excludes_reaped_and_scrubs_secrets_everywhere() {
    let dir = TempDir::new("plugin-close-secret-retain");
    let sessions = dir.path().join("sessions");

    // 1. `da session stop` failing `session/close`: secret scrubbed, state & handle retained, retry succeeds.
    let stop_id = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--output",
            "json",
        ],
        &[],
    ))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let step_payload = json!({"value": format!("${{env:{SECRET_VARIABLE}}}")}).to_string();
    let step = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &stop_id,
            "custom.act",
            "--with",
            &step_payload,
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(step.status.code(), Some(0), "{}", stderr(&step));
    let state_path = sessions.join(&stop_id).join("state.json");
    let steps_path = sessions.join(&stop_id).join("steps.jsonl");
    let initial_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    let stop_token = initial_state["handles"]["custom"]["token"]
        .as_str()
        .unwrap()
        .to_string();

    let failed_stop = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "stop",
            &stop_id,
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_MODE", "close-fail-secret"),
            (SECRET_VARIABLE, SECRET),
        ],
    );
    assert_eq!(
        failed_stop.status.code(),
        Some(4),
        "{}",
        stdout(&failed_stop)
    );
    let failed_stop_out = stdout(&failed_stop);
    assert_no_secret(&failed_stop_out, SECRET);
    assert_no_secret(&stderr(&failed_stop), SECRET);
    assert!(failed_stop_out.contains("***"), "{failed_stop_out}");
    let failed_stop_json = json_stdout(&failed_stop);
    assert_eq!(failed_stop_json["ok"], false, "{failed_stop_json}");
    assert!(
        state_path.exists(),
        "state.json must be retained when session/close fails"
    );
    let retained_state: Value =
        serde_json::from_str(&fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(
        retained_state["handles"]["custom"]["token"],
        json!(stop_token)
    );
    let steps_text = fs::read_to_string(&steps_path).expect("steps.jsonl must exist");
    assert_no_secret(&steps_text, SECRET);
    assert!(steps_text.contains("***"), "{steps_text}");

    let close_log = dir.path().join("retry-close.log");
    let recovered_stop = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "stop",
            &stop_id,
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_CLOSE_LOG", path_str(&close_log)),
            (SECRET_VARIABLE, SECRET),
        ],
    );
    assert_eq!(
        recovered_stop.status.code(),
        Some(0),
        "{}",
        stdout(&recovered_stop)
    );
    assert_eq!(json_stdout(&recovered_stop)["ok"], true);
    assert!(
        !sessions.join(&stop_id).exists(),
        "session dir must be removed once retry close succeeds"
    );
    assert!(
        fs::read_to_string(&close_log)
            .unwrap()
            .lines()
            .any(|line| line == stop_token),
        "retry stop must close retained handle `{stop_token}`"
    );

    // 2. `da session list` and `da session start` reap failures: secret scrubbed,
    // `reaped` excludes failed session, state & handle retained, retry reaps cleanly.
    let reap_id = json_stdout(&run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "--idle-timeout-ms",
            "1",
            "start",
            "--output",
            "json",
        ],
        &[],
    ))["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        run_da(
            dir.path(),
            &[
                "session",
                "--sessions-dir",
                path_str(&sessions),
                "step",
                &reap_id,
                "custom.act",
                "--with",
                &step_payload,
                "--output",
                "json",
            ],
            &[(SECRET_VARIABLE, SECRET)],
        )
        .status
        .code(),
        Some(0)
    );
    let reap_state_path = sessions.join(&reap_id).join("state.json");
    let reap_state: Value =
        serde_json::from_str(&fs::read_to_string(&reap_state_path).unwrap()).unwrap();
    let reap_token = reap_state["handles"]["custom"]["token"]
        .as_str()
        .unwrap()
        .to_string();
    std::thread::sleep(Duration::from_millis(50));

    let failed_list = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "list",
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_MODE", "close-fail-secret"),
            (SECRET_VARIABLE, SECRET),
        ],
    );
    assert_eq!(
        failed_list.status.code(),
        Some(4),
        "{}",
        stdout(&failed_list)
    );
    let failed_list_out = stdout(&failed_list);
    assert_no_secret(&failed_list_out, SECRET);
    assert_no_secret(&stderr(&failed_list), SECRET);
    assert!(failed_list_out.contains("***"), "{failed_list_out}");
    let failed_list_json = json_stdout(&failed_list);
    assert_eq!(failed_list_json["ok"], false, "{failed_list_json}");
    assert!(
        !failed_list_json["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &json!(reap_id)),
        "failed reap must not enter `reaped`: {failed_list_json}"
    );
    assert!(
        reap_state_path.exists(),
        "state.json must be retained after failed list reap"
    );
    let reap_steps_text =
        fs::read_to_string(sessions.join(&reap_id).join("steps.jsonl")).expect("steps.jsonl");
    assert_no_secret(&reap_steps_text, SECRET);
    assert!(reap_steps_text.contains("***"), "{reap_steps_text}");

    let extra_secret = "extra-observe-secret-77";
    let failed_start = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--observe",
            "metrics=custom.metrics:value=${env:DA_PLUGIN_EXTRA_SECRET}",
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_MODE", "close-fail-secret"),
            (SECRET_VARIABLE, SECRET),
            ("DA_PLUGIN_EXTRA_SECRET", extra_secret),
        ],
    );
    assert_eq!(
        failed_start.status.code(),
        Some(0),
        "{}\n{}",
        stdout(&failed_start),
        stderr(&failed_start)
    );
    let failed_start_out = stdout(&failed_start);
    assert_no_secret(&failed_start_out, SECRET);
    assert_no_secret(&failed_start_out, extra_secret);
    assert_no_secret(&stderr(&failed_start), SECRET);
    assert_no_secret(&stderr(&failed_start), extra_secret);
    assert!(failed_start_out.contains("***"), "{failed_start_out}");
    let failed_start_json = json_stdout(&failed_start);
    assert!(
        !failed_start_json["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &json!(reap_id)),
        "failed reap must not enter `reaped`: {failed_start_json}"
    );
    assert!(
        !failed_start_json["reap_errors"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{failed_start_json}"
    );
    assert!(reap_state_path.exists());

    let recovered_list = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "list",
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_CLOSE_LOG", path_str(&close_log)),
            (SECRET_VARIABLE, SECRET),
        ],
    );
    assert_eq!(
        recovered_list.status.code(),
        Some(0),
        "{}",
        stdout(&recovered_list)
    );
    let recovered_list_json = json_stdout(&recovered_list);
    assert!(
        recovered_list_json["reaped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == &json!(reap_id)),
        "recovered list must reap `{reap_id}`: {recovered_list_json}"
    );
    assert!(!sessions.join(&reap_id).exists());
    assert!(
        fs::read_to_string(&close_log)
            .unwrap()
            .lines()
            .any(|line| line == reap_token),
        "recovered list must close retained handle `{reap_token}`"
    );

    // 3. `da run` bundles and records scrub secrets returned by failing `session/close`.
    let case = dir.write(
        "close-fail.yaml",
        &format!(
            "name: Close secret redaction\nsteps:\n  - custom.act:\n      value: ${{env:{SECRET_VARIABLE}}}\n"
        ),
    );
    let bundles = dir.path().join("bundles");
    let record = dir.path().join("record");
    let run = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "run",
            path_str(&case),
            "--bundle-dir",
            path_str(&bundles),
            "--record",
            path_str(&record),
            "--output",
            "json",
        ],
        &[
            ("DA_FIXTURE_MODE", "close-fail-secret"),
            (SECRET_VARIABLE, SECRET),
        ],
    );
    assert_eq!(
        run.status.code(),
        Some(4),
        "{}\n{}",
        stdout(&run),
        stderr(&run)
    );
    assert_no_secret(&stdout(&run), SECRET);
    assert_no_secret(&stderr(&run), SECRET);
    assert!(stdout(&run).contains("***"), "{}", stdout(&run));

    for root in [&bundles, &record] {
        for file in files_under(root) {
            if file.extension().and_then(|e| e.to_str()) == Some("bin") {
                continue;
            }
            let contents = fs::read_to_string(&file).expect("text bundle file");
            assert_no_secret(&contents, SECRET);
        }
        let report_file = find_named(root, "report.json");
        let report_text = fs::read_to_string(&report_file).expect("bundle report.json");
        assert!(report_text.contains("***"), "{report_text}");
    }

    let run_text = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "run",
            path_str(&case),
            "--no-bundle",
            "--output",
            "text",
        ],
        &[
            ("DA_FIXTURE_MODE", "close-fail-secret"),
            (SECRET_VARIABLE, SECRET),
        ],
    );
    assert_eq!(
        run_text.status.code(),
        Some(4),
        "{}\n{}",
        stdout(&run_text),
        stderr(&run_text)
    );
    assert_no_secret(&stdout(&run_text), SECRET);
    assert_no_secret(&stderr(&run_text), SECRET);
    assert!(stdout(&run_text).contains("***"), "{}", stdout(&run_text));
}

const PLUGIN_MOCK_HOOK_RS: &str = r#"
use std::env;
use std::fs;
use std::io::{self, Read, Write};

fn main() {
    let secret = env::var("DA_PLUGIN_TEST_SECRET").unwrap_or_default();
    let mut stdin_bytes = Vec::new();
    let _ = io::stdin().read_to_end(&mut stdin_bytes);
    if let Some(out_path) = env::args().nth(1) {
        let _ = fs::write(out_path, &stdin_bytes);
    }
    let line = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"passed\":true,\"rationale\":\"verified {secret}\",\"confidence\":0.95,\"model\":\"mock-judge-v1\",\"latency_ms\":7,\"usage\":{{\"tokens\":12}},\"trace_id\":\"tr-{secret}\"}}}}"
    );
    let _ = writeln!(io::stdout(), "{line}");
    let _ = io::stdout().flush();
}
"#;

fn compile_mock_hook(dir: &TempDir) -> PathBuf {
    let src = dir.write("plugin_mock_hook.rs", PLUGIN_MOCK_HOOK_RS);
    let bin = dir.path().join(if cfg!(windows) {
        "plugin_mock_hook.exe"
    } else {
        "plugin_mock_hook"
    });
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let status = Command::new(rustc)
        .arg(&src)
        .arg("-O")
        .arg("-o")
        .arg(&bin)
        .status()
        .expect("compile plugin_mock_hook.rs");
    assert!(status.success(), "rustc failed for plugin_mock_hook.rs");
    bin
}

#[test]
fn session_persists_plugin_and_model_hook_together_without_repeating_plugin() {
    let dir = TempDir::new("plugin-and-model-hook-cli");
    let sessions = dir.path().join("sessions");
    let hook_bin = compile_mock_hook(&dir);
    let req_dump = dir.path().join("hook_req.json");

    let start = run_da(
        dir.path(),
        &[
            "--plugin",
            fixture(),
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "start",
            "--model-hook",
            path_str(&hook_bin),
            "--model-hook-arg",
            path_str(&req_dump),
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(start.status.code(), Some(0), "{}", stderr(&start));
    let start_json = json_stdout(&start);
    assert_eq!(start_json["ok"], true, "{start_json}");
    assert_eq!(
        start_json["plugins"].as_array().map(Vec::len),
        Some(1),
        "{start_json}"
    );
    assert!(start_json["model_hook"].is_string(), "{start_json}");
    let id = start_json["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let step_payload = format!(r#"{{"value":"${{env:{SECRET_VARIABLE}}}"}}"#);
    let step = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "step",
            &id,
            "custom.act",
            "--with",
            &step_payload,
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(step.status.code(), Some(0), "{}", stdout(&step));

    // Invoke `judge` without repeating `--plugin` or `--hook`; persisted
    // `plugins` must reattach `custom` to collect its snapshot, and persisted
    // `model_hook` must run with secret redaction in both directions.
    let rubric = format!("Verify ${{env:{SECRET_VARIABLE}}} plugin session");
    let judge = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "judge",
            &id,
            "--rubric",
            &rubric,
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(judge.status.code(), Some(0), "{}", stdout(&judge));
    let judge_out = stdout(&judge);
    assert_no_secret(&judge_out, SECRET);
    let judge_json = json_stdout(&judge);
    assert_eq!(judge_json["ok"], true, "{judge_json}");
    assert_eq!(judge_json["passed"], true, "{judge_json}");
    assert_eq!(judge_json["session_kind"], "custom", "{judge_json}");
    assert_eq!(judge_json["rationale"], "verified ***", "{judge_json}");
    assert_eq!(judge_json["trace_id"], "tr-***", "{judge_json}");

    let req_raw = fs::read_to_string(&req_dump).expect("read hook request");
    assert_no_secret(&req_raw, SECRET);
    let req_json: Value = serde_json::from_str(req_raw.trim()).expect("hook request json");
    assert_eq!(req_json["method"], "model/judge");
    assert_eq!(req_json["params"]["session_kind"], "custom");
    assert_eq!(req_json["params"]["snapshot"]["session"], "custom");
    assert_eq!(req_json["params"]["snapshot"]["count"], 1);

    let exported = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "export",
            &id,
            "--output",
            "json",
        ],
        &[(SECRET_VARIABLE, SECRET)],
    );
    assert_eq!(exported.status.code(), Some(0), "{}", stdout(&exported));
    let export_json = json_stdout(&exported);
    assert_eq!(export_json["exported_steps"], 1, "{export_json}");
    let yaml = export_json["yaml"].as_str().expect("yaml");
    assert!(yaml.contains("custom.act:"), "{yaml}");
    assert!(!yaml.contains("model.judge"), "{yaml}");

    let stopped = run_da(
        dir.path(),
        &[
            "session",
            "--sessions-dir",
            path_str(&sessions),
            "stop",
            &id,
            "--output",
            "json",
        ],
        &[],
    );
    assert_eq!(stopped.status.code(), Some(0), "{}", stdout(&stopped));
    assert!(!sessions.join(&id).exists());
}

#[test]
#[cfg(feature = "mcp")]
fn mcp_composes_plugin_and_model_hook_flags() {
    let dir = TempDir::new("plugin-and-model-hook-mcp");
    let sessions = dir.path().join("mcp-sessions");
    let hook_bin = compile_mock_hook(&dir);
    let req_dump = dir.path().join("mcp_hook_req.json");

    let mut server = McpServer::start_with_args(
        dir.path(),
        &sessions,
        &[
            "--model-hook",
            path_str(&hook_bin),
            "--model-hook-arg",
            path_str(&req_dump),
        ],
    );
    server.initialize();

    let listed = server.request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "session_start",
            "step",
            "snapshot",
            "select_ref",
            "locate",
            "judge",
            "extract",
            "act",
            "export",
            "run",
            "session_stop"
        ]
    );

    let started = payload(&server.call(2, "session_start", json!({})));
    assert_eq!(started["ok"], true, "{started}");
    assert_eq!(started["plugins"].as_array().map(Vec::len), Some(1));
    assert!(started["model_hook"].is_string(), "{started}");
    let id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let step = payload(&server.call(
        3,
        "step",
        json!({
            "session_id": id,
            "use": "custom.act",
            "with": {"value": format!("${{env:{SECRET_VARIABLE}}}")},
        }),
    ));
    assert_eq!(step["ok"], true, "{step}");

    let judged = payload(&server.call(
        4,
        "judge",
        json!({
            "session_id": id,
            "rubric": format!("Check ${{env:{SECRET_VARIABLE}}} over MCP"),
        }),
    ));
    assert_eq!(judged["ok"], true, "{judged}");
    assert_eq!(judged["passed"], true, "{judged}");
    assert_eq!(judged["session_kind"], "custom", "{judged}");
    assert_eq!(judged["rationale"], "verified ***", "{judged}");

    let req_raw = fs::read_to_string(&req_dump).expect("read mcp hook request");
    assert_no_secret(&req_raw, SECRET);
    let req_json: Value = serde_json::from_str(req_raw.trim()).expect("mcp hook request json");
    assert_eq!(req_json["params"]["snapshot"]["session"], "custom");

    let exported = payload(&server.call(5, "export", json!({"session_id": id})));
    assert_eq!(exported["exported_steps"], 1, "{exported}");
    let yaml = exported["yaml"].as_str().expect("yaml");
    assert!(yaml.contains("custom.act:"), "{yaml}");
    assert!(!yaml.contains("model.judge"), "{yaml}");

    let stopped = payload(&server.call(6, "session_stop", json!({"session_id": id})));
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(!sessions.join(&id).exists());

    assert_no_secret(&server.seen, SECRET);
    server.stdin = None;
    let mut rest = String::new();
    let _ = server.stdout.read_to_string(&mut rest);
    assert!(rest.trim().is_empty(), "unsolicited output: {rest:?}");
    let status = server.child.wait().expect("wait for da mcp");
    assert!(status.success(), "da mcp exited with {status}");
}
