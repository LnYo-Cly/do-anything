// Each integration test target compiles this module separately and uses a
// different subset of its helpers.
#![allow(dead_code)]

//! Helpers shared by the integration tests: a local HTTP server and temporary
//! case files. Nothing here reaches the network beyond loopback.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A request the test server received.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Request {
    /// The query string of the request target, or an empty string.
    pub fn query(&self) -> &str {
        self.target
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or("")
    }

    /// The path part of the request target.
    pub fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(&self.target)
    }
}

/// How long the `/sleep` route waits before it answers.
pub const SLEEP_ROUTE_DELAY: Duration = Duration::from_millis(1000);

/// How many connections [`TestServer::timing_report`] describes.
const MAX_TIMING_LINES: usize = 32;

/// When one connection and the request it carried crossed the test server.
///
/// A navigation that reaches the server 30 s late and one the server answers
/// 30 s late are different defects; these times tell them apart.
#[derive(Debug, Clone)]
pub struct Timing {
    /// The server accepted the connection this long after it started.
    pub accepted: Duration,
    /// The request read on this connection, if it ever sent one.
    pub request: Option<RequestTiming>,
}

/// When the request of one connection arrived and when it was answered.
#[derive(Debug, Clone)]
pub struct RequestTiming {
    /// The request target, as `method target`.
    pub target: String,
    /// The complete request head and body had arrived this long after the start.
    pub arrived: Duration,
    /// The response had been handed to the operating system this long after the
    /// start. `None` while the server is still answering, or when it gave up.
    pub answered: Option<Duration>,
}

/// A minimal HTTP server bound to a loopback port, started by the test.
pub struct TestServer {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    connections: Arc<Mutex<Vec<Arc<Mutex<Timing>>>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TestServer {
    /// Starts a server on a random loopback port that Chrome does not block.
    pub fn start() -> Self {
        let listener = bind_browser_safe_listener();
        let port = listener
            .local_addr()
            .expect("failed to read the test server address")
            .port();
        listener
            .set_nonblocking(true)
            .expect("failed to make the test server non-blocking");

        let started = Instant::now();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections: Arc<Mutex<Vec<Arc<Mutex<Timing>>>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_requests = Arc::clone(&requests);
        let server_connections = Arc::clone(&connections);
        let server_stop = Arc::clone(&stop);

        let handle = thread::spawn(move || {
            while !server_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    // One thread per connection: a test that starts several
                    // requests at once must be answered in parallel, the way a
                    // real server answers, or a slowly answering route would
                    // serialize the clients that the test runs concurrently.
                    // The threads are not joined; they only hold the shared
                    // request log, which outlives them.
                    Ok((stream, _)) => {
                        let requests = Arc::clone(&server_requests);
                        let timing = Arc::new(Mutex::new(Timing {
                            accepted: started.elapsed(),
                            request: None,
                        }));
                        // The connection is recorded before it is read, so a
                        // report taken while a request is still pending names it.
                        server_connections
                            .lock()
                            .expect("test server timings mutex is poisoned")
                            .push(Arc::clone(&timing));
                        thread::spawn(move || {
                            handle_connection(stream, &requests, &timing, started)
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            requests,
            connections,
            stop,
            handle: Some(handle),
        }
    }

    /// The loopback port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// An absolute URL for `path` on this server.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.port, path)
    }

    /// Every request the server received so far.
    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .expect("test server requests mutex is poisoned")
            .clone()
    }

    /// The number of requests the server received so far.
    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .expect("test server requests mutex is poisoned")
            .len()
    }

    /// When every connection the server accepted arrived, and when it answered.
    pub fn timings(&self) -> Vec<Timing> {
        self.connections
            .lock()
            .expect("test server timings mutex is poisoned")
            .iter()
            .map(|timing| {
                timing
                    .lock()
                    .expect("test server timing mutex is poisoned")
                    .clone()
            })
            .collect()
    }

    /// A short report of the server's side of a test, for a failure message.
    ///
    /// Every time is milliseconds since the server started. A connection with no
    /// request is what Chrome's speculative socket looks like: the browser
    /// reached the server, but that connection never asked for anything.
    pub fn timing_report(&self) -> String {
        let timings = self.timings();
        if timings.is_empty() {
            return "the test server accepted no connection".to_string();
        }
        let answered = timings
            .iter()
            .filter(|timing| {
                timing
                    .request
                    .as_ref()
                    .is_some_and(|request| request.answered.is_some())
            })
            .count();
        let mut lines = vec![format!(
            "the test server accepted {} connection(s) and answered {answered} request(s), \
             in milliseconds since it started:",
            timings.len()
        )];
        for timing in timings.iter().take(MAX_TIMING_LINES) {
            lines.push(describe_timing(timing));
        }
        if timings.len() > MAX_TIMING_LINES {
            lines.push(format!(
                "... and {} more connection(s)",
                timings.len() - MAX_TIMING_LINES
            ));
        }
        lines.join("\n")
    }
}

/// One line of [`TestServer::timing_report`].
fn describe_timing(timing: &Timing) -> String {
    let accepted = timing.accepted.as_millis();
    match &timing.request {
        Some(request) => {
            let arrived = request.arrived.as_millis();
            match request.answered {
                Some(answered) => format!(
                    "  +{accepted} ms accepted, +{arrived} ms request `{}`, +{} ms answered",
                    request.target,
                    answered.as_millis()
                ),
                None => format!(
                    "  +{accepted} ms accepted, +{arrived} ms request `{}`, not answered",
                    request.target
                ),
            }
        }
        None => format!("  +{accepted} ms accepted, no request"),
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    stream: TcpStream,
    requests: &Mutex<Vec<Request>>,
    timing: &Mutex<Timing>,
    started: Instant,
) {
    // Accepting from a non-blocking listener yields a non-blocking socket on
    // some platforms, so the connection mode is set back to blocking here.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).into_owned();

    {
        let mut timing = timing.lock().expect("test server timing mutex is poisoned");
        timing.request = Some(RequestTiming {
            target: format!("{method} {target}"),
            arrived: started.elapsed(),
            answered: None,
        });
    }

    let poll_attempt = {
        let mut guard = requests
            .lock()
            .expect("test server requests mutex is poisoned");
        guard.push(Request {
            method: method.clone(),
            target: target.clone(),
            headers: headers.clone(),
            body: body.clone(),
        });
        guard
            .iter()
            .filter(|request| request.path() == "/poll")
            .count()
    };

    let (path, query) = target
        .split_once('?')
        .map_or((target.as_str(), ""), |(path, query)| (path, query));
    let response = route(&method, path, query, &headers, &body, poll_attempt);

    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
    if let Some(request) = timing
        .lock()
        .expect("test server timing mutex is poisoned")
        .request
        .as_mut()
    {
        request.answered = Some(started.elapsed());
    }
}

fn route(
    method: &str,
    path: &str,
    query: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
    poll_attempt: usize,
) -> String {
    match (method, path) {
        ("HEAD", "/poll") => {
            let ready_at = query_usize(query, "ready_at").unwrap_or(1);
            if poll_attempt >= ready_at {
                head_response("200 OK")
            } else {
                head_response("404 Not Found")
            }
        }
        (_, "/poll") => {
            let ready_at = query_usize(query, "ready_at").unwrap_or(1);
            let ready = poll_attempt >= ready_at;
            let state = if ready { "done" } else { "pending" };
            let payload = serde_json::json!({
                "attempt": poll_attempt,
                "ready": ready,
                "state": state,
            });
            json_response("200 OK", &payload.to_string())
        }
        (_, "/health") => json_response("200 OK", r#"{"status":"ok"}"#),
        ("POST", "/login") => json_response("200 OK", r#"{"token":"t-123","user":"alice"}"#),
        (_, "/me") => {
            let authorized = headers
                .get("authorization")
                .is_some_and(|value| value == "Bearer t-123");
            if authorized {
                json_response("200 OK", r#"{"name":"alice"}"#)
            } else {
                json_response("401 Unauthorized", r#"{"error":"unauthorized"}"#)
            }
        }
        (_, "/slow") => {
            thread::sleep(Duration::from_millis(400));
            json_response("200 OK", r#"{"slow":true}"#)
        }
        // A wait long enough that four cases overlap for a measurable time when
        // they run at once, and dominate the run when they do not.
        (_, "/sleep") => {
            thread::sleep(SLEEP_ROUTE_DELAY);
            json_response("200 OK", r#"{"slept":true}"#)
        }
        (_, "/status/500") => json_response("500 Internal Server Error", r#"{"error":"boom"}"#),
        (_, "/text") => text_response("200 OK", "hello world"),
        // The browser tests drive real pages. The label in `?case=` lets four
        // cases prove that each one sees its own page and its own browser.
        (_, "/browser/form") => html_response("200 OK", &browser_form(query)),
        (_, "/browser/mask") => html_response("200 OK", browser_mask()),
        // A page whose button starts three fetches after the click step has
        // ended: a 200, a 404 and a refused connection. The marker element is
        // created only after all three settled, so a `browser.wait` step sees
        // requests that completed during that step.
        (_, "/browser/network") => html_response("200 OK", &browser_network(query)),
        (_, "/browser/locators") => html_response("200 OK", &browser_locators(query)),
        (_, "/browser/actionability") => html_response("200 OK", browser_actionability()),
        (_, "/browser/probe/iframe") => html_response("200 OK", browser_probe_iframe()),
        (_, "/browser/probe/iframe-inner") => html_response(
            "200 OK",
            "<!doctype html><html><body><button id=\"inside-frame\" type=\"button\" onclick=\"document.getElementById('probe-status').textContent='clicked';\">Inside Frame</button><p id=\"probe-status\">idle</p></body></html>",
        ),
        (_, "/browser/iframes/host") => html_response("200 OK", &browser_iframes_host(query)),
        (_, "/browser/iframes/inner") => html_response("200 OK", browser_iframes_inner()),
        (_, "/browser/iframes/nested") => html_response("200 OK", browser_iframes_nested()),
        (_, "/browser/iframes/occlusion") => html_response("200 OK", browser_iframes_occlusion()),
        (_, "/browser/iframes/occ-inner") => html_response("200 OK", browser_iframes_occ_inner()),
        (_, "/browser/iframes/slow-host") => html_response("200 OK", browser_iframes_slow_host()),
        (_, "/browser/iframes/slow-inner") => {
            thread::sleep(Duration::from_millis(300));
            html_response(
                "200 OK",
                "<!doctype html><html><body><p id=\"slow-ready\">loaded-late</p><button id=\"slow-btn\" type=\"button\" onclick=\"document.getElementById('slow-ready').textContent='slow-clicked';\">Slow Ready</button></body></html>",
            )
        }
        (_, "/browser/iframes/frameset") => html_response("200 OK", browser_iframes_frameset()),
        (_, "/browser/iframes/scaled-host") => {
            html_response("200 OK", &browser_iframes_scaled_host(query))
        }
        (_, "/browser/iframes/scaled-inner") => {
            html_response("200 OK", browser_iframes_scaled_inner())
        }
        (_, "/browser/iframes/unmappable-transform") => {
            html_response("200 OK", &browser_iframes_unmappable_transform(query))
        }
        (_, "/browser/iframes/retry") => html_response("200 OK", browser_iframes_retry()),
        (_, "/browser/mask-iframe") => html_response("200 OK", browser_mask_iframe()),
        (_, "/browser/mask-iframe-inner") => html_response("200 OK", browser_mask_iframe_inner()),
        (_, "/browser/mask-iframe-nested") => html_response("200 OK", browser_mask_iframe_nested()),
        (_, "/browser/probe/shadow") => html_response("200 OK", browser_probe_shadow()),
        (_, "/browser/shadow") => html_response("200 OK", &browser_shadow(query)),
        (_, "/browser/shadow-closed") => html_response("200 OK", browser_shadow_closed()),
        (_, "/browser/shadow-mask") => html_response("200 OK", browser_shadow_mask()),
        (_, "/browser/probe/new-tab") => html_response("200 OK", browser_probe_new_tab()),
        (_, "/browser/probe/new-tab-target") => {
            html_response("200 OK", browser_probe_new_tab_target())
        }
        (_, "/browser/probe/new-tab-window") => {
            html_response("200 OK", browser_probe_new_tab_window())
        }
        (_, "/browser/probe/new-tab-second") => {
            html_response("200 OK", browser_probe_new_tab_second())
        }
        // The fetch a tab opened by a page action issues once it is switched to.
        (_, "/browser/probe/new-tab-fetch") => json_response("200 OK", r#"{"fetched":true}"#),
        (_, "/browser/probe/dialog") => html_response("200 OK", browser_probe_dialog()),
        (_, "/browser/dialogs") => html_response("200 OK", browser_dialogs()),
        (_, "/browser/dialogs/onload") => html_response("200 OK", browser_dialogs_onload()),
        (_, "/browser/dialogs/beforeunload") => {
            html_response("200 OK", browser_dialogs_beforeunload())
        }
        (_, "/browser/dialogs/retry") => html_response("200 OK", browser_dialogs_retry()),
        (_, "/browser/dialogs/left") => html_response(
            "200 OK",
            "<!doctype html><html><head><title>Left Page</title></head><body><p id=\"left-status\">left</p></body></html>",
        ),
        (_, "/browser/probe/upload") => html_response("200 OK", browser_probe_upload()),
        (_, "/browser/probe/coords") => html_response("200 OK", browser_probe_coords()),
        // The file-upload pages: controls for every locator and error, a form
        // whose submission carries the chosen file's bytes back here, and the
        // page a submitted form lands on.
        (_, "/browser/upload") => html_response("200 OK", browser_upload()),
        (_, "/browser/upload/form") => html_response("200 OK", browser_upload_form()),
        ("POST", "/browser/upload/submit") => html_response("200 OK", browser_upload_received()),
        (_, "/browser/upload/tab") => html_response("200 OK", browser_upload_tab()),
        (_, "/browser/upload/tab-form") => html_response("200 OK", browser_upload_tab_form()),
        (_, "/browser/record") => html_response("200 OK", &browser_record(query)),
        (_, "/browser/second") => html_response("200 OK", &browser_second(query)),
        // Headers and the start of the page arrive, but the document never
        // finishes loading before a short navigation timeout, which is what
        // `browser.open`'s own `timeout_ms` has to report.
        (_, "/browser/stall") => {
            thread::sleep(Duration::from_secs(5));
            html_response("200 OK", &browser_second(query))
        }
        // A login form, the page it signs in to, and a page that writes the
        // same state with a persistent expiry. The cookie and the
        // `localStorage` value are the two secrets the storage-state tests
        // assert never reach output, bundles or reports.
        (_, "/auth/login") => html_response("200 OK", &auth_login_page()),
        (_, "/auth/persist") => html_response("200 OK", &auth_persist_page()),
        (_, "/auth/me") => {
            let authorized = headers.get("cookie").is_some_and(|value| {
                value
                    .split(';')
                    .any(|part| part.trim() == format!("sid={AUTH_SESSION_COOKIE}"))
            });
            html_response("200 OK", &auth_me_page(authorized))
        }
        // Reports which login source the browser sent, using labels rather than
        // the cookie value, so a precedence test never prints a secret.
        (_, "/auth/which") => {
            let sent = |wanted: &str| {
                headers.get("cookie").is_some_and(|value| {
                    value
                        .split(';')
                        .any(|part| part.trim() == format!("sid={wanted}"))
                })
            };
            let source = if sent(AUTH_SESSION_COOKIE) {
                "session cookie"
            } else if sent("state-file-session") {
                "state file"
            } else {
                "none"
            };
            html_response("200 OK", &auth_which_page(source))
        }
        // A fixed body with one number, one boolean and one null, so a test can
        // assert on how each JSON scalar is redacted without depending on a
        // duration or a timestamp.
        (_, "/scalars") => json_response("200 OK", r#"{"count":10,"flag":true,"nothing":null}"#),
        // Encodes a value from the request body into the three forms a service
        // commonly echoes back: a query string, an authorization header and a
        // JSON document. The secret never has to be written into a case file.
        ("POST", "/encoded-secret") => encoded_secret_response(body),
        // Writes a file while the request is answered, so a case can produce
        // log lines during its own step: an observer sees only what changed
        // between its start and its collect call.
        ("POST", "/write-log") => match write_log(body) {
            Ok(()) => json_response("200 OK", r#"{"written":true}"#),
            Err(error) => json_response(
                "500 Internal Server Error",
                &serde_json::json!({"error": error}).to_string(),
            ),
        },
        (_, "/echo") => {
            let echo = serde_json::json!({
                "method": method,
                "path": path,
                "query": query,
                "headers": headers,
                "body": body,
            });
            json_response("200 OK", &echo.to_string())
        }
        _ => json_response("404 Not Found", r#"{"error":"not found"}"#),
    }
}

/// Answers `/encoded-secret` with percent-encoded and base64 forms.
fn encoded_secret_response(body: &str) -> String {
    let Ok(request) = serde_json::from_str::<serde_json::Value>(body) else {
        return json_response("400 Bad Request", r#"{"error":"body must be JSON"}"#);
    };
    let Some(secret) = request["secret"].as_str() else {
        return json_response("400 Bad Request", r#"{"error":"secret must be a string"}"#);
    };
    let user = request["user"].as_str().unwrap_or("user");
    let json = serde_json::json!({"token": secret}).to_string();
    let response = serde_json::json!({
        "url": format!("http://example.test/?token={}", percent_encode(secret.as_bytes())),
        "authorization": format!("Basic {}", base64_encode(format!("{user}:{secret}").as_bytes())),
        "json": base64_encode(json.as_bytes()),
    });
    json_response("200 OK", &response.to_string())
}

/// Encodes every byte with its two hexadecimal digits.
pub fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len() * 3);
    for byte in bytes {
        encoded.push_str(&format!("%{byte:02X}"));
    }
    encoded
}

/// Encodes bytes with the standard base64 alphabet and padding.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// Appends to or rewrites the file named in a `/write-log` request body.
///
/// The body is a JSON object with `path` and `text`, plus `append` to add to
/// the file instead of replacing it. A missing file is created.
fn write_log(body: &str) -> Result<(), String> {
    let request: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("the body must be a JSON object: {error}"))?;
    let path = request["path"]
        .as_str()
        .ok_or("`path` must be a string naming the file to write")?;
    let text = request["text"]
        .as_str()
        .ok_or("`text` must be a string holding the content")?;
    let append = request["append"].as_bool().unwrap_or(false);

    let mut options = fs::OpenOptions::new();
    options.create(true).write(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not open {path}: {error}"))?;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("could not write {path}: {error}"))?;
    file.flush()
        .map_err(|error| format!("could not flush {path}: {error}"))
}

fn head_response(status: &str) -> String {
    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
}

fn json_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn text_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn html_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The page the browser tests open: a form, a delayed element and a link.
///
/// The script counts visits in `localStorage`, which survives a navigation but
/// not a new browser profile. A case that opens the page twice therefore proves
/// both that the second `browser.open` navigates and that the browser session was
/// reused.
fn browser_form(query: &str) -> String {
    let label = escape_html(&query_value(query, "case"));
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Browser form</title></head>
<body>
<h1>Browser form</h1>
<p id="case">{label}</p>
<p id="visits"></p>
<form id="form">
<label for="name">Name</label>
<input id="name" name="name" autocomplete="off">
<button id="submit" type="submit">Greet</button>
</form>
<p id="greeting" data-state="ready">no greeting yet</p>
<div id="late-target"></div>
<button id="later" type="button">Later</button>
<button id="trusted" type="button"
  onclick="document.getElementById('trusted-result').textContent =
    (window.event &amp;&amp; window.event.isTrusted) ? 'trusted click' : 'untrusted click';">Click me</button>
<p id="trusted-result"></p>
<div id="editor" contenteditable="true">Editor</div>
<a id="next" href="/browser/second?case={label}&amp;token=ok">Next</a>
<script>
var visits = Number(localStorage.getItem('browser-visits') || '0') + 1;
localStorage.setItem('browser-visits', String(visits));
document.getElementById('visits').textContent = 'visits: ' + visits;
document.getElementById('form').addEventListener('submit', function (event) {{
  event.preventDefault();
  document.getElementById('greeting').textContent =
    'Hello, ' + document.getElementById('name').value + '!';
}});
document.getElementById('later').addEventListener('click', function () {{
  setTimeout(function () {{
    var p = document.createElement('p');
    p.id = 'late';
    p.textContent = 'late result';
    document.getElementById('late-target').appendChild(p);
  }}, 300);
}});
</script>
</body></html>"#
    )
}

/// A page whose script records whether every visible editable field was
/// covered while a screenshot mask existed.
///
/// A `MutationObserver` on `document.documentElement` watches for the masks the
/// driver appends; the page is not told about them by any custom event. The
/// record is deliberately latched: the cleanup removes the masks afterwards,
/// but the report keeps what was true during the screenshot so a later
/// `browser.read` can assert on it.
fn browser_mask() -> &'static str {
    r##"<!doctype html>
<html><head><meta charset="utf-8"><title>Mask probe</title>
<style>
body{margin:0;font:16px sans-serif;color:#111;background:#fff}
input,textarea,select,#editor{border:1px solid #333;background:#fff}
#name{position:fixed;left:20px;top:80px;width:320px;height:48px;font-size:24px;color:#f00}
#notes{position:fixed;left:20px;top:150px;width:320px;height:60px;font-size:20px;color:#f00}
#choice{position:fixed;left:20px;top:230px;width:160px;height:32px}
#editor{position:fixed;left:20px;top:280px;width:320px;height:48px;font-size:20px;color:#f00}
#button{position:fixed;left:20px;top:350px;width:100px;height:32px}
#checkbox{position:fixed;left:20px;top:400px;width:20px;height:20px}
#range{position:fixed;left:60px;top:400px;width:120px;height:24px}
#color{position:fixed;left:200px;top:400px;width:40px;height:24px}
#mask-report{position:fixed;left:20px;top:460px}
</style></head>
<body>
<h1>Mask probe</h1>
<input id="name" name="name" autocomplete="off">
<textarea id="notes">visible note</textarea>
<select id="choice"><option>one</option><option>two</option></select>
<div id="editor" contenteditable="true">Editor visible</div>
<button id="button" type="button">Button</button>
<input id="checkbox" type="checkbox">
<input id="hidden" type="hidden" value="hidden">
<input id="range" type="range">
<input id="color" type="color" value="#ff0000">
<p id="mask-report">not recorded</p>
<script>
function isCovered(el, masks) {
  var rect = el.getBoundingClientRect();
  for (var i = 0; i < masks.length; i++) {
    var mask = masks[i];
    var box = mask.getBoundingClientRect();
    var style = window.getComputedStyle(mask);
    if (style.opacity === '1' &&
        style.zIndex === '2147483647' &&
        box.left <= rect.left + 0.5 &&
        box.top <= rect.top + 0.5 &&
        box.right >= rect.right - 0.5 &&
        box.bottom >= rect.bottom - 0.5) {
      return true;
    }
  }
  return false;
}
function recordMasks() {
  var masks = document.querySelectorAll('[data-da-screenshot-mask]');
  if (!masks.length) return;
  var ids = ['name', 'notes', 'choice', 'editor', 'button', 'checkbox', 'hidden', 'range', 'color'];
  var parts = [];
  for (var i = 0; i < ids.length; i++) {
    var el = document.getElementById(ids[i]);
    if (!el) continue;
    var rect = el.getBoundingClientRect();
    var visible = rect.width > 0 && rect.height > 0;
    parts.push(ids[i] + '=' + (visible && isCovered(el, masks)));
  }
  document.getElementById('mask-report').textContent = parts.join(',');
}
var observer = new MutationObserver(function (records) {
  for (var i = 0; i < records.length; i++) {
    var added = records[i].addedNodes;
    for (var j = 0; j < added.length; j++) {
      var node = added[j];
      if (node.nodeType === 1 && node.hasAttribute('data-da-screenshot-mask')) {
        recordMasks();
        return;
      }
    }
  }
});
observer.observe(document.documentElement, { childList: true, subtree: true });
</script>
</body></html>"##
}

/// A page that makes the requests a network observer has to report.
///
/// The fetches start 300 ms after the click, so they complete in the following
/// `browser.wait` step instead of inside the click step. The `#network-done`
/// marker appears 500 ms after every fetch settled, which leaves a slow machine
/// time to deliver `Network.loadingFinished` and `Network.loadingFailed`
/// before the wait step collects them. The page never sees the observer: it
/// only uses ordinary `fetch` calls.
fn browser_network(query: &str) -> String {
    let label = escape_html(&query_value(query, "case"));
    let closed = query_value(query, "closed");
    let closed = closed.parse::<u16>().unwrap_or(9);
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Network page</title><link rel="icon" href="data:,"></head>
<body>
<h1>Network page</h1>
<p id="case">{label}</p>
<button id="net-all" type="button">Load requests</button>
<div id="network-target"></div>
<script>
document.getElementById('net-all').addEventListener('click', function () {{
  setTimeout(function () {{
    var requests = [
      fetch('/health?kind=post', {{ method: 'POST' }}),
      fetch('/nowhere?kind=missing'),
      fetch('http://127.0.0.1:{closed}/closed')
    ];
    Promise.allSettled(requests).then(function () {{
      setTimeout(function () {{
        var p = document.createElement('p');
        p.id = 'network-done';
        p.textContent = 'network done';
        document.getElementById('network-target').appendChild(p);
      }}, 100);
    }});
  }}, 300);
}});
</script>
</body></html>"#
    )
}

fn browser_second(query: &str) -> String {
    let label = escape_html(&query_value(query, "case"));
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Second page</title></head>
<body>
<p id="case">{label}</p>
<p id="second">second page</p>
</body></html>"#
    )
}

/// The session cookie value the auth pages write.
pub const AUTH_SESSION_COOKIE: &str = "s3cr3t-session";

/// The `localStorage` value the auth pages write.
pub const AUTH_LOCAL_STORAGE_TOKEN: &str = "s3cr3t-token";

/// Splits a secret into two JavaScript string literals joined at runtime, so a
/// DOM snapshot of the page never contains the whole value.
fn js_secret(value: &str) -> String {
    let (head, tail) = value.split_at(value.len() / 2);
    format!("'{head}' + '{tail}'")
}

/// A login form that stores a session cookie and a `localStorage` token, then
/// navigates to the signed-in page.
fn auth_login_page() -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Sign in</title></head>
<body>
<h1>Sign in</h1>
<form id="form">
<label for="user">Username</label>
<input id="user" name="user" autocomplete="off">
<label for="password">Password</label>
<input id="password" name="password" type="password">
<button id="signin" type="submit">Sign in</button>
</form>
<p id="login-state">signed out</p>
<script>
document.getElementById('form').addEventListener('submit', function (event) {{
  event.preventDefault();
  var session = {session};
  document.cookie = 'sid=' + session + '; path=/';
  window.localStorage.setItem('auth_token', {token});
  document.getElementById('login-state').textContent = 'signing in';
  window.location.href = '/auth/me';
}});
</script>
</body></html>"#,
        session = js_secret(AUTH_SESSION_COOKIE),
        token = js_secret(AUTH_LOCAL_STORAGE_TOKEN),
    )
}

/// A page that writes the same state as the login form but with a persistent
/// cookie, so a reused browser profile still has it after a restart.
fn auth_persist_page() -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Persist sign in</title></head>
<body>
<h1>Persist sign in</h1>
<p id="persisted">pending</p>
<script>
var session = {session};
document.cookie = 'sid=' + session + '; path=/; expires=Fri, 31 Dec 9999 23:59:59 GMT; SameSite=Lax';
window.localStorage.setItem('auth_token', {token});
document.getElementById('persisted').textContent = 'saved';
</script>
</body></html>"#,
        session = js_secret(AUTH_SESSION_COOKIE),
        token = js_secret(AUTH_LOCAL_STORAGE_TOKEN),
    )
}

/// The page behind the login: the server checks the cookie, and the script
/// checks `localStorage`, so a test sees which half of the state was restored.
fn auth_me_page(authorized: bool) -> String {
    let who = if authorized {
        "signed in as alice"
    } else {
        "signed out"
    };
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Account</title></head>
<body>
<h1>Account</h1>
<p id="who">{who}</p>
<p id="token">unknown</p>
<script>
var want = {token};
document.getElementById('token').textContent =
  window.localStorage.getItem('auth_token') === want ? 'token present' : 'token missing';
</script>
</body></html>"#,
        token = js_secret(AUTH_LOCAL_STORAGE_TOKEN),
    )
}

/// Names which login source sent the session cookie, without printing it.
fn auth_which_page(source: &str) -> String {
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Which login</title></head>
<body>
<p id="source">{source}</p>
</body></html>"#
    )
}

/// A page for the record/report gate: it echoes a secret and carries markup
/// that would be dangerous if the report embedded it without escaping.
fn browser_record(query: &str) -> String {
    let secret = escape_html(&query_value(query, "secret"));
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Record probe</title></head>
<body>
<h1>Record probe</h1>
<p id="secret">{secret}</p>
<pre id="evil">&lt;img src="https://evil.example/x"&gt;</pre>
</body></html>"#
    )
}

/// A page for testing `testid`, `within`, `nth`, and strict unique matching.
fn browser_locators(query: &str) -> String {
    let secret = escape_html(&query_value(query, "secret"));
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Locator probe</title></head>
<body>
<h1>Locator probe</h1>
<p id="locator-result">none</p>

<button id="solo-btn" data-testid="solo-btn" type="button"
  onclick="document.getElementById('locator-result').textContent = 'solo';">Solo</button>
<button id="qa-btn" data-qa="custom-btn" type="button"
  onclick="document.getElementById('locator-result').textContent = 'qa';">QA Button</button>
<input id="qa-input" data-qa="custom-input" autocomplete="off">

<section id="panel-a" class="panel" data-testid="panel" role="region" aria-label="Alpha Panel">
  <div class="group">
    <button id="save-a" class="scoped-btn" data-testid="save-btn" type="button"
      onclick="document.getElementById('locator-result').textContent = 'alpha';">Save</button>
    <input class="scoped-input" data-testid="note-input" autocomplete="off">
  </div>
</section>
<section id="panel-b" class="panel" data-testid="panel" role="region" aria-label="Beta Panel">
  <div class="group">
    <button id="save-b" class="scoped-btn" data-testid="save-btn" type="button"
      onclick="document.getElementById('locator-result').textContent = 'beta';">Save</button>
    <input class="scoped-input" data-testid="note-input" autocomplete="off">
  </div>
</section>

<div id="nth-list">
  <button class="nth-btn" data-testid="nth-item" type="button"
    onclick="document.getElementById('locator-result').textContent = 'nth-0';">Item 0</button>
  <button class="nth-btn" data-testid="nth-item" type="button"
    onclick="document.getElementById('locator-result').textContent = 'nth-1';">Item 1</button>
  <button class="nth-btn" data-testid="nth-item" type="button"
    onclick="document.getElementById('locator-result').textContent = 'nth-2';">Item 2</button>
</div>

<div id="delayed-nth">
  <button class="async-nth" type="button"
    onclick="document.getElementById('locator-result').textContent = 'async-0';">First</button>
</div>
<button id="spawn-second" type="button"
  onclick="setTimeout(function () {{
    var b = document.createElement('button');
    b.className = 'async-nth';
    b.type = 'button';
    b.textContent = 'Second';
    b.onclick = function () {{ document.getElementById('locator-result').textContent = 'async-1'; }};
    document.getElementById('delayed-nth').appendChild(b);
  }}, 250);">Spawn second</button>

<div id="hidden-dup-wrap">
  <button class="hidden-plus-visible" data-testid="hv-btn" type="button" style="display:none">Hidden</button>
  <button class="hidden-plus-visible" data-testid="hv-btn" type="button">Visible</button>
</div>

<div id="dup-gate">
  <button id="dup-one" class="dup-gate-btn" data-testid="dup-gate-testid" type="button"
    onclick="document.getElementById('locator-result').textContent = 'dup-1';">Gate Duplicate</button>
  <button id="dup-two" class="dup-gate-btn" data-testid="dup-gate-testid" type="button"
    onclick="document.getElementById('locator-result').textContent = 'dup-2';">Gate Duplicate</button>
</div>

<ul id="many-items">
  <li id="many-1" class="many-item">First candidate</li>
  <li id="many-2" class="many-item">Second candidate with a very long text payload that definitely exceeds sixty characters in length</li>
  <li id="many-3" class="many-item">Secret token {secret}</li>
  <li id="many-4" class="many-item">Item 4</li>
  <li id="many-5" class="many-item">Item 5</li>
  <li id="many-6" class="many-item">Item 6</li>
  <li id="many-7" class="many-item">Item 7</li>
</ul>

<p id="hidden-note" data-testid="hidden-note" style="display:none">Hidden note</p>
</body></html>"#
    )
}

/// A page for testing actionability checks on `browser.click` and `browser.type`.
fn browser_actionability() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Actionability probe</title></head>
<body>
<h1>Actionability probe</h1>
<p id="action-result">none</p>

<button id="always-disabled" disabled type="button"
  onclick="document.getElementById('action-result').textContent = 'always-disabled';">Always Disabled</button>
<button id="always-aria-disabled" aria-disabled="true" type="button"
  onclick="document.getElementById('action-result').textContent = 'always-aria-disabled';">Aria Disabled</button>
<input id="disabled-input" disabled autocomplete="off"
  oninput="document.getElementById('action-result').textContent = 'typed:' + this.value;">

<button id="start-enable" type="button"
  onclick="setTimeout(function () {
    document.getElementById('enable-later').disabled = false;
  }, 250);">Start Enable</button>
<button id="enable-later" disabled type="button"
  onclick="document.getElementById('action-result').textContent = 'enabled-clicked';">Enable Later</button>

<button id="start-enable-input" type="button"
  onclick="setTimeout(function () {
    document.getElementById('enable-input-later').removeAttribute('aria-disabled');
  }, 250);">Start Enable Input</button>
<input id="enable-input-later" aria-disabled="true" autocomplete="off"
  oninput="document.getElementById('action-result').textContent = 'aria-typed:' + this.value;">

<div style="height:44px;">
  <button id="always-moving" type="button" style="position:relative;left:0px;"
    onclick="document.getElementById('action-result').textContent = 'moving-clicked';">Always Moving</button>
</div>
<script>
(function () {
  var offset = 0;
  setInterval(function () {
    offset = (offset + 7) % 1000;
    var el = document.getElementById('always-moving');
    if (el) el.style.left = offset + 'px';
  }, 15);
})();
</script>

<div style="height:44px;">
  <button id="start-move" type="button"
    onclick="
      var step = 0;
      var btn = document.getElementById('settle-later');
      var timer = setInterval(function () {
        step += 1;
        btn.style.left = (step * 30) + 'px';
        if (step >= 6) {
          clearInterval(timer);
          btn.style.left = '200px';
          btn.dataset.settled = 'yes';
        }
      }, 30);
    ">Start Move</button>
  <button id="settle-later" type="button" style="position:relative;left:0px;"
    onclick="document.getElementById('action-result').textContent = 'settle:' + (this.dataset.settled || 'no');">Settle Later</button>
</div>

<div style="position:relative;width:220px;height:40px;">
  <button id="occluded-btn" type="button" style="width:220px;height:40px;"
    onclick="document.getElementById('action-result').textContent = 'occluded-clicked';">Occluded Target</button>
  <div id="blocker-overlay" style="position:absolute;inset:0;background:rgba(0,0,0,0.2);"
    onclick="document.getElementById('action-result').textContent = 'blocker-clicked';">Blocking Overlay</div>
</div>

<div style="position:relative;width:220px;height:32px;">
  <input id="occluded-input" style="width:220px;height:30px;" autocomplete="off"
    oninput="document.getElementById('action-result').textContent = 'occluded-typed:' + this.value;">
  <div id="input-blocker" style="position:absolute;inset:0;background:rgba(0,0,0,0.2);">Input Blocker</div>
</div>

<button id="start-uncover" type="button"
  onclick="setTimeout(function () {
    document.getElementById('temp-blocker').style.display = 'none';
  }, 250);">Start Uncover</button>
<div style="position:relative;width:220px;height:40px;">
  <button id="uncover-later" type="button" style="width:220px;height:40px;"
    onclick="document.getElementById('action-result').textContent = 'uncovered-clicked';">Uncover Later</button>
  <div id="temp-blocker" style="position:absolute;inset:0;background:rgba(0,0,0,0.2);">Temp Blocker</div>
</div>

<button id="child-btn" type="button"
  onclick="document.getElementById('action-result').textContent = 'child-clicked';"><span id="inner-label">Nested Child</span></button>
</body></html>"#
}

fn browser_probe_iframe() -> &'static str {
    r#"<!doctype html>
<html><head><title>Probe Iframe</title></head>
<body>
<iframe id="probe-frame" src="/browser/probe/iframe-inner" width="240" height="80"></iframe>
</body></html>"#
}

fn browser_iframes_host(query: &str) -> String {
    let cross_port = query_value(query, "cross_port").parse::<u16>().unwrap_or(0);
    let cross_iframe = if cross_port > 0 {
        format!(
            r#"<iframe id="cross-origin-frame" src="http://127.0.0.1:{cross_port}/browser/probe/iframe-inner" width="240" height="80"></iframe>"#
        )
    } else {
        String::new()
    };
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Iframe Host</title></head>
<body>
<button id="decoy-btn" type="button"
  style="position:fixed;left:10px;top:10px;width:140px;height:50px;"
  onclick="document.getElementById('decoy-result').textContent='decoy-clicked';">Decoy</button>
<p id="decoy-result" style="position:fixed;left:10px;top:72px;">decoy-idle</p>

<iframe id="embed" data-testid="embed-frame" src="/browser/iframes/inner"
  style="position:fixed;left:180px;top:240px;width:440px;height:340px;border:6px solid #333;padding:8px;margin:0;background:#fff;"></iframe>

<div style="position:fixed;left:660px;top:20px;">
  {cross_iframe}
  <iframe id="sandboxed-frame" sandbox="" src="/browser/probe/iframe-inner" width="240" height="80"></iframe>
</div>
</body></html>"#
    )
}

fn browser_iframes_inner() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Inner Frame</title></head>
<body style="margin:0;">
<button id="frame-btn" data-testid="frame-btn" type="button"
  style="position:fixed;left:10px;top:10px;width:140px;height:50px;"
  onclick="document.getElementById('frame-result').textContent='frame-clicked';">Frame Action</button>
<p id="frame-result" style="margin:70px 0 4px 10px;">idle</p>

<div id="inner-section" role="region" aria-label="Inner Panel" style="margin-left:10px;">
  <button id="panel-btn" class="dup-btn" data-testid="panel-btn" type="button"
    onclick="document.getElementById('frame-result').textContent='inner-panel-clicked';">Panel Save</button>
</div>
<div id="other-section" style="margin-left:10px;">
  <button class="dup-btn" data-testid="panel-btn" type="button"
    onclick="document.getElementById('frame-result').textContent='other-panel-clicked';">Other Save</button>
</div>

<div style="margin:4px 0 4px 10px;">
  <input id="frame-input" data-testid="frame-input" aria-label="Frame Note" autocomplete="off"
    oninput="document.getElementById('frame-result').textContent='typed:' + this.value;">
  <button id="frame-alert-btn" type="button"
    onclick="window.alert('inner alert'); document.getElementById('frame-result').textContent='alert-ok';">Frame Alert</button>
  <button id="frame-confirm-btn" type="button"
    onclick="document.getElementById('frame-result').textContent = window.confirm('Proceed inside?') ? 'confirmed' : 'dismissed';">Frame Confirm</button>
</div>

<iframe id="nested-frame" data-testid="nested-frame" src="/browser/iframes/nested"
  style="border:4px solid #666;padding:6px;margin-left:10px;width:380px;height:140px;display:block;"></iframe>
</body></html>"#
}

fn browser_iframes_nested() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Nested Frame</title></head>
<body style="margin:4px;">
<p id="nested-result">nested-idle</p>
<div class="nested-box">
  <button id="nested-btn" data-testid="nested-btn" type="button"
    onclick="document.getElementById('nested-result').textContent='nested-clicked';">Nested Click</button>
</div>
<input id="nested-input" data-testid="nested-input" aria-label="Nested Field" autocomplete="off"
  oninput="document.getElementById('nested-result').textContent='nested-typed:' + this.value;">
<button id="spawn-nested-badge" type="button"
  onclick="setTimeout(function () { var s = document.createElement('span'); s.id = 'nested-badge'; s.textContent = 'nested-ready'; document.body.appendChild(s); }, 200);">Spawn Badge</button>
</body></html>"#
}

fn browser_iframes_occlusion() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Iframe Occlusion</title></head>
<body style="margin:0;">
<iframe id="occ-frame" src="/browser/iframes/occ-inner"
  style="position:fixed;left:40px;top:40px;width:340px;height:200px;border:4px solid #333;padding:6px;background:#fff;"></iframe>

<div id="top-modal-backdrop"
  style="position:fixed;left:40px;top:40px;width:360px;height:80px;background:rgba(0,0,0,0.25);z-index:10;"
  onclick="document.getElementById('top-modal-log').textContent='modal-clicked';">Top Modal Backdrop</div>

<div id="temp-modal-backdrop"
  style="position:fixed;left:40px;top:120px;width:360px;height:70px;background:rgba(0,0,0,0.2);z-index:10;">Temp Modal</div>

<button id="uncover-iframe-modal" type="button" style="position:fixed;left:420px;top:40px;"
  onclick="setTimeout(function () { document.getElementById('temp-modal-backdrop').style.display = 'none'; }, 250);">Uncover Modal</button>
<p id="top-modal-log" style="position:fixed;left:420px;top:80px;">none</p>

<iframe id="offscreen-frame" src="/browser/probe/iframe-inner"
  style="margin-top:1600px;width:280px;height:120px;"></iframe>
</body></html>"#
}

fn browser_iframes_occ_inner() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Occlusion Inner</title></head>
<body style="margin:0;">
<button id="top-blocked-btn" type="button"
  style="position:fixed;left:10px;top:10px;width:200px;height:40px;"
  onclick="document.getElementById('occ-result').textContent='top-blocked-clicked';">Top Blocked</button>

<button id="temp-blocked-btn" type="button"
  style="position:fixed;left:10px;top:80px;width:200px;height:40px;"
  onclick="document.getElementById('occ-result').textContent='temp-blocked-clicked';">Temp Blocked</button>

<div style="position:fixed;left:10px;top:135px;width:200px;height:36px;">
  <button id="inner-blocked-btn" type="button" style="width:200px;height:36px;">Inner Blocked</button>
  <div id="inner-overlay" style="position:absolute;inset:0;background:rgba(0,0,0,0.2);">Inner Overlay</div>
</div>

<p id="occ-result" style="position:fixed;left:220px;top:10px;">occ-idle</p>
</body></html>"#
}

fn browser_iframes_slow_host() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Slow Iframe Host</title></head>
<body>
<button id="spawn-lazy-frame" type="button"
  onclick="var f = document.createElement('iframe'); f.id = 'lazy-frame'; f.src = '/browser/iframes/slow-inner'; f.width = '240'; f.height = '80'; document.body.appendChild(f);">Spawn Lazy Frame</button>
</body></html>"#
}

fn browser_iframes_frameset() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Frameset Host</title></head>
<frameset cols="50%,50%">
  <frame id="frame-left" name="left" src="/browser/probe/iframe-inner">
  <frame id="frame-right" name="right" src="/browser/iframes/nested">
</frameset>
</html>"#
}

fn browser_iframes_retry() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Retry Iframe Host</title></head>
<body style="margin:0;">
<iframe id="retry-frame" src="/browser/dialogs/retry"
  style="position:fixed;left:80px;top:60px;width:360px;height:180px;border:4px solid #333;padding:6px;background:#fff;"></iframe>
</body></html>"#
}

fn browser_iframes_scaled_host(query: &str) -> String {
    let overlay = match query_value(query, "overlay").as_str() {
        "unscaled" => {
            r#"<div id="unscaled-modal" style="position:fixed;left:170px;top:105px;width:50px;height:30px;background:rgba(0,0,0,0.2);z-index:10;">Unscaled Modal</div>"#
        }
        "scaled" => {
            r#"<div id="scaled-modal" style="position:fixed;left:240px;top:140px;width:70px;height:40px;background:rgba(0,0,0,0.2);z-index:10;">Scaled Modal</div>"#
        }
        _ => "",
    };
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Scaled Iframe Host</title></head>
<body style="margin:0;background:#fff;">
<iframe id="scaled-frame" src="/browser/iframes/scaled-inner"
  style="position:fixed;left:40px;top:40px;width:320px;height:320px;border:6px solid #333;padding:8px;margin:0;background:#fff;transform-origin:top left;transform:scale(1.5);"></iframe>
<div id="scale-wrapper" style="position:fixed;left:580px;top:40px;transform-origin:top left;transform:scale(1.5);">
  <iframe id="ancestor-scaled-frame" src="/browser/iframes/scaled-inner"
    style="width:260px;height:180px;border:4px solid #333;padding:6px;margin:0;background:#fff;"></iframe>
</div>
{overlay}
</body></html>"#
    )
}

fn browser_iframes_scaled_inner() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Scaled Inner Frame</title></head>
<body style="margin:0;background:#fff;">
<button id="unscaled-decoy" type="button"
  style="position:fixed;left:50px;top:20px;width:55px;height:26px;"
  onclick="document.getElementById('decoy-hit').textContent='decoy-clicked';">Decoy</button>
<p id="decoy-hit" style="position:fixed;left:10px;top:5px;margin:0;">idle</p>
<button id="scaled-btn" type="button"
  style="position:fixed;left:100px;top:50px;width:80px;height:30px;"
  onclick="this.textContent='done';document.getElementById('scaled-result').textContent='done';">Click</button>
<p id="scaled-result" style="position:fixed;left:10px;top:60px;margin:0;">idle</p>
<input id="scaled-pw" type="password" value="scaled-secret" autocomplete="off"
  style="position:fixed;left:100px;top:95px;width:120px;height:28px;background:#ff0000;color:#ff0000;border:2px solid #ff0000;padding:0;margin:0;outline:none;">
<iframe id="nested-scaled" src="/browser/iframes/nested"
  style="position:fixed;left:10px;top:140px;width:240px;height:120px;border:3px solid #555;padding:4px;margin:0;background:#fff;transform-origin:top left;transform:scale(1.2);"></iframe>
</body></html>"#
}

fn browser_iframes_unmappable_transform(query: &str) -> String {
    match query_value(query, "kind").as_str() {
        "skew" => r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Skewed Iframe</title></head>
<body style="margin:0;background:#fff;">
<iframe id="bad-frame" src="/browser/iframes/scaled-inner"
  style="position:fixed;left:80px;top:80px;width:300px;height:220px;transform-origin:top left;transform:skewX(20deg);"></iframe>
</body></html>"#
            .to_string(),
        "flip" => r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Flipped Iframe</title></head>
<body style="margin:0;background:#fff;">
<iframe id="bad-frame" src="/browser/iframes/scaled-inner"
  style="position:fixed;left:80px;top:80px;width:300px;height:220px;transform-origin:center;transform:scale(-1, 1);"></iframe>
</body></html>"#
            .to_string(),
        "3d" => r#"<!doctype html>
<html><head><meta charset="utf-8"><title>3D Iframe</title></head>
<body style="margin:0;background:#fff;">
<iframe id="bad-frame" src="/browser/iframes/scaled-inner"
  style="position:fixed;left:80px;top:80px;width:300px;height:220px;transform:perspective(400px) rotateY(35deg);"></iframe>
</body></html>"#
            .to_string(),
        "ancestor_rotate" => r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Ancestor Rotated Iframe</title></head>
<body style="margin:0;background:#fff;">
<div style="position:fixed;left:80px;top:80px;transform-origin:top left;transform:rotate(15deg);">
  <iframe id="bad-frame" src="/browser/iframes/scaled-inner" style="width:300px;height:220px;"></iframe>
</div>
</body></html>"#
            .to_string(),
        "rotate_no_inputs" => r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Rotated Frame Without Inputs</title></head>
<body style="margin:0;background:#fff;">
<iframe id="bad-frame" src="/browser/probe/iframe-inner"
  style="position:fixed;left:80px;top:80px;width:240px;height:100px;transform-origin:top left;transform:rotate(25deg);"></iframe>
</body></html>"#
            .to_string(),
        _ => r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Rotated Iframe</title></head>
<body style="margin:0;background:#fff;">
<iframe id="bad-frame" src="/browser/iframes/scaled-inner"
  style="position:fixed;left:80px;top:80px;width:300px;height:220px;transform-origin:top left;transform:rotate(25deg);"></iframe>
</body></html>"#
            .to_string(),
    }
}

fn browser_mask_iframe() -> &'static str {
    r##"<!doctype html>
<html><head><meta charset="utf-8"><title>Mask Iframe Probe</title>
<style>
body{margin:0;font:16px sans-serif;background:#fff}
#top-pw{position:fixed;left:20px;top:30px;width:200px;height:32px;color:#f00}
#mask-frame{position:fixed;left:80px;top:100px;width:440px;height:260px;border:5px solid #333;padding:8px;background:#fff}
#mask-sandbox{position:fixed;left:560px;top:100px;width:140px;height:60px}
#mask-report{position:fixed;left:20px;top:400px}
</style></head>
<body>
<input id="top-pw" type="password" value="top-secret" autocomplete="off">
<iframe id="mask-frame" src="/browser/mask-iframe-inner"></iframe>
<iframe id="mask-sandbox" sandbox="" src="/browser/probe/iframe-inner"></iframe>
<p id="mask-report">not recorded</p>
<script>
function boxInTop(el, frames) {
  var r = el.getBoundingClientRect();
  var ox = 0;
  var oy = 0;
  for (var i = 0; i < frames.length; i++) {
    var f = frames[i];
    var fr = f.getBoundingClientRect();
    var fs = f.ownerDocument.defaultView.getComputedStyle(f);
    ox += fr.left + (f.clientLeft || parseFloat(fs.borderLeftWidth) || 0) + (parseFloat(fs.paddingLeft) || 0);
    oy += fr.top + (f.clientTop || parseFloat(fs.borderTopWidth) || 0) + (parseFloat(fs.paddingTop) || 0);
  }
  return { left: ox + r.left, top: oy + r.top, right: ox + r.right, bottom: oy + r.bottom };
}
function isRectCovered(rect, masks) {
  for (var i = 0; i < masks.length; i++) {
    var mask = masks[i];
    var box = mask.getBoundingClientRect();
    var style = window.getComputedStyle(mask);
    if (style.opacity === '1' &&
        style.zIndex === '2147483647' &&
        box.left <= rect.left + 0.5 &&
        box.top <= rect.top + 0.5 &&
        box.right >= rect.right - 0.5 &&
        box.bottom >= rect.bottom - 0.5) {
      return true;
    }
  }
  return false;
}
function recordIframeMasks() {
  var masks = document.querySelectorAll('[data-da-screenshot-mask]');
  if (!masks.length) return;
  var f1 = document.getElementById('mask-frame');
  var d1 = f1 && f1.contentDocument;
  var f2 = d1 && d1.getElementById('nested-mask-frame');
  var d2 = f2 && f2.contentDocument;
  var topPw = document.getElementById('top-pw');
  var framePw = d1 && d1.getElementById('iframe-pw');
  var frameNote = d1 && d1.getElementById('iframe-note');
  var nestedPw = d2 && d2.getElementById('nested-pw');
  if (!topPw || !framePw || !frameNote || !nestedPw) return;
  var parts = [
    'top_pw=' + isRectCovered(boxInTop(topPw, []), masks),
    'iframe_pw=' + isRectCovered(boxInTop(framePw, [f1]), masks),
    'iframe_note=' + isRectCovered(boxInTop(frameNote, [f1]), masks),
    'nested_pw=' + isRectCovered(boxInTop(nestedPw, [f1, f2]), masks)
  ];
  document.getElementById('mask-report').textContent = parts.join(',');
}
var observer = new MutationObserver(function (records) {
  for (var i = 0; i < records.length; i++) {
    var added = records[i].addedNodes;
    for (var j = 0; j < added.length; j++) {
      var node = added[j];
      if (node.nodeType === 1 && node.hasAttribute('data-da-screenshot-mask')) {
        recordIframeMasks();
        return;
      }
    }
  }
});
observer.observe(document.documentElement, { childList: true, subtree: true });
</script>
</body></html>"##
}

fn browser_mask_iframe_inner() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Mask Frame Inner</title></head>
<body style="margin:0;background:#fff;">
<input id="iframe-pw" type="password" value="iframe-secret" autocomplete="off"
  style="position:fixed;left:16px;top:16px;width:220px;height:32px;color:#f00;">
<textarea id="iframe-note" style="position:fixed;left:16px;top:64px;width:220px;height:40px;color:#f00;">secret note</textarea>
<iframe id="nested-mask-frame" src="/browser/mask-iframe-nested"
  style="position:fixed;left:16px;top:120px;width:360px;height:100px;border:3px solid #555;padding:4px;background:#fff;"></iframe>
</body></html>"#
}

fn browser_mask_iframe_nested() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Mask Frame Nested</title></head>
<body style="margin:0;background:#fff;">
<input id="nested-pw" type="password" value="nested-secret" autocomplete="off"
  style="position:fixed;left:12px;top:12px;width:200px;height:30px;color:#f00;">
</body></html>"#
}

fn browser_probe_shadow() -> &'static str {
    r#"<!doctype html>
<html><head><title>Probe Shadow DOM</title></head>
<body>
<div id="shadow-host"></div>
<p id="shadow-status">idle</p>
<script>
var root = document.getElementById('shadow-host').attachShadow({ mode: 'open' });
var btn = document.createElement('button');
btn.id = 'inside-shadow';
btn.textContent = 'Inside Shadow';
btn.onclick = function () { document.getElementById('shadow-status').textContent = 'clicked'; };
root.appendChild(btn);
</script>
</body></html>"#
}

/// A page whose controls live inside open shadow roots of several depths.
///
/// Every control records its own name into the light-DOM `#shadow-status`
/// paragraph when it is clicked, so one case proves which element `da` actually
/// reached: a click that lands on a shadow host instead of the control inside it
/// records nothing.
///
/// `?overlay=1` adds a light-DOM overlay above everything, which must still be
/// reported as occluding a control that lives inside a shadow root.
fn browser_shadow(query: &str) -> String {
    let overlay = if query_value(query, "overlay") == "1" {
        r#"<div id="real-overlay"></div>"#
    } else {
        ""
    };
    format!(
        r##"<!doctype html>
<html><head><meta charset="utf-8"><title>Shadow matrix</title>
<style>
  body {{ margin: 0; padding: 8px; font: 16px sans-serif; color: #111; background: #fff; }}
  button, input, .row, p {{ font: 16px sans-serif; margin: 6px; padding: 6px 10px; min-width: 180px; }}
  .row {{ display: block; border: 1px solid #333; }}
  #real-overlay {{ position: fixed; left: 0; top: 0; width: 100%; height: 100%; z-index: 9999; background: #000; opacity: 0.01; }}
</style></head>
<body>
<h1>Shadow matrix</h1>
<div id="single-host"></div>
<div id="field-host"></div>
<div id="text-host"></div>
<div id="depth-host"></div>
<div id="nth-host"></div>
<div id="within-host"></div>
<div id="frame-host"></div>
<div id="overlay-host"></div>
<p id="shadow-status">idle</p>
<p id="shadow-typed">idle</p>
{overlay}
<script>
function record(name) {{
  var status = document.getElementById('shadow-status');
  status.textContent = (status.textContent === 'idle' ? '' : status.textContent + ',') + name;
}}
function openRoot(id) {{
  return document.getElementById(id).attachShadow({{ mode: 'open' }});
}}
function makeButton(id, label, name) {{
  var button = document.createElement('button');
  button.setAttribute('id', id);
  button.type = 'button';
  button.textContent = label;
  button.addEventListener('click', function () {{ record(name); }});
  return button;
}}
openRoot('single-host').appendChild(makeButton('single-button', 'Single Shadow', 'single'));
var fieldRoot = openRoot('field-host');
var field = document.createElement('input');
field.setAttribute('id', 'shadow-field');
field.setAttribute('data-testid', 'shadow-field');
field.type = 'text';
var note = function () {{ document.getElementById('shadow-typed').textContent = field.value; }};
field.addEventListener('input', note);
field.addEventListener('keyup', note);
fieldRoot.appendChild(field);
var textRoot = openRoot('text-host');
var prefix = document.createElement('span');
prefix.textContent = 'prefix';
textRoot.appendChild(prefix);
textRoot.appendChild(makeButton('text-button', 'Composed Text', 'text'));
var depthRoot = openRoot('depth-host');
var innerHost = document.createElement('div');
innerHost.setAttribute('id', 'inner-host');
depthRoot.appendChild(innerHost);
var innerRoot = innerHost.attachShadow({{ mode: 'open' }});
innerRoot.appendChild(makeButton('nested-button', 'Nested Shadow', 'nested'));
var marker = document.createElement('span');
marker.setAttribute('data-testid', 'nested-marker');
marker.textContent = 'nested marker';
innerRoot.appendChild(marker);
var nthRoot = openRoot('nth-host');
for (var i = 0; i < 3; i++) {{
  var row = document.createElement('div');
  row.setAttribute('class', 'row');
  row.setAttribute('id', 'row-' + i);
  row.textContent = 'row ' + i;
  row.addEventListener('click', (function (index) {{
    return function () {{ record('row' + index); }};
  }})(i));
  nthRoot.appendChild(row);
}}
openRoot('within-host').appendChild(
  makeButton('within-button', 'Scoped Shadow', 'within')
);
var frameRoot = openRoot('frame-host');
var frame = document.createElement('iframe');
frame.setAttribute('id', 'shadow-frame');
frame.setAttribute('src', '/browser/probe/iframe-inner');
frame.setAttribute('width', '260');
frame.setAttribute('height', '90');
frameRoot.appendChild(frame);
openRoot('overlay-host').appendChild(makeButton('overlay-target', 'Overlay Target', 'overlay'));
</script>
</body></html>"##
    )
}

/// A page whose only control lives inside a **closed** shadow root.
///
/// No page script can look into a closed root (`host.shadowRoot` is `null`), so
/// `da` has to explain the closed root instead of reporting a plain miss.
fn browser_shadow_closed() -> &'static str {
    r#"<!doctype html>
<html><head><meta charset="utf-8"><title>Closed shadow</title></head>
<body>
<div id="closed-host"></div>
<p id="closed-status">idle</p>
<script>
var root = document.getElementById('closed-host').attachShadow({ mode: 'closed' });
var button = document.createElement('button');
button.setAttribute('id', 'closed-button');
button.type = 'button';
button.textContent = 'Closed Shadow';
button.addEventListener('click', function () {
  document.getElementById('closed-status').textContent = 'clicked';
});
root.appendChild(button);
</script>
</body></html>"#
}

/// A page that records whether the screenshot masks covered editable fields
/// living inside an open shadow root.
///
/// The `MutationObserver` watches the mask elements the driver appends to
/// `document.documentElement`, the way `browser_mask` does, so the record is
/// taken while the masks exist and survives their removal.
fn browser_shadow_mask() -> &'static str {
    r##"<!doctype html>
<html><head><meta charset="utf-8"><title>Shadow mask</title>
<style>
body{margin:0;font:16px sans-serif;background:#fff}
#shadow-pw{position:fixed;left:20px;top:80px;width:300px;height:44px;font-size:22px;color:#f00}
#shadow-note{position:fixed;left:20px;top:150px;width:300px;height:60px;font-size:20px;color:#f00}
#mask-report{position:fixed;left:20px;top:240px}
</style></head>
<body>
<div id="mask-host"></div>
<p id="mask-report">not recorded</p>
<script>
var root = document.getElementById('mask-host').attachShadow({ mode: 'open' });
var password = document.createElement('input');
password.setAttribute('id', 'shadow-pw');
password.type = 'password';
password.value = 'shadow-secret';
root.appendChild(password);
var note = document.createElement('textarea');
note.setAttribute('id', 'shadow-note');
note.textContent = 'shadow note';
root.appendChild(note);
function isCovered(el, masks) {
  var rect = el.getBoundingClientRect();
  for (var i = 0; i < masks.length; i++) {
    var mask = masks[i];
    var box = mask.getBoundingClientRect();
    var style = window.getComputedStyle(mask);
    if (style.opacity === '1' &&
        style.zIndex === '2147483647' &&
        box.left <= rect.left + 0.5 &&
        box.top <= rect.top + 0.5 &&
        box.right >= rect.right - 0.5 &&
        box.bottom >= rect.bottom - 0.5) {
      return true;
    }
  }
  return false;
}
var observer = new MutationObserver(function (records) {
  for (var i = 0; i < records.length; i++) {
    var added = records[i].addedNodes;
    for (var j = 0; j < added.length; j++) {
      var node = added[j];
      if (node.nodeType === 1 && node.hasAttribute('data-da-screenshot-mask')) {
        var masks = document.querySelectorAll('[data-da-screenshot-mask]');
        document.getElementById('mask-report').textContent =
          'shadow-pw=' + isCovered(password, masks) + ',shadow-note=' + isCovered(note, masks);
        return;
      }
    }
  }
});
observer.observe(document.documentElement, { childList: true, subtree: true });
</script>
</body></html>"##
}

fn browser_probe_new_tab() -> &'static str {
    r#"<!doctype html>
<html><head><title>Probe New Tab</title></head>
<body>
<a id="open-new-tab" href="/browser/probe/new-tab-target" target="_blank">Open Tab</a>
<a id="open-second-tab" href="/browser/probe/new-tab-second" target="_blank">Open Second Tab</a>
<button id="open-window" type="button" onclick="window.open('/browser/probe/new-tab-window');">Open Window</button>
<p id="probe-status">idle</p>
</body></html>"#
}

fn browser_probe_new_tab_target() -> &'static str {
    // The fetch starts half a second after the click, so it finishes during a
    // *later* step: `browser.network` reports a request in the step that
    // completed it, and a case that asserts on it must wait for it there.
    //
    // The button inside an open shadow root proves that the composed locators
    // still work on a tab the session switched to.
    r#"<!doctype html>
<html><head><title>New Tab Target</title></head>
<body>
<p id="new-tab-marker">Opened</p>
<button id="new-tab-fetch" type="button"
  onclick="setTimeout(function () {
    fetch('/browser/probe/new-tab-fetch?case=tab').then(function (r) { return r.text(); }).then(function () {
      setTimeout(function () {
        var p = document.createElement('p');
        p.id = 'new-tab-fetched';
        p.textContent = 'fetched';
        document.body.appendChild(p);
      }, 100);
    });
  }, 500);">Fetch</button>
<p id="new-tab-status">idle</p>
<div id="new-tab-shadow"></div>
<p id="new-tab-shadow-status">idle</p>
<script>
var shadowRootHandle = document.getElementById('new-tab-shadow').attachShadow({ mode: 'open' });
var shadowButton = document.createElement('button');
shadowButton.setAttribute('id', 'new-tab-shadow-button');
shadowButton.type = 'button';
shadowButton.textContent = 'Shadow Button';
shadowButton.addEventListener('click', function () {
  document.getElementById('new-tab-shadow-status').textContent = 'shadow-clicked';
});
shadowRootHandle.appendChild(shadowButton);
</script>
</body></html>"#
}

fn browser_probe_new_tab_window() -> &'static str {
    r#"<!doctype html>
<html><head><title>New Tab Window</title></head>
<body><p id="window-marker">Window</p></body></html>"#
}

fn browser_probe_new_tab_second() -> &'static str {
    r#"<!doctype html>
<html><head><title>Second Tab</title></head>
<body><p id="second-marker">Second</p></body></html>"#
}

fn browser_probe_dialog() -> &'static str {
    r#"<!doctype html>
<html><head><title>Probe Dialog</title></head>
<body>
<button id="trigger-alert" type="button"
  onclick="window.alert('probe alert'); document.getElementById('dialog-status').textContent = 'after-alert';">Alert</button>
<p id="dialog-status">before-alert</p>
</body></html>"#
}

fn browser_dialogs() -> &'static str {
    r#"<!doctype html>
<html><head><title>Dialogs</title></head>
<body>
<button id="btn-alert" type="button"
  onclick="window.alert('unexpected warning'); document.getElementById('status').textContent = 'alert-done';">Alert</button>
<button id="btn-confirm" type="button"
  onclick="document.getElementById('status').textContent = window.confirm('Proceed with delete?') ? 'confirmed' : 'cancelled';">Confirm</button>
<button id="btn-prompt" type="button"
  onclick="var v = window.prompt('Enter token', 'default'); document.getElementById('status').textContent = v === null ? 'prompt-cancelled' : 'prompt:' + v;">Prompt</button>
<input id="type-dialog" type="text"
  oninput="if (this.value === 'go') { document.getElementById('status').textContent = window.confirm('Type confirm?') ? 'type-confirmed' : 'type-cancelled'; }">
<input id="press-input" type="text"
  onkeydown="if (event.key === 'Enter') { window.alert('press alert'); document.getElementById('status').textContent = 'press-done'; }">
<input id="secret-input" type="text">
<button id="btn-secret-alert" type="button"
  onclick="window.alert('secret=' + document.getElementById('secret-input').value);">Secret Alert</button>
<button id="btn-secret-confirm" type="button"
  onclick="document.getElementById('status').textContent = window.confirm('confirm=' + document.getElementById('secret-input').value) ? 'secret-confirmed' : 'secret-cancelled';">Secret Confirm</button>
<p id="status">idle</p>
</body></html>"#
}

fn browser_dialogs_onload() -> &'static str {
    r#"<!doctype html>
<html><head><title>Onload Dialog</title></head>
<body>
<script>window.alert('load alert');</script>
<p id="loaded">ready</p>
</body></html>"#
}

fn browser_dialogs_beforeunload() -> &'static str {
    r#"<!doctype html>
<html><head><title>Beforeunload Dialog</title></head>
<body>
<button id="arm-leave" type="button"
  onclick="document.getElementById('armed').textContent = 'armed';">Arm</button>
<p id="armed">idle</p>
<script>
window.addEventListener('beforeunload', function (e) {
  if (document.getElementById('armed').textContent === 'armed') {
    e.preventDefault();
    e.returnValue = '';
  }
});
</script>
</body></html>"#
}

fn browser_dialogs_retry() -> &'static str {
    // Both buttons arm a timer that is comfortably longer than the click that
    // arms it, so the dialog can only open after that step finished; a retried
    // step is then the one that meets it.
    r#"<!doctype html>
<html><head><title>Retry Dialog</title></head>
<body>
<button id="arm-confirm" type="button"
  onclick="setTimeout(function() { document.getElementById('status').textContent = window.confirm('Ready now?') ? 'confirmed' : 'cancelled'; }, 1500);">Confirm Later</button>
<button id="arm-unexpected" type="button"
  onclick="setTimeout(function() { window.alert('surprise'); document.getElementById('status').textContent = 'after-alert'; }, 2000);">Alert Later</button>
<p id="status">idle</p>
</body></html>"#
}

fn browser_probe_upload() -> &'static str {
    // The probe is honest about both halves: typing into a file input attaches
    // nothing, and `browser.upload` is the step that does attach a file.
    r#"<!doctype html>
<html><head><title>Probe File Upload</title></head>
<body>
<input id="file-upload" type="file">
<p id="file-upload-events">idle</p>
<script>
var probeNode = document.getElementById('file-upload');
var probeEvents = document.getElementById('file-upload-events');
probeNode.addEventListener('change', function () {
  probeEvents.textContent = 'change:' + probeNode.files.length;
});
</script>
</body></html>"#
}

/// The page whose file inputs cover every locator and every refusal.
///
/// `#single-picker` holds exactly one file input, so a `role` locator scoped by
/// `within` resolves without a name: Chrome's accessibility query matches the
/// input by role but not by its label-derived name.
fn browser_upload() -> &'static str {
    r#"<!doctype html>
<html><head><title>Upload Controls</title></head>
<body>
<div id="single-picker">
  <label for="single-file">Single file</label>
  <input id="single-file" name="single" type="file">
</div>
<label for="hidden-file">Choose avatar</label>
<input id="hidden-file" name="hidden" type="file" style="display:none">
<input id="faded-file" name="faded" type="file" multiple style="opacity:0;position:absolute;left:0;top:0;width:0;height:0">
<input id="multi-file" name="multi" type="file" multiple>
<div id="picker">
  <input id="nested-one" name="nested" type="file" data-testid="nested-file">
  <input id="nested-two" name="nested" type="file" data-testid="nested-file">
</div>
<input id="not-a-file-input" name="typed" type="text">
<div id="plain-div">plain</div>
<input id="disabled-file" name="disabled" type="file" disabled>
<fieldset disabled><input id="fieldset-file" name="fieldset" type="file"></fieldset>
<div id="shadow-host"></div>
<p id="file-events">idle</p>
<script>
var uploadEvents = document.getElementById('file-events');
var uploadInputs = document.querySelectorAll('input[type="file"]');
for (var i = 0; i < uploadInputs.length; i++) {
  uploadInputs[i].addEventListener('change', (function (node) {
    return function () {
      var names = [];
      for (var j = 0; j < node.files.length; j++) names.push(node.files[j].name);
      uploadEvents.textContent = node.id + ':' + node.files.length + ':' + names.join(',');
    };
  })(uploadInputs[i]));
}
var uploadShadowRoot = document.getElementById('shadow-host').attachShadow({ mode: 'open' });
uploadShadowRoot.innerHTML = '<label for="shadow-file">Shadow file</label>' +
  '<input id="shadow-file" name="shadow" type="file">';
uploadShadowRoot.getElementById('shadow-file').addEventListener('change', function () {
  var shadowNames = [];
  for (var k = 0; k < this.files.length; k++) shadowNames.push(this.files[k].name);
  uploadEvents.textContent = 'shadow-file:' + this.files.length + ':' + shadowNames.join(',');
});
</script>
</body></html>"#
}

/// A form that submits the chosen file, so the server receives its bytes.
fn browser_upload_form() -> &'static str {
    r#"<!doctype html>
<html><head><title>Upload Form</title></head>
<body>
<form id="upload-form" method="post" enctype="multipart/form-data" action="/browser/upload/submit">
  <label for="avatar-file">Choose avatar</label>
  <input id="avatar-file" name="avatar" type="file">
  <button id="send-avatar" type="submit">Send</button>
</form>
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
</body></html>"#
}

/// The page a submitted upload form lands on.
fn browser_upload_received() -> &'static str {
    r#"<!doctype html>
<html><head><title>Upload Received</title></head><body>
<p id="upload-received">received</p>
</body></html>"#
}

/// A page whose link opens the upload form in a second tab.
fn browser_upload_tab() -> &'static str {
    r#"<!doctype html>
<html><head><title>Upload Tab Origin</title></head><body>
<a id="open-upload-tab" href="/browser/upload/tab-form" target="_blank">Open Upload Tab</a>
<p id="origin-events">idle</p>
</body></html>"#
}

/// The tab `browser_upload_tab` opens: a file input and its change handler.
fn browser_upload_tab_form() -> &'static str {
    r#"<!doctype html>
<html><head><title>Upload Tab Form</title></head><body>
<label for="tab-file">Tab file</label>
<input id="tab-file" name="tab" type="file">
<p id="tab-events">idle</p>
<script>
var tabNode = document.getElementById('tab-file');
var tabEvents = document.getElementById('tab-events');
tabNode.addEventListener('change', function () {
  var names = [];
  for (var i = 0; i < tabNode.files.length; i++) names.push(tabNode.files[i].name);
  tabEvents.textContent = 'tab-file:' + tabNode.files.length + ':' + names.join(',');
});
</script>
</body></html>"#
}

fn browser_probe_coords() -> &'static str {
    r#"<!doctype html>
<html><head><title>Probe Coordinates</title>
<style>
html, body { margin: 0; padding: 0; font: 16px sans-serif; }
#probe-canvas { display: block; }
#probe-editor { position: absolute; left: 0; top: 120px; width: 200px; height: 40px; }
#probe-tab-link { position: absolute; left: 220px; top: 10px; }
#probe-shadow-host { position: absolute; left: 220px; top: 40px; width: 160px; height: 40px; }
</style>
</head>
<body>
<canvas id="probe-canvas" width="200" height="100"></canvas>
<div id="probe-editor" contenteditable="true"></div>
<p id="probe-status">idle</p>
<a id="probe-tab-link" href="/browser/probe/coords?case=tab" target="_blank">Open canvas tab</a>
<div id="probe-shadow-host"></div>
<script>
(function () {
  var canvas = document.getElementById('probe-canvas');
  var context = canvas.getContext('2d');
  context.fillStyle = '#3355aa';
  context.fillRect(80, 30, 40, 40);
  canvas.addEventListener('click', function (event) {
    document.getElementById('probe-status').textContent =
      'canvas ' + event.clientX + ',' + event.clientY;
  });
  var host = document.getElementById('probe-shadow-host');
  var root = host.attachShadow({ mode: 'open' });
  var button = document.createElement('button');
  button.id = 'probe-shadow-button';
  button.textContent = 'Shadow target';
  button.addEventListener('click', function () {
    document.getElementById('probe-status').textContent = 'shadow-clicked';
  });
  root.appendChild(button);
})();
</script>
</body></html>"#
}

/// The value of one `name=value` pair of a query string.
fn query_value(query: &str, name: &str) -> String {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
        .unwrap_or_default()
}

fn query_usize(query: &str, name: &str) -> Option<usize> {
    query_value(query, name).parse::<usize>().ok()
}

/// Escapes text for HTML text and double-quoted attribute values.
fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// A temporary directory removed when the value is dropped.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates an empty temporary directory with a unique name.
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "da-integration-{}-{label}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create the temporary directory");
        Self { path }
    }

    /// The directory path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Writes `contents` to `name` inside the directory, creating parents.
    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("failed to create a case directory");
        }
        fs::write(&path, contents).expect("failed to write a case file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Returns a loopback port that nothing is listening on.
pub fn closed_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind a probe socket");
    let port = listener
        .local_addr()
        .expect("failed to read the probe address")
        .port();
    drop(listener);
    port
}

/// Binds a loopback listener whose port is not on Chrome's restricted-port list
/// (`ERR_UNSAFE_PORT`). Holds any rejected listener open until a safe port is
/// acquired so the OS does not hand back the same restricted port.
pub fn bind_browser_safe_listener() -> TcpListener {
    let mut rejected = Vec::new();
    for _ in 0..32 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind the test server");
        let port = listener
            .local_addr()
            .expect("failed to read the test server address")
            .port();
        if !is_chrome_restricted_port(port) {
            return listener;
        }
        rejected.push(listener);
    }
    TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind the test server")
}

/// Ports Chrome refuses with `net::ERR_UNSAFE_PORT` (from Chromium `port_util.cc`).
fn is_chrome_restricted_port(port: u16) -> bool {
    matches!(
        port,
        1 | 7
            | 9
            | 11
            | 13
            | 15
            | 17
            | 19
            | 20
            | 21
            | 22
            | 23
            | 25
            | 37
            | 42
            | 43
            | 53
            | 69
            | 77
            | 79
            | 87
            | 95
            | 101
            | 102
            | 103
            | 104
            | 109
            | 110
            | 111
            | 113
            | 115
            | 117
            | 119
            | 123
            | 135
            | 137
            | 139
            | 143
            | 161
            | 179
            | 389
            | 427
            | 465
            | 512
            | 513
            | 514
            | 515
            | 526
            | 530
            | 531
            | 532
            | 540
            | 548
            | 554
            | 556
            | 563
            | 587
            | 601
            | 636
            | 989
            | 990
            | 993
            | 995
            | 1719
            | 1720
            | 1723
            | 2049
            | 3659
            | 4045
            | 4190
            | 5060
            | 5061
            | 6000
            | 6566
            | 6665..=6669 | 6697 | 10080
    )
}
