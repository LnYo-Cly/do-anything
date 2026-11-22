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

    fn find_and_advance(&mut self, needle: &[u8]) -> Option<String> {
        let start = self.cursor.saturating_sub(self.base_offset) as usize;
        let slice = self.bytes.make_contiguous();
        let unconsumed = &slice[start..];
        let pos = find_subslice(unconsumed, needle)?;
        let text = String::from_utf8_lossy(unconsumed).into_owned();
        self.cursor = self.base_offset + (start + pos + needle.len()) as u64;
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

struct ManagedProcess {
    child: Child,
    stdin: Option<ChildStdin>,
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
        let stdin = child.stdin.take();
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

        loop {
            let (pipes_closed, found) = {
                let mut guard = self.streams.state.lock().map_err(|_| {
                    StepError::Environment(format!("stream lock poisoned for process `{name}`"))
                })?;

                let found = match stream {
                    OutputStream::Stdout => {
                        guard
                            .stdout
                            .find_and_advance(needle)
                            .map(|stdout| WaitMatch {
                                matched_stream: StreamKind::Stdout,
                                stdout,
                                stderr: String::new(),
                            })
                    }
                    OutputStream::Stderr => {
                        guard
                            .stderr
                            .find_and_advance(needle)
                            .map(|stderr| WaitMatch {
                                matched_stream: StreamKind::Stderr,
                                stdout: String::new(),
                                stderr,
                            })
                    }
                    OutputStream::Both => {
                        if let Some(stdout) = guard.stdout.find_and_advance(needle) {
                            let stderr = guard.stderr.unconsumed_text();
                            Some(WaitMatch {
                                matched_stream: StreamKind::Stdout,
                                stdout,
                                stderr,
                            })
                        } else if let Some(stderr) = guard.stderr.find_and_advance(needle) {
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
                    let (out, err) = self.snapshot_unconsumed();
                    let status_text = self
                        .exit_status
                        .and_then(|s| s.code())
                        .map_or_else(|| "signal".to_string(), |c| format!("exit code {c}"));
                    let snippet = output_excerpt(&out, &err);
                    return Err(StepError::Execution(format!(
                        "`{step_name}` for process `{name}` exited ({status_text}) before emitting `{pattern}`{snippet}"
                    )));
                }
            } else if pipes_closed {
                // Pipes reached EOF while child status hasn't been polled yet; keep looping briefly so exit status settles.
                thread::sleep(Duration::from_millis(5));
            }

            let now = Instant::now();
            if now >= deadline {
                let (out, err) = self.snapshot_unconsumed();
                let snippet = output_excerpt(&out, &err);
                return Err(StepError::Timeout(format!(
                    "`{step_name}` for process `{name}` did not observe `{pattern}` within {} ms{snippet}",
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
        drop(self.stdin.take());

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
        let pipes_closed = self.both_pipes_closed();
        let kill_result = terminate_process_tree(
            &mut self.child,
            &mut self.exit_status,
            pid,
            timeout,
            pipes_closed,
        );

        self.drain_pipes(DRAIN_TIMEOUT.min(timeout.max(Duration::from_millis(100))));
        self.stopped = true;

        kill_result.map_err(|error| format!("process `{name}` (pid {pid}): {error}"))?;
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
        let single_default = self.order.len() == 1 && self.order[0] == DEFAULT_NAME;
        let mut files = Vec::new();
        for name in &self.order {
            let Some(proc) = self.processes.get(name) else {
                continue;
            };
            let (stdout, stderr) = proc.snapshot_evidence();
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

fn run_start(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ProcStartArgs = parse(args).map_err(StepError::InvalidArguments)?;
    check_start(&args).map_err(StepError::InvalidArguments)?;

    let session = sessions
        .get_or_create::<ProcSession>(SESSION_KIND, || Ok(ProcSession::default()))
        .map_err(StepError::Environment)?;

    if let Some(existing) = session.processes.get_mut(&args.name)
        && existing.is_running()
    {
        return Err(StepError::Execution(format!(
            "process `{}` is already running; stop it with `proc.stop` first or choose another `name`",
            args.name
        )));
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

    let mut bytes_written = 0_usize;
    if let Some(text) = &args.input {
        let Some(stdin) = proc.stdin.as_mut() else {
            return Err(StepError::Execution(format!(
                "stdin of process `{}` is already closed",
                args.name
            )));
        };
        stdin.write_all(text.as_bytes()).map_err(|error| {
            StepError::Execution(format!(
                "could not write to stdin of process `{}`: {error}",
                args.name
            ))
        })?;
        stdin.flush().map_err(|error| {
            StepError::Execution(format!(
                "could not flush stdin of process `{}`: {error}",
                args.name
            ))
        })?;
        bytes_written = text.len();
    }

    if args.eof {
        drop(proc.stdin.take());
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

fn output_excerpt(stdout: &str, stderr: &str) -> String {
    let out = stdout.trim();
    let err = stderr.trim();
    match (out.is_empty(), err.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("; stdout: {}", truncate_excerpt(out)),
        (true, false) => format!("; stderr: {}", truncate_excerpt(err)),
        (false, false) => format!(
            "; stdout: {}; stderr: {}",
            truncate_excerpt(out),
            truncate_excerpt(err)
        ),
    }
}

fn truncate_excerpt(text: &str) -> String {
    const LIMIT: usize = 300;
    if text.chars().count() <= LIMIT {
        text.to_string()
    } else {
        let prefix: String = text.chars().take(LIMIT).collect();
        format!("{prefix}...")
    }
}

fn terminate_process_tree(
    child: &mut Child,
    exit_status: &mut Option<ExitStatus>,
    pid: u32,
    timeout: Duration,
    pipes_closed: bool,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        let _ = pipes_closed;
        terminate_unix_process_group(child, exit_status, pid, timeout)
    }
    #[cfg(windows)]
    {
        terminate_windows_process_tree(child, exit_status, pid, timeout, pipes_closed)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (pid, timeout, pipes_closed);
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
    let pgid = format!("-{pid}");
    let already_exited = exit_status.is_some()
        || matches!(child.try_wait(), Ok(Some(status)) if { *exit_status = Some(status); true });

    if !already_exited {
        let _ = run_unix_kill(&["-TERM", &pgid]);
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
    let _ = run_unix_kill(&["-KILL", &pgid]);
    if exit_status.is_none() {
        let _ = child.kill();
        *exit_status = child
            .wait()
            .map(Some)
            .map_err(|error| format!("could not wait for process exit: {error}"))?;
    }

    // Leader is now reaped; confirm no member remains in process group `pid`.
    let wait_deadline = Instant::now() + timeout.min(KILL_HELPER_TIMEOUT);
    while unix_process_group_alive(&pgid) {
        if Instant::now() >= wait_deadline {
            return Err("process group survived SIGKILL".to_string());
        }
        let _ = run_unix_kill(&["-KILL", &pgid]);
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(unix)]
fn run_unix_kill(args: &[&str]) -> std::io::Result<ExitStatus> {
    let bin = if std::path::Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else {
        "kill"
    };
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

#[cfg(unix)]
fn unix_process_group_alive(pgid: &str) -> bool {
    run_unix_kill(&["-0", pgid]).is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn terminate_windows_process_tree(
    child: &mut Child,
    exit_status: &mut Option<ExitStatus>,
    pid: u32,
    timeout: Duration,
    pipes_closed: bool,
) -> Result<(), String> {
    let already_exited = exit_status.is_some()
        || matches!(child.try_wait(), Ok(Some(status)) if { *exit_status = Some(status); true });

    if already_exited && pipes_closed {
        return Ok(());
    }

    let taskkill = windows_system_tool(&["taskkill.exe"], "taskkill");
    let pid_str = pid.to_string();
    let mut cmd = Command::new(&taskkill);
    cmd.args(["/PID", &pid_str, "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let taskkill_status = run_bounded_helper(cmd, timeout.min(KILL_HELPER_TIMEOUT));

    if !pipes_closed && (already_exited || !taskkill_status.is_some_and(|s| s.success())) {
        // If the parent already exited while a descendant still holds pipes
        // open, `taskkill /PID <parent>` reports not-found while `self.child`
        // still holds the parent's HANDLE (preventing PID reuse).
        kill_windows_orphaned_descendants(pid, timeout.max(Duration::from_secs(5)));
    }

    if exit_status.is_none() {
        let _ = child.kill();
        *exit_status = child
            .wait()
            .map(Some)
            .map_err(|error| format!("could not wait for process exit: {error}"))?;
    }
    Ok(())
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
fn run_bounded_helper(mut command: Command, budget: Duration) -> Option<ExitStatus> {
    let mut helper = command.spawn().ok()?;
    let deadline = Instant::now() + budget;
    loop {
        match helper.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => break,
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
    let _ = helper.kill();
    let _ = helper.wait();
    None
}

#[cfg(windows)]
fn kill_windows_orphaned_descendants(root_pid: u32, budget: Duration) {
    let powershell = windows_system_tool(
        &["WindowsPowerShell", "v1.0", "powershell.exe"],
        "powershell.exe",
    );
    let script = format!(
        "$procs = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId; \
         $queue = [System.Collections.Generic.Queue[uint32]]::new(); \
         $seen = [System.Collections.Generic.HashSet[uint32]]::new(); \
         $queue.Enqueue({root_pid}); \
         $targets = @(); \
         while ($queue.Count -gt 0) {{ \
           $p = $queue.Dequeue(); \
           foreach ($c in $procs | Where-Object {{ $_.ParentProcessId -eq $p }}) {{ \
             $cid = [uint32]$c.ProcessId; \
             if ($seen.Add($cid)) {{ $queue.Enqueue($cid); $targets += $cid }} \
           }} \
         }}; \
         foreach ($t in $targets) {{ Stop-Process -Id $t -Force -ErrorAction SilentlyContinue }}"
    );
    let mut cmd = Command::new(powershell);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = run_bounded_helper(cmd, budget);
}

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
}
