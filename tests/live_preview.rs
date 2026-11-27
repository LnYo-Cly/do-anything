#![cfg(feature = "live-preview")]

//! End-to-end tests for the opt-in `--features live-preview` loopback (`127.0.0.1`)
//! bearer-authenticated CDP screencast & human-takeover bridge.

mod support;

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use do_anything::session::live_preview::{BridgeConfig, LivePreviewBridge};
use serde_json::{Value, json};
use support::TempDir;
use tungstenite::Message;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(da())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run da")
}

fn json_stdout(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "invalid JSON stdout ({e}): {text}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn raw_http(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))
        .expect("connect to loopback bridge");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

#[test]
fn loopback_live_preview_bridge_enforces_auth_streams_frames_and_gates_human_takeover() {
    let dir = TempDir::new("live-preview-bridge");
    let sessions_dir = dir.path().join(".da").join("sessions");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test page server");
    listener.set_nonblocking(true).expect("nonblocking");
    let page_port = listener.local_addr().unwrap().port();
    let stop_http = Arc::new(AtomicBool::new(false));
    let stop_http_srv = Arc::clone(&stop_http);
    let html = r#"<!doctype html><html><head><title>Live Takeover</title><style>body{margin:0;padding:16px;background:#eef;}#box{width:260px;height:40px;font-size:18px;}#btn{margin-top:12px;width:160px;height:44px;}</style></head><body><input id="box" type="text" value=""/><br/><button id="btn" onclick="document.getElementById('status').textContent='HUMAN_CLICKED'">Takeover</button><p id="status">IDLE</p><script>setInterval(()=>{document.body.dataset.tick=String((Number(document.body.dataset.tick||0))+1);},100);</script></body></html>"#;
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let http_thread = thread::spawn(move || {
        while !stop_http_srv.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                    let mut buf = [0u8; 1024];
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(resp.as_bytes());
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let start = run_da_in(dir.path(), &["session", "start", "--preview-frames"]);
    assert_eq!(start.status.code(), Some(0), "{start:?}");
    let id = json_stdout(&start)["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    let open = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.open",
            "--arg",
            &format!("url=http://127.0.0.1:{page_port}/"),
        ],
    );
    if open.status.code() != Some(0) {
        let stderr = String::from_utf8_lossy(&open.stderr);
        let stdout = String::from_utf8_lossy(&open.stdout);
        let _ = run_da_in(dir.path(), &["session", "stop", &id]);
        stop_http.store(true, Ordering::Relaxed);
        let _ = http_thread.join();
        if stdout.contains("could not find Chrome") || stderr.contains("could not find Chrome") {
            return;
        }
        panic!("browser.open failed unexpectedly: stdout={stdout}\nstderr={stderr}");
    }

    let mut bridge = LivePreviewBridge::start(BridgeConfig {
        sessions_dir: sessions_dir.clone(),
        session_id: id.clone(),
        port: 0,
        format: "jpeg".to_string(),
        quality: 75,
        max_width: Some(640),
        max_height: Some(480),
    })
    .expect("bridge must start on loopback");
    let addr = bridge.addr();
    assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

    // 1. Unauthenticated and wrong-token HTTP requests must be rejected with 401.
    let (status_no_auth, _) = raw_http(
        addr,
        &format!("GET /status HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(status_no_auth, 401);

    let (status_bad_auth, _) = raw_http(
        addr,
        &format!(
            "GET /status HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer wrong-token\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(status_bad_auth, 401);

    let (pause_no_auth, _) = raw_http(
        addr,
        &format!(
            "POST /pause HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(pause_no_auth, 401);

    // 2. Non-loopback Origin must be rejected with 403 even with a valid bearer token (CSWSH defense).
    let (status_bad_origin, _) = raw_http(
        addr,
        &format!(
            "GET /status HTTP/1.1\r\nHost: {addr}\r\nOrigin: https://evil.example\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            bridge.token()
        ),
    );
    assert_eq!(status_bad_origin, 403);

    // 3. Unauthenticated WebSocket upgrade must be rejected with 401.
    let ws_no_auth_req = bridge.ws_url().into_client_request().unwrap();
    assert!(
        tungstenite::connect(ws_no_auth_req).is_err(),
        "unauthenticated WebSocket must be rejected"
    );

    // 4. Authenticated GET /status, POST /pause, and POST /resume gate queued agent steps.
    let (status_ok, body_ok) = raw_http(
        addr,
        &format!(
            "GET /status HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",
            bridge.token()
        ),
    );
    assert_eq!(status_ok, 200, "{body_ok}");
    let status_json: Value = serde_json::from_str(&body_ok).unwrap();
    assert_eq!(status_json["paused_for_human"], json!(false));
    assert_eq!(status_json["target_id"], json!(bridge.target_id()));

    let pause_body = r#"{"reason":"human takeover button"}"#;
    let (pause_ok, pause_resp) = raw_http(
        addr,
        &format!(
            "POST /pause HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{pause_body}",
            bridge.token(),
            pause_body.len()
        ),
    );
    assert_eq!(pause_ok, 200, "{pause_resp}");

    let blocked = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.read",
            "--arg",
            "css=#status",
        ],
    );
    assert_ne!(
        blocked.status.code(),
        Some(0),
        "agent step must be rejected while paused"
    );
    assert_eq!(
        json_stdout(&blocked)["error"]["kind"],
        json!("paused_for_human")
    );

    // 5. Authenticated WebSocket receives ready + CDP screencast frame, rejects raw CDP methods,
    //    and forwards human mouse/insert_text input to the owned tab.
    let mut ws_req = bridge.ws_url().into_client_request().unwrap();
    ws_req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", bridge.token())).unwrap(),
    );
    let (mut ws, _) = tungstenite::connect(ws_req).expect("authenticated WebSocket must connect");

    let mut saw_ready = false;
    let mut saw_frame = false;
    let mut saw_raw_cdp_error = false;
    let mut saw_mouse_ack = false;
    let mut saw_resume_state = false;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let msg: Value = serde_json::from_str(&text).unwrap();
                match msg.get("type").and_then(Value::as_str) {
                    Some("ready") => {
                        saw_ready = true;
                        // Arbitrary raw CDP methods must be rejected.
                        ws.send(Message::Text(
                            json!({ "type": "Page.navigate", "url": "https://evil.example" })
                                .to_string()
                                .into(),
                        ))
                        .unwrap();
                        // Click the #btn button via forwarded human pointer input (at x=80, y=90).
                        ws.send(Message::Text(
                            json!({
                                "type": "mouse",
                                "event_type": "mousePressed",
                                "x": 80.0,
                                "y": 90.0,
                                "button": "left",
                                "click_count": 1
                            })
                            .to_string()
                            .into(),
                        ))
                        .unwrap();
                        ws.send(Message::Text(
                            json!({
                                "type": "mouse",
                                "event_type": "mouseReleased",
                                "x": 80.0,
                                "y": 90.0,
                                "button": "left",
                                "click_count": 1
                            })
                            .to_string()
                            .into(),
                        ))
                        .unwrap();
                        ws.send(Message::Text(
                            json!({ "type": "resume" }).to_string().into(),
                        ))
                        .unwrap();
                    }
                    Some("frame") => {
                        if msg
                            .get("data")
                            .and_then(Value::as_str)
                            .is_some_and(|b64| !b64.is_empty())
                        {
                            saw_frame = true;
                        }
                    }
                    Some("error") => {
                        if msg
                            .get("message")
                            .and_then(Value::as_str)
                            .is_some_and(|m| m.contains("unsupported message type"))
                        {
                            saw_raw_cdp_error = true;
                        }
                    }
                    Some("ack") if msg.get("for").and_then(Value::as_str) == Some("mouse") => {
                        saw_mouse_ack = true;
                    }
                    Some("state") if msg.get("paused_for_human") == Some(&json!(false)) => {
                        saw_resume_state = true;
                    }
                    _ => {}
                }
                if saw_ready && saw_frame && saw_raw_cdp_error && saw_mouse_ack && saw_resume_state
                {
                    break;
                }
            }
            Ok(Message::Ping(p)) => {
                let _ = ws.send(Message::Pong(p));
            }
            Ok(_) => {}
            Err(err) => panic!("ws read failed: {err}"),
        }
    }
    let _ = ws.close(None);

    assert!(saw_ready, "expected ready message over WS");
    assert!(
        saw_frame,
        "expected at least one CDP screencast frame over WS"
    );
    assert!(saw_raw_cdp_error, "raw CDP method must be rejected");
    assert!(
        saw_mouse_ack,
        "mouse input must be forwarded and acknowledged"
    );
    assert!(saw_resume_state, "WS resume must clear paused_for_human");

    bridge.shutdown();

    // After WS resume + human click on #btn, agent can read the updated DOM state!
    let read_after = run_da_in(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "browser.read",
            "--arg",
            "css=#status",
        ],
    );
    assert_eq!(read_after.status.code(), Some(0), "{read_after:?}");
    assert_eq!(
        json_stdout(&read_after)["step"]["result"]["text"],
        json!("HUMAN_CLICKED")
    );

    let stop = run_da_in(dir.path(), &["session", "stop", &id]);
    assert_eq!(stop.status.code(), Some(0), "{stop:?}");

    stop_http.store(true, Ordering::Relaxed);
    let _ = http_thread.join();
}

#[test]
fn preview_bridge_cli_validates_args_and_exits_cleanly_on_stdin_eof() {
    let dir = TempDir::new("live-preview-cli");
    // Invalid format -> usage error (exit 2).
    let bad_fmt = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "preview-bridge",
            "s-123",
            "--format",
            "gif",
        ],
    );
    assert_eq!(bad_fmt.status.code(), Some(2), "{bad_fmt:?}");

    // Non-existent session -> environment error (exit 4).
    let missing = run_da_in(
        dir.path(),
        &["--output", "json", "session", "preview-bridge", "s-missing"],
    );
    assert_eq!(missing.status.code(), Some(4), "{missing:?}");
}
