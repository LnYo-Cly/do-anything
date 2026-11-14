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
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// How long the endpoint may take to answer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The largest response accepted; `/json/version` is a few hundred bytes.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

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
}
