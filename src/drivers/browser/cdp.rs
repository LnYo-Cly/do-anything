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

/// How long a collection waits for a message that has not arrived yet.
///
/// The observer reads the socket without waiting for the browser, so a step
/// does not pay a full [`READ_POLL`] when nothing is pending. `tungstenite`
/// keeps a half-read frame across a timed-out read, so this short wait cannot
/// corrupt the connection.
const IMMEDIATE_POLL: Duration = Duration::from_millis(1);

/// How many messages a connection keeps while it waits for a reply.
///
/// A case may run for minutes and a page may emit far more events than any step
/// reads. The queue is bounded so that the oldest messages are discarded and
/// counted instead of the process growing without limit.
const MAX_QUEUED: usize = 10_000;

/// Maximum time one non-blocking poll spends draining the socket.
///
/// A busy page can send events faster than a 1 ms read times out, so without a
/// budget [`Connection::poll_messages`] could stay inside one step far longer
/// than the step's own deadline. Once the budget is spent the poll returns and
/// whatever is still unread stays on the socket for the next collection.
const POLL_BUDGET: Duration = Duration::from_millis(200);

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

    /// Returns a deadline no later than `cap` from now, preserving an earlier
    /// outer deadline.
    pub fn capped(self, cap: Duration) -> Self {
        let capped_at = Instant::now() + cap;
        if capped_at < self.at {
            Self {
                at: capped_at,
                timeout: cap,
            }
        } else {
            self
        }
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
    /// A browser event showed the target or session ended while waiting.
    Aborted { waiting_for: String },
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
            Self::Aborted { waiting_for } => {
                write!(formatter, "`{waiting_for}` was aborted by a browser event")
            }
            Self::Transport(message) => write!(formatter, "{message}"),
        }
    }
}

/// Messages received while waiting for a reply, oldest first and bounded.
struct MessageQueue {
    messages: VecDeque<Value>,
    /// How many messages were discarded to keep the queue at [`MAX_QUEUED`].
    dropped: u64,
}

impl MessageQueue {
    /// Creates an empty queue.
    fn new() -> Self {
        Self {
            messages: VecDeque::new(),
            dropped: 0,
        }
    }

    /// Appends one message, discarding the oldest when the queue is full.
    fn push(&mut self, message: Value) {
        if self.messages.len() == MAX_QUEUED {
            self.messages.pop_front();
            self.dropped += 1;
        }
        self.messages.push_back(message);
    }

    /// Removes and returns the first message a call named by `method`.
    fn take(&mut self, method: &str) -> Option<Value> {
        let index = self
            .messages
            .iter()
            .position(|message| message.get("method").and_then(Value::as_str) == Some(method))?;
        self.messages.remove(index)
    }

    /// Takes every message received so far.
    fn take_all(&mut self) -> Vec<Value> {
        self.messages.drain(..).collect()
    }

    /// How many messages were discarded since the last call, resetting it.
    fn take_dropped(&mut self) -> u64 {
        std::mem::take(&mut self.dropped)
    }

    /// Whether any currently queued message satisfies `predicate`.
    fn any<F>(&self, predicate: F) -> bool
    where
        F: FnMut(&Value) -> bool,
    {
        self.messages.iter().any(predicate)
    }
}

/// One connection to a browser's DevTools WebSocket.
pub struct Connection {
    socket: WebSocket<TcpStream>,
    next_id: u64,
    /// Messages received while waiting for a reply, in arrival order.
    queued: MessageQueue,
}

/// Result of evaluating a script that returns either a DOM node handle or a JSON value.
pub enum RemoteResult {
    /// A remote object handle (`objectId`).
    Object(String),
    /// A primitive JSON value returned directly.
    Value(Value),
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
            queued: MessageQueue::new(),
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
        self.call_until(method, params, session_id, deadline, |_| false)
    }

    /// Sends one command and returns its `result`, aborting early if any
    /// already-queued or incoming event satisfies `should_abort`.
    ///
    /// Any matching event stays in the queue so callers can inspect it for
    /// diagnostics.
    pub fn call_until<F>(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        deadline: Deadline,
        mut should_abort: F,
    ) -> Result<Value, CdpError>
    where
        F: FnMut(&Value) -> bool,
    {
        if self.queued.any(&mut should_abort) {
            return Err(CdpError::Aborted {
                waiting_for: method.to_string(),
            });
        }
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
            let abort = should_abort(&message);
            self.queued.push(message);
            if abort {
                return Err(CdpError::Aborted {
                    waiting_for: method.to_string(),
                });
            }
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

    /// Evaluates a JavaScript expression and returns either a remote `objectId`
    /// (for a DOM node or object) or a primitive JSON value.
    pub fn evaluate_remote(
        &mut self,
        session_id: &str,
        expression: &str,
        deadline: Deadline,
    ) -> Result<RemoteResult, CdpError> {
        let reply = self.call(
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": false,
                "awaitPromise": false,
            }),
            Some(session_id),
            deadline,
        )?;
        parse_remote_result(&reply)
    }

    /// Calls a JavaScript function with a DOM node as `this` and returns either
    /// a remote `objectId` or a primitive JSON value.
    pub fn call_function_remote(
        &mut self,
        session_id: &str,
        object_id: &str,
        function: &str,
        deadline: Deadline,
    ) -> Result<RemoteResult, CdpError> {
        let reply = self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "functionDeclaration": function,
                "returnByValue": false,
                "awaitPromise": false,
            }),
            Some(session_id),
            deadline,
        )?;
        parse_remote_result(&reply)
    }

    /// Calls a JavaScript function with `this` and one `objectId` argument, returning a value.
    pub fn call_function_with_object(
        &mut self,
        session_id: &str,
        object_id: &str,
        arg_object_id: &str,
        function: &str,
        deadline: Deadline,
    ) -> Result<Value, CdpError> {
        let reply = self.call(
            "Runtime.callFunctionOn",
            json!({
                "objectId": object_id,
                "arguments": [{ "objectId": arg_object_id }],
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
            if let Some(event) = self.queued.take(method) {
                return Ok(event);
            }
            let message = self.read(method, deadline)?;
            self.queued.push(message);
        }
    }

    /// Takes every message received so far that no call consumed.
    ///
    /// `browser.open` clears stale events before it navigates; the network
    /// observer reads the same queue for page events.
    pub fn take_messages(&mut self) -> Vec<Value> {
        self.queued.take_all()
    }

    /// Puts messages back, in order, after a caller read and kept them.
    ///
    /// The bounded queue may still discard its oldest message, which is what
    /// makes a long wait finite.
    pub fn restore_messages(&mut self, messages: impl IntoIterator<Item = Value>) {
        for message in messages {
            self.queued.push(message);
        }
    }

    /// How many messages the queue discarded to stay within its bound.
    ///
    /// The count is read and reset, so every step reports only its own drops.
    pub fn take_dropped(&mut self) -> u64 {
        self.queued.take_dropped()
    }

    /// Reads what has already arrived without waiting for more.
    ///
    /// The observer calls this after a step, so a response that arrived while
    /// no command was waiting is part of that step's observation. The socket
    /// timeout is shortened for the duration and restored afterwards.
    pub fn poll_messages(&mut self) -> Result<(), CdpError> {
        self.socket
            .get_mut()
            .set_read_timeout(Some(IMMEDIATE_POLL))
            .map_err(|error| {
                CdpError::Transport(format!("could not configure the connection: {error}"))
            })?;
        let outcome = self.poll_available();
        let restored = self.socket.get_mut().set_read_timeout(Some(READ_POLL));
        match (outcome, restored) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(CdpError::Transport(format!(
                "could not configure the connection: {error}"
            ))),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Reads messages until the socket has none left or the poll budget is spent.
    fn poll_available(&mut self) -> Result<(), CdpError> {
        let deadline = Deadline::after(POLL_BUDGET);
        loop {
            if deadline.passed() {
                return Ok(());
            }
            match self.socket.read() {
                Ok(Message::Text(text)) => {
                    let value = parse_message(text.as_str())?;
                    self.queued.push(value);
                }
                // Pings are answered by tungstenite on the next read or write,
                // and binary frames are not part of CDP.
                Ok(Message::Binary(_)) | Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
                Ok(Message::Close(_)) => {
                    return Err(CdpError::Transport(
                        "the browser closed the connection while its events were read".to_string(),
                    ));
                }
                Ok(Message::Frame(_)) => {}
                Err(WebSocketError::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(());
                }
                Err(WebSocketError::ConnectionClosed | WebSocketError::AlreadyClosed) => {
                    return Err(CdpError::Transport(
                        "the browser closed the connection while its events were read".to_string(),
                    ));
                }
                Err(error) => {
                    return Err(CdpError::Transport(format!(
                        "the browser connection failed while its events were read: {error}"
                    )));
                }
            }
        }
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
                Ok(Message::Text(text)) => return parse_message(text.as_str()),
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

/// The loopback address a `ws://` URL names; any other host is refused.
///
/// Chrome always prints `ws://127.0.0.1:<port>/...` in `DevToolsActivePort`;
/// parsing it here keeps the connection independent of a URL helper crate.
fn socket_address(url: &str) -> Result<SocketAddr, CdpError> {
    let (host, port) = split_ws_url(url)?;
    let address = format!("{host}:{port}");
    address
        .to_socket_addrs()
        .map_err(|error| CdpError::Transport(format!("`{url}` is not a usable address: {error}")))?
        // A debug port is full control of a browser: whatever a DevTools
        // endpoint answers, the connection stays on this machine.
        .find(|address| address.ip().is_loopback())
        .ok_or_else(|| {
            CdpError::Transport(format!(
                "`{url}` is not a loopback address; only a browser on this machine is used"
            ))
        })
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

/// Extracts either a remote `objectId` or a primitive value from a Runtime reply.
fn parse_remote_result(reply: &Value) -> Result<RemoteResult, CdpError> {
    if let Some(details) = reply.get("exceptionDetails") {
        return Err(CdpError::Script(exception_message(details)));
    }
    let result = reply.get("result");
    if let Some(object_id) = result
        .and_then(|value| value.get("objectId"))
        .and_then(Value::as_str)
    {
        return Ok(RemoteResult::Object(object_id.to_string()));
    }
    Ok(RemoteResult::Value(
        result
            .and_then(|value| value.get("value"))
            .cloned()
            .unwrap_or(Value::Null),
    ))
}

/// Parses one text frame as a CDP message.
fn parse_message(text: &str) -> Result<Value, CdpError> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        CdpError::Transport(format!(
            "the browser sent a message that is not JSON: {error}"
        ))
    })?;
    if !value.is_object() {
        return Err(CdpError::Transport(
            "the browser sent a message that is not a JSON object".to_string(),
        ));
    }
    Ok(value)
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
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use super::*;

    /// Stops a mock server thread even when a test assertion panics.
    struct StopServer(Arc<AtomicBool>);

    impl Drop for StopServer {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    /// Encodes one short unmasked server-to-client WebSocket text frame.
    fn ws_text_frame(payload: &str) -> Vec<u8> {
        assert!(payload.len() < 126, "the helper only encodes short frames");
        let mut frame = Vec::with_capacity(2 + payload.len());
        frame.push(0x81);
        frame.push(payload.len() as u8);
        frame.extend_from_slice(payload.as_bytes());
        frame
    }

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
    fn only_a_loopback_websocket_address_is_used() {
        assert!(socket_address("ws://127.0.0.1:9222/devtools/browser/a").is_ok());
        assert!(socket_address("ws://[::1]:9222/devtools/browser/a").is_ok());
        // A DevTools endpoint could name another host; a debug port is full
        // control of a browser, so the connection must stay on this machine.
        assert!(socket_address("ws://192.0.2.1:9222/devtools/browser/a").is_err());
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
    fn the_message_queue_keeps_its_bound_and_counts_what_it_dropped() {
        let mut queue = MessageQueue::new();
        for index in 0..MAX_QUEUED + 3 {
            queue.push(json!({ "method": "Network.loadingFinished", "index": index }));
        }

        let messages = queue.take_all();
        assert_eq!(
            messages.len(),
            MAX_QUEUED,
            "the queue must not grow past its bound"
        );
        assert_eq!(
            messages.first().map(|message| message["index"].clone()),
            Some(json!(3)),
            "the oldest messages must be the ones discarded"
        );
        assert_eq!(
            messages.last().map(|message| message["index"].clone()),
            Some(json!(MAX_QUEUED + 2)),
            "the newest message must be kept"
        );
        assert_eq!(queue.take_dropped(), 3);
        assert_eq!(
            queue.take_dropped(),
            0,
            "reading the count must reset it for the next step"
        );
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

    #[test]
    fn poll_messages_returns_within_its_budget_while_events_keep_arriving() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("must bind a mock CDP port");
        let port = listener
            .local_addr()
            .expect("must read the mock port")
            .port();
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        // The guard stops the server even when an assertion below panics, so a
        // failure cannot leave a thread writing into a socket forever.
        let _guard = StopServer(Arc::clone(&stop));
        let server_stop = Arc::clone(&stop);
        let server_pause = Arc::clone(&pause);
        let server_started = Arc::clone(&started);

        let server = thread::spawn(move || {
            let (stream, _) = listener
                .accept()
                .expect("the mock CDP must accept a connection");
            stream
                .set_nodelay(true)
                .expect("must disable Nagle on the mock CDP");
            let mut socket = tungstenite::accept(stream).expect("the mock handshake must succeed");
            let payload = json!({
                "method": "Network.loadingFinished",
                "params": { "requestId": "mock-request" }
            })
            .to_string();
            let frame = ws_text_frame(&payload);
            // One write syscall carries a whole batch, so the server stays ahead
            // of a client that parses every frame and the socket never runs dry
            // while the test polls.
            let mut batch = Vec::with_capacity(frame.len() * 64);
            for _ in 0..64 {
                batch.extend_from_slice(&frame);
            }
            let limit = Instant::now() + Duration::from_secs(10);
            while !server_stop.load(Ordering::Relaxed) && Instant::now() < limit {
                if server_pause.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                if socket.get_mut().write_all(&batch).is_err() {
                    return;
                }
                server_started.store(true, Ordering::Release);
            }
        });

        let url = format!("ws://127.0.0.1:{port}/devtools/page/1");
        let mut connection = Connection::connect(&url, Deadline::after(Duration::from_secs(5)))
            .expect("must connect to the mock CDP");

        let wait_limit = Instant::now() + Duration::from_secs(5);
        while !started.load(Ordering::Acquire) && Instant::now() < wait_limit {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            started.load(Ordering::Acquire),
            "the mock server did not start sending"
        );

        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let begin = Instant::now();
            let outcome = connection.poll_messages();
            let _ = tx.send((outcome, begin.elapsed(), connection));
        });

        // The timeout is the point of the test: without a poll budget this
        // receive fails instead of the suite hanging on a page that never stops.
        let (outcome, elapsed, mut connection) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("`poll_messages` did not return while events kept arriving");
        worker.join().expect("the polling thread must finish");
        outcome.expect("`poll_messages` must succeed");

        assert!(
            elapsed >= POLL_BUDGET / 2 && elapsed < POLL_BUDGET * 4,
            "`poll_messages` took {elapsed:?} with a budget of {POLL_BUDGET:?}"
        );
        let timeout = connection
            .socket
            .get_ref()
            .read_timeout()
            .expect("must read the socket timeout");
        assert_eq!(
            timeout,
            Some(READ_POLL),
            "`poll_messages` must restore the read timeout"
        );

        let messages = connection.take_messages();
        assert!(
            !messages.is_empty() && messages.len() <= MAX_QUEUED,
            "expected a bounded batch of messages, got {}",
            messages.len()
        );

        // Stop new events while keeping the socket open: what the budget left
        // unread must still be there for the next collection.
        pause.store(true, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(5));
        connection
            .poll_messages()
            .expect("the second poll must succeed");
        assert!(
            !connection.take_messages().is_empty(),
            "messages left on the socket must remain available to the next read"
        );

        stop.store(true, Ordering::Relaxed);
        drop(connection);
        server.join().expect("the mock server thread must join");
    }

    #[test]
    fn deadline_capped_never_extends_outer_budget() {
        let short = Deadline::after(Duration::from_millis(80));
        let capped_by_short = short.capped(Duration::from_secs(5));
        assert_eq!(capped_by_short.timeout_ms(), 80);

        let long = Deadline::after(Duration::from_secs(30));
        let capped_by_cap = long.capped(Duration::from_millis(200));
        assert_eq!(capped_by_cap.timeout_ms(), 200);
    }

    #[test]
    fn call_until_aborts_immediately_on_queued_or_arriving_event() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("must bind mock CDP port");
        let port = listener
            .local_addr()
            .expect("must read mock CDP port")
            .port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("mock CDP must accept");
            let mut socket = tungstenite::accept(stream).expect("mock handshake must succeed");
            // First command receives an aborting event without ever answering.
            let first = match socket.read().expect("read first command") {
                Message::Text(text) => serde_json::from_str::<Value>(text.as_str()).expect("json"),
                other => panic!("unexpected frame: {other:?}"),
            };
            assert_eq!(first["method"], "Page.enable");
            socket
                .send(Message::text(
                    json!({
                        "method": "Inspector.targetCrashed",
                        "sessionId": "s-1"
                    })
                    .to_string(),
                ))
                .expect("send aborting event");
            while socket.read().is_ok() {}
        });

        let url = format!("ws://127.0.0.1:{port}/devtools/browser/test");
        let mut conn =
            Connection::connect(&url, Deadline::after(Duration::from_secs(2))).expect("connect");

        let started = Instant::now();
        let err = conn
            .call_until(
                "Page.enable",
                json!({}),
                Some("s-1"),
                Deadline::after(Duration::from_secs(5)),
                |msg| msg.get("method").and_then(Value::as_str) == Some("Inspector.targetCrashed"),
            )
            .expect_err("must abort on matching event");
        assert!(
            matches!(err, CdpError::Aborted { .. }),
            "expected Aborted, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "abort must be fast, took {:?}",
            started.elapsed()
        );

        // Calling again while the event is still queued must abort immediately without sending.
        let second = conn
            .call_until(
                "Runtime.enable",
                json!({}),
                Some("s-1"),
                Deadline::after(Duration::from_secs(5)),
                |msg| msg.get("method").and_then(Value::as_str) == Some("Inspector.targetCrashed"),
            )
            .expect_err("must abort on already-queued event");
        assert!(matches!(second, CdpError::Aborted { .. }));
        assert_eq!(conn.take_messages().len(), 1);

        drop(conn);
        server.join().expect("server thread must finish");
    }
}
