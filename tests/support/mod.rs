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
use std::time::Duration;

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

/// A minimal HTTP server bound to a loopback port, started by the test.
pub struct TestServer {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
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

        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let server_requests = Arc::clone(&requests);
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
                        thread::spawn(move || handle_connection(stream, &requests));
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
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(stream: TcpStream, requests: &Mutex<Vec<Request>>) {
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

    requests
        .lock()
        .expect("test server requests mutex is poisoned")
        .push(Request {
            method: method.clone(),
            target: target.clone(),
            headers: headers.clone(),
            body: body.clone(),
        });

    let (path, query) = target
        .split_once('?')
        .map_or((target.as_str(), ""), |(path, query)| (path, query));
    let response = route(&method, path, query, &headers, &body);

    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn route(
    method: &str,
    path: &str,
    query: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
) -> String {
    match (method, path) {
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
        (_, "/browser/second") => html_response("200 OK", &browser_second(query)),
        // Headers and the start of the page arrive, but the document never
        // finishes loading before a short navigation timeout, which is what
        // `browser.open`'s own `timeout_ms` has to report.
        (_, "/browser/stall") => {
            thread::sleep(Duration::from_secs(5));
            html_response("200 OK", &browser_second(query))
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
<html><head><meta charset="utf-8"><title>Network page</title></head>
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

/// The value of one `name=value` pair of a query string.
fn query_value(query: &str, name: &str) -> String {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
        .unwrap_or_default()
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
fn bind_browser_safe_listener() -> TcpListener {
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
