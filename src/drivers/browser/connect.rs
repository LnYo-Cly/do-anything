//! Finds the DevTools WebSocket of a browser the user already runs.
//!
//! `browser.open` with `connect` names a loopback HTTP endpoint. Chrome answers
//! `GET /json/version` there with the `webSocketDebuggerUrl` of its browser
//! target. The request is one HTTP/1.1 GET over a standard-library socket
//! because the browser driver has no HTTP client of its own and the answer is
//! tiny and local.
//!
//! Only a loopback endpoint is accepted: a debug port is full control of the
//! browser, so it must never be reachable from another host.

use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the endpoint may take to answer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Fast probe timeout used while auto-discovering a local DevTools endpoint.
const AUTO_PROBE_TIMEOUT: Duration = Duration::from_millis(400);
/// Default loopback ports checked during `connect: "auto"` when no `DevToolsActivePort` answers.
const AUTO_PROBE_PORTS: [u16; 2] = [9222, 9229];
/// Environment variable overriding `connect: "auto"` discovery with a loopback endpoint.
pub const CONNECT_ENV_VAR: &str = "DA_BROWSER_CONNECT";

/// The largest response accepted; `/json/version` is a few hundred bytes.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// Parsed form of `browser.open`'s `connect` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectTarget {
    /// Explicit loopback endpoint (`http://127.0.0.1:9222`, `http://localhost:...`, `http://[::1]:...`).
    Explicit(Endpoint),
    /// Discover a live loopback DevTools endpoint (`DA_BROWSER_CONNECT`, `DevToolsActivePort`, or ports 9222/9229).
    Auto,
}

impl ConnectTarget {
    /// Parses `"auto"` or an explicit loopback `http://<host>:<port>` endpoint.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.trim().eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        Endpoint::parse(value).map(Self::Explicit)
    }

    /// Resolves this target into a concrete loopback [`Endpoint`].
    pub fn resolve(&self) -> Result<Endpoint, String> {
        match self {
            Self::Explicit(endpoint) => Ok(endpoint.clone()),
            Self::Auto => Endpoint::discover_auto(),
        }
    }
}

/// A loopback DevTools HTTP endpoint named by `browser.open`'s `connect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    /// Parses `http://<host>:<port>` and refuses anything but loopback.
    pub fn parse(value: &str) -> Result<Self, String> {
        let trimmed = value.trim();
        let Some(rest) = trimmed.strip_prefix("http://") else {
            return Err(format!(
                "invalid arguments: `connect` must be `http://<host>:<port>` with a loopback host, got `{value}`"
            ));
        };
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty()
            || authority
                .chars()
                .any(|character| matches!(character, '/' | '?' | '#' | '@'))
        {
            return Err(format!(
                "invalid arguments: `connect` must be `http://<host>:<port>` with no path, got `{value}`"
            ));
        }
        let (host, port) = split_authority(authority).ok_or_else(|| {
            format!("invalid arguments: `connect` must name a host and a port, got `{value}`")
        })?;
        if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
            return Err(format!(
                "invalid arguments: `connect` must use a loopback host: `127.0.0.1`, `localhost` or `[::1]`; got `{host}`"
            ));
        }
        let port = port.parse::<u16>().map_err(|error| {
            format!("invalid arguments: `connect` has no usable port `{port}`: {error}")
        })?;
        if port == 0 {
            return Err(
                "invalid arguments: `connect` must use a port between 1 and 65535".to_string(),
            );
        }
        Ok(Self { host, port })
    }

    /// The endpoint as `connect` names it, without a trailing slash.
    pub fn name(&self) -> String {
        format!("http://{}:{}", self.host, self.port)
    }

    /// The debug port the endpoint listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Discovers an active loopback DevTools endpoint for `connect: "auto"`.
    pub fn discover_auto() -> Result<Self, String> {
        let env_override = std::env::var(CONNECT_ENV_VAR)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let active_port_files = default_active_port_files();
        Self::discover_from(
            env_override.as_deref(),
            &active_port_files,
            &AUTO_PROBE_PORTS,
            AUTO_PROBE_TIMEOUT,
        )
    }

    /// Testable core of `discover_auto`.
    fn discover_from(
        env_override: Option<&str>,
        active_port_files: &[PathBuf],
        probe_ports: &[u16],
        probe_timeout: Duration,
    ) -> Result<Self, String> {
        if let Some(raw) = env_override {
            let endpoint = Self::parse(raw).map_err(|err| {
                format!("`{CONNECT_ENV_VAR}` is not a valid loopback DevTools endpoint: {err}")
            })?;
            return Ok(endpoint);
        }

        let mut seen_ports = Vec::new();
        for path in active_port_files {
            if let Some(port) = read_active_port_file(path)
                && !seen_ports.contains(&port)
            {
                seen_ports.push(port);
                let candidate = Self {
                    host: "127.0.0.1".to_string(),
                    port,
                };
                if candidate.websocket_url(probe_timeout).is_ok() {
                    return Ok(candidate);
                }
            }
        }
        for &port in probe_ports {
            if port == 0 || seen_ports.contains(&port) {
                continue;
            }
            seen_ports.push(port);
            let candidate = Self {
                host: "127.0.0.1".to_string(),
                port,
            };
            if candidate.websocket_url(probe_timeout).is_ok() {
                return Ok(candidate);
            }
        }

        Err(format!(
            "could not auto-discover a local Chrome/Edge DevTools endpoint (checked `{CONNECT_ENV_VAR}`, Chrome/Edge `DevToolsActivePort`, and loopback ports {}); start Chrome or Edge with `--remote-debugging-port=9222`, or set `{CONNECT_ENV_VAR}=http://127.0.0.1:<port>`",
            probe_ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    /// Reads `/json/version` and returns its `webSocketDebuggerUrl`.
    pub fn websocket_url(&self, timeout: Duration) -> Result<String, String> {
        let (status, body) = self
            .get_json_version(timeout)
            .map_err(|detail| self.explain(detail))?;
        if status != 200 {
            return Err(self.explain(format!(
                "the endpoint answered HTTP {status}; it is not a Chrome DevTools endpoint"
            )));
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| self.explain(format!("the answer is not JSON: {error}")))?;
        let url = value
            .get("webSocketDebuggerUrl")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                self.explain("the answer has no `webSocketDebuggerUrl`; it is not a Chrome DevTools endpoint")
            })?;
        if !url.starts_with("ws://") {
            return Err(self.explain(format!(
                "the answer names `{url}`, which is not a ws:// DevTools URL"
            )));
        }
        Ok(url.to_string())
    }

    /// Prefixes a failure with what the user can do about it.
    pub fn explain(&self, detail: impl fmt::Display) -> String {
        format!(
            "could not use the browser at {}: {detail}; start Chrome with `--remote-debugging-port={}` and keep it running",
            self.name(),
            self.port
        )
    }

    /// Sends the request and reads the status and body.
    fn get_json_version(&self, timeout: Duration) -> Result<(u16, String), String> {
        let deadline = Instant::now() + timeout;
        let address = format!("{}:{}", self.host, self.port);
        let addresses = address
            .to_socket_addrs()
            .map_err(|error| format!("`{address}` is not a usable address: {error}"))?;
        let mut stream = connect_any(addresses, deadline)
            .map_err(|error| format!("could not connect to `{address}`: {error}"))?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        stream
            .set_write_timeout(Some(remaining))
            .map_err(|error| format!("could not configure the connection: {error}"))?;
        let request = format!(
            "GET /json/version HTTP/1.1\r\nHost: {}:{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
            self.host, self.port
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("could not send the request: {error}"))?;
        read_response(&mut stream, deadline)
    }
}

/// Connects to the first address that accepts, so `localhost` can try both
/// `127.0.0.1` and `::1`.
fn connect_any(
    addresses: impl Iterator<Item = std::net::SocketAddr>,
    deadline: Instant,
) -> Result<TcpStream, String> {
    let mut last = None;
    for socket in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the connection timed out".to_string());
        }
        match TcpStream::connect_timeout(&socket, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(format!("{socket}: {error}")),
        }
    }
    Err(last.unwrap_or_else(|| "the host did not resolve to an address".to_string()))
}

/// Splits `host:port` and `[::1]:port`.
fn split_authority(authority: &str) -> Option<(String, &str)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        return Some((format!("[{host}]"), port));
    }
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_string(), port))
}

/// Reads one HTTP response, using `content-length` to know when the body ends.
fn read_response(stream: &mut TcpStream, deadline: Instant) -> Result<(u16, String), String> {
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut body_start = None;
    loop {
        if body_start.is_none()
            && let Some(index) = response.windows(4).position(|window| window == b"\r\n\r\n")
        {
            body_start = Some(index + 4);
        }
        if let Some(start) = body_start {
            let header = String::from_utf8_lossy(&response[..start]);
            if let Some(length) = content_length(&header)
                && response.len() >= start + length
            {
                break;
            }
        }
        if response.len() >= MAX_RESPONSE_BYTES {
            return Err("the endpoint answered more than 1 MiB".to_string());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("the endpoint did not answer before the timeout".to_string());
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|error| format!("could not configure the connection: {error}"))?;
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => response.extend_from_slice(&buffer[..read]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return Err("the endpoint did not answer before the timeout".to_string());
            }
            Err(error) => return Err(format!("could not read the answer: {error}")),
        }
    }

    let start = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| "the endpoint answered without a header block".to_string())?;
    let header = String::from_utf8_lossy(&response[..start]);
    let status = header
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| format!("the endpoint answered without a status line: {header}"))?;
    let body = String::from_utf8_lossy(&response[start..]).into_owned();
    Ok((status, body))
}

/// The `content-length` of one response header block.
fn content_length(header: &str) -> Option<usize> {
    header.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    })
}

/// Reads line 1 of a Chrome/Edge `DevToolsActivePort` file as a non-zero TCP port.
fn read_active_port_file(path: &Path) -> Option<u16> {
    let text = fs::read_to_string(path).ok()?;
    let port: u16 = text.lines().next()?.trim().parse().ok()?;
    (port > 0).then_some(port)
}

/// Well-known Chrome/Edge/Chromium `DevToolsActivePort` file locations for the current OS.
fn default_active_port_files() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    #[cfg(windows)]
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let root = PathBuf::from(local_app_data);
        paths.push(root.join("Google/Chrome/User Data/DevToolsActivePort"));
        paths.push(root.join("Microsoft/Edge/User Data/DevToolsActivePort"));
        paths.push(root.join("Chromium/User Data/DevToolsActivePort"));
    }
    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        let root = PathBuf::from(home).join("Library/Application Support");
        paths.push(root.join("Google/Chrome/DevToolsActivePort"));
        paths.push(root.join("Microsoft Edge/DevToolsActivePort"));
        paths.push(root.join("Chromium/DevToolsActivePort"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Ok(home) = std::env::var("HOME") {
        let root = PathBuf::from(home).join(".config");
        paths.push(root.join("google-chrome/DevToolsActivePort"));
        paths.push(root.join("microsoft-edge/DevToolsActivePort"));
        paths.push(root.join("chromium/DevToolsActivePort"));
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_endpoint_is_accepted() {
        for value in [
            ("http://127.0.0.1:9222", "http://127.0.0.1:9222"),
            ("http://localhost:9222", "http://localhost:9222"),
            ("http://[::1]:9222", "http://[::1]:9222"),
            ("http://127.0.0.1:9222/", "http://127.0.0.1:9222"),
        ] {
            let (value, expected) = value;
            let endpoint =
                Endpoint::parse(value).unwrap_or_else(|error| panic!("{value}: {error}"));
            assert_eq!(endpoint.port(), 9222, "{value}");
            assert_eq!(endpoint.name(), expected, "{value}");
        }
    }

    #[test]
    fn a_non_loopback_or_malformed_endpoint_is_refused() {
        for value in [
            "https://127.0.0.1:9222",
            "http://192.168.1.10:9222",
            "http://example.com:9222",
            "http://[::2]:9222",
            "http://127.0.0.1",
            "http://127.0.0.1:not-a-port",
            "http://127.0.0.1:0",
            "http://127.0.0.1:9222/json",
            "http://user@127.0.0.1:9222",
        ] {
            let error = Endpoint::parse(value).expect_err("the endpoint must be refused");
            assert!(
                error.starts_with("invalid arguments:"),
                "`{value}`: {error}"
            );
        }
    }

    #[test]
    fn an_explanation_names_the_endpoint_and_the_debugging_flag() {
        let endpoint = Endpoint::parse("http://127.0.0.1:9222").expect("the endpoint is valid");
        let text = endpoint.explain("connection refused");
        assert!(text.contains("http://127.0.0.1:9222"), "{text}");
        assert!(text.contains("--remote-debugging-port=9222"), "{text}");
    }

    #[test]
    fn a_content_length_is_read_from_the_header_block() {
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\nContent-Length: 42\r\n\r\n"),
            Some(42)
        );
        assert_eq!(content_length("HTTP/1.1 404 Not Found\r\n\r\n"), None);
    }

    #[test]
    fn connect_target_accepts_auto_and_explicit_loopback() {
        assert_eq!(ConnectTarget::parse("auto").unwrap(), ConnectTarget::Auto);
        assert_eq!(
            ConnectTarget::parse("  AUTO ").unwrap(),
            ConnectTarget::Auto
        );
        let explicit = ConnectTarget::parse("http://127.0.0.1:9222").unwrap();
        assert_eq!(
            explicit,
            ConnectTarget::Explicit(Endpoint::parse("http://127.0.0.1:9222").unwrap())
        );
        assert!(ConnectTarget::parse("http://192.168.1.2:9222").is_err());
    }

    #[test]
    fn discover_from_uses_env_override_and_refuses_non_loopback() {
        let ep = Endpoint::discover_from(
            Some("http://127.0.0.1:9333"),
            &[],
            &[],
            Duration::from_millis(100),
        )
        .expect("loopback override must succeed");
        assert_eq!(ep.name(), "http://127.0.0.1:9333");

        let err = Endpoint::discover_from(
            Some("http://10.0.0.5:9222"),
            &[],
            &[],
            Duration::from_millis(100),
        )
        .expect_err("non-loopback override must fail");
        assert!(err.contains(CONNECT_ENV_VAR), "{err}");
    }

    #[test]
    fn discover_from_reads_active_port_file_and_probes_json_version() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0_u8; 512];
                let _ = stream.read(&mut buf);
                let body = format!(
                    r#"{{"webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/browser/test"}}"#
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });

        let active_port = std::env::temp_dir().join(format!(
            "da-test-devtools-active-port-{}-{port}",
            std::process::id()
        ));
        fs::write(&active_port, format!("{port}\n/devtools/browser/test\n")).unwrap();

        let discovered = Endpoint::discover_from(
            None,
            std::slice::from_ref(&active_port),
            &[],
            Duration::from_secs(2),
        )
        .expect("must discover loopback endpoint from DevToolsActivePort");
        let _ = fs::remove_file(&active_port);
        let _ = server.join();
        assert_eq!(discovered.port(), port);
        assert_eq!(discovered.name(), format!("http://127.0.0.1:{port}"));
    }

    #[test]
    fn discover_from_reports_actionable_error_when_nothing_answers() {
        let err = Endpoint::discover_from(None, &[], &[], Duration::from_millis(50))
            .expect_err("must error when no endpoint is discovered");
        assert!(err.contains("--remote-debugging-port=9222"), "{err}");
        assert!(err.contains(CONNECT_ENV_VAR), "{err}");
    }
}
