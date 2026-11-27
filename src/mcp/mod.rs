//! `da mcp`: the session protocol as a Model Context Protocol server on stdio.
//!
//! The server is synchronous and has no runtime to pump: standard input is read
//! one JSON-RPC 2.0 message at a time, the request is answered by the same
//! transport-neutral [`crate::session`] handler the command line uses, and the
//! response is written back and flushed before the next message is read. Two
//! framings are accepted on input, one JSON object per line and the
//! `Content-Length` framing of the Language Server Protocol, and every response
//! mirrors the framing of the request it answers.
//!
//! The base tool set is fixed at eight, so its shape never depends on which
//! driver features a build enabled: `session_start`, `step`, `snapshot`,
//! `pause_for_human`, `resume`, `export`, `run` and `session_stop`. When model
//! access is configured on `da mcp` (`--model`, `--model-hook`, or `DA_MODEL`),
//! `tools/list` also exposes `select_ref`, `locate`, `judge`, `extract`, and
//! `act` (thirteen tools total). `step` is the single generic step tool and
//! takes the same `use`/`with`/`expect`/`save` step a case file holds. Every
//! `tools/call` response emits standard MCP `structuredContent` alongside
//! backward-compatible text `content`.
//!
//! Standard output carries responses and nothing else, so a host can parse it
//! without stripping diagnostics. Everything written back passes through
//! [`Secrets`] first: the session handler already redacts the values it read
//! from the environment, and the boundary redaction here is the second lock on
//! the same door.

pub mod tools;

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::core::case;
use crate::core::registry::Registry;
use crate::core::secrets::Secrets;
use crate::exit::ExitCode;
use crate::session;
use crate::session::model_hook::{BuiltinModelSpec, ModelHookSpec};

use self::tools::{Context, ToolOutcome};

/// How `da mcp` was started.
#[derive(Debug, Clone)]
pub struct Options {
    /// Directory that stores session state, like `da session --sessions-dir`.
    pub sessions_dir: PathBuf,
    /// Idle timeout `session_start` applies, in milliseconds.
    pub idle_timeout_ms: u64,
    /// Optional external model hook configured for sessions started by this server.
    pub model_hook: Option<ModelHookSpec>,
    /// Optional built-in model access configuration for sessions started by this server.
    pub model: Option<BuiltinModelSpec>,
    /// Absolute paths of the explicit plugins this server was started with.
    pub plugins: Vec<String>,
}

/// JSON-RPC error code for a body that is not JSON.
const PARSE_ERROR: i64 = -32700;
/// JSON-RPC error code for JSON that is not a JSON-RPC request.
const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC error code for a method this server does not implement.
const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code for a `tools/call` this server cannot route.
const INVALID_PARAMS: i64 = -32602;

/// The Model Context Protocol revisions this server can answer.
const SUPPORTED_PROTOCOLS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

/// The revision reported when the client asks for one this server does not know.
const LATEST_PROTOCOL: &str = "2025-06-18";

/// The largest frame body this server accepts.
///
/// A `Content-Length` header decides how much memory the next read allocates, so
/// a malformed or hostile header must not be able to ask for an unbounded one.
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// What the client is told about driving `da` before it calls the first tool.
const INSTRUCTIONS: &str = "\
`da` is a declarative test runner for HTTP APIs, command-line programs, SQL databases and browsers.\n\
Drive one interactive session: call `session_start` once (pass `preview_frames: true` to atomically write input-masked `preview.png` and `preview.json` after UI steps without adding images to LLM `content`), then `step` for every action \
(`use` names the step, `with` holds its arguments, `expect` and `save` work as in a case file), \
`snapshot` to inspect a browser without running a step, `pause_for_human` / `resume` to gate agent steps during human takeover, `export` to turn the passed steps into a \
replayable YAML case, and `session_stop` when the session is done. `run` executes existing case \
files or directories.\n\
An invalid `step` returns the step's JSON Schema; fix `with` and call `step` again with the same \
`session_id`. Every `tools/call` result populates standard MCP `structuredContent` alongside `content` text, and every value read through \
`${env:NAME}` is redacted before leaving the server.";

/// Runs the server until standard input reaches end of file.
///
/// The process exit code is [`ExitCode::Success`] for a clean end of input and
/// [`ExitCode::Environment`] when standard input or output failed.
pub fn serve(registry: &Registry, options: Options) -> ExitCode {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut writer = io::stdout().lock();
    let mut server = Server {
        registry,
        options,
        secrets: Secrets::new(),
    };
    server.observe_startup_options();

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return ExitCode::Success,
            Err(error) => {
                report(&format!("da mcp: could not read standard input: {error}\n"));
                return ExitCode::Environment;
            }
        };
        let response = match frame.body {
            Ok(text) => server.handle(&text),
            Err(message) => Some(error_response(Value::Null, PARSE_ERROR, message)),
        };
        if let Some(response) = response
            && let Err(error) = write_frame(&mut writer, frame.framing, &response)
        {
            report(&format!(
                "da mcp: could not write to standard output: {error}\n"
            ));
            return ExitCode::Environment;
        }
    }
}

/// Writes one diagnostic line to standard error.
///
/// `eprintln!` panics when standard error cannot be written; a closed pipe must
/// not replace the exit code the server already decided on.
fn report(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_all(text.as_bytes());
    let _ = stderr.flush();
}

/// Who framed one message, so the response can use the same framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// One JSON object per line.
    Lines,
    /// `Content-Length: <bytes>` headers, an empty line, then the body.
    ContentLength,
}

/// One message read from standard input.
struct Frame {
    framing: Framing,
    /// The JSON text, or the reason the message could not be read as JSON.
    body: Result<String, String>,
}

/// Reads the next message, skipping empty lines.
///
/// The first non-empty line decides the framing: a line that starts a JSON
/// document is a whole message, anything else is the first header of a framed
/// message. `None` means the input reached its end.
fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Frame>> {
    loop {
        let mut bytes = Vec::new();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            return Ok(None);
        }
        let Ok(line) = String::from_utf8(bytes) else {
            return Ok(Some(Frame {
                framing: Framing::Lines,
                body: Err("the message is not valid UTF-8".to_string()),
            }));
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            return Ok(Some(Frame {
                framing: Framing::Lines,
                body: Ok(trimmed.to_string()),
            }));
        }
        // Only a header this server knows starts a framed message. Everything
        // else is a malformed line-delimited message, and answering it in the
        // line framing keeps a client that never used `Content-Length` in sync.
        let lowered = trimmed.to_ascii_lowercase();
        if !(lowered.starts_with("content-length:") || lowered.starts_with("content-type:")) {
            return Ok(Some(Frame {
                framing: Framing::Lines,
                body: Err(format!(
                    "a message must be one JSON object per line, or be framed with `Content-Length`; got `{trimmed}`"
                )),
            }));
        }
        return read_framed_body(reader, trimmed);
    }
}

/// Reads the headers and then the body of one `Content-Length` frame.
fn read_framed_body(reader: &mut impl BufRead, first_header: &str) -> io::Result<Option<Frame>> {
    let mut length: Option<usize> = None;
    let mut header = first_header.to_string();
    loop {
        let Some((name, value)) = header.split_once(':') else {
            return Ok(Some(Frame {
                framing: Framing::ContentLength,
                body: Err(format!(
                    "`{header}` is not a `Name: value` header; send one JSON-RPC message per line or frame it with `Content-Length`"
                )),
            }));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            match value.trim().parse::<usize>() {
                Ok(parsed) => length = Some(parsed),
                Err(_) => {
                    return Ok(Some(Frame {
                        framing: Framing::ContentLength,
                        body: Err(format!(
                            "`Content-Length` must be a number of bytes, got `{}`",
                            value.trim()
                        )),
                    }));
                }
            }
        }
        let mut bytes = Vec::new();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            return Ok(None);
        }
        let Ok(line) = String::from_utf8(bytes) else {
            return Ok(Some(Frame {
                framing: Framing::ContentLength,
                body: Err("the frame headers are not valid UTF-8".to_string()),
            }));
        };
        if line.trim().is_empty() {
            break;
        }
        header = line.trim().to_string();
    }

    let Some(length) = length else {
        return Ok(Some(Frame {
            framing: Framing::ContentLength,
            body: Err("a framed message needs a `Content-Length` header".to_string()),
        }));
    };
    if length > MAX_FRAME_BYTES {
        return Ok(Some(Frame {
            framing: Framing::ContentLength,
            body: Err(format!(
                "a frame of {length} bytes exceeds the {MAX_FRAME_BYTES}-byte limit"
            )),
        }));
    }
    let mut body = vec![0_u8; length];
    match reader.read_exact(&mut body) {
        Ok(()) => {}
        // A truncated frame is an input that ended in the middle of a message.
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    match String::from_utf8(body) {
        Ok(text) => Ok(Some(Frame {
            framing: Framing::ContentLength,
            body: Ok(text),
        })),
        Err(_) => Ok(Some(Frame {
            framing: Framing::ContentLength,
            body: Err("the frame body is not valid UTF-8".to_string()),
        })),
    }
}

/// Writes one response with the framing of the request it answers, then flushes.
fn write_frame(writer: &mut impl Write, framing: Framing, response: &Value) -> io::Result<()> {
    let body = serde_json::to_string(response)
        .map_err(|error| io::Error::other(format!("could not serialize a response: {error}")))?;
    match framing {
        Framing::Lines => {
            writer.write_all(body.as_bytes())?;
            writer.write_all(b"\n")?;
        }
        Framing::ContentLength => {
            write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
            writer.write_all(body.as_bytes())?;
        }
    }
    writer.flush()
}

/// The protocol state of one running server.
struct Server<'a> {
    registry: &'a Registry,
    options: Options,
    /// Every value read from the environment that a response may contain.
    secrets: Secrets,
}

impl Server<'_> {
    /// Answers one JSON text, or returns `None` for a message that needs no reply.
    fn handle(&mut self, text: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(error) => {
                return Some(error_response(
                    Value::Null,
                    PARSE_ERROR,
                    format!("could not parse the JSON-RPC message: {error}"),
                ));
            }
        };
        self.dispatch(message)
    }

    /// Routes one JSON-RPC message by method.
    fn dispatch(&mut self, message: Value) -> Option<Value> {
        let Value::Object(mut fields) = message else {
            return Some(error_response(
                Value::Null,
                INVALID_REQUEST,
                "a JSON-RPC message must be an object; batch requests are not supported",
            ));
        };
        let id = fields.remove("id");
        // A message without `id` is a notification, and JSON-RPC forbids
        // answering one, including with an error.
        let notification = id.is_none();
        let id = id.unwrap_or(Value::Null);
        let method = fields
            .remove("method")
            .and_then(|value| value.as_str().map(str::to_string));
        let params = fields.remove("params").unwrap_or(Value::Null);
        let Some(method) = method else {
            if notification {
                return None;
            }
            return Some(error_response(
                id,
                INVALID_REQUEST,
                "a JSON-RPC request needs a `method` string",
            ));
        };

        match method.as_str() {
            "initialize" => Some(success(id, initialize(&params))),
            "ping" => Some(success(id, json!({}))),
            "tools/list" => Some(success(
                id,
                json!({"tools": tools::definitions_for(self.has_model_access())}),
            )),
            "tools/call" => Some(self.call_tool(id, &params)),
            "notifications/initialized" => {
                if notification {
                    None
                } else {
                    Some(success(id, json!({})))
                }
            }
            other => {
                if notification {
                    return None;
                }
                Some(self.error(
                    id,
                    METHOD_NOT_FOUND,
                    format!(
                        "unknown method `{other}`; this server implements `initialize`, `ping`, `tools/list` and `tools/call`"
                    ),
                ))
            }
        }
    }

    /// Runs one `tools/call` request.
    fn call_tool(&mut self, id: Value, params: &Value) -> Value {
        let Some(params) = params.as_object() else {
            return self.error(
                id,
                INVALID_PARAMS,
                "`tools/call` needs an object with `name` and `arguments`",
            );
        };
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return self.error(id, INVALID_PARAMS, "`tools/call` needs a `name` string");
        };
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => {
                return self.error(id, INVALID_PARAMS, "`arguments` must be an object");
            }
        };

        self.observe_environment(&Value::Object(arguments.clone()));
        // The context borrows the server, and the tool result below needs it
        // mutably; one block keeps the two borrows from overlapping.
        let outcome = {
            let context = Context {
                registry: self.registry,
                sessions_dir: self.options.sessions_dir.as_path(),
                idle_timeout_ms: self.options.idle_timeout_ms,
                model_hook: self.options.model_hook.as_ref(),
                model: self.options.model.as_ref(),
                plugins: &self.options.plugins,
            };
            tools::call(&context, name, &arguments)
        };

        let Some(outcome) = outcome else {
            let exposed = if self.has_model_access() {
                tools::ALL_NAMES.join(", ")
            } else {
                tools::NAMES.join(", ")
            };
            return self.error(
                id,
                INVALID_PARAMS,
                format!("unknown tool `{name}`; this server exposes {exposed}"),
            );
        };
        success(id, self.tool_result(outcome))
    }

    /// Turns one tool outcome into an MCP `CallToolResult`.
    ///
    /// The payload is redacted as a JSON tree, not as text: a secret that
    /// contains `"` or `\` is escaped in the serialized form, so a textual
    /// replacement would miss it. Both backward-compatible `content` text and
    /// standard MCP `structuredContent` are populated from the redacted tree.
    fn tool_result(&mut self, outcome: ToolOutcome) -> Value {
        let payload = self.secrets.redact_json(&outcome.payload);
        let structured = build_structured_content(&payload);
        json!({
            "content": [{"type": "text", "text": payload.to_string()}],
            "structuredContent": structured,
            "isError": outcome.is_error,
        })
    }

    fn has_model_access(&self) -> bool {
        self.options.model_hook.is_some()
            || self.options.model.is_some()
            || (cfg!(feature = "ai") && session::builtin_model_from_env().is_some())
    }

    fn record_model_spec_secrets(&mut self, spec: &BuiltinModelSpec) {
        let mut names = BTreeSet::new();
        session::collect_builtin_model_env_names(spec, &mut names);
        for name in names {
            if let Ok(value) = std::env::var(&name) {
                self.secrets.record(&value);
            }
        }
    }

    fn observe_startup_options(&mut self) {
        if let Some(hook) = self.options.model_hook.clone() {
            self.observe_environment(&json!([hook.command, hook.args]));
        }
        if let Some(model) = self.options.model.clone() {
            self.record_model_spec_secrets(&model);
        } else if cfg!(feature = "ai")
            && let Some(env_model) = session::builtin_model_from_env()
        {
            self.record_model_spec_secrets(&env_model);
        }
    }

    /// Records the values of every `${env:NAME}` a request mentions.
    ///
    /// Session state already knows the names its steps used, so this is a
    /// second source for the same set: a tool call that mentions a secret is
    /// answered with that secret redacted even when the session was started by
    /// an older build or by another process.
    fn observe_environment(&mut self, arguments: &Value) {
        for name in case::env_references(arguments) {
            if let Ok(value) = std::env::var(&name) {
                self.secrets.record(&value);
            }
        }
        for key in ["api_key_env", "model_api_key_env"] {
            if let Some(name) = arguments
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                && let Ok(value) = std::env::var(name)
            {
                self.secrets.record(&value);
            }
        }
        if let Some(model) = arguments
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let opt = |keys: [&str; 2]| {
                keys.into_iter().find_map(|k| {
                    arguments
                        .get(k)
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                })
            };
            let spec = BuiltinModelSpec {
                model: model.to_string(),
                adapter: opt(["adapter", "model_adapter"]),
                endpoint: opt(["endpoint", "model_endpoint"]),
                api_key_env: opt(["api_key_env", "model_api_key_env"]),
                reasoning_effort: opt(["reasoning_effort", "model_reasoning_effort"]),
            };
            self.record_model_spec_secrets(&spec);
        }
    }

    /// Builds an error response with its message redacted.
    fn error(&self, id: Value, code: i64, message: impl Into<String>) -> Value {
        error_response(id, code, self.secrets.redact(&message.into()))
    }
}

/// The `initialize` result: the revision, the capabilities and the server name.
fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|version| SUPPORTED_PROTOCOLS.contains(version))
        .unwrap_or(LATEST_PROTOCOL);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "da", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

/// One JSON-RPC success response.
fn success(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// One JSON-RPC error response.
fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    let message = message.into();
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Builds the standard MCP `structuredContent` object from a redacted tool payload.
///
/// Preserves every redacted tool field while promoting canonical host-UI fields
/// (`url`, `title`, `target_bounds`, `step_result`, `report_path`) to the top
/// level when available so desktop/IDE hosts never have to parse `content[0].text`.
fn build_structured_content(payload: &Value) -> Value {
    let mut obj = match payload {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = Map::new();
            map.insert("value".to_string(), other.clone());
            map
        }
    };
    if !obj.contains_key("url")
        && let Some(url) = payload
            .pointer("/snapshot/url")
            .or_else(|| payload.pointer("/step/result/url"))
            .and_then(Value::as_str)
    {
        obj.insert("url".to_string(), Value::String(url.to_string()));
    }
    if !obj.contains_key("title")
        && let Some(title) = payload
            .pointer("/snapshot/title")
            .or_else(|| payload.pointer("/step/result/title"))
            .and_then(Value::as_str)
    {
        obj.insert("title".to_string(), Value::String(title.to_string()));
    }
    if !obj.contains_key("target_bounds")
        && let Some(bounds) = payload
            .pointer("/step/target_bounds")
            .or_else(|| payload.pointer("/step/result/target_bounds"))
            .filter(|v| !v.is_null())
            .cloned()
    {
        obj.insert("target_bounds".to_string(), bounds);
    }
    if !obj.contains_key("step_result")
        && let Some(result) = payload
            .pointer("/step/result")
            .filter(|v| !v.is_null())
            .cloned()
    {
        obj.insert("step_result".to_string(), result);
    }
    if !obj.contains_key("report_path")
        && let Some(path) = payload.pointer("/report/path").and_then(Value::as_str)
    {
        obj.insert("report_path".to_string(), Value::String(path.to_string()));
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// Collects every message a reader produces from `input`.
    fn frames(input: &str) -> Vec<Frame> {
        let mut reader = BufReader::new(input.as_bytes());
        let mut frames = Vec::new();
        while let Some(frame) = read_frame(&mut reader).expect("the fixture must be readable") {
            frames.push(frame);
        }
        frames
    }

    #[test]
    fn line_delimited_messages_keep_line_framing() {
        let read = frames("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n");

        assert_eq!(read.len(), 1, "an empty line is not a message");
        assert_eq!(read[0].framing, Framing::Lines);
        assert_eq!(
            read[0].body.as_deref(),
            Ok("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}")
        );
    }

    #[test]
    fn content_length_frames_are_read_with_their_headers() {
        let body = "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}";
        let input = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}\n",
            body.len()
        );

        let read = frames(&input);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].framing, Framing::ContentLength);
        assert_eq!(read[0].body.as_deref(), Ok(body));
    }

    #[test]
    fn a_malformed_length_header_is_reported_instead_of_read_as_json() {
        let read = frames("Content-Length: twelve\r\n\r\n");

        assert_eq!(read.len(), 1);
        assert_eq!(read[0].framing, Framing::ContentLength);
        assert!(
            read[0]
                .body
                .as_ref()
                .is_err_and(|message| message.contains("number of bytes")),
            "{:?}",
            read[0].body
        );
    }

    #[test]
    fn an_oversized_frame_is_refused_before_its_body_is_allocated() {
        let input = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);

        let read = frames(&input);
        assert_eq!(read.len(), 1);
        assert!(
            read[0]
                .body
                .as_ref()
                .is_err_and(|message| message.contains("exceeds")),
            "{:?}",
            read[0].body
        );
    }

    #[test]
    fn a_response_is_framed_the_way_its_request_was() {
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": {}});

        let mut lines = Vec::new();
        write_frame(&mut lines, Framing::Lines, &response).expect("writing must succeed");
        assert_eq!(
            String::from_utf8(lines).expect("a response is UTF-8"),
            format!("{response}\n")
        );

        let mut framed = Vec::new();
        write_frame(&mut framed, Framing::ContentLength, &response).expect("writing must succeed");
        let framed = String::from_utf8(framed).expect("a response is UTF-8");
        let (head, body) = framed.split_once("\r\n\r\n").expect("a framed response");
        assert_eq!(head, format!("Content-Length: {}", body.len()));
    }
}
