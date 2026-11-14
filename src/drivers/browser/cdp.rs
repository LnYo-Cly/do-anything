//! A minimal synchronous client for the Chrome DevTools Protocol.
//!
//! Chrome speaks CDP as JSON messages over one WebSocket: a command carries an
//! `id` and the reply carries the same `id`, while events arrive in between
//! without one. This client sends one command at a time and waits for its
//! reply, keeping every other message for the caller so a later step can read
//! the events of S3-T3.
//!
//! The client is synchronous because the rest of the engine is: `tungstenite`
//! gives a blocking `WebSocket` over a `TcpStream`, and a read timeout only ends
//! one poll of a deadline. `tungstenite` keeps a half-read frame in its own
//! buffer, so a timed-out read does not corrupt the connection.

use std::collections::VecDeque;
use std::fmt;
use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::Error as WebSocketError;
use tungstenite::Message;
use tungstenite::protocol::WebSocket;

/// How long one socket read waits before the deadline is checked again.
const READ_POLL: Duration = Duration::from_millis(100);

/// The instant one step must finish by.
#[derive(Clone, Copy, Debug)]
pub struct Deadline {
    at: Instant,
    timeout: Duration,
}

impl Deadline {
    /// Starts a budget of `timeout` from now.
    pub fn after(timeout: Duration) -> Self {
        Self {
            at: Instant::now() + timeout,
            timeout,
        }
    }

    /// How long is left, or `None` once the budget is spent.
    fn remaining(self) -> Option<Duration> {
        let now = Instant::now();
        if now >= self.at {
            None
        } else {
            Some(self.at - now)
        }
    }

    /// Whether the budget is already spent.
    pub fn passed(self) -> bool {
        self.remaining().is_none()
    }

    /// The budget in milliseconds, as the step arguments express it.
    pub fn timeout_ms(self) -> u128 {
        self.timeout.as_millis()
    }
}

/// Why a CDP command did not return a result.
#[derive(Debug)]
pub enum CdpError {
    /// The browser answered with a protocol error.
    Method { method: String, message: String },
    /// A page script raised an exception, for example a bad selector.
    Script(String),
    /// The deadline passed before the expected message arrived.
    Timeout {
        waiting_for: String,
        timeout: Duration,
    },
    /// The connection failed, closed, or carried an unreadable message.
    Transport(String),
}

impl fmt::Display for CdpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Method { method, message } => {
                write!(formatter, "`{method}` failed: {message}")
            }
            Self::Script(message) => write!(formatter, "the page script failed: {message}"),
            Self::Timeout {
                waiting_for,
                timeout,
            } => write!(
                formatter,
                "`{waiting_for}` did not answer within {} ms",
                timeout.as_millis()
            ),
            Self::Transport(message) => write!(formatter, "{message}"),
        }
    }
}

/// One connection to a browser's DevTools WebSocket.
pub struct Connection {
    socket: WebSocket<TcpStream>,
    next_id: u64,
    /// Messages received while waiting for a reply, in arrival order.
    queued: VecDeque<Value>,
}

impl Connection {
    /// Connects to `url`, a `ws://` URL that the browser printed.
    pub fn connect(url: &str, deadline: Deadline) -> Result<Self, CdpError> {
        let address = socket_address(url)?;
        let remaining = deadline.remaining().ok_or_else(|| CdpError::Timeout {
            waiting_for: "the DevTools connection".to_string(),
            timeout: deadline.timeout,
        })?;
        let stream = TcpStream::connect_timeout(&address, remaining).map_err(|error| {
            CdpError::Transport(format!("could not connect to {address}: {error}"))
        })?;
        stream.set_nodelay(true).map_err(|error| {
            CdpError::Transport(format!("could not configure the connection: {error}"))
        })?;
        // The handshake may wait for the browser's HTTP response, so it gets the
        // whole remaining budget. Once connected, reads poll in short slices so
        // every step can honour its own deadline.
        stream.set_read_timeout(Some(remaining)).map_err(|error| {
            CdpError::Transport(format!("could not configure the connection: {error}"))
        })?;
        stream.set_write_timeout(Some(remaining)).map_err(|error| {
            CdpError::Transport(format!("could not configure the connection: {error}"))
        })?;

        let (mut socket, _response) = tungstenite::client(url, stream).map_err(|error| {
            CdpError::Transport(format!("could not start the WebSocket connection: {error}"))
        })?;
        socket
            .get_mut()
            .set_read_timeout(Some(READ_POLL))
            .map_err(|error| {
                CdpError::Transport(format!("could not configure the connection: {error}"))
            })?;
        Ok(Self {
            socket,
            next_id: 1,
            queued: VecDeque::new(),
        })
    }

    /// Sends one command and returns its `result`.
    ///
    /// `session_id` addresses a page attached with `Target.attachToTarget`; the
    /// browser itself takes `None`.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        deadline: Deadline,
    ) -> Result<Value, CdpError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(method, id, params, session_id)?;

        loop {
            let message = self.read(method, deadline)?;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(error) = message.get("error") {
                    return Err(CdpError::Method {
                        method: method.to_string(),
                        message: describe_error(error),
                    });
                }
                return Ok(message.get("result").cloned().unwrap_or(Value::Null));
            }
            self.queued.push_back(message);
        }
    }

    /// Evaluates a JavaScript expression in the page and returns its value.
    ///
    /// The expression must produce a JSON value; `returnByValue` makes Chrome
    /// serialize it instead of returning an object handle.
    pub fn evaluate(
        &mut self,
        session_id: &str,
        expression: &str,
        deadline: Deadline,
    ) -> Result<Value, CdpError> {
        let reply = self.call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": false,
            }),
            Some(session_id),
            deadline,
        )?;
        if let Some(details) = reply.get("exceptionDetails") {
            return Err(CdpError::Script(exception_message(details)));
        }
        Ok(reply
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Calls a JavaScript function with a DOM node as `this`.
    ///
    /// The accessibility tree names a node by its backend id; after
    /// `DOM.resolveNode` this form runs the action on that exact element rather
    /// than searching for it again by a selector.
    pub fn call_function(
        &mut self,
        session_id: &str,
        object_id: &str,
        function: &str,
        deadline: Deadline,
    ) -> Result<Value, CdpError> {
        let reply = self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": function,
                "returnByValue": true,
                "awaitPromise": false,
            }),
            Some(session_id),
            deadline,
        )?;
        if let Some(details) = reply.get("exceptionDetails") {
            return Err(CdpError::Script(exception_message(details)));
        }
        Ok(reply
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Waits for one event and returns the whole CDP message.
    pub fn wait_for_event(&mut self, method: &str, deadline: Deadline) -> Result<Value, CdpError> {
        loop {
            if let Some(index) = self
                .queued
                .iter()
                .position(|message| message.get("method").and_then(Value::as_str) == Some(method))
                && let Some(event) = self.queued.remove(index)
            {
                return Ok(event);
            }
            let message = self.read(method, deadline)?;
            self.queued.push_back(message);
        }
    }

    /// Takes every message received so far that no call consumed.
    ///
    /// `browser.open` clears stale events before it navigates; S3-T3 reads the
    /// same queue for network and page events.
    pub fn take_messages(&mut self) -> Vec<Value> {
        self.queued.drain(..).collect()
    }

    /// Writes one CDP command.
    fn write(
        &mut self,
        method: &str,
        id: u64,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<(), CdpError> {
        let mut command = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session_id {
            command["sessionId"] = Value::String(session.to_string());
        }
        self.socket
            .send(Message::text(command.to_string()))
            .map_err(|error| {
                CdpError::Transport(format!("could not send `{method}` to the browser: {error}"))
            })
    }

    /// Reads one CDP message, polling the deadline in short slices.
    fn read(&mut self, waiting_for: &str, deadline: Deadline) -> Result<Value, CdpError> {
        loop {
            if deadline.passed() {
                return Err(CdpError::Timeout {
                    waiting_for: waiting_for.to_string(),
                    timeout: deadline.timeout,
                });
            }
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let value: Value = serde_json::from_str(text.as_str()).map_err(|error| {
                        CdpError::Transport(format!(
                            "the browser sent a message that is not JSON: {error}"
                        ))
                    })?;
                    if !value.is_object() {
                        return Err(CdpError::Transport(
                            "the browser sent a message that is not a JSON object".to_string(),
                        ));
                    }
                    return Ok(value);
                }
                // Pings are answered by tungstenite on the next read or write,
                // and binary frames are not part of CDP.
                Ok(Message::Binary(_)) | Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => {
                    return Err(CdpError::Transport(format!(
                        "the browser closed the connection while waiting for `{waiting_for}`"
                    )));
                }
                Ok(Message::Frame(_)) => {}
                Err(WebSocketError::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed) => {
                    return Err(CdpError::Transport(format!(
                        "the browser closed the connection while waiting for `{waiting_for}`"
                    )));
                }
                Err(error) => {
                    return Err(CdpError::Transport(format!(
                        "the browser connection failed while waiting for `{waiting_for}`: {error}"
                    )));
                }
            }
        }
    }
}

/// The loopback address a `ws://` URL names.
///
/// Chrome always prints `ws://127.0.0.1:<port>/...` in `DevToolsActivePort`;
/// parsing it here keeps the connection independent of a URL helper crate.
fn socket_address(url: &str) -> Result<SocketAddr, CdpError> {
    let (host, port) = split_ws_url(url)?;
    let address = format!("{host}:{port}");
    address
        .to_socket_addrs()
        .map_err(|error| CdpError::Transport(format!("`{url}` is not a usable address: {error}")))?
        .next()
        .ok_or_else(|| CdpError::Transport(format!("`{url}` did not resolve to an address")))
}

/// Splits a `ws://host:port/path` URL into its host and port.
fn split_ws_url(url: &str) -> Result<(&str, u16), CdpError> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| CdpError::Transport(format!("`{url}` is not a ws:// URL")))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| CdpError::Transport(format!("`{url}` does not name a host and a port")))?;
    let port = port
        .parse::<u16>()
        .map_err(|error| CdpError::Transport(format!("`{url}` has no usable port: {error}")))?;
    Ok((host, port))
}

/// Flattens a CDP `exceptionDetails` member into one message.
fn exception_message(details: &Value) -> String {
    details
        .get("exception")
        .and_then(|exception| exception.get("description"))
        .and_then(Value::as_str)
        .or_else(|| details.get("text").and_then(Value::as_str))
        .unwrap_or("the page script raised an exception")
        .to_string()
}

/// Flattens a CDP `error` member into one message.
fn describe_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the browser reported an error");
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{message} (code {code})"),
        None => message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_websocket_url_yields_its_host_and_port() {
        assert_eq!(
            split_ws_url("ws://127.0.0.1:41234/devtools/browser/abc").ok(),
            Some(("127.0.0.1", 41234))
        );
        assert_eq!(
            split_ws_url("ws://localhost:1/devtools/page/1").ok(),
            Some(("localhost", 1))
        );
    }

    #[test]
    fn a_url_without_a_ws_scheme_or_port_is_refused() {
        for url in [
            "wss://127.0.0.1:1/devtools",
            "http://127.0.0.1:1/",
            "ws://127.0.0.1/devtools",
            "ws://127.0.0.1:not-a-port/devtools",
        ] {
            assert!(
                split_ws_url(url).is_err(),
                "`{url}` must not be accepted as a plain ws:// address"
            );
        }
    }

    #[test]
    fn a_cdp_error_names_the_code_and_the_message() {
        let error = json!({"code": -32601, "message": "Method not found"});
        assert_eq!(describe_error(&error), "Method not found (code -32601)");
        assert_eq!(
            describe_error(&json!({"message": "No target"})),
            "No target"
        );
        assert_eq!(describe_error(&json!({})), "the browser reported an error");
    }

    #[test]
    fn a_deadline_reports_its_budget_and_expires() {
        let deadline = Deadline::after(Duration::from_millis(500));
        assert_eq!(deadline.timeout_ms(), 500);
        assert!(!deadline.passed(), "a fresh deadline must not be spent");

        let spent = Deadline::after(Duration::ZERO);
        assert!(spent.passed());
        assert!(spent.remaining().is_none());
    }
}
