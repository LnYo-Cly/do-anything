//! The `proc.*` long-lived process session steps.
//!
//! Unlike `cli.run`, which spawns one command and waits for it to exit,
//! `proc.start`, `proc.send`, `proc.wait_output`, and `proc.stop` keep one or
//! more named child processes alive across steps in a [`ProcSession`]. Output
//! pipes are continuously drained on background reader threads into bounded
//! 1 MiB ring buffers so a noisy child can never block on a full OS pipe.
//!
//! When a case ends — whether it passed, failed, errored, or timed out —
//! [`Session::close`] terminates every managed process tree across Linux,
//! macOS, and Windows and collects buffered stdout/stderr via
//! [`Session::evidence`] so failure bundles hold redacted `proc/` logs and
//! zero child or grandchild processes leak.

use std::collections::{BTreeMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::core::paths;
use crate::core::registry::{
    Registry, StepDefinition, StepError, disallow_retry, no_lint_warnings,
};
use crate::core::session::{Evidence, Session, Sessions};

/// Session kind shared by `proc.*` steps.
pub const SESSION_KIND: &str = "proc";

/// Step names registered by this module.
pub const STEP_NAMES: [&str; 4] = ["proc.start", "proc.send", "proc.wait_output", "proc.stop"];

const DEFAULT_NAME: &str = "default";
const DEFAULT_READY_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_STOP_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_SEND_TIMEOUT_MS: u64 = 5_000;

const READ_CHUNK: usize = 8 * 1024;
/// Maximum retained bytes per stream (stdout / stderr) per named process.
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const KILL_HELPER_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(unix)]
const GRACEFUL_TERM_CAP: Duration = Duration::from_millis(300);

/// Registers the `proc.*` steps.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    let start_schema = serde_json::to_value(schema_for!(ProcStartArgs))
        .map_err(|error| format!("could not build the schema of `proc.start`: {error}"))?;
    let mut send_schema = serde_json::to_value(schema_for!(ProcSendArgs))
        .map_err(|error| format!("could not build the schema of `proc.send`: {error}"))?;
    if let Value::Object(entries) = &mut send_schema {
        entries.insert(
            "anyOf".to_string(),
            json!([
                { "required": ["input"] },
                { "required": ["eof"], "properties": { "eof": { "const": true } } }
            ]),
        );
    }
    let wait_schema = serde_json::to_value(schema_for!(ProcWaitOutputArgs))
        .map_err(|error| format!("could not build the schema of `proc.wait_output`: {error}"))?;
    let stop_schema = serde_json::to_value(schema_for!(ProcStopArgs))
        .map_err(|error| format!("could not build the schema of `proc.stop`: {error}"))?;

    for definition in [
        StepDefinition {
            name: STEP_NAMES[0],
            description: "Starts a long-lived background process without a shell, optionally waiting for `ready_pattern`, and returns `name`, `pid`, `ready`, `stdout`, and `stderr`.",
            args_schema: start_schema,
            validate: validate_start,
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_start,
        },
        StepDefinition {
            name: STEP_NAMES[1],
            description: "Writes `input` to a running process's standard input and optionally closes stdin with `eof`.",
            args_schema: send_schema,
            validate: validate_send,
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_send,
        },
        StepDefinition {
            name: STEP_NAMES[2],
            description: "Waits for `pattern` in a running process's unconsumed output and returns `name`, `matched`, `stream`, `stdout`, and `stderr`.",
            args_schema: wait_schema,
            validate: validate_wait_output,
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_wait_output,
        },
        StepDefinition {
            name: STEP_NAMES[3],
            description: "Stops a running process and its entire process tree, returning `name`, `was_running`, `exit_code`, `stdout`, and `stderr`.",
            args_schema: stop_schema,
            validate: validate_stop,
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_stop,
        },
    ] {
        registry.register(definition)?;
    }
    Ok(())
}

/// Arguments of `proc.start`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProcStartArgs {
    /// Logical name of the process within the case. Defaults to `"default"`.
    #[schemars(extend("minLength" = 1, "pattern" = "^[A-Za-z0-9][A-Za-z0-9._-]*$"))]
    #[serde(default = "default_name")]
    name: String,
    /// Program name or path. Started directly without a shell.
    #[schemars(extend("minLength" = 1))]
    command: String,
    /// Arguments passed to the program, one per entry.
    #[serde(default)]
    args: Vec<String>,
    /// Working directory of the program. Defaults to the working directory of `da`.
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// Environment variables added to the inherited environment.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Substring to wait for in stdout or stderr before continuing.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    ready_pattern: Option<String>,
    /// Maximum wait for `ready_pattern` in milliseconds. Defaults to 30000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_ready_timeout_ms")]
    ready_timeout_ms: u64,
}

/// Arguments of `proc.send`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProcSendArgs {
    /// Logical process name. Defaults to `"default"`.
    #[schemars(extend("minLength" = 1, "pattern" = "^[A-Za-z0-9][A-Za-z0-9._-]*$"))]
    #[serde(default = "default_name")]
    name: String,
    /// UTF-8 text written verbatim to the process's standard input.
    #[serde(default)]
    input: Option<String>,
    /// Close standard input after writing `input` (or immediately if `input` is omitted).
    #[serde(default)]
    eof: bool,
    /// Maximum wait for the write to be accepted in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_send_timeout_ms")]
    timeout_ms: u64,
}

/// Which output stream `proc.wait_output` searches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
enum OutputStream {
    /// Match in either stdout or stderr.
    #[default]
    Both,
    /// Match only in stdout.
    Stdout,
    /// Match only in stderr.
    Stderr,
}

/// Arguments of `proc.wait_output`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProcWaitOutputArgs {
    /// Logical process name. Defaults to `"default"`.
    #[schemars(extend("minLength" = 1, "pattern" = "^[A-Za-z0-9][A-Za-z0-9._-]*$"))]
    #[serde(default = "default_name")]
    name: String,
    /// Non-empty substring to wait for in unconsumed process output.
    #[schemars(extend("minLength" = 1))]
    pattern: String,
    /// Stream to search (`both`, `stdout`, or `stderr`). Defaults to `both`.
    #[serde(default)]
    stream: OutputStream,
    /// Maximum wait in milliseconds. Defaults to 30000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_wait_timeout_ms")]
    timeout_ms: u64,
}

/// Arguments of `proc.stop`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProcStopArgs {
    /// Logical process name. Defaults to `"default"`.
    #[schemars(extend("minLength" = 1, "pattern" = "^[A-Za-z0-9][A-Za-z0-9._-]*$"))]
    #[serde(default = "default_name")]
    name: String,
    /// Maximum teardown wait in milliseconds before failing close. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_stop_timeout_ms")]
    timeout_ms: u64,
}

fn default_name() -> String {
    DEFAULT_NAME.to_string()
}

fn default_ready_timeout_ms() -> u64 {
    DEFAULT_READY_TIMEOUT_MS
}

fn default_wait_timeout_ms() -> u64 {
    DEFAULT_WAIT_TIMEOUT_MS
}

fn default_stop_timeout_ms() -> u64 {
    DEFAULT_STOP_TIMEOUT_MS
}

fn default_send_timeout_ms() -> u64 {
    DEFAULT_SEND_TIMEOUT_MS
}

fn validate_process_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("invalid arguments: `name` must not be empty".to_string());
    };
    if !first.is_ascii_alphanumeric()
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "invalid arguments: `name` must start with an ASCII letter or digit and use only ASCII letters, digits, `.`, `_`, and `-`, got `{name}`"
        ));
    }
    Ok(())
}

fn parse<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|error| format!("invalid arguments: {error}"))
}

fn check_start(args: &ProcStartArgs) -> Result<(), String> {
    validate_process_name(&args.name)?;
    if args.command.is_empty() {
        return Err("invalid arguments: `command` must name a program".to_string());
    }
    if let Some(pattern) = &args.ready_pattern
        && pattern.is_empty()
    {
        return Err("invalid arguments: `ready_pattern` must not be empty".to_string());
    }
    if args.ready_timeout_ms == 0 {
        return Err("invalid arguments: `ready_timeout_ms` must be at least 1".to_string());
    }
    Ok(())
}

fn check_send(args: &ProcSendArgs) -> Result<(), String> {
    validate_process_name(&args.name)?;
    if args.input.is_none() && !args.eof {
        return Err("invalid arguments: `proc.send` requires `input` or `eof: true`".to_string());
    }
    if args.timeout_ms == 0 {
        return Err("invalid arguments: `timeout_ms` must be at least 1".to_string());
    }
    Ok(())
}

fn check_wait_output(args: &ProcWaitOutputArgs) -> Result<(), String> {
    validate_process_name(&args.name)?;
    if args.pattern.is_empty() {
        return Err("invalid arguments: `pattern` must not be empty".to_string());
    }
    if args.timeout_ms == 0 {
        return Err("invalid arguments: `timeout_ms` must be at least 1".to_string());
    }
    Ok(())
}

fn check_stop(args: &ProcStopArgs) -> Result<(), String> {
    validate_process_name(&args.name)?;
    if args.timeout_ms == 0 {
        return Err("invalid arguments: `timeout_ms` must be at least 1".to_string());
    }
    Ok(())
}

fn validate_start(args: &Value) -> Result<(), String> {
    check_start(&parse::<ProcStartArgs>(args)?)
}

fn validate_send(args: &Value) -> Result<(), String> {
    check_send(&parse::<ProcSendArgs>(args)?)
}

fn validate_wait_output(args: &Value) -> Result<(), String> {
    check_wait_output(&parse::<ProcWaitOutputArgs>(args)?)
}

fn validate_stop(args: &Value) -> Result<(), String> {
    check_stop(&parse::<ProcStopArgs>(args)?)
}

#[derive(Default)]
struct StreamBuffer {
    bytes: VecDeque<u8>,
    base_offset: u64,
    total_bytes: u64,
    cursor: u64,
    closed: bool,
}

impl StreamBuffer {
    fn push_chunk(&mut self, chunk: &[u8]) {
        if chunk.len() >= MAX_STREAM_BYTES {
            self.bytes.clear();
            let tail = &chunk[chunk.len() - MAX_STREAM_BYTES..];
            self.bytes.extend(tail);
        } else {
            let overflow = (self.bytes.len() + chunk.len()).saturating_sub(MAX_STREAM_BYTES);
            if overflow > 0 {
                self.bytes.drain(..overflow);
            }
            self.bytes.extend(chunk);
        }
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        self.base_offset = self.total_bytes - self.bytes.len() as u64;
        if self.cursor < self.base_offset {
            self.cursor = self.base_offset;
        }
    }

    fn unconsumed_text(&mut self) -> String {
        let start = self.cursor.saturating_sub(self.base_offset) as usize;
        let slice = self.bytes.make_contiguous();
        String::from_utf8_lossy(&slice[start..]).into_owned()
    }

    #[cfg(test)]
    fn find_and_advance(&mut self, needle: &[u8]) -> Option<String> {
        let mut scanned_up_to = 0;
        self.find_and_advance_from(needle, &mut scanned_up_to)
    }

    fn find_and_advance_from(&mut self, needle: &[u8], scanned_up_to: &mut u64) -> Option<String> {
        if needle.is_empty() {
            return None;
        }
        let overlap = needle.len().saturating_sub(1) as u64;
        let search_abs = (*scanned_up_to)
            .saturating_sub(overlap)
            .max(self.cursor)
            .max(self.base_offset);
        let unconsumed_start = self.cursor.saturating_sub(self.base_offset) as usize;
        let search_start = search_abs.saturating_sub(self.base_offset) as usize;
        let slice = self.bytes.make_contiguous();
        *scanned_up_to = self.total_bytes;
        if search_start >= slice.len() {
            return None;
        }
        let rel_pos = find_subslice(&slice[search_start..], needle)?;
        let match_end = search_start + rel_pos + needle.len();
        let text = String::from_utf8_lossy(&slice[unconsumed_start..]).into_owned();
        self.cursor = self.base_offset + match_end as u64;
        Some(text)
    }

    fn evidence_text(&mut self) -> String {
        let slice = self.bytes.make_contiguous();
        let body = String::from_utf8_lossy(slice);
        if self.base_offset == 0 {
            body.into_owned()
        } else {
            format!(
                "[... {} earlier bytes truncated ...]\n{body}",
                self.base_offset
            )
        }
    }
}

struct ProcessStreams {
    stdout: StreamBuffer,
    stderr: StreamBuffer,
}

struct SharedStreams {
    state: Mutex<ProcessStreams>,
    notify: Condvar,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    mut pipe: R,
    shared: Arc<SharedStreams>,
    kind: StreamKind,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut chunk = [0_u8; READ_CHUNK];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    if let Ok(mut guard) = shared.state.lock() {
                        match kind {
                            StreamKind::Stdout => guard.stdout.push_chunk(&chunk[..read]),
                            StreamKind::Stderr => guard.stderr.push_chunk(&chunk[..read]),
                        }
                        shared.notify.notify_all();
                    } else {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if let Ok(mut guard) = shared.state.lock() {
            match kind {
                StreamKind::Stdout => guard.stdout.closed = true,
                StreamKind::Stderr => guard.stderr.closed = true,
            }
            shared.notify.notify_all();
        }
    })
}

enum StdinWriteError {
    Closed,
    TimedOut,
    Io(std::io::Error),
}

struct StdinRequest {
    bytes: Vec<u8>,
    close: bool,
    done: mpsc::Sender<std::io::Result<()>>,
}

/// Writes to a child's stdin on its own thread so a child that stops reading
/// cannot block the runner past a deadline.
struct StdinWriter {
    sender: Option<mpsc::Sender<StdinRequest>>,
    handle: Option<JoinHandle<()>>,
    closed: bool,
}

impl StdinWriter {
    fn spawn(mut stdin: ChildStdin) -> Self {
        let (sender, receiver) = mpsc::channel::<StdinRequest>();
        let handle = thread::spawn(move || {
            while let Ok(request) = receiver.recv() {
                if request.close {
                    let _ = request.done.send(Ok(()));
                    break;
                }
                let result = stdin.write_all(&request.bytes).and_then(|()| stdin.flush());
                let failed = result.is_err();
                let _ = request.done.send(result);
                if failed {
                    break;
                }
            }
        });
        Self {
            sender: Some(sender),
            handle: Some(handle),
            closed: false,
        }
    }

    fn write(&mut self, bytes: &[u8], timeout: Duration) -> Result<(), StdinWriteError> {
        if self.closed {
            return Err(StdinWriteError::Closed);
        }
        let sender = self.sender.as_ref().ok_or(StdinWriteError::Closed)?;
        let (done, result) = mpsc::channel();
        sender
            .send(StdinRequest {
                bytes: bytes.to_vec(),
                close: false,
                done,
            })
            .map_err(|_| StdinWriteError::Closed)?;
        match result.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(StdinWriteError::Io(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(StdinWriteError::TimedOut),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(StdinWriteError::Closed),
        }
    }

    fn close(&mut self, timeout: Duration) -> Result<(), StdinWriteError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let Some(sender) = self.sender.take() else {
            return Ok(());
        };
        let (done, result) = mpsc::channel();
        if sender
            .send(StdinRequest {
                bytes: Vec::new(),
                close: true,
                done,
            })
            .is_err()
        {
            return Ok(());
        }
        let outcome = match result.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(StdinWriteError::Io(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(StdinWriteError::TimedOut),
            Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
        };
        drop(sender);
        if outcome.is_ok()
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
        outcome
    }
}

struct ManagedProcess {
    child: Child,
    stdin: Option<StdinWriter>,
    streams: Arc<SharedStreams>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    exit_status: Option<ExitStatus>,
    stopped: bool,
}

struct WaitMatch {
    matched_stream: StreamKind,
    stdout: String,
    stderr: String,
}

impl ManagedProcess {
    fn spawn(args: &ProcStartArgs) -> Result<Self, StepError> {
        let mut command = Command::new(&args.command);
        command
            .args(&args.args)
            .envs(&args.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &args.cwd {
            match std::fs::metadata(dir) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(StepError::Environment(format!(
                        "could not start `{}`: `cwd` `{}` is not a directory",
                        args.command,
                        paths::display(dir)
                    )));
                }
                Err(error) => {
                    return Err(StepError::Environment(format!(
                        "could not start `{}`: invalid `cwd` `{}`: {error}",
                        args.command,
                        paths::display(dir)
                    )));
                }
            }
            command.current_dir(dir);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = command
            .spawn()
            .map_err(|error| spawn_error(&args.command, &error))?;
        let stdin = child.stdin.take().map(StdinWriter::spawn);
        let stdout_pipe = child
            .stdout
            .take()
            .ok_or_else(|| StepError::Environment("failed to capture child stdout".to_string()))?;
        let stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| StepError::Environment("failed to capture child stderr".to_string()))?;

        let streams = Arc::new(SharedStreams {
            state: Mutex::new(ProcessStreams {
                stdout: StreamBuffer::default(),
                stderr: StreamBuffer::default(),
            }),
            notify: Condvar::new(),
        });
        let stdout_reader = Some(spawn_reader(
            stdout_pipe,
            Arc::clone(&streams),
            StreamKind::Stdout,
        ));
        let stderr_reader = Some(spawn_reader(
            stderr_pipe,
            Arc::clone(&streams),
            StreamKind::Stderr,
        ));

        Ok(Self {
            child,
            stdin,
            streams,
            stdout_reader,
            stderr_reader,
            exit_status: None,
            stopped: false,
        })
    }

    fn refresh_exit(&mut self) -> Result<Option<ExitStatus>, String> {
        if let Some(status) = self.exit_status {
            return Ok(Some(status));
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit_status = Some(status);
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(format!("could not poll child status: {error}")),
        }
    }

    fn is_running(&mut self) -> bool {
        !self.stopped && self.refresh_exit().is_ok_and(|status| status.is_none())
    }

    fn snapshot_unconsumed(&self) -> (String, String) {
        match self.streams.state.lock() {
            Ok(mut guard) => (
                guard.stdout.unconsumed_text(),
                guard.stderr.unconsumed_text(),
            ),
            Err(_) => (String::new(), String::new()),
        }
    }

    fn snapshot_evidence(&self) -> (String, String) {
        match self.streams.state.lock() {
            Ok(mut guard) => (guard.stdout.evidence_text(), guard.stderr.evidence_text()),
            Err(_) => (String::new(), String::new()),
        }
    }

    fn wait_for_pattern(
        &mut self,
        name: &str,
        step_name: &str,
        pattern: &str,
        stream: OutputStream,
        timeout: Duration,
    ) -> Result<WaitMatch, StepError> {
        let needle = pattern.as_bytes();
        let deadline = Instant::now() + timeout;
        let mut exited_at: Option<Instant> = None;
        let mut stdout_scanned: u64 = 0;
        let mut stderr_scanned: u64 = 0;

        loop {
            let (pipes_closed, found) = {
                let mut guard = self.streams.state.lock().map_err(|_| {
                    StepError::Environment(format!("stream lock poisoned for process `{name}`"))
                })?;

                let found = match stream {
                    OutputStream::Stdout => guard
                        .stdout
                        .find_and_advance_from(needle, &mut stdout_scanned)
                        .map(|stdout| WaitMatch {
                            matched_stream: StreamKind::Stdout,
                            stdout,
                            stderr: String::new(),
                        }),
                    OutputStream::Stderr => guard
                        .stderr
                        .find_and_advance_from(needle, &mut stderr_scanned)
                        .map(|stderr| WaitMatch {
                            matched_stream: StreamKind::Stderr,
                            stdout: String::new(),
                            stderr,
                        }),
                    OutputStream::Both => {
                        if let Some(stdout) = guard
                            .stdout
                            .find_and_advance_from(needle, &mut stdout_scanned)
                        {
                            let stderr = guard.stderr.unconsumed_text();
                            Some(WaitMatch {
                                matched_stream: StreamKind::Stdout,
                                stdout,
                                stderr,
                            })
                        } else if let Some(stderr) = guard
                            .stderr
                            .find_and_advance_from(needle, &mut stderr_scanned)
                        {
                            let stdout = guard.stdout.unconsumed_text();
                            Some(WaitMatch {
                                matched_stream: StreamKind::Stderr,
                                stdout,
                                stderr,
                            })
                        } else {
                            None
                        }
                    }
                };
                let closed = match stream {
                    OutputStream::Stdout => guard.stdout.closed,
                    OutputStream::Stderr => guard.stderr.closed,
                    OutputStream::Both => guard.stdout.closed && guard.stderr.closed,
                };
                (closed, found)
            };

            if let Some(matched) = found {
                return Ok(matched);
            }

            if self.refresh_exit().is_ok_and(|status| status.is_some()) {
                let first_exit = *exited_at.get_or_insert_with(Instant::now);
                if pipes_closed || first_exit.elapsed() >= DRAIN_TIMEOUT {
                    let status_text = self
                        .exit_status
                        .and_then(|s| s.code())
                        .map_or_else(|| "signal".to_string(), |c| format!("exit code {c}"));
                    return Err(StepError::Execution(format!(
                        "`{step_name}` for process `{name}` exited ({status_text}) before emitting `{pattern}`; see the `proc/` logs in the failure bundle for captured stdout and stderr"
                    )));
                }
            } else if pipes_closed {
                // Pipes reached EOF while child status hasn't been polled yet; keep looping briefly so exit status settles.
                thread::sleep(Duration::from_millis(5));
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(StepError::Timeout(format!(
                    "`{step_name}` for process `{name}` did not observe `{pattern}` within {} ms; see the `proc/` logs in the failure bundle for captured stdout and stderr",
                    timeout.as_millis()
                )));
            }

            let remaining = deadline.saturating_duration_since(now);
            let wait_slice = remaining.min(POLL_INTERVAL);
            if let Ok(guard) = self.streams.state.lock() {
                let _ = self.streams.notify.wait_timeout(guard, wait_slice);
            }
        }
    }

    fn drain_pipes(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        if let Ok(mut guard) = self.streams.state.lock() {
            while !(guard.stdout.closed && guard.stderr.closed) {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline.saturating_duration_since(now);
                let (next_guard, _) = match self.streams.notify.wait_timeout(guard, remaining) {
                    Ok(pair) => pair,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard = next_guard;
            }
        }
        let (join_stdout, join_stderr) = match self.streams.state.lock() {
            Ok(guard) => (guard.stdout.closed, guard.stderr.closed),
            Err(_) => (false, false),
        };
        if join_stdout && let Some(handle) = self.stdout_reader.take() {
            let _ = handle.join();
        }
        if join_stderr && let Some(handle) = self.stderr_reader.take() {
            let _ = handle.join();
        }
    }

    fn stop(&mut self, name: &str, timeout: Duration) -> Result<bool, String> {
        if self.stopped {
            return Ok(false);
        }

        // Close stdin first so processes waiting on EOF can exit cleanly.
        let stdin_was_already_closed = self.stdin.is_none();
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.close(Duration::from_millis(100));
        }

        let mut was_running = self.refresh_exit()?.is_none();
        if was_running && stdin_was_already_closed && self.both_pipes_closed() {
            // A process that already had stdin closed and closed both output
            // pipes is usually in final CRT exit; wait briefly before killing
            // so it keeps its natural exit code without spawning kill helpers.
            let settle_deadline = Instant::now() + timeout.min(Duration::from_millis(100));
            while Instant::now() < settle_deadline {
                if self.refresh_exit()?.is_some() {
                    was_running = false;
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }

        if !was_running && !self.both_pipes_closed() {
            // Give reader threads a brief chance to observe pipe EOF from an
            // already-exited child before deciding a descendant holds the pipe.
            self.drain_pipes(Duration::from_millis(50));
        }

        let pid = self.child.id();
        let kill_result =
            terminate_process_tree(&mut self.child, &mut self.exit_status, pid, timeout);

        self.drain_pipes(DRAIN_TIMEOUT.min(timeout.max(Duration::from_millis(100))));
        kill_result.map_err(|error| format!("process `{name}` (pid {pid}): {error}"))?;
        self.stopped = true;
        Ok(was_running)
    }

    fn both_pipes_closed(&self) -> bool {
        self.streams
            .state
            .lock()
            .is_ok_and(|guard| guard.stdout.closed && guard.stderr.closed)
    }
}

/// Keeps all named long-lived processes for a single test case.
#[derive(Default)]
struct ProcSession {
    processes: BTreeMap<String, ManagedProcess>,
    order: Vec<String>,
    retired: Vec<RetiredLogs>,
}

struct RetiredLogs {
    name: String,
    stdout: String,
    stderr: String,
}

impl ProcSession {
    fn close_all_processes(&mut self) -> Result<(), String> {
        let names: Vec<String> = self.order.iter().rev().cloned().collect();
        let mut errors = Vec::new();
        for name in names {
            if let Some(proc) = self.processes.get_mut(&name)
                && let Err(error) = proc.stop(&name, Duration::from_millis(DEFAULT_STOP_TIMEOUT_MS))
            {
                errors.push(error);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Drop for ProcSession {
    fn drop(&mut self) {
        // Fallback cleanup on panic or early unwind so no child tree survives.
        let _ = self.close_all_processes();
    }
}

impl Session for ProcSession {
    fn close(&mut self) -> Result<(), String> {
        self.close_all_processes()
    }

    fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        let mut logs: BTreeMap<String, (String, String)> = BTreeMap::new();
        for retired in &self.retired {
            let entry = logs.entry(retired.name.clone()).or_default();
            append_evidence(&mut entry.0, &retired.stdout);
            append_evidence(&mut entry.1, &retired.stderr);
        }
        for name in &self.order {
            let entry = logs.entry(name.clone()).or_default();
            if let Some(proc) = self.processes.get(name) {
                let (stdout, stderr) = proc.snapshot_evidence();
                append_evidence(&mut entry.0, &stdout);
                append_evidence(&mut entry.1, &stderr);
            }
        }
        let single_default = logs.len() == 1 && logs.contains_key(DEFAULT_NAME);
        let mut files = Vec::new();
        for (name, (stdout, stderr)) in logs {
            let (out_name, err_name) = if single_default {
                ("stdout.log".to_string(), "stderr.log".to_string())
            } else {
                (format!("{name}-stdout.log"), format!("{name}-stderr.log"))
            };
            files.push(
                Evidence::text(out_name, stdout)
                    .map_err(|error| format!("could not attach stdout evidence: {error}"))?,
            );
            files.push(
                Evidence::text(err_name, stderr)
                    .map_err(|error| format!("could not attach stderr evidence: {error}"))?,
            );
        }
        Ok(files)
    }
}

fn append_evidence(destination: &mut String, part: &str) {
    if part.is_empty() {
        return;
    }
    if !destination.is_empty() {
        destination.push_str("\n[... earlier process logs ...]\n");
    }
    destination.push_str(part);
}

fn run_start(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ProcStartArgs = parse(args).map_err(StepError::InvalidArguments)?;
    check_start(&args).map_err(StepError::InvalidArguments)?;

    let session = sessions
        .get_or_create::<ProcSession>(SESSION_KIND, || Ok(ProcSession::default()))
        .map_err(StepError::Environment)?;

    let mut retired_logs = None;
    if let Some(existing) = session.processes.get_mut(&args.name) {
        if existing.is_running() {
            return Err(StepError::Execution(format!(
                "process `{}` is already running; stop it with `proc.stop` first or choose another `name`",
                args.name
            )));
        }
        // Reusing a name must not drop the old process object without stopping
        // its tree: an exited leader can still own live descendants.
        existing
            .stop(&args.name, Duration::from_millis(DEFAULT_STOP_TIMEOUT_MS))
            .map_err(|error| {
                StepError::Environment(format!(
                    "could not retire exited process `{}` before restarting it: {error}",
                    args.name
                ))
            })?;
        let (stdout, stderr) = existing.snapshot_evidence();
        retired_logs = Some((stdout, stderr));
    }
    if let Some((stdout, stderr)) = retired_logs {
        session.processes.remove(&args.name);
        session.retired.push(RetiredLogs {
            name: args.name.clone(),
            stdout,
            stderr,
        });
    }

    let proc = ManagedProcess::spawn(&args)?;
    let pid = proc.child.id();
    session.processes.insert(args.name.clone(), proc);
    if !session.order.iter().any(|existing| existing == &args.name) {
        session.order.push(args.name.clone());
    }

    let (stdout, stderr) = if let Some(pattern) = &args.ready_pattern {
        let timeout = Duration::from_millis(args.ready_timeout_ms);
        let result = {
            let proc = session
                .processes
                .get_mut(&args.name)
                .ok_or_else(|| StepError::Environment("missing spawned process".to_string()))?;
            proc.wait_for_pattern(
                &args.name,
                "proc.start",
                pattern,
                OutputStream::Both,
                timeout,
            )
        };
        match result {
            Ok(matched) => (matched.stdout, matched.stderr),
            Err(error) => {
                // Kill any spawned process tree on ready timeout while keeping
                // the captured logs in `ProcSession` for failure bundles.
                if let Some(proc) = session.processes.get_mut(&args.name) {
                    let _ = proc.stop(&args.name, Duration::from_millis(DEFAULT_STOP_TIMEOUT_MS));
                }
                return Err(error);
            }
        }
    } else {
        let proc = session
            .processes
            .get(&args.name)
            .ok_or_else(|| StepError::Environment("missing spawned process".to_string()))?;
        proc.snapshot_unconsumed()
    };

    Ok(json!({
        "name": args.name,
        "pid": pid,
        "ready": true,
        "stdout": stdout,
        "stderr": stderr,
    }))
}

fn lookup_process<'a>(
    sessions: &'a mut Sessions,
    name: &str,
) -> Result<&'a mut ManagedProcess, StepError> {
    let session = sessions
        .get::<ProcSession>(SESSION_KIND)
        .map_err(StepError::Environment)?
        .ok_or_else(|| {
            StepError::Execution(format!(
                "no process `{name}` is running; start it with `proc.start` first"
            ))
        })?;
    session.processes.get_mut(name).ok_or_else(|| {
        StepError::Execution(format!(
            "no process `{name}` was started in this case; start it with `proc.start` first"
        ))
    })
}

fn run_send(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ProcSendArgs = parse(args).map_err(StepError::InvalidArguments)?;
    check_send(&args).map_err(StepError::InvalidArguments)?;
    let proc = lookup_process(sessions, &args.name)?;

    if proc.stopped {
        return Err(StepError::Execution(format!(
            "process `{}` is already stopped",
            args.name
        )));
    }

    let timeout = Duration::from_millis(args.timeout_ms);
    let mut bytes_written = 0_usize;
    if let Some(text) = &args.input {
        let written = match proc.stdin.as_mut() {
            Some(stdin) => stdin.write(text.as_bytes(), timeout),
            None => Err(StdinWriteError::Closed),
        };
        match written {
            Ok(()) => bytes_written = text.len(),
            Err(StdinWriteError::Closed) => {
                return Err(StepError::Execution(format!(
                    "stdin of process `{}` is already closed",
                    args.name
                )));
            }
            Err(StdinWriteError::Io(error)) => {
                return Err(StepError::Execution(format!(
                    "could not write to stdin of process `{}`: {error}",
                    args.name
                )));
            }
            Err(StdinWriteError::TimedOut) => {
                let cleanup =
                    match proc.stop(&args.name, Duration::from_millis(DEFAULT_STOP_TIMEOUT_MS)) {
                        Ok(_) => String::new(),
                        Err(error) => format!("; process-tree cleanup also failed: {error}"),
                    };
                return Err(StepError::Timeout(format!(
                    "`proc.send` for process `{}` could not write {} bytes within {} ms because the process stopped reading its input; the process tree was stopped{cleanup}",
                    args.name,
                    text.len(),
                    args.timeout_ms
                )));
            }
        }
    }

    if args.eof
        && let Some(mut stdin) = proc.stdin.take()
    {
        match stdin.close(timeout) {
            Ok(()) | Err(StdinWriteError::Closed) => {}
            Err(StdinWriteError::Io(error)) => {
                return Err(StepError::Execution(format!(
                    "could not close stdin of process `{}`: {error}",
                    args.name
                )));
            }
            Err(StdinWriteError::TimedOut) => {
                drop(stdin);
                let cleanup =
                    match proc.stop(&args.name, Duration::from_millis(DEFAULT_STOP_TIMEOUT_MS)) {
                        Ok(_) => String::new(),
                        Err(error) => format!("; process-tree cleanup also failed: {error}"),
                    };
                return Err(StepError::Timeout(format!(
                    "`proc.send` for process `{}` could not close stdin within {} ms; the process tree was stopped{cleanup}",
                    args.name, args.timeout_ms
                )));
            }
        }
    }

    Ok(json!({
        "name": args.name,
        "bytes_written": bytes_written,
        "eof": args.eof,
    }))
}

fn run_wait_output(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ProcWaitOutputArgs = parse(args).map_err(StepError::InvalidArguments)?;
    check_wait_output(&args).map_err(StepError::InvalidArguments)?;
    let proc = lookup_process(sessions, &args.name)?;

    let matched = proc.wait_for_pattern(
        &args.name,
        "proc.wait_output",
        &args.pattern,
        args.stream,
        Duration::from_millis(args.timeout_ms),
    )?;

    Ok(json!({
        "name": args.name,
        "matched": true,
        "stream": matched.matched_stream.as_str(),
        "stdout": matched.stdout,
        "stderr": matched.stderr,
    }))
}

fn run_stop(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ProcStopArgs = parse(args).map_err(StepError::InvalidArguments)?;
    check_stop(&args).map_err(StepError::InvalidArguments)?;
    let proc = lookup_process(sessions, &args.name)?;

    let was_running = proc
        .stop(&args.name, Duration::from_millis(args.timeout_ms))
        .map_err(StepError::Environment)?;
    let (stdout, stderr) = proc.snapshot_evidence();
    let exit_code = proc.exit_status.and_then(|status| status.code());

    let mut result = Map::new();
    result.insert("name".to_string(), json!(args.name));
    result.insert("was_running".to_string(), json!(was_running));
    result.insert(
        "exit_code".to_string(),
        match exit_code {
            Some(code) => json!(code),
            None => Value::Null,
        },
    );
    result.insert("stdout".to_string(), json!(stdout));
    result.insert("stderr".to_string(), json!(stderr));
    Ok(Value::Object(result))
}

fn spawn_error(command: &str, error: &std::io::Error) -> StepError {
    let hint = match error.kind() {
        ErrorKind::NotFound => {
            "check that the program exists and is on `PATH`, or pass an absolute path"
        }
        ErrorKind::PermissionDenied => "check that the program is executable",
        _ => "check `command`, `cwd` and `env`",
    };
    StepError::Environment(format!("could not start `{command}`: {error}; {hint}"))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn terminate_process_tree(
    child: &mut Child,
    exit_status: &mut Option<ExitStatus>,
    pid: u32,
    timeout: Duration,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        terminate_unix_process_group(child, exit_status, pid, timeout)
    }
    #[cfg(windows)]
    {
        terminate_windows_process_tree(child, exit_status, pid, timeout)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, timeout);
        if exit_status.is_none() {
            let _ = child.kill();
            *exit_status = child.wait().ok();
        }
        Ok(())
    }
}

#[cfg(unix)]
fn terminate_unix_process_group(
    child: &mut Child,
    exit_status: &mut Option<ExitStatus>,
    pid: u32,
    timeout: Duration,
) -> Result<(), String> {
    let already_exited = exit_status.is_some()
        || matches!(child.try_wait(), Ok(Some(status)) if { *exit_status = Some(status); true });

    if !already_exited {
        let _ = signal_unix_process_group("TERM", pid);
        let grace_deadline = Instant::now() + timeout.min(GRACEFUL_TERM_CAP);
        while Instant::now() < grace_deadline {
            if let Ok(Some(status)) = child.try_wait() {
                *exit_status = Some(status);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    // Send SIGKILL to the whole process group even if the leader already exited,
    // because grandchildren in that process group can outlive their parent.
    let _ = signal_unix_process_group("KILL", pid);
    if exit_status.is_none() {
        let _ = child.kill();
        *exit_status = child
            .wait()
            .map(Some)
            .map_err(|error| format!("could not wait for process exit: {error}"))?;
    }

    // Leader is now reaped; confirm no non-zombie member remains in process group `pid`.
    let wait_deadline = Instant::now() + timeout.min(KILL_HELPER_TIMEOUT);
    while unix_process_group_alive(pid) {
        if Instant::now() >= wait_deadline {
            return Err("process group survived SIGKILL".to_string());
        }
        let _ = signal_unix_process_group("KILL", pid);
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(unix)]
fn signal_unix_process_group(signal: &str, pgid: u32) -> std::io::Result<ExitStatus> {
    // First signal the process group atomically (`--` prevents dash/procps
    // from parsing `-<pgid>` as signal option flags), then also signal any
    // discovered live member PIDs as positive integers so no shell/kill
    // variant can miss surviving grandchildren.
    let pid_str = pgid.to_string();
    let group_status = Command::new("/bin/sh")
        .args([
            "-c",
            "kill -s \"$1\" -- -\"$2\" 2>/dev/null || /bin/kill -s \"$1\" -- -\"$2\" 2>/dev/null",
            "sh",
            signal,
            &pid_str,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let members = collect_unix_pgid_members(pgid);
    if !members.is_empty() {
        let member_args: Vec<String> = members.into_iter().map(|m| m.to_string()).collect();
        let bin = if std::path::Path::new("/bin/kill").is_file() {
            "/bin/kill"
        } else {
            "kill"
        };
        let _ = Command::new(bin)
            .arg("-s")
            .arg(signal)
            .args(&member_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    group_status
}

#[cfg(any(unix, test))]
fn parse_proc_stat_pgrp_and_state(stat_text: &str) -> Option<(u32, char)> {
    let (_, rest) = stat_text.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<u32>().ok()?;
    Some((pgrp, state))
}

#[cfg(any(unix, test))]
fn is_dead_or_zombie_state(state: char) -> bool {
    matches!(state, 'Z' | 'X' | 'x')
}

#[cfg(any(unix, test))]
fn ps_output_pgid_members(stdout: &str, target_pgid: u32, self_pid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let Some(pid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(pgid) = fields.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(stat) = fields.next() else {
            continue;
        };
        if pid != 0
            && pid != self_pid
            && pgid == target_pgid
            && !stat.chars().next().is_some_and(is_dead_or_zombie_state)
        {
            pids.push(pid);
        }
    }
    pids
}

#[cfg(unix)]
fn collect_unix_pgid_members(target_pgid: u32) -> Vec<u32> {
    let self_pid = std::process::id();
    let proc_root = std::path::Path::new("/proc");
    if proc_root.is_dir()
        && let Ok(entries) = std::fs::read_dir(proc_root)
    {
        let mut pids = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            if pid == 0 || pid == self_pid {
                continue;
            }
            if let Ok(stat) = std::fs::read_to_string(entry.path().join("stat"))
                && let Some((pgrp, state)) = parse_proc_stat_pgrp_and_state(&stat)
                && pgrp == target_pgid
                && !is_dead_or_zombie_state(state)
            {
                pids.push(pid);
            }
        }
        return pids;
    }

    let Ok(output) = Command::new("ps")
        .args(["-ww", "-A", "-o", "pid=,pgid=,stat="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    ps_output_pgid_members(
        &String::from_utf8_lossy(&output.stdout),
        target_pgid,
        self_pid,
    )
}

#[cfg(unix)]
fn unix_process_group_alive(target_pgid: u32) -> bool {
    !collect_unix_pgid_members(target_pgid).is_empty()
}

#[cfg(windows)]
fn terminate_windows_process_tree(
    child: &mut Child,
    exit_status: &mut Option<ExitStatus>,
    pid: u32,
    timeout: Duration,
) -> Result<(), String> {
    if exit_status.is_none()
        && let Ok(Some(status)) = child.try_wait()
    {
        *exit_status = Some(status);
    }

    if exit_status.is_none() {
        let taskkill_ok = taskkill_windows_tree(pid, timeout.min(KILL_HELPER_TIMEOUT));
        let _ = child.kill();
        *exit_status = child
            .wait()
            .map(Some)
            .map_err(|error| format!("could not wait for process exit: {error}"))?;
        if taskkill_ok {
            // `taskkill /T` already walked and killed the live tree.
            return Ok(());
        }
    }

    // A child can exit while a grandchild survives with stdout/stderr attached
    // to files or null. Pipe EOF therefore says nothing about the process
    // tree, so inspect descendants by PID lineage and verify they are gone.
    // Windows PowerShell startup dominates the cost here, so one bounded
    // process owns the enumerate/kill/verify loop instead of one probe per
    // iteration.
    let budget = timeout.max(Duration::from_secs(10));
    terminate_windows_descendants(pid, budget)
}

#[cfg(windows)]
fn windows_system_tool(parts: &[&str], fallback: &str) -> PathBuf {
    let Some(root) = std::env::var_os("SystemRoot") else {
        return PathBuf::from(fallback);
    };
    let mut path = PathBuf::from(root).join("System32");
    for part in parts {
        path = path.join(part);
    }
    if path.is_file() {
        path
    } else {
        PathBuf::from(fallback)
    }
}

#[cfg(windows)]
struct HelperOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

#[cfg(windows)]
fn run_bounded_helper(mut command: Command, budget: Duration) -> Option<HelperOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut helper = command.spawn().ok()?;
    let stdout = helper.stdout.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let stderr = helper.stderr.take().map(|mut pipe| {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            buffer
        })
    });
    let deadline = Instant::now() + budget;
    let status = loop {
        match helper.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break None;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    };
    let Some(status) = status else {
        let _ = helper.kill();
        let _ = helper.wait();
        return None;
    };
    let stdout = join_reader(stdout, Duration::from_secs(1))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let stderr = join_reader(stderr, Duration::from_secs(1))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    Some(HelperOutput {
        status,
        stdout,
        stderr,
    })
}

#[cfg(windows)]
fn join_reader(handle: Option<JoinHandle<Vec<u8>>>, timeout: Duration) -> Option<Vec<u8>> {
    let handle = handle?;
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
    handle.join().ok()
}

#[cfg(windows)]
fn taskkill_windows_tree(pid: u32, budget: Duration) -> bool {
    let taskkill = windows_system_tool(&["taskkill.exe"], "taskkill");
    let pid_text = pid.to_string();
    let mut command = Command::new(taskkill);
    command.args(["/PID", &pid_text, "/T", "/F"]);
    run_bounded_helper(command, budget).is_some_and(|output| output.status.success())
}

#[cfg(windows)]
fn terminate_windows_descendants(root_pid: u32, budget: Duration) -> Result<(), String> {
    let deadline = Instant::now() + budget;
    match terminate_windows_descendants_wmic(root_pid, deadline) {
        Ok(Some(())) => Ok(()),
        Ok(None) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            terminate_windows_descendants_powershell(
                root_pid,
                remaining.max(Duration::from_secs(1)),
            )
        }
        Err(wmic_error) => {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining < Duration::from_secs(1) {
                return Err(wmic_error);
            }
            terminate_windows_descendants_powershell(root_pid, remaining)
                .map_err(|powershell_error| format!("{wmic_error}; {powershell_error}"))
        }
    }
}

/// Enumerates the process table with WMIC and terminates every descendant of
/// `root_pid`. Returns `Ok(None)` only when WMIC is unavailable before any
/// teardown was attempted, so the caller can fall back to PowerShell.
#[cfg(windows)]
fn terminate_windows_descendants_wmic(
    root_pid: u32,
    deadline: Instant,
) -> Result<Option<()>, String> {
    let Some(wmic) = windows_wmic() else {
        return Ok(None);
    };
    let mut attempted = false;
    let mut last_targets = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(descendant_survivor_error(&last_targets));
        }
        let parents = match windows_process_parent_map(&wmic, remaining) {
            Ok(parents) => parents,
            Err(_error) if !attempted => return Ok(None),
            Err(error) => return Err(error),
        };
        let targets = windows_descendants(root_pid, &parents);
        if targets.is_empty() {
            return Ok(Some(()));
        }
        last_targets = targets.clone();
        attempted = true;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(descendant_survivor_error(&last_targets));
        }
        let _ = taskkill_windows_pids(&targets, remaining.min(KILL_HELPER_TIMEOUT));
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(windows)]
fn windows_wmic() -> Option<PathBuf> {
    let path = windows_system_tool(&["wbem", "WMIC.exe"], "wmic.exe");
    path.is_file().then_some(path)
}

#[cfg(windows)]
fn windows_process_parent_map(
    wmic: &std::path::Path,
    budget: Duration,
) -> Result<BTreeMap<u32, u32>, String> {
    let mut command = Command::new(wmic);
    command.args(["process", "get", "ProcessId,ParentProcessId", "/format:csv"]);
    let output = run_bounded_helper(command, budget).ok_or_else(|| {
        "could not enumerate Windows processes within the teardown budget".to_string()
    })?;
    if !output.status.success() {
        return Err(format!(
            "could not enumerate Windows processes ({}): {}",
            output.status,
            output.stderr.trim()
        ));
    }
    let mut parents = BTreeMap::new();
    for line in output.stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line.contains("ParentProcessId") {
            continue;
        }
        let mut fields = line.rsplit(',');
        let Some(pid) = fields
            .next()
            .and_then(|field| field.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let Some(parent) = fields
            .next()
            .and_then(|field| field.trim().parse::<u32>().ok())
        else {
            continue;
        };
        parents.insert(pid, parent);
    }
    if parents.is_empty() {
        return Err("Windows process enumeration returned no rows".to_string());
    }
    Ok(parents)
}

#[cfg(windows)]
fn windows_descendants(root_pid: u32, parents: &BTreeMap<u32, u32>) -> Vec<u32> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (&pid, &parent) in parents {
        children.entry(parent).or_default().push(pid);
    }
    let mut targets = Vec::new();
    let mut seen = BTreeMap::new();
    let mut queue = VecDeque::from([root_pid]);
    while let Some(parent) = queue.pop_front() {
        for &child in children.get(&parent).into_iter().flatten() {
            if seen.insert(child, ()).is_none() {
                queue.push_back(child);
                targets.push(child);
            }
        }
    }
    targets.sort_unstable();
    targets
}

#[cfg(windows)]
fn descendant_survivor_error(targets: &[u32]) -> String {
    if targets.is_empty() {
        "descendant teardown did not complete within the budget".to_string()
    } else {
        format!(
            "descendant processes survived teardown: {}",
            targets
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(windows)]
fn taskkill_windows_pids(pids: &[u32], budget: Duration) -> bool {
    let taskkill = windows_system_tool(&["taskkill.exe"], "taskkill");
    let mut command = Command::new(taskkill);
    for pid in pids {
        command.args(["/PID", &pid.to_string()]);
    }
    command.args(["/T", "/F"]);
    run_bounded_helper(command, budget).is_some_and(|output| output.status.success())
}

#[cfg(windows)]
fn terminate_windows_descendants_powershell(root_pid: u32, budget: Duration) -> Result<(), String> {
    let powershell = windows_powershell();
    let script = WINDOWS_DESCENDANT_SCRIPT
        .replace("__ROOT_PID__", &root_pid.to_string())
        .replace("__BUDGET_MS__", &budget.as_millis().max(1).to_string());
    let mut command = Command::new(powershell);
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    let output = run_bounded_helper(command, budget + Duration::from_secs(2)).ok_or_else(|| {
        "could not inspect or terminate descendant processes within the teardown budget".to_string()
    })?;
    if output.status.success() {
        return Ok(());
    }
    let survivors = output
        .stdout
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect::<Vec<_>>();
    if survivors.is_empty() {
        return Err(format!(
            "descendant teardown failed ({}): {}",
            output.status,
            output.stderr.trim()
        ));
    }
    Err(format!(
        "descendant processes survived teardown: {}",
        survivors
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Prefer PowerShell 7 when the host has it because its startup is cheaper;
/// Windows PowerShell 5.1 is always available as the fallback.
#[cfg(windows)]
fn windows_powershell() -> PathBuf {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("pwsh.exe");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    windows_system_tool(
        &["WindowsPowerShell", "v1.0", "powershell.exe"],
        "powershell.exe",
    )
}

#[cfg(windows)]
const WINDOWS_DESCENDANT_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$root = __ROOT_PID__
$deadline = [DateTime]::UtcNow.AddMilliseconds(__BUDGET_MS__)
while ($true) {
  $children = @{}
  foreach ($process in Get-CimInstance Win32_Process) {
    $parent_id = [int]$process.ParentProcessId
    if (-not $children.ContainsKey($parent_id)) {
      $children[$parent_id] = [System.Collections.Generic.List[int]]::new()
    }
    $children[$parent_id].Add([int]$process.ProcessId)
  }
  $queue = [System.Collections.Generic.Queue[int]]::new()
  $seen = [System.Collections.Generic.HashSet[int]]::new()
  $queue.Enqueue([int]$root)
  $targets = [System.Collections.Generic.List[int]]::new()
  while ($queue.Count -gt 0) {
    $parent = $queue.Dequeue()
    $kids = $children[$parent]
    if ($null -ne $kids) {
      foreach ($kid in $kids) {
        $kid_id = [int]$kid
        if ($seen.Add($kid_id)) {
          $queue.Enqueue($kid_id)
          $targets.Add($kid_id)
        }
      }
    }
  }
  if ($targets.Count -eq 0) { exit 0 }
  if ([DateTime]::UtcNow -ge $deadline) {
    foreach ($target in $targets) { Write-Output $target }
    exit 2
  }
  foreach ($target in $targets) {
    Stop-Process -Id $target -Force -ErrorAction SilentlyContinue
  }
  Start-Sleep -Milliseconds 50
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_process_names_and_arguments() {
        assert!(validate_start(&json!({"command": "da"})).is_ok());
        assert!(validate_start(&json!({"command": "da", "name": "api_1.0-a"})).is_ok());
        assert!(validate_start(&json!({"command": ""})).is_err());
        assert!(validate_start(&json!({"command": "da", "name": "."})).is_err());
        assert!(validate_start(&json!({"command": "da", "name": "../bad"})).is_err());
        assert!(validate_start(&json!({"command": "da", "ready_timeout_ms": 0})).is_err());
        assert!(validate_start(&json!({"command": "da", "ready_pattern": ""})).is_err());

        assert!(validate_send(&json!({"input": "hi\n"})).is_ok());
        assert!(validate_send(&json!({"eof": true})).is_ok());
        assert!(validate_send(&json!({})).is_err());
        assert!(validate_send(&json!({"eof": false})).is_err());
        assert!(validate_send(&json!({"input": "hi", "timeout_ms": 0})).is_err());

        assert!(validate_wait_output(&json!({"pattern": "ok"})).is_ok());
        assert!(validate_wait_output(&json!({"pattern": ""})).is_err());
        assert!(validate_wait_output(&json!({"pattern": "ok", "timeout_ms": 0})).is_err());

        assert!(validate_stop(&json!({})).is_ok());
        assert!(validate_stop(&json!({"timeout_ms": 0})).is_err());
    }

    #[test]
    fn stream_buffer_caps_memory_and_advances_cursor_past_repeats() {
        let mut buf = StreamBuffer::default();
        buf.push_chunk(b"ping\npong\n");
        assert_eq!(
            buf.find_and_advance(b"ping").as_deref(),
            Some("ping\npong\n")
        );
        assert_eq!(buf.find_and_advance(b"pong").as_deref(), Some("\npong\n"));
        assert!(buf.find_and_advance(b"ping").is_none());

        // Pushing another `ping` wakes a subsequent search.
        buf.push_chunk(b"ping\n");
        assert_eq!(buf.find_and_advance(b"ping").as_deref(), Some("\nping\n"));

        // Pushing more than MAX_STREAM_BYTES drops oldest bytes without losing marker.
        let flood = vec![b'x'; MAX_STREAM_BYTES + 128];
        buf.push_chunk(&flood);
        buf.push_chunk(b"TAIL_MARKER");
        let evidence = buf.evidence_text();
        assert!(evidence.contains("earlier bytes truncated"), "{evidence}");
        assert!(evidence.ends_with("TAIL_MARKER"));
        assert!(buf.find_and_advance(b"TAIL_MARKER").is_some());

        // Patterns split across consecutive pipe chunks still match.
        buf.push_chunk(b"SPLIT_");
        assert!(buf.find_and_advance(b"SPLIT_READY").is_none());
        buf.push_chunk(b"READY_OK");
        assert_eq!(
            buf.find_and_advance(b"SPLIT_READY").as_deref(),
            Some("SPLIT_READY_OK")
        );
    }

    #[test]
    fn unix_pgrp_parsers_ignore_zombie_members_and_handle_paren_comm() {
        assert_eq!(
            parse_proc_stat_pgrp_and_state("6624 (my ) helper) S 1 6624 6624 0 -1"),
            Some((6624, 'S'))
        );
        let (_, zstate) =
            parse_proc_stat_pgrp_and_state("6625 (defunct) Z 1 6624 6624 0 -1").unwrap();
        assert!(is_dead_or_zombie_state(zstate));

        let ps_out = "  6625  6624 Z+\n  7001  7000 S\n";
        assert!(ps_output_pgid_members(ps_out, 6624, 9999).is_empty());
        assert_eq!(ps_output_pgid_members(ps_out, 7000, 9999), vec![7001]);
    }
}
