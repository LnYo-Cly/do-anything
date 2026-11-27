//! Opt-in loopback-only (`127.0.0.1`) CDP Screencast & human takeover bridge.
//!
//! Compiled only when the default-off Cargo feature `live-preview` is enabled,
//! so the default `da` binary surface and size stay unchanged.
//!
//! Security boundaries:
//! - Hardcoded to `127.0.0.1` (remote IDE setups must tunnel over SSH/TLS).
//! - Every HTTP and WebSocket request requires `Authorization: Bearer <token>`
//!   verified in constant time against a 256-bit per-session CSPRNG token.
//! - Requests carrying a non-loopback `Origin` header are refused with `403`.
//! - Screencast streaming and human input forwarding (`Input.dispatchMouseEvent`,
//!   `Input.dispatchKeyEvent`, `Input.insertText`) target strictly the session's
//!   owned CDP `target_id` persisted in `state.json`; arbitrary raw CDP methods
//!   are never exposed to clients.

use std::io::{self, Cursor, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tungstenite::Message;

use crate::drivers::browser::cdp::{Connection, Deadline};
use crate::session::{self, store};

/// Maximum HTTP request header size accepted on the loopback bridge.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// Maximum HTTP JSON request body size accepted on `/pause`.
const MAX_BODY_BYTES: usize = 8 * 1024;
/// Socket read/poll slice for responsive shutdown and bidirectional CDP relay.
const POLL_SLICE: Duration = Duration::from_millis(25);
/// Timeout for initial CDP connection / attach / screencast control calls.
const CDP_CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Timeout for forwarding a single human input event to CDP.
const CDP_INPUT_TIMEOUT: Duration = Duration::from_secs(2);

/// Configuration for [`LivePreviewBridge::start`].
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub sessions_dir: PathBuf,
    pub session_id: String,
    /// Loopback TCP port (`0` asks the OS for an ephemeral port on `127.0.0.1`).
    pub port: u16,
    /// Screencast image format: `"jpeg"` or `"png"`.
    pub format: String,
    /// JPEG compression quality (`1..=100`), ignored for PNG.
    pub quality: u8,
    /// Optional maximum screencast frame width in pixels.
    pub max_width: Option<u32>,
    /// Optional maximum screencast frame height in pixels.
    pub max_height: Option<u32>,
}

impl BridgeConfig {
    pub fn validate(&self) -> Result<(), String> {
        store::validate_session_id(&self.session_id)?;
        if !matches!(self.format.as_str(), "jpeg" | "png") {
            return Err(format!(
                "invalid screencast format `{}`; use `jpeg` or `png`",
                self.format
            ));
        }
        if self.quality == 0 || self.quality > 100 {
            return Err(format!(
                "invalid screencast quality {}; use 1..=100",
                self.quality
            ));
        }
        if self.max_width == Some(0) || self.max_height == Some(0) {
            return Err("`max_width` and `max_height` must be at least 1".to_string());
        }
        Ok(())
    }
}

/// Owned CDP endpoint extracted from a session's persisted `handles.browser`.
#[derive(Debug, Clone)]
struct OwnedBrowserTarget {
    websocket_url: String,
    target_id: String,
}

/// Shared runtime metrics and control state for one running bridge instance.
struct BridgeShared {
    session_dir: PathBuf,
    session_id: String,
    target: OwnedBrowserTarget,
    token: String,
    format: String,
    quality: u8,
    max_width: Option<u32>,
    max_height: Option<u32>,
    shutdown: AtomicBool,
    active_clients: AtomicUsize,
    frames_sent: AtomicU64,
}

/// Handle to a running loopback live-preview & human-takeover bridge.
pub struct LivePreviewBridge {
    addr: SocketAddr,
    shared: Arc<BridgeShared>,
    thread: Option<JoinHandle<()>>,
}

impl LivePreviewBridge {
    /// Starts a bearer-authenticated loopback bridge bound strictly to `127.0.0.1`.
    pub fn start(config: BridgeConfig) -> Result<Self, String> {
        config.validate()?;
        let session_dir = config.sessions_dir.join(&config.session_id);
        let target = load_owned_browser_target(&session_dir, &config.session_id)?;
        let token = generate_bearer_token();

        let bind_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, config.port));
        let listener = TcpListener::bind(bind_addr).map_err(|error| {
            format!("could not bind live-preview bridge on `{bind_addr}`: {error}")
        })?;
        let addr = listener.local_addr().map_err(|error| {
            format!("could not read live-preview bridge local address: {error}")
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("could not configure live-preview listener: {error}"))?;

        let shared = Arc::new(BridgeShared {
            session_dir,
            session_id: config.session_id,
            target,
            token,
            format: config.format,
            quality: config.quality,
            max_width: config.max_width,
            max_height: config.max_height,
            shutdown: AtomicBool::new(false),
            active_clients: AtomicUsize::new(0),
            frames_sent: AtomicU64::new(0),
        });
        let loop_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("da-live-preview".to_string())
            .spawn(move || accept_loop(listener, loop_shared))
            .map_err(|error| format!("could not spawn live-preview server thread: {error}"))?;

        Ok(Self {
            addr,
            shared,
            thread: Some(thread),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn http_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/ws", self.addr)
    }

    pub fn token(&self) -> &str {
        &self.shared.token
    }

    pub fn target_id(&self) -> &str {
        &self.shared.target.target_id
    }

    pub fn session_id(&self) -> &str {
        &self.shared.session_id
    }

    /// Startup announcement JSON written once when launching from the CLI.
    pub fn announcement_json(&self) -> Value {
        json!({
            "ok": true,
            "session_id": self.shared.session_id,
            "bind": self.addr.to_string(),
            "http_url": self.http_url(),
            "ws_url": self.ws_url(),
            "token": self.token(),
            "target_id": self.target_id(),
            "format": self.shared.format,
            "quality": self.shared.quality,
        })
    }

    /// Signals the bridge to stop accepting connections and joins its accept loop.
    pub fn shutdown(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for LivePreviewBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Generates a 256-bit URL-safe random bearer token using `tungstenite`'s CSPRNG key generator.
fn generate_bearer_token() -> String {
    let part1 = tungstenite::handshake::client::generate_key();
    let part2 = tungstenite::handshake::client::generate_key();
    let combined = format!("{part1}{part2}");
    combined
        .chars()
        .filter(|&c| c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect()
}

/// Constant-time byte slice equality for bearer token verification.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (&a, &b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn load_owned_browser_target(
    session_dir: &Path,
    session_id: &str,
) -> Result<OwnedBrowserTarget, String> {
    if !session_dir.is_dir() || !session_dir.join(store::STATE_FILE).is_file() {
        return Err(format!(
            "session `{session_id}` does not exist; run `da session start` first"
        ));
    }
    let state = store::load_state(session_dir)?;
    let Some(browser_handle) = state.handles.get("browser") else {
        return Err(format!(
            "session `{session_id}` has no active browser handle; run `browser.open` first"
        ));
    };
    let websocket_url = browser_handle
        .get("websocket_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("session `{session_id}` browser handle is missing `websocket_url`"))?
        .to_string();
    if !is_loopback_ws_url(&websocket_url) {
        return Err(format!(
            "live-preview refuses non-loopback CDP endpoint `{websocket_url}`"
        ));
    }
    let target_id = browser_handle
        .get("target_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("session `{session_id}` browser handle is missing `target_id`"))?
        .to_string();
    Ok(OwnedBrowserTarget {
        websocket_url,
        target_id,
    })
}

fn is_loopback_ws_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("ws://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    is_loopback_authority(authority)
}

fn is_loopback_origin(origin: &str) -> bool {
    let trimmed = origin.trim();
    let rest = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"));
    let Some(rest) = rest else {
        return false;
    };
    if rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return false;
    }
    is_loopback_authority(rest)
}

fn is_loopback_authority(authority: &str) -> bool {
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((ipv6, tail)) = bracketed.split_once(']') else {
            return false;
        };
        if !tail.is_empty() && !tail.starts_with(':') {
            return false;
        }
        ipv6
    } else {
        authority.split(':').next().unwrap_or("")
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

fn accept_loop(listener: TcpListener, shared: Arc<BridgeShared>) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    while !shared.shutdown.load(Ordering::Relaxed) {
        workers.retain(|h| !h.is_finished());
        match listener.accept() {
            Ok((stream, _peer)) => {
                let conn_shared = Arc::clone(&shared);
                if let Ok(handle) = thread::Builder::new()
                    .name("da-live-preview-conn".to_string())
                    .spawn(move || handle_connection(stream, conn_shared))
                {
                    workers.push(handle);
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_SLICE);
            }
            Err(_) => break,
        }
    }
    for handle in workers {
        let _ = handle.join();
    }
}

struct HttpHead {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    raw_head: Vec<u8>,
    extra_bytes: Vec<u8>,
}

impl HttpHead {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn read_http_head(stream: &mut TcpStream) -> Result<HttpHead, &'static str> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if buf.len() >= MAX_HEADER_BYTES {
            return Err("headers too large");
        }
        let n = stream.read(&mut chunk).map_err(|_| "read failed")?;
        if n == 0 {
            return Err("unexpected eof");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
    };

    let raw_head = buf[..header_end].to_vec();
    let extra_bytes = buf[header_end..].to_vec();
    let head_str = std::str::from_utf8(&raw_head).map_err(|_| "invalid utf-8 headers")?;
    let mut lines = head_str.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing target")?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(HttpHead {
        method,
        target,
        headers,
        raw_head,
        extra_bytes,
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
}

fn write_http_json(stream: &mut TcpStream, status: &str, body: &Value) {
    let payload = body.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Replays already-read HTTP head bytes before delegating to the underlying `TcpStream`,
/// allowing `tungstenite::accept` to complete the WebSocket handshake cleanly after auth.
struct PrefixedStream {
    prefix: Cursor<Vec<u8>>,
    stream: TcpStream,
}

impl Read for PrefixedStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = usize::try_from(self.prefix.position()).unwrap_or(usize::MAX);
        if pos < self.prefix.get_ref().len() {
            return self.prefix.read(buf);
        }
        self.stream.read(buf)
    }
}

impl Write for PrefixedStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn handle_connection(mut stream: TcpStream, shared: Arc<BridgeShared>) {
    let Ok(head) = read_http_head(&mut stream) else {
        write_http_json(
            &mut stream,
            "400 Bad Request",
            &json!({ "ok": false, "error": "malformed HTTP request" }),
        );
        return;
    };

    // Defense-in-depth against Cross-Site WebSocket Hijacking (CSWSH): reject
    // any browser request originating from a non-loopback Origin.
    if let Some(origin) = head.header("origin")
        && !is_loopback_origin(origin)
    {
        write_http_json(
            &mut stream,
            "403 Forbidden",
            &json!({ "ok": false, "error": "non-loopback Origin refused" }),
        );
        return;
    }

    // Require Bearer token authentication on every HTTP and WebSocket route.
    let provided = head
        .header("authorization")
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(str::trim);
    let authorized = provided
        .is_some_and(|candidate| constant_time_eq(candidate.as_bytes(), shared.token.as_bytes()));
    if !authorized {
        write_http_json(
            &mut stream,
            "401 Unauthorized",
            &json!({ "ok": false, "error": "missing or invalid bearer token" }),
        );
        return;
    }

    let path = head.target.split('?').next().unwrap_or(&head.target);
    let is_ws_upgrade = head
        .header("upgrade")
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    if is_ws_upgrade && path == "/ws" {
        let mut combined = head.raw_head;
        combined.extend_from_slice(&head.extra_bytes);
        let prefixed = PrefixedStream {
            prefix: Cursor::new(combined),
            stream,
        };
        if let Ok(ws) = tungstenite::accept(prefixed) {
            handle_ws_client(ws, shared);
        }
        return;
    }

    match (head.method.as_str(), path) {
        ("GET", "/status") => handle_http_status(&mut stream, &shared),
        ("POST", "/pause") => handle_http_pause(&mut stream, &head, &shared),
        ("POST", "/resume") => handle_http_resume(&mut stream, &shared),
        _ => write_http_json(
            &mut stream,
            "404 Not Found",
            &json!({ "ok": false, "error": format!("unknown route `{} {path}`", head.method) }),
        ),
    }
}

fn handle_http_status(stream: &mut TcpStream, shared: &BridgeShared) {
    let state = store::load_state(&shared.session_dir).ok();
    let flag = store::read_pause_flag(&shared.session_dir);
    let paused_for_human = flag.is_some() || state.as_ref().is_some_and(|s| s.paused_for_human);
    let paused_reason = flag
        .and_then(|f| f.reason)
        .or_else(|| state.and_then(|s| s.paused_reason));
    let mut body = json!({
        "ok": true,
        "session_id": shared.session_id,
        "target_id": shared.target.target_id,
        "paused_for_human": paused_for_human,
        "screencast_active": shared.active_clients.load(Ordering::Relaxed) > 0,
        "frames_sent": shared.frames_sent.load(Ordering::Relaxed),
    });
    if let Some(reason) = paused_reason {
        body["paused_reason"] = Value::String(reason);
    }
    write_http_json(stream, "200 OK", &body);
}

fn read_optional_json_body(
    stream: &mut TcpStream,
    head: &HttpHead,
) -> Result<Option<Value>, String> {
    let content_length = match head.header("content-length") {
        Some(raw) => raw
            .trim()
            .parse::<usize>()
            .map_err(|_| "invalid Content-Length header".to_string())?,
        None => 0,
    };
    if content_length == 0 {
        return Ok(None);
    }
    if content_length > MAX_BODY_BYTES {
        return Err(format!("request body exceeds {MAX_BODY_BYTES} bytes"));
    }
    let mut body = Vec::with_capacity(content_length);
    let take_from_extra = content_length.min(head.extra_bytes.len());
    body.extend_from_slice(&head.extra_bytes[..take_from_extra]);
    while body.len() < content_length {
        let mut buf = [0u8; 1024];
        let want = (content_length - body.len()).min(buf.len());
        let n = stream
            .read(&mut buf[..want])
            .map_err(|error| format!("could not read request body: {error}"))?;
        if n == 0 {
            return Err("truncated request body".to_string());
        }
        body.extend_from_slice(&buf[..n]);
    }
    let value: Value =
        serde_json::from_slice(&body).map_err(|error| format!("invalid JSON body: {error}"))?;
    Ok(Some(value))
}

fn handle_http_pause(stream: &mut TcpStream, head: &HttpHead, shared: &BridgeShared) {
    let body = match read_optional_json_body(stream, head) {
        Ok(val) => val,
        Err(msg) => {
            write_http_json(
                stream,
                "400 Bad Request",
                &json!({ "ok": false, "error": msg }),
            );
            return;
        }
    };
    let reason = body
        .as_ref()
        .and_then(|v| v.get("reason"))
        .and_then(Value::as_str);
    match session::pause_session_dir(&shared.session_dir, &shared.session_id, reason) {
        Ok(payload) => write_http_json(stream, "200 OK", &payload),
        Err((_code, kind, msg)) => {
            let status = match kind {
                "not_found" => "404 Not Found",
                "busy" => "409 Conflict",
                _ => "500 Internal Server Error",
            };
            write_http_json(stream, status, &json!({ "ok": false, "error": msg }));
        }
    }
}

fn handle_http_resume(stream: &mut TcpStream, shared: &BridgeShared) {
    match session::resume_session_dir(&shared.session_dir, &shared.session_id) {
        Ok(payload) => write_http_json(stream, "200 OK", &payload),
        Err((_code, kind, msg)) => {
            let status = match kind {
                "not_found" => "404 Not Found",
                "busy" => "409 Conflict",
                _ => "500 Internal Server Error",
            };
            write_http_json(stream, status, &json!({ "ok": false, "error": msg }));
        }
    }
}

fn handle_ws_client(mut ws: tungstenite::WebSocket<PrefixedStream>, shared: Arc<BridgeShared>) {
    let _ = ws.get_ref().stream.set_read_timeout(Some(POLL_SLICE));
    let _ = ws
        .get_ref()
        .stream
        .set_write_timeout(Some(Duration::from_secs(5)));

    // Refresh owned browser target from state.json in case tabs changed, still
    // strictly owned by this session.
    let target = match load_owned_browser_target(&shared.session_dir, &shared.session_id) {
        Ok(target) => target,
        Err(msg) => {
            let _ = ws.send(Message::Text(
                json!({ "type": "error", "message": msg })
                    .to_string()
                    .into(),
            ));
            let _ = ws.close(None);
            return;
        }
    };

    let deadline = Deadline::after(CDP_CONTROL_TIMEOUT);
    let mut cdp = match Connection::connect(&target.websocket_url, deadline) {
        Ok(conn) => conn,
        Err(err) => {
            let _ = ws.send(Message::Text(
                json!({ "type": "error", "message": format!("CDP connect failed: {err}") })
                    .to_string()
                    .into(),
            ));
            let _ = ws.close(None);
            return;
        }
    };

    let attach_reply = match cdp.call(
        "Target.attachToTarget",
        json!({ "targetId": target.target_id, "flatten": true }),
        None,
        Deadline::after(CDP_CONTROL_TIMEOUT),
    ) {
        Ok(reply) => reply,
        Err(err) => {
            let _ = ws.send(Message::Text(
                json!({ "type": "error", "message": format!("Target.attachToTarget failed: {err}") })
                    .to_string()
                    .into(),
            ));
            let _ = ws.close(None);
            return;
        }
    };
    let Some(page_session_id) = attach_reply
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let _ = ws.send(Message::Text(
            json!({ "type": "error", "message": "CDP attach reply missing sessionId" })
                .to_string()
                .into(),
        ));
        let _ = ws.close(None);
        return;
    };

    let _ = cdp.call(
        "Page.enable",
        json!({}),
        Some(&page_session_id),
        Deadline::after(CDP_CONTROL_TIMEOUT),
    );

    let mut screencast_params = Map::new();
    screencast_params.insert("format".to_string(), Value::String(shared.format.clone()));
    if shared.format == "jpeg" {
        screencast_params.insert("quality".to_string(), json!(shared.quality));
    }
    if let Some(max_w) = shared.max_width {
        screencast_params.insert("maxWidth".to_string(), json!(max_w));
    }
    if let Some(max_h) = shared.max_height {
        screencast_params.insert("maxHeight".to_string(), json!(max_h));
    }
    if let Err(err) = cdp.call(
        "Page.startScreencast",
        Value::Object(screencast_params),
        Some(&page_session_id),
        Deadline::after(CDP_CONTROL_TIMEOUT),
    ) {
        let _ = ws.send(Message::Text(
            json!({ "type": "error", "message": format!("Page.startScreencast failed: {err}") })
                .to_string()
                .into(),
        ));
        let _ = detach_cdp_target(&mut cdp, &page_session_id);
        let _ = ws.close(None);
        return;
    }

    shared.active_clients.fetch_add(1, Ordering::Relaxed);
    struct ActiveClientGuard<'a>(&'a AtomicUsize);
    impl Drop for ActiveClientGuard<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let _active_guard = ActiveClientGuard(&shared.active_clients);
    let _ = ws.send(Message::Text(
        json!({
            "type": "ready",
            "session_id": shared.session_id,
            "target_id": target.target_id,
            "format": shared.format,
        })
        .to_string()
        .into(),
    ));

    while !shared.shutdown.load(Ordering::Relaxed) {
        // 1. Drain CDP events and forward `Page.screencastFrame` to the host.
        if cdp.poll_messages().is_err() {
            let _ = ws.send(Message::Text(
                json!({ "type": "closed", "reason": "cdp connection closed" })
                    .to_string()
                    .into(),
            ));
            break;
        }
        let mut target_closed = false;
        let mut client_closed = false;
        for msg in cdp.take_messages() {
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            if method == "Page.screencastFrame" {
                let matches_session = msg
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_none_or(|sid| sid == page_session_id);
                if matches_session && let Some(params) = msg.get("params") {
                    if let Some(frame_sid) = params.get("sessionId").and_then(Value::as_i64) {
                        let _ = cdp.call(
                            "Page.screencastFrameAck",
                            json!({ "sessionId": frame_sid }),
                            Some(&page_session_id),
                            Deadline::after(CDP_INPUT_TIMEOUT),
                        );
                    }
                    shared.frames_sent.fetch_add(1, Ordering::Relaxed);
                    let outbound = json!({
                        "type": "frame",
                        "format": shared.format,
                        "data": params.get("data").cloned().unwrap_or(Value::Null),
                        "metadata": params.get("metadata").cloned().unwrap_or(Value::Null),
                        "frame_session_id": params.get("sessionId").cloned().unwrap_or(Value::Null),
                    });
                    if ws.send(Message::Text(outbound.to_string().into())).is_err() {
                        client_closed = true;
                        break;
                    }
                }
            } else if method == "Target.detachedFromTarget"
                && msg.pointer("/params/sessionId").and_then(Value::as_str)
                    == Some(page_session_id.as_str())
            {
                target_closed = true;
                break;
            }
        }
        if target_closed || client_closed {
            break;
        }

        // 2. Read one host control or human-input message if available.
        match ws.read() {
            Ok(Message::Text(text)) => {
                if let Some(reply) =
                    handle_ws_input_message(&text, &mut cdp, &page_session_id, &shared)
                    && ws.send(Message::Text(reply.to_string().into())).is_err()
                {
                    break;
                }
            }
            Ok(Message::Ping(payload)) => {
                if ws.send(Message::Pong(payload)).is_err() {
                    break;
                }
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(err))
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
    }

    let _ = cdp.call(
        "Page.stopScreencast",
        json!({}),
        Some(&page_session_id),
        Deadline::after(CDP_INPUT_TIMEOUT),
    );
    let _ = detach_cdp_target(&mut cdp, &page_session_id);
    let _ = ws.close(None);
}

fn detach_cdp_target(cdp: &mut Connection, page_session_id: &str) -> Result<(), String> {
    cdp.call(
        "Target.detachFromTarget",
        json!({ "sessionId": page_session_id }),
        None,
        Deadline::after(CDP_INPUT_TIMEOUT),
    )
    .map(|_| ())
    .map_err(|err| err.to_string())
}

fn handle_ws_input_message(
    raw: &str,
    cdp: &mut Connection,
    page_session_id: &str,
    shared: &BridgeShared,
) -> Option<Value> {
    let msg: Value = match serde_json::from_str(raw) {
        Ok(val) => val,
        Err(err) => {
            return Some(json!({
                "type": "error",
                "message": format!("invalid JSON message: {err}"),
            }));
        }
    };
    let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "mouse" => {
            let Some(event_type) = msg.get("event_type").and_then(Value::as_str) else {
                return Some(
                    json!({ "type": "error", "message": "mouse event missing `event_type`" }),
                );
            };
            if !matches!(
                event_type,
                "mousePressed" | "mouseReleased" | "mouseMoved" | "mouseWheel"
            ) {
                return Some(json!({
                    "type": "error",
                    "message": format!("unsupported mouse `event_type` `{event_type}`"),
                }));
            }
            let (Some(x), Some(y)) = (
                msg.get("x")
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite()),
                msg.get("y")
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite()),
            ) else {
                return Some(
                    json!({ "type": "error", "message": "mouse event requires finite `x` and `y`" }),
                );
            };
            let mut params = Map::new();
            params.insert("type".to_string(), json!(event_type));
            params.insert("x".to_string(), json!(x));
            params.insert("y".to_string(), json!(y));
            if let Some(button) = msg.get("button").and_then(Value::as_str) {
                params.insert("button".to_string(), json!(button));
            }
            if let Some(click_count) = msg.get("click_count").and_then(Value::as_i64) {
                params.insert("clickCount".to_string(), json!(click_count));
            }
            if let Some(modifiers) = msg.get("modifiers").and_then(Value::as_i64) {
                params.insert("modifiers".to_string(), json!(modifiers));
            }
            if let Some(dx) = msg
                .get("delta_x")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
            {
                params.insert("deltaX".to_string(), json!(dx));
            }
            if let Some(dy) = msg
                .get("delta_y")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite())
            {
                params.insert("deltaY".to_string(), json!(dy));
            }
            match cdp.call(
                "Input.dispatchMouseEvent",
                Value::Object(params),
                Some(page_session_id),
                Deadline::after(CDP_INPUT_TIMEOUT),
            ) {
                Ok(_) => Some(json!({ "type": "ack", "for": "mouse" })),
                Err(err) => Some(json!({ "type": "error", "message": err.to_string() })),
            }
        }
        "key" => {
            let Some(event_type) = msg.get("event_type").and_then(Value::as_str) else {
                return Some(
                    json!({ "type": "error", "message": "key event missing `event_type`" }),
                );
            };
            if !matches!(event_type, "keyDown" | "keyUp" | "rawKeyDown" | "char") {
                return Some(json!({
                    "type": "error",
                    "message": format!("unsupported key `event_type` `{event_type}`"),
                }));
            }
            let mut params = Map::new();
            params.insert("type".to_string(), json!(event_type));
            for (src, dst) in [
                ("key", "key"),
                ("code", "code"),
                ("text", "text"),
                ("unmodified_text", "unmodifiedText"),
            ] {
                if let Some(val) = msg.get(src).and_then(Value::as_str) {
                    params.insert(dst.to_string(), json!(val));
                }
            }
            if let Some(modifiers) = msg.get("modifiers").and_then(Value::as_i64) {
                params.insert("modifiers".to_string(), json!(modifiers));
            }
            if let Some(vk) = msg.get("windows_virtual_key_code").and_then(Value::as_i64) {
                params.insert("windowsVirtualKeyCode".to_string(), json!(vk));
            }
            match cdp.call(
                "Input.dispatchKeyEvent",
                Value::Object(params),
                Some(page_session_id),
                Deadline::after(CDP_INPUT_TIMEOUT),
            ) {
                Ok(_) => Some(json!({ "type": "ack", "for": "key" })),
                Err(err) => Some(json!({ "type": "error", "message": err.to_string() })),
            }
        }
        "insert_text" => {
            let Some(text) = msg.get("text").and_then(Value::as_str) else {
                return Some(json!({ "type": "error", "message": "insert_text missing `text`" }));
            };
            match cdp.call(
                "Input.insertText",
                json!({ "text": text }),
                Some(page_session_id),
                Deadline::after(CDP_INPUT_TIMEOUT),
            ) {
                Ok(_) => Some(json!({ "type": "ack", "for": "insert_text" })),
                Err(err) => Some(json!({ "type": "error", "message": err.to_string() })),
            }
        }
        "pause" => {
            let reason = msg.get("reason").and_then(Value::as_str);
            match session::pause_session_dir(&shared.session_dir, &shared.session_id, reason) {
                Ok(mut payload) => {
                    payload["type"] = json!("state");
                    Some(payload)
                }
                Err((_code, _kind, err)) => Some(json!({ "type": "error", "message": err })),
            }
        }
        "resume" => match session::resume_session_dir(&shared.session_dir, &shared.session_id) {
            Ok(mut payload) => {
                payload["type"] = json!("state");
                Some(payload)
            }
            Err((_code, _kind, err)) => Some(json!({ "type": "error", "message": err })),
        },
        other => Some(json!({
            "type": "error",
            "message": format!(
                "unsupported message type `{other}`; allowed: mouse, key, insert_text, pause, resume"
            ),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_is_url_safe_and_unique() {
        let a = generate_bearer_token();
        let b = generate_bearer_token();
        assert_ne!(a, b);
        assert!(
            a.len() >= 40,
            "expected >=256-bit token, got len {}",
            a.len()
        );
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn loopback_origin_and_ws_url_validators_reject_remote_hosts() {
        assert!(is_loopback_origin("http://127.0.0.1:3000"));
        assert!(is_loopback_origin("http://localhost:5173"));
        assert!(is_loopback_origin("https://[::1]:8443"));
        assert!(!is_loopback_origin("https://evil.example"));
        assert!(!is_loopback_origin("http://127.0.0.1.evil.example"));
        assert!(!is_loopback_origin("http://127.0.0.1@evil.example"));

        assert!(is_loopback_ws_url("ws://127.0.0.1:9222/devtools/browser/1"));
        assert!(is_loopback_ws_url("ws://localhost:9222/devtools/browser/1"));
        assert!(is_loopback_ws_url("ws://[::1]:9222/devtools/browser/1"));
        assert!(!is_loopback_ws_url(
            "ws://192.168.1.5:9222/devtools/browser/1"
        ));
        assert!(!is_loopback_ws_url(
            "wss://127.0.0.1:9222/devtools/browser/1"
        ));
    }

    #[test]
    fn constant_time_eq_checks_exact_bytes() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-tokeN"));
        assert!(!constant_time_eq(b"secret-token", b"secret"));
    }
}
