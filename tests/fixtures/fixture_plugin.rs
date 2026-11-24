//! Test-only plugin executable for `tests/plugin.rs`.
//!
//! It speaks the newline-delimited JSON-RPC 2.0 protocol documented in
//! `docs/guide.md`. Behavior is selected with `DA_FIXTURE_MODE`:
//! `normal`, `hang`, `hang-manifest`, `hang-stdin`, `crash`, `bad-json`,
//! `duplicate`, `validate-reject`, `collide-builtin`, or `close-fail-secret`.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

#[derive(Clone, Default)]
struct SessionState {
    token: String,
    count: u64,
    last: Value,
}

fn main() {
    let mode = std::env::var("DA_FIXTURE_MODE").unwrap_or_else(|_| "normal".to_string());
    let pid_file = std::env::var("DA_FIXTURE_PID_FILE").ok();
    if let Some(path) = &pid_file {
        let _ = std::fs::write(path, std::process::id().to_string());
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut sessions: BTreeMap<String, SessionState> = BTreeMap::new();
    let mut next_token = 1_u64;

    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let result = match method.as_str() {
            "plugin/manifest" => {
                if mode == "hang-manifest" {
                    loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    }
                }
                Ok(manifest(&mode))
            }
            "step/validate" => {
                if mode == "validate-reject" {
                    Err((
                        -32602,
                        "rejected by the fixture's step/validate".to_string(),
                    ))
                } else {
                    validate_step(&params)
                }
            }
            "step/run" => {
                match mode.as_str() {
                    "hang" => loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    },
                    "crash" => std::process::exit(7),
                    "bad-json" => {
                        let _ = writeln!(out, "this is not json");
                        let _ = out.flush();
                        continue;
                    }
                    _ => {}
                }
                run_step(&params, &mut sessions, &mut next_token)
            }
            "observer/start" => start_observer(&params),
            "observer/collect" => collect_observer(&params),
            "session/snapshot" => snapshot_session(&params, &sessions),
            "session/evidence" => evidence_session(&params, &sessions),
            "session/detach" => detach_session(&params, &mut sessions),
            "session/reattach" => reattach_session(&params, &mut sessions),
            "session/close" => close_session(&mode, &params, &mut sessions),
            other => Err((-32601, format!("unknown method `{other}`"))),
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message},
            }),
        };
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
        if mode == "hang-stdin" && method == "plugin/manifest" {
            // Keep stdin open and unread so requests larger than the OS pipe
            // buffer block on write unless the host enforces its deadline.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    }

    // The pid file is left in place: tests read it after the host has already
    // terminated the process, and the temporary directory owns its cleanup.
    let _ = pid_file;
}

fn manifest(mode: &str) -> Value {
    let step_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["value"],
        "properties": {
            "value": {"type": "string", "minLength": 1},
            "count": {"type": "integer", "minimum": 1, "maximum": 5},
            "fail": {"type": "boolean"}
        }
    });
    let mut steps = vec![
        json!({
            "name": "custom.echo",
            "summary": "Echoes the `value` argument in several result shapes.",
            "args_schema": step_schema.clone(),
            "allow_retry": true
        }),
        json!({
            "name": "custom.act",
            "summary": "Updates the detachable `custom` session.",
            "args_schema": step_schema,
            "allow_retry": false,
            "session_kind": "custom"
        }),
    ];
    if mode == "duplicate" {
        steps.push(json!({
            "name": "custom.echo",
            "summary": "A duplicate that must be refused.",
            "args_schema": {"type": "object"},
            "allow_retry": true
        }));
    }
    if mode == "collide-builtin" {
        steps.push(json!({
            "name": "http.request",
            "summary": "A name a built-in driver owns.",
            "args_schema": {"type": "object"},
            "allow_retry": true
        }));
    }
    json!({
        "name": "fixture",
        "version": "0.1.0",
        "pid": std::process::id(),
        "steps": steps,
        "observers": [{
            "name": "custom.metrics",
            "summary": "Reports the configured `value` as a read-only metric.",
            "args_schema": {
                "type": "object",
                "additionalProperties": false,
                "required": ["value"],
                "properties": {"value": {"type": "string", "minLength": 1}}
            }
        }],
        "sessions": [{"kind": "custom", "detachable": true}]
    })
}

fn validate_step(params: &Value) -> Result<Value, (i64, String)> {
    let step = params
        .get("step")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    if !matches!(step, "custom.echo" | "custom.act") {
        return Err((-32601, format!("unknown step `{step}`")));
    }
    validate_args(&args)?;
    Ok(json!({"valid": true}))
}

fn validate_args(args: &Value) -> Result<(), (i64, String)> {
    let Some(value) = args.get("value").and_then(Value::as_str) else {
        return Err((-32602, "`value` must be a non-empty string".to_string()));
    };
    if value.is_empty() {
        return Err((-32602, "`value` must be a non-empty string".to_string()));
    }
    if let Some(count) = args.get("count").and_then(Value::as_u64)
        && !(1..=5).contains(&count)
    {
        return Err((-32602, "`count` must be between 1 and 5".to_string()));
    }
    Ok(())
}

fn run_step(
    params: &Value,
    sessions: &mut BTreeMap<String, SessionState>,
    next_token: &mut u64,
) -> Result<Value, (i64, String)> {
    let step = params
        .get("step")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    validate_args(&args)?;

    if step == "custom.echo" {
        let value = args.get("value").cloned().unwrap_or(Value::Null);
        return Ok(json!({
            "result": {
                "echo": value,
                "nested": {"value": value},
            },
        }));
    }

    let session = params.get("session").cloned().unwrap_or(Value::Null);
    let mut state = state_from_handle(session.get("handle").unwrap_or(&Value::Null));
    if state.token.is_empty() {
        state.token = format!("fixture-{}-{next_token}", std::process::id());
        *next_token += 1;
    }
    let value = args.get("value").cloned().unwrap_or(Value::Null);
    state.count += 1;
    state.last = value.clone();
    let fail = args.get("fail").and_then(Value::as_bool).unwrap_or(false);
    sessions.insert(state.token.clone(), state.clone());
    if fail {
        return Err((
            -32000,
            format!("custom.act failed while handling `{value}`"),
        ));
    }
    Ok(json!({
        "result": {"count": state.count, "last": state.last},
        "session": handle_from_state(&state),
    }))
}

fn start_observer(params: &Value) -> Result<Value, (i64, String)> {
    let observer = params
        .get("observer")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if observer != "custom.metrics" {
        return Err((-32601, format!("unknown observer `{observer}`")));
    }
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let Some(value) = args.get("value").and_then(Value::as_str) else {
        return Err((-32602, "`value` must be a non-empty string".to_string()));
    };
    Ok(json!({"handle": {"observer": observer, "value": value}}))
}

fn collect_observer(params: &Value) -> Result<Value, (i64, String)> {
    let handle = params.get("handle").cloned().unwrap_or(Value::Null);
    Ok(json!({"value": {"value": handle.get("value").cloned().unwrap_or(Value::Null)}}))
}

fn snapshot_session(
    params: &Value,
    sessions: &BTreeMap<String, SessionState>,
) -> Result<Value, (i64, String)> {
    let handle = params.get("handle").cloned().unwrap_or(Value::Null);
    let state = state_from_handle(&handle);
    let state = sessions.get(&state.token).cloned().unwrap_or(state);
    Ok(json!({
        "snapshot": {
            "session": "custom",
            "count": state.count,
            "last": state.last,
        }
    }))
}

fn evidence_session(
    params: &Value,
    sessions: &BTreeMap<String, SessionState>,
) -> Result<Value, (i64, String)> {
    let handle = params.get("handle").cloned().unwrap_or(Value::Null);
    let state = state_from_handle(&handle);
    let state = sessions.get(&state.token).cloned().unwrap_or(state);
    Ok(json!({
        "files": [
            {
                "name": "session.txt",
                "text": format!("count={}\nlast={}\n", state.count, state.last),
            },
            {
                "name": "opaque.bin",
                "bytes": [0, 1, 2, 255],
            }
        ]
    }))
}

fn detach_session(
    params: &Value,
    sessions: &mut BTreeMap<String, SessionState>,
) -> Result<Value, (i64, String)> {
    let handle = params.get("handle").cloned().unwrap_or(Value::Null);
    let state = state_from_handle(&handle);
    let state = sessions.get(&state.token).cloned().unwrap_or(state);
    sessions.insert(state.token.clone(), state.clone());
    Ok(json!({"handle": handle_from_state(&state)}))
}

fn reattach_session(
    params: &Value,
    sessions: &mut BTreeMap<String, SessionState>,
) -> Result<Value, (i64, String)> {
    let handle = params.get("handle").cloned().unwrap_or(Value::Null);
    let state = state_from_handle(&handle);
    if state.token.is_empty() {
        return Err((-32602, "the session handle has no `token`".to_string()));
    }
    sessions.insert(state.token.clone(), state.clone());
    Ok(json!({"handle": handle_from_state(&state)}))
}

fn close_session(
    mode: &str,
    params: &Value,
    sessions: &mut BTreeMap<String, SessionState>,
) -> Result<Value, (i64, String)> {
    let handle = params.get("handle").cloned().unwrap_or(Value::Null);
    let state = state_from_handle(&handle);
    if mode == "close-fail-secret" {
        let secret = std::env::var("DA_PLUGIN_TEST_SECRET").unwrap_or_default();
        let extra = std::env::var("DA_PLUGIN_EXTRA_SECRET").unwrap_or_default();
        return Err((
            -32000,
            format!(
                "close failed for token={} secret={secret} extra={extra} last={}",
                state.token, state.last
            ),
        ));
    }
    sessions.remove(&state.token);
    if let Ok(path) = std::env::var("DA_FIXTURE_CLOSE_LOG")
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{}", state.token);
    }
    Ok(json!({"closed": true}))
}

fn state_from_handle(handle: &Value) -> SessionState {
    SessionState {
        token: handle
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        count: handle.get("count").and_then(Value::as_u64).unwrap_or(0),
        last: handle.get("last").cloned().unwrap_or(Value::Null),
    }
}

fn handle_from_state(state: &SessionState) -> Value {
    json!({"token": state.token, "count": state.count, "last": state.last})
}
