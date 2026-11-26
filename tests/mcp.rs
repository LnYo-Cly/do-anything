//! End-to-end tests for `da mcp`.
//!
//! Every test drives a real `da mcp` child process through its own standard
//! input and output, the way an MCP host does: no in-process shortcut, no
//! library call. The session tests compare the exported case with the one
//! `da session export` writes for the same steps, and the whole standard output
//! of the server is checked for secret leakage in every form `Secrets` knows.

#![cfg(feature = "mcp")]

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

use serde_json::{Value, json};
use support::TempDir;

/// The environment variable the server and the case steps read the secret from.
const SECRET_VARIABLE: &str = "DA_MCP_TEST_SECRET";

/// A value long enough that its percent-encoded and base64 forms are searched.
const SECRET: &str = "mcp-secret-9f3a-DA";

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

/// Writes one diagnostic line to standard error.
///
/// `eprintln!` panics when the pipe is closed, and the crate denies the print
/// macros for the same reason; a skip note must not abort the test process.
#[cfg(feature = "browser")]
fn note(message: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}

/// A `da mcp` child process driven over its real standard input and output.
struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    /// Every byte the server wrote, so the leak checks see all of it.
    seen: String,
}

impl Server {
    /// Starts `da mcp` in `dir` with the test secret in its environment.
    fn start(dir: &Path) -> Self {
        let mut child = Command::new(da())
            .arg("mcp")
            .current_dir(dir)
            .env_remove("DA_MODEL")
            .env_remove("DA_MODEL_ADAPTER")
            .env_remove("DA_MODEL_ENDPOINT")
            .env_remove("DA_MODEL_API_KEY_ENV")
            .env(SECRET_VARIABLE, SECRET)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start `da mcp`");
        let stdin = child
            .stdin
            .take()
            .expect("the server's standard input must be piped");
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .expect("the server's standard output must be piped"),
        );
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            seen: String::new(),
        }
    }

    /// Sends one message on a line and reads the answer to it.
    fn request(&mut self, message: Value) -> Value {
        self.send(&message);
        self.receive()
    }

    /// Sends one message without reading an answer.
    fn send(&mut self, message: &Value) {
        let text = message.to_string();
        self.send_line(&text);
    }

    /// Sends one raw line, which may not be JSON at all.
    fn send_line(&mut self, text: &str) {
        let stdin = self
            .stdin
            .as_mut()
            .expect("the server's standard input is still open");
        writeln!(stdin, "{text}").expect("failed to write to the server");
        stdin
            .flush()
            .expect("failed to flush the server's standard input");
    }

    /// Reads one line-framed response.
    fn receive(&mut self) -> Value {
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("failed to read the server's answer");
        assert!(
            read > 0,
            "the server closed its standard output instead of answering"
        );
        self.seen.push_str(&line);
        serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!("the server must answer with one JSON object ({error}): {line:?}")
        })
    }

    /// Sends one message with `Content-Length` framing and reads a framed answer.
    fn request_framed(&mut self, message: Value) -> Value {
        let text = message.to_string();
        {
            let stdin = self
                .stdin
                .as_mut()
                .expect("the server's standard input is still open");
            write!(stdin, "Content-Length: {}\r\n\r\n{text}", text.len())
                .expect("failed to write a framed message");
            stdin
                .flush()
                .expect("failed to flush the server's standard input");
        }
        self.receive_framed()
    }

    /// Reads one response that must use `Content-Length` framing.
    fn receive_framed(&mut self) -> Value {
        let mut length: Option<usize> = None;
        loop {
            let mut line = String::new();
            let read = self
                .stdout
                .read_line(&mut line)
                .expect("failed to read the server's answer");
            assert!(
                read > 0,
                "the server closed its standard output instead of answering"
            );
            self.seen.push_str(&line);
            if line.trim().is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                panic!("a framed answer must start with headers, got {line:?}");
            };
            assert_eq!(
                name.trim().to_ascii_lowercase(),
                "content-length",
                "a framed answer carries only the length header"
            );
            length = Some(
                value
                    .trim()
                    .parse()
                    .expect("`Content-Length` must be a number of bytes"),
            );
        }
        let length = length.expect("a framed answer needs a `Content-Length` header");
        let mut body = vec![0_u8; length];
        self.stdout
            .read_exact(&mut body)
            .expect("failed to read a framed answer body");
        let text = String::from_utf8(body).expect("a response is UTF-8");
        self.seen.push_str(&text);
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("a framed answer must be JSON ({error}): {text:?}"))
    }

    /// Calls one tool and returns the whole JSON-RPC response.
    fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }))
    }

    /// Hashes one `initialize` handshake, as a host does before the first call.
    fn initialize(&mut self) {
        let answer = self.request(json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "da-mcp-test", "version": "0"},
            },
        }));
        assert_eq!(answer["result"]["serverInfo"]["name"], "da", "{answer}");
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// Closes standard input, proves nothing else was written, and waits.
    fn finish(mut self) -> ExitStatus {
        self.stdin = None;
        let mut rest = String::new();
        let _ = self.stdout.read_to_string(&mut rest);
        assert!(
            rest.trim().is_empty(),
            "the server wrote output nobody asked for: {rest:?}"
        );
        self.seen.push_str(&rest);
        let status = self.child.wait().expect("failed to wait for `da mcp`");
        let mut diagnostics = String::new();
        if let Some(mut stderr) = self.child.stderr.take() {
            let _ = stderr.read_to_string(&mut diagnostics);
        }
        assert!(
            status.success(),
            "`da mcp` exited with {status}; standard error: {diagnostics}"
        );
        status
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // A failed assertion must not leave a server process behind.
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The parsed JSON payload of one tool call.
fn payload(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("a tool result must carry text: {response}"));
    serde_json::from_str(text)
        .unwrap_or_else(|error| panic!("the tool text must be JSON ({error}): {text:?}"))
}

/// Whether one tool call reported a failure.
fn failed(response: &Value) -> bool {
    response["result"]["isError"] == json!(true)
}

/// Asserts no captured output carries the secret in any form `Secrets` knows.
#[cfg(feature = "cli")]
fn assert_no_leak(seen: &str) {
    for form in [
        SECRET.to_string(),
        support::percent_encode(SECRET.as_bytes()),
        support::base64_encode(SECRET.as_bytes()),
    ] {
        assert!(
            !seen.contains(&form),
            "the server wrote the secret as `{form}`"
        );
    }
    assert!(
        seen.contains("***"),
        "the redaction marker must show that a secret was replaced"
    );
}

/// The step definitions every session test sends, in order.
///
/// The first one has no `command`, so `cli.run` refuses it; the last one names
/// the secret as the program, so its failure message would carry the secret if
/// redaction were missing.
#[cfg(feature = "cli")]
fn flow_steps() -> Vec<Value> {
    vec![
        json!({"use": "cli.run", "with": {}}),
        json!({
            "use": "cli.run",
            "with": {"command": da(), "args": ["--version"]},
            "expect": [{"path": "$.exit_code", "equals": 0}],
        }),
        json!({
            "use": "cli.run",
            "with": {
                "command": da(),
                "args": ["--version"],
                "env": {"DA_MCP_PROBE": format!("${{env:{SECRET_VARIABLE}}}")},
            },
            "expect": [{"path": "$.exit_code", "equals": 0}],
        }),
        json!({"use": "cli.run", "with": {"command": format!("${{env:{SECRET_VARIABLE}}}")}}),
    ]
}

/// The `step` tool arguments that carry one step envelope and a session id.
#[cfg(feature = "cli")]
fn tool_arguments(session_id: &str, step: &Value) -> Value {
    let mut arguments = step.as_object().cloned().unwrap_or_default();
    arguments.insert("session_id".to_string(), json!(session_id));
    Value::Object(arguments)
}

/// Replays `steps` through `da session` and returns the session id.
#[cfg(feature = "cli")]
fn cli_session(dir: &Path, steps: &[Value]) -> String {
    let started = Command::new(da())
        .current_dir(dir)
        .arg("session")
        .arg("start")
        .env(SECRET_VARIABLE, SECRET)
        .output()
        .expect("failed to run `da session start`");
    assert!(
        started.status.success(),
        "`da session start` failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let started: Value = serde_json::from_slice(&started.stdout)
        .expect("`session start` must print one JSON object");
    let session_id = started["session_id"]
        .as_str()
        .expect("`session start` must print a session id")
        .to_string();

    for (index, step) in steps.iter().enumerate() {
        let path = dir.join(format!("step-{index}.json"));
        std::fs::write(&path, step.to_string()).expect("failed to write a step file");
        // The refused step exits non-zero; the session records it all the same.
        let _ = Command::new(da())
            .current_dir(dir)
            .arg("session")
            .arg("step")
            .arg(&session_id)
            .arg("--file")
            .arg(&path)
            .env(SECRET_VARIABLE, SECRET)
            .output()
            .expect("failed to run `da session step`");
    }
    session_id
}

#[test]
fn the_handshake_lists_the_six_tools_and_answers_ping() {
    let dir = TempDir::new("mcp-protocol");
    let mut server = Server::start(dir.path());

    let initialized = server.request(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "da-mcp-test", "version": "0"},
        },
    }));
    assert_eq!(initialized["jsonrpc"], "2.0");
    assert_eq!(initialized["id"], 1);
    assert_eq!(initialized["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(initialized["result"]["serverInfo"]["name"], "da");
    assert_eq!(
        initialized["result"]["capabilities"]["tools"]["listChanged"],
        false
    );
    assert!(
        initialized["result"]["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("session_start")),
        "{initialized}"
    );

    // A notification is never answered, so the next line is the ping's answer
    // even though the notification was written first.
    server.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let pong = server.request(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    assert_eq!(pong["id"], 2);
    assert_eq!(pong["result"], json!({}));

    let listed = server.request(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}));
    let tools = listed["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("`tools/list` must return a list: {listed}"));
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(
        names,
        [
            "session_start",
            "step",
            "snapshot",
            "export",
            "run",
            "session_stop"
        ]
    );
    for tool in tools {
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "every tool needs a description: {tool}"
        );
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
    }
    let step_tool = tools
        .iter()
        .find(|tool| tool["name"] == "step")
        .unwrap_or_else(|| panic!("`step` must be listed: {listed}"));
    assert_eq!(
        step_tool["inputSchema"]["required"],
        json!(["session_id", "use"])
    );

    let unknown_method =
        server.request(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/reset"}));
    assert_eq!(unknown_method["id"], 4);
    assert_eq!(unknown_method["error"]["code"], -32601);
    assert!(
        unknown_method["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("tools/reset")),
        "{unknown_method}"
    );

    let unknown_tool = server.request(json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {"name": "session_reset", "arguments": {}},
    }));
    assert_eq!(unknown_tool["error"]["code"], -32602);
    assert!(
        unknown_tool["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("session_reset")),
        "{unknown_tool}"
    );

    server.send_line("this is not a JSON-RPC message");
    let unparsable = server.receive();
    assert_eq!(unparsable["id"], Value::Null);
    assert_eq!(unparsable["error"]["code"], -32700);

    // An unknown protocol version is answered with the newest one this server
    // speaks, as the Model Context Protocol requires.
    let other_version = server.request(json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "initialize",
        "params": {"protocolVersion": "1999-01-01"},
    }));
    assert_eq!(other_version["result"]["protocolVersion"], "2025-06-18");

    server.finish();
}

#[test]
fn content_length_frames_are_answered_with_content_length_frames() {
    let dir = TempDir::new("mcp-framing");
    let mut server = Server::start(dir.path());

    let listed = server.request_framed(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {},
    }));
    assert_eq!(listed["id"], 1);
    assert_eq!(
        listed["result"]["tools"]
            .as_array()
            .map(|tools| tools.len()),
        Some(6)
    );

    // The next message can still be line framed: the stream is not switched.
    let pong = server.request(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
    assert_eq!(pong["id"], 2);

    server.finish();
}

#[test]
fn session_tools_answer_with_a_message_when_no_step_can_run() {
    let dir = TempDir::new("mcp-session-bare");
    let mut server = Server::start(dir.path());
    server.initialize();

    let started = payload(&server.call_tool(1, "session_start", json!({})));
    assert_eq!(started["ok"], true, "{started}");
    let session_id = started["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("`session_start` must return an id: {started}"))
        .to_string();
    assert!(
        dir.path()
            .join(".da")
            .join("sessions")
            .join(&session_id)
            .exists(),
        "the session directory must be created"
    );

    let snapshot = server.call_tool(2, "snapshot", json!({"session_id": session_id}));
    assert!(failed(&snapshot), "{snapshot}");
    let snapshot_payload = payload(&snapshot);
    assert!(
        snapshot_payload["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("snapshot-capable")),
        "{snapshot_payload}"
    );

    let export = server.call_tool(3, "export", json!({"session_id": session_id}));
    assert!(failed(&export), "{export}");
    assert!(
        payload(&export)["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("exportable")),
        "{export}"
    );

    let unknown_step = server.call_tool(
        4,
        "step",
        json!({"session_id": session_id, "use": "no.such.step"}),
    );
    assert!(failed(&unknown_step), "{unknown_step}");
    let unknown_step = payload(&unknown_step);
    assert!(
        unknown_step["available_steps"].is_array(),
        "an unknown step must list the steps there are: {unknown_step}"
    );
    assert!(
        unknown_step["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("available_steps")),
        "{unknown_step}"
    );

    let stopped = payload(&server.call_tool(5, "session_stop", json!({"session_id": session_id})));
    assert_eq!(stopped["ok"], true, "{stopped}");
    assert!(
        !dir.path()
            .join(".da")
            .join("sessions")
            .join(&session_id)
            .exists(),
        "`session_stop` must remove the session directory"
    );

    server.finish();
}

#[test]
fn the_run_tool_reports_a_path_it_cannot_read() {
    let dir = TempDir::new("mcp-run-missing");
    let mut server = Server::start(dir.path());

    let response = server.call_tool(1, "run", json!({"paths": ["definitely-absent.yaml"]}));
    assert!(failed(&response), "{response}");
    let report = payload(&response);
    assert_eq!(report["ok"], false, "{report}");
    assert!(
        report["errors"]
            .as_array()
            .is_some_and(|errors| !errors.is_empty()),
        "{report}"
    );
    assert!(
        report["errors"][0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("definitely-absent.yaml")),
        "{report}"
    );

    server.finish();
}

#[cfg(feature = "cli")]
#[test]
fn a_session_flow_exports_the_same_yaml_as_the_cli() {
    let mcp_dir = TempDir::new("mcp-flow");
    let cli_dir = TempDir::new("mcp-flow-cli");
    let steps = flow_steps();

    let mut server = Server::start(mcp_dir.path());
    server.initialize();
    let started = payload(&server.call_tool(1, "session_start", json!({})));
    let session_id = started["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("`session_start` must return an id: {started}"))
        .to_string();

    // The first step is refused by the step's own schema, and the answer has to
    // carry that schema so a model can correct itself.
    let refused = server.call_tool(2, "step", tool_arguments(&session_id, &steps[0]));
    assert!(failed(&refused), "{refused}");
    let refused = payload(&refused);
    assert_eq!(refused["error"]["kind"], "invalid_input", "{refused}");
    assert_eq!(
        refused["arguments_schema"]["properties"]["command"]["type"], "string",
        "{refused}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("command")),
        "{refused}"
    );
    assert!(
        refused["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("arguments_schema")),
        "{refused}"
    );

    // The next two steps pass; the last one names the secret as the program, so
    // its message is where an unredacted value would surface.
    let mut secret_failure = String::new();
    for (index, step) in steps.iter().enumerate().skip(1) {
        let response =
            server.call_tool(3 + index as u64, "step", tool_arguments(&session_id, step));
        let step_payload = payload(&response);
        if index + 1 == steps.len() {
            assert!(failed(&response), "{step_payload}");
            secret_failure = step_payload["step"]["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .to_string();
        } else {
            assert_eq!(
                step_payload["step"]["status"], "passed",
                "step {index} must pass: {step_payload}"
            );
        }
    }

    // Nothing written so far may carry the secret in any form, and the marker
    // proves a secret was there to replace instead of never being present.
    assert_no_leak(&server.seen);
    assert!(
        secret_failure.contains("***"),
        "the failure message must be redacted: {secret_failure}"
    );

    let snapshot = server.call_tool(10, "snapshot", json!({"session_id": session_id}));
    assert!(failed(&snapshot), "{snapshot}");

    let exported = payload(&server.call_tool(
        11,
        "export",
        json!({"session_id": session_id, "name": "MCP session"}),
    ));
    assert_eq!(exported["ok"], true, "{exported}");
    let yaml = exported["yaml"]
        .as_str()
        .unwrap_or_else(|| panic!("the export must carry YAML: {exported}"))
        .to_string();
    assert!(
        yaml.contains("${env:DA_MCP_TEST_SECRET}"),
        "the exported case must keep the reference: {yaml}"
    );
    assert_eq!(exported["exported_steps"], 2, "{exported}");

    let stopped = payload(&server.call_tool(12, "session_stop", json!({"session_id": session_id})));
    assert_eq!(stopped["ok"], true, "{stopped}");

    // The same steps through `da session` must produce the same case file.
    let cli_id = cli_session(cli_dir.path(), &steps);
    let cli_export = Command::new(da())
        .current_dir(cli_dir.path())
        .arg("session")
        .arg("export")
        .arg(&cli_id)
        .arg("--name")
        .arg("MCP session")
        .arg("--yaml")
        .env(SECRET_VARIABLE, SECRET)
        .output()
        .expect("failed to run `da session export`");
    assert!(
        cli_export.status.success(),
        "`da session export` failed: {}",
        String::from_utf8_lossy(&cli_export.stderr)
    );
    let cli_yaml = String::from_utf8(cli_export.stdout).expect("the exported case is UTF-8");
    assert_eq!(
        yaml, cli_yaml,
        "the session export must equal the one `da session export` writes"
    );
    let _ = Command::new(da())
        .current_dir(cli_dir.path())
        .arg("session")
        .arg("stop")
        .arg(&cli_id)
        .env(SECRET_VARIABLE, SECRET)
        .output();

    assert_no_leak(&server.seen);
    server.finish();
}

#[cfg(feature = "cli")]
#[test]
fn the_run_tool_reports_the_case_it_ran() {
    let dir = TempDir::new("mcp-run");
    let case = json!({
        "name": "Run through MCP",
        "steps": [{
            "cli.run": {"command": da(), "args": ["--version"]},
            "expect": [{"path": "$.exit_code", "equals": 0}],
        }],
    });
    let case_path = dir.write("case.yaml", &case.to_string());

    let mut server = Server::start(dir.path());
    server.initialize();
    let response = server.call_tool(1, "run", json!({"paths": [case_path.to_string_lossy()]}));
    assert!(!failed(&response), "{response}");
    let report = payload(&response);
    assert_eq!(report["ok"], true, "{report}");
    assert_eq!(report["cases"][0]["name"], "Run through MCP", "{report}");
    assert_eq!(report["cases"][0]["status"], "passed", "{report}");
    assert_eq!(
        report["cases"][0]["steps"][0]["status"], "passed",
        "{report}"
    );
    assert_eq!(report["summary"]["cases"], 1, "{report}");
    assert_eq!(report["summary"]["steps"], 1, "{report}");

    server.finish();
}

#[cfg(feature = "browser")]
#[test]
fn a_browser_step_and_a_snapshot_round_trip() {
    let page = support::TestServer::start();
    let dir = TempDir::new("mcp-browser");
    let mut server = Server::start(dir.path());
    server.initialize();
    let started = payload(&server.call_tool(1, "session_start", json!({})));
    let session_id = started["session_id"]
        .as_str()
        .unwrap_or_else(|| panic!("`session_start` must return an id: {started}"))
        .to_string();

    let opened = server.call_tool(
        2,
        "step",
        json!({
            "session_id": session_id,
            "use": "browser.open",
            "with": {"url": page.url("/browser/form?case=mcp")},
        }),
    );
    let opened_payload = payload(&opened);
    if opened_payload["step"]["status"] != "passed" {
        let message = opened_payload["step"]["error"]["message"]
            .as_str()
            .unwrap_or("the browser step failed without a message");
        if std::env::var("DA_TEST_BROWSER").as_deref() == Ok("1") {
            panic!("DA_TEST_BROWSER=1 requires a working Chrome: {message}");
        }
        note(&format!(
            "skipping the MCP browser test: {message}; set DA_TEST_BROWSER=1 to fail instead of skipping"
        ));
        return;
    }

    let snapshot = server.call_tool(3, "snapshot", json!({"session_id": session_id}));
    assert!(!failed(&snapshot), "{snapshot}");
    let snapshot = payload(&snapshot);
    assert_eq!(snapshot["ok"], true, "{snapshot}");
    assert!(
        snapshot["snapshot"]["tree"].is_array(),
        "the snapshot must carry an accessibility tree: {snapshot}"
    );
    assert!(
        snapshot["snapshot"]["title"]
            .as_str()
            .is_some_and(|title| title.contains("Browser form")),
        "{snapshot}"
    );

    let stopped = payload(&server.call_tool(4, "session_stop", json!({"session_id": session_id})));
    assert_eq!(stopped["ok"], true, "{stopped}");
    server.finish();
}

#[cfg(feature = "cli")]
const MCP_MOCK_HOOK_RS: &str = r#"
use std::env;
use std::fs;
use std::io::{self, Read, Write};

fn main() {
    let secret = env::var("DA_MCP_TEST_SECRET").unwrap_or_default();
    let mut stdin_bytes = Vec::new();
    let _ = io::stdin().read_to_end(&mut stdin_bytes);
    if let Some(out_path) = env::args().nth(1) {
        let _ = fs::write(out_path, &stdin_bytes);
    }
    let stdin_text = String::from_utf8_lossy(&stdin_bytes);
    let passed = !stdin_text.contains("\"fail-now\"");
    let line = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"passed\":{passed},\"rationale\":\"checked {secret}\",\"confidence\":0.9,\"model\":\"mcp-judge-v1\",\"latency_ms\":23,\"usage\":{{\"prompt_tokens\":10}},\"trace_id\":\"tr-{secret}\"}}}}"
    );
    let _ = writeln!(io::stdout(), "{line}");
    let _ = io::stdout().flush();
}
"#;

#[cfg(feature = "cli")]
#[test]
fn mcp_with_model_hook_exposes_model_tools_redacts_secrets_and_omits_from_export() {
    let dir = TempDir::new("mcp-model-hook");
    let hook_src = dir.write("mcp_mock_hook.rs", MCP_MOCK_HOOK_RS);
    let hook_bin = dir.path().join(if cfg!(windows) {
        "mcp_mock_hook.exe"
    } else {
        "mcp_mock_hook"
    });
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let status = Command::new(rustc)
        .arg(&hook_src)
        .arg("-O")
        .arg("-o")
        .arg(&hook_bin)
        .status()
        .expect("compile mcp_mock_hook.rs");
    assert!(status.success(), "rustc failed for mcp_mock_hook.rs");

    let req_dump = dir.path().join("mcp_hook_stdin.json");
    let mut child = Command::new(da())
        .arg("mcp")
        .arg("--model-hook")
        .arg(&hook_bin)
        .arg("--model-hook-arg")
        .arg(&req_dump)
        .current_dir(dir.path())
        .env(SECRET_VARIABLE, SECRET)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start `da mcp --model-hook`");
    let stdin = child.stdin.take().expect("mcp stdin");
    let stdout = BufReader::new(child.stdout.take().expect("mcp stdout"));
    let mut server = Server {
        child,
        stdin: Some(stdin),
        stdout,
        seen: String::new(),
    };
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
            "export",
            "run",
            "session_stop"
        ]
    );

    let started = payload(&server.call_tool(2, "session_start", json!({})));
    let session_id = started["session_id"].as_str().unwrap().to_string();

    let step_resp = server.call_tool(
        3,
        "step",
        json!({
            "session_id": session_id,
            "use": "cli.run",
            "with": {
                "command": da(),
                "args": ["--version"],
                "env": {"PROBE": format!("${{env:{SECRET_VARIABLE}}}")}
            },
            "expect": [{"path": "$.exit_code", "equals": 0}]
        }),
    );
    assert!(!failed(&step_resp), "{step_resp}");

    let judged = server.call_tool(
        4,
        "judge",
        json!({
            "session_id": session_id,
            "rubric": format!("Verify ${{env:{SECRET_VARIABLE}}} run succeeded")
        }),
    );
    assert!(!failed(&judged), "{judged}");
    let judged_payload = payload(&judged);
    assert_eq!(judged_payload["passed"], true);
    assert_eq!(judged_payload["model"], "mcp-judge-v1");
    assert_eq!(judged_payload["rationale"], "checked ***");

    let hook_stdin = std::fs::read_to_string(&req_dump).expect("hook stdin");
    assert!(
        !hook_stdin.contains(SECRET),
        "MCP judge leaked secret into model hook stdin: {hook_stdin}"
    );

    let judged_fail = server.call_tool(
        5,
        "judge",
        json!({
            "session_id": session_id,
            "rubric": "fail-now"
        }),
    );
    assert!(failed(&judged_fail), "{judged_fail}");
    assert_eq!(payload(&judged_fail)["passed"], false);

    let exported = payload(&server.call_tool(6, "export", json!({"session_id": session_id})));
    assert_eq!(exported["exported_steps"], 1, "{exported}");
    let yaml = exported["yaml"].as_str().unwrap();
    assert!(!yaml.contains("model.judge"), "{yaml}");

    let stopped = payload(&server.call_tool(7, "session_stop", json!({"session_id": session_id})));
    assert_eq!(stopped["ok"], true, "{stopped}");

    assert_no_leak(&server.seen);
    server.finish();
}

#[cfg(all(feature = "cli", feature = "ai"))]
mod ai_mcp_tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    struct McpModelMock {
        port: u16,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl McpModelMock {
        fn start(reply_content: Value) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mcp mock model");
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();

            let requests = Arc::new(Mutex::new(Vec::new()));
            let body = json!({
                "id": "chatcmpl-mcp-1",
                "object": "chat.completion",
                "created": 1700000000,
                "model": "mcp-builtin-v1",
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": reply_content.to_string() },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
            })
            .to_string();
            let stop = Arc::new(AtomicBool::new(false));

            let reqs_c = Arc::clone(&requests);
            let stop_c = Arc::clone(&stop);
            let handle = thread::spawn(move || {
                while !stop_c.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let reqs = Arc::clone(&reqs_c);
                            let rep = body.clone();
                            thread::spawn(move || serve_once(stream, &reqs, &rep));
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

    impl Drop for McpModelMock {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    fn serve_once(stream: TcpStream, requests: &Mutex<Vec<String>>, body: &str) {
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
        let mut auth = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap_or(0);
                } else if k.trim().eq_ignore_ascii_case("authorization") {
                    auth = v.trim().to_string();
                }
            }
        }
        let mut body_buf = vec![0u8; len];
        if len > 0 {
            let _ = std::io::Read::read_exact(&mut reader, &mut body_buf);
        }
        let raw_req = format!(
            "{req_line}AUTH={auth}\n{}",
            String::from_utf8_lossy(&body_buf)
        );
        requests.lock().unwrap().push(raw_req);

        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = writer.write_all(resp.as_bytes());
        let _ = writer.flush();
    }

    #[test]
    fn mcp_with_builtin_model_exposes_tools_runs_judge_and_redacts_secrets() {
        let dir = TempDir::new("mcp-builtin-model");
        let api_key_secret = "mcp-model-api-key-918273";
        let mock = McpModelMock::start(json!({
            "passed": true,
            "rationale": format!("verified {SECRET} and {api_key_secret} cleanly"),
            "confidence": 0.91
        }));

        let mut child = Command::new(da())
            .arg("mcp")
            .arg("--model")
            .arg("openai::gpt-4o-mini")
            .arg("--model-endpoint")
            .arg(mock.endpoint())
            .arg("--model-api-key-env")
            .arg("DA_MCP_TEST_KEY")
            .current_dir(dir.path())
            .env(SECRET_VARIABLE, SECRET)
            .env("DA_MCP_TEST_KEY", api_key_secret)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start `da mcp --model`");
        let stdin = child.stdin.take().expect("mcp stdin");
        let stdout = BufReader::new(child.stdout.take().expect("mcp stdout"));
        let mut server = Server {
            child,
            stdin: Some(stdin),
            stdout,
            seen: String::new(),
        };
        server.initialize();

        let listed = server.request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
        let tools = listed["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            names,
            [
                "session_start",
                "step",
                "snapshot",
                "select_ref",
                "locate",
                "judge",
                "export",
                "run",
                "session_stop"
            ]
        );
        let judge_schema = tools
            .iter()
            .find(|t| t["name"] == "judge")
            .expect("judge tool schema");
        assert!(judge_schema["inputSchema"]["properties"]["screenshot"].is_object());
        assert!(judge_schema["inputSchema"]["properties"]["model"].is_object());

        let started = payload(&server.call_tool(2, "session_start", json!({})));
        let session_id = started["session_id"].as_str().unwrap().to_string();

        let step_resp = server.call_tool(
            3,
            "step",
            json!({
                "session_id": session_id,
                "use": "cli.run",
                "with": {
                    "command": da(),
                    "args": ["--version"],
                    "env": {"PROBE": format!("${{env:{SECRET_VARIABLE}}}")}
                },
                "expect": [{"path": "$.exit_code", "equals": 0}]
            }),
        );
        assert!(!failed(&step_resp), "{step_resp}");

        let judged = server.call_tool(
            4,
            "judge",
            json!({
                "session_id": session_id,
                "rubric": format!("Verify ${{env:{SECRET_VARIABLE}}} succeeded")
            }),
        );
        assert!(!failed(&judged), "{judged}");
        let judged_payload = payload(&judged);
        assert_eq!(judged_payload["passed"], true);
        assert_eq!(judged_payload["model"], "mcp-builtin-v1");
        assert_eq!(judged_payload["rationale"], "verified *** and *** cleanly");
        assert!(!server.seen.contains(api_key_secret), "{}", server.seen);

        let recorded = mock.requests.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].contains(&format!("AUTH=Bearer {api_key_secret}")));
        assert!(!recorded[0].contains(SECRET), "{}", recorded[0]);

        // `screenshot: true` without an active visual session returns `environment` error.
        let shot_err = server.call_tool(
            5,
            "judge",
            json!({
                "session_id": session_id,
                "rubric": "Check screenshot",
                "screenshot": true
            }),
        );
        assert!(failed(&shot_err), "{shot_err}");
        assert_eq!(payload(&shot_err)["error"]["kind"], "environment");

        let _ = server.call_tool(6, "session_stop", json!({"session_id": session_id}));
        assert_no_leak(&server.seen);
        server.finish();
    }

    #[test]
    fn mcp_with_da_model_env_exposes_model_tools_and_runs_judge() {
        let dir = TempDir::new("mcp-da-model-env");
        let mock = McpModelMock::start(json!({
            "passed": true,
            "rationale": "Env-configured model verified session",
            "confidence": 0.88
        }));

        let mut child = Command::new(da())
            .arg("mcp")
            .current_dir(dir.path())
            .env("DA_MODEL", "qwen-local")
            .env("DA_MODEL_ADAPTER", "omlx")
            .env("DA_MODEL_ENDPOINT", mock.endpoint())
            .env_remove("DA_MODEL_API_KEY_ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start `da mcp` with DA_MODEL");
        let stdin = child.stdin.take().expect("mcp stdin");
        let stdout = BufReader::new(child.stdout.take().expect("mcp stdout"));
        let mut server = Server {
            child,
            stdin: Some(stdin),
            stdout,
            seen: String::new(),
        };
        server.initialize();

        let listed = server.request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names.len(), 9, "{names:?}");
        assert!(names.contains(&"judge"));

        let started = payload(&server.call_tool(2, "session_start", json!({})));
        let session_id = started["session_id"].as_str().unwrap().to_string();

        let judged = server.call_tool(
            3,
            "judge",
            json!({
                "session_id": session_id,
                "rubric": "Verify env fallback",
                "evidence": {"status": "ready"}
            }),
        );
        assert!(!failed(&judged), "{judged}");
        let judged_payload = payload(&judged);
        assert_eq!(judged_payload["passed"], true);
        assert_eq!(judged_payload["model"], "mcp-builtin-v1");

        let _ = server.call_tool(4, "session_stop", json!({"session_id": session_id}));
        server.finish();
    }
}
