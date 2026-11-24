//! Synchronous newline-delimited JSON-RPC 2.0 transport for one plugin.
//!
//! One child process is started per explicit `--plugin` path. Dedicated writer
//! and reader threads own standard input and output so each request enforces a
//! single deadline across both writing the request and waiting for the answer.
//! A timeout or a broken pipe terminates the child instead of leaving a
//! process behind.

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Default budget for one request, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Environment variable that overrides [`DEFAULT_TIMEOUT_MS`].
pub const TIMEOUT_ENV: &str = "DA_PLUGIN_TIMEOUT_MS";

/// Why one JSON-RPC request did not return a result.
#[derive(Debug, Clone)]
pub enum CallError {
    /// The plugin did not answer before the deadline; its process was killed.
    Timeout(String),
    /// The plugin could not be started, exited, broke the protocol or the pipe.
    Environment(String),
    /// The plugin answered with a JSON-RPC error object.
    Remote { code: i64, message: String },
}

impl CallError {
    /// The message shown to the user.
    pub fn message(&self) -> &str {
        match self {
            Self::Timeout(message) | Self::Environment(message) | Self::Remote { message, .. } => {
                message
            }
        }
    }
}

type WriteRequest = (String, Sender<io::Result<()>>);

/// One running plugin executable.
pub struct PluginClient {
    name: String,
    child: Option<Child>,
    requests: Option<Sender<WriteRequest>>,
    writer: Option<JoinHandle<()>>,
    replies: Receiver<Result<String, String>>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
    timeout: Duration,
    dead: bool,
}

impl PluginClient {
    /// Starts `path` with piped standard input and output.
    pub fn spawn(path: &Path, name: &str) -> Result<Self, String> {
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                format!(
                    "could not start plugin `{name}` from `{}`: {error}",
                    path.display()
                )
            })?;
        let Some(stdin) = child.stdin.take() else {
            kill_and_wait(&mut child);
            return Err(format!(
                "plugin `{name}` did not expose a standard-input pipe"
            ));
        };
        let Some(stdout) = child.stdout.take() else {
            kill_and_wait(&mut child);
            return Err(format!(
                "plugin `{name}` did not expose a standard-output pipe"
            ));
        };

        let (requests_tx, requests_rx) = mpsc::channel::<WriteRequest>();
        let writer = match thread::Builder::new()
            .name(format!("da-plugin-{name}-writer"))
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok((line, done_tx)) = requests_rx.recv() {
                    let result = stdin
                        .write_all(line.as_bytes())
                        .and_then(|()| stdin.flush());
                    let failed = result.is_err();
                    let _ = done_tx.send(result);
                    if failed {
                        return;
                    }
                }
            }) {
            Ok(writer) => writer,
            Err(error) => {
                kill_and_wait(&mut child);
                return Err(format!(
                    "could not start the writer for plugin `{name}`: {error}"
                ));
            }
        };

        let (sender, replies) = mpsc::channel();
        let reader = match thread::Builder::new()
            .name(format!("da-plugin-{name}"))
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    match line {
                        Ok(line) => {
                            if sender.send(Ok(line)).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(Err(error.to_string()));
                            return;
                        }
                    }
                }
                let _ = sender.send(Err("the plugin closed its standard output".to_string()));
            }) {
            Ok(reader) => reader,
            Err(error) => {
                drop(requests_tx);
                kill_and_wait(&mut child);
                let _ = writer.join();
                return Err(format!(
                    "could not start the reader for plugin `{name}`: {error}"
                ));
            }
        };

        Ok(Self {
            name: name.to_string(),
            child: Some(child),
            requests: Some(requests_tx),
            writer: Some(writer),
            replies,
            reader: Some(reader),
            next_id: 1,
            timeout: timeout_from_env(),
            dead: false,
        })
    }

    /// Sends one request and waits for the response with the same id.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        if self.dead {
            return Err(CallError::Environment(format!(
                "plugin `{}` is no longer running",
                self.name
            )));
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&request).map_err(|error| {
            CallError::Environment(format!(
                "could not encode the request to plugin `{}`: {error}",
                self.name
            ))
        })?;
        line.push('\n');

        let deadline = Instant::now() + self.timeout;
        self.write_request(method, line, deadline)?;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.timed_out(method));
            }
            match self.replies.recv_timeout(remaining) {
                Ok(Ok(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let message: Value = match serde_json::from_str(&line) {
                        Ok(message) => message,
                        Err(error) => {
                            return Err(self.fail_environment(format!(
                                "plugin `{}` sent malformed JSON for `{method}`: {error}",
                                self.name
                            )));
                        }
                    };
                    match message.get("id").and_then(Value::as_u64) {
                        Some(response_id) if response_id == id => {
                            if let Some(error) = message.get("error") {
                                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                                let text = error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("the plugin reported an unspecified error")
                                    .to_string();
                                return Err(CallError::Remote {
                                    code,
                                    message: text,
                                });
                            }
                            return message.get("result").cloned().ok_or_else(|| {
                                CallError::Environment(format!(
                                    "plugin `{}` answered `{method}` without a result",
                                    self.name
                                ))
                            });
                        }
                        _ => continue,
                    }
                }
                Ok(Err(error)) => {
                    return Err(self.fail_environment(format!(
                        "plugin `{}` stopped while answering `{method}`: {error}",
                        self.name
                    )));
                }
                Err(RecvTimeoutError::Timeout) => return Err(self.timed_out(method)),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(self.fail_environment(format!(
                        "plugin `{}` stopped before answering `{method}`",
                        self.name
                    )));
                }
            }
        }
    }

    fn write_request(
        &mut self,
        method: &str,
        line: String,
        deadline: Instant,
    ) -> Result<(), CallError> {
        let (write_done_tx, write_done_rx) = mpsc::channel();
        let Some(requests) = self.requests.as_ref() else {
            return Err(self.fail_environment(format!(
                "plugin `{}` has no standard-input writer",
                self.name
            )));
        };
        if requests.send((line, write_done_tx)).is_err() {
            return Err(self.fail_environment(format!(
                "could not write `{method}` to plugin `{}`: the writer stopped",
                self.name
            )));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(self.timed_out(method));
        }
        match write_done_rx.recv_timeout(remaining) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(self.fail_environment(format!(
                "could not write `{method}` to plugin `{}`: {error}",
                self.name
            ))),
            Err(RecvTimeoutError::Timeout) => Err(self.timed_out(method)),
            Err(RecvTimeoutError::Disconnected) => Err(self.fail_environment(format!(
                "could not write `{method}` to plugin `{}`: the writer disconnected",
                self.name
            ))),
        }
    }

    fn fail_environment(&mut self, message: String) -> CallError {
        self.terminate();
        CallError::Environment(message)
    }

    fn timed_out(&mut self, method: &str) -> CallError {
        let millis = self.timeout.as_millis();
        let message = format!(
            "plugin `{}` did not answer `{method}` within {millis} ms; the process was terminated",
            self.name
        );
        self.terminate();
        CallError::Timeout(message)
    }

    fn terminate(&mut self) {
        self.dead = true;
        // Close the request channel, then kill and reap the child before
        // joining worker threads so blocked stdin writes fail with BrokenPipe
        // and stdout reads see EOF immediately.
        self.requests.take();
        if let Some(mut child) = self.child.take() {
            kill_and_wait(&mut child);
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn kill_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for PluginClient {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// The per-request deadline, read from [`TIMEOUT_ENV`] when it names a positive
/// number of milliseconds.
pub fn timeout_from_env() -> Duration {
    std::env::var(TIMEOUT_ENV)
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(DEFAULT_TIMEOUT_MS))
}
