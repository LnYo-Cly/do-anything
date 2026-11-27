//! Integration tests for synchronous JSON-RPC 2.0 model hooks (`#83`):
//! `model/select_ref`, `model/locate`, `model/judge`, `Secrets` scrubbing across
//! both directions of the hook boundary, input-masked viewport capture,
//! deterministic locator write-back vs `<canvas>` coordinate fallback,
//! `steps.jsonl` audit metadata, and bounded timeout/crash child cleanup.

mod support;

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use do_anything::core::registry::{
    Registry, SessionDefinition, StepDefinition, StepError, disallow_retry, no_lint_warnings,
};
use do_anything::core::session::{MaskedViewportCapture, Session, Sessions, SynthesizedTarget};
use do_anything::exit::ExitCode;
use do_anything::report::base64_encode;
use do_anything::session::export::ExportOptions;
use do_anything::session::model_hook::ModelHookSpec;
use do_anything::session::store;
use do_anything::session::{
    self, JudgeRequest, LocateRequest, SelectRefAction, SelectRefRequest, SessionRequest, StepInput,
};
use serde_json::{Value, json};
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

/// Source of a zero-dependency native Rust mock JSON-RPC 2.0 model hook binary.
const MOCK_HOOK_RS: &str = r#"
use std::collections::HashMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

fn parse_kv(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    let cfg_path = env::args()
        .nth(1)
        .or_else(|| env::var("DA_MOCK_HOOK_CONFIG").ok())
        .expect("missing mock hook config path");
    let cfg_text = fs::read_to_string(&cfg_path).expect("read mock hook config");
    let cfg = parse_kv(&cfg_text);

    if let Some(pid_path) = cfg.get("pid_file") {
        let _ = fs::write(pid_path, format!("{}", std::process::id()));
    }

    let mut _held_lock = None;
    if let Some(lock_path) = cfg.get("lock_file") {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .expect("open lock file");
        file.try_lock().expect("acquire child lock");
        _held_lock = Some(file);
    }

    let mode = cfg.get("mode").map(String::as_str).unwrap_or("normal");
    if mode == "hang_before_stdin" {
        thread::sleep(Duration::from_secs(60));
        return;
    }

    let mut stdin_bytes = Vec::new();
    let _ = io::stdin().read_to_end(&mut stdin_bytes);
    let stdin_text = String::from_utf8_lossy(&stdin_bytes).into_owned();

    if let Some(req_file) = cfg.get("request_file") {
        let _ = fs::write(req_file, &stdin_bytes);
    }
    if let Some(log_file) = cfg.get("requests_log") {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(log_file) {
            let single = stdin_text.trim();
            let _ = writeln!(f, "{single}");
        }
    }

    match mode {
        "hang" => {
            thread::sleep(Duration::from_secs(60));
        }
        "crash" => {
            let msg = cfg.get("stderr").map(String::as_str).unwrap_or("hook crashed");
            let _ = writeln!(io::stderr(), "{msg}");
            std::process::exit(42);
        }
        "malformed" => {
            let _ = io::stdout().write_all(b"not-json-at-all{{{");
            let _ = io::stdout().flush();
        }
        "rpc_error" => {
            let msg = cfg.get("error_message").map(String::as_str).unwrap_or("hook failed");
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{{\"code\":-32001,\"message\":{}}}}}",
                json_escape(msg)
            );
            let _ = writeln!(io::stdout(), "{line}");
            let _ = io::stdout().flush();
        }
        _ => {
            let model = cfg.get("model").map(String::as_str).unwrap_or("mock-jev-v1");
            let latency_ms = cfg.get("latency_ms").map(String::as_str).unwrap_or("17");
            let confidence = cfg.get("confidence").map(String::as_str).unwrap_or("0.93");
            let trace_id = cfg.get("trace_id").map(String::as_str).unwrap_or("trace-abc-01");
            let usage_tag = cfg.get("usage_tag").map(String::as_str).unwrap_or("ok");

            let result_json = if stdin_text.contains("\"model/select_ref\"") {
                let r = cfg.get("select_ref").map(String::as_str).unwrap_or("e2");
                format!(
                    "{{\"ref\":{},\"confidence\":{confidence},\"model\":{},\"latency_ms\":{latency_ms},\"usage\":{{\"prompt_tokens\":64,\"completion_tokens\":2,\"note\":{}}},\"trace_id\":{}}}",
                    json_escape(r),
                    json_escape(model),
                    json_escape(usage_tag),
                    json_escape(trace_id),
                )
            } else if stdin_text.contains("\"model/locate\"") {
                let x = cfg.get("locate_x").map(String::as_str).unwrap_or("120");
                let y = cfg.get("locate_y").map(String::as_str).unwrap_or("45");
                format!(
                    "{{\"x\":{x},\"y\":{y},\"confidence\":{confidence},\"model\":{},\"latency_ms\":{latency_ms},\"usage\":{{\"prompt_tokens\":512,\"completion_tokens\":6,\"note\":{}}},\"trace_id\":{}}}",
                    json_escape(model),
                    json_escape(usage_tag),
                    json_escape(trace_id),
                )
            } else {
                let passed = cfg.get("judge_passed").map(String::as_str).unwrap_or("true");
                let rationale = cfg.get("judge_rationale").map(String::as_str).unwrap_or("looks good");
                format!(
                    "{{\"passed\":{passed},\"rationale\":{},\"confidence\":{confidence},\"model\":{},\"latency_ms\":{latency_ms},\"usage\":{{\"prompt_tokens\":128,\"completion_tokens\":24,\"note\":{}}},\"trace_id\":{}}}",
                    json_escape(rationale),
                    json_escape(model),
                    json_escape(usage_tag),
                    json_escape(trace_id),
                )
            };

            let envelope = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{result_json}}}");
            let _ = writeln!(io::stdout(), "{envelope}");
            let _ = io::stdout().flush();
        }
    }
}
"#;

fn mock_hook_exe() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("da-mock-hook-bin-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create mock hook build dir");
        let src = dir.join("mock_hook.rs");
        fs::write(&src, MOCK_HOOK_RS).expect("write mock_hook.rs");
        let exe_name = if cfg!(windows) {
            "mock_hook.exe"
        } else {
            "mock_hook"
        };
        let out = dir.join(exe_name);
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let status = Command::new(rustc)
            .arg(&src)
            .arg("-O")
            .arg("-o")
            .arg(&out)
            .status()
            .expect("invoke rustc for mock_hook.rs");
        assert!(status.success(), "rustc failed to compile mock_hook.rs");
        out
    })
}

fn run_da_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(da());
    cmd.current_dir(dir)
        .args(args)
        .env_remove("DA_MODEL")
        .env_remove("DA_MODEL_ADAPTER")
        .env_remove("DA_MODEL_ENDPOINT")
        .env_remove("DA_MODEL_API_KEY_ENV");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to execute da")
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout must be valid JSON ({err})\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[derive(Default)]
struct FakeUiSession {
    step_meta: Option<Value>,
}

fn large_masked_png() -> Vec<u8> {
    let mut buf = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    // Pad to 96 KiB so `image_base64` exceeds a typical 64 KiB OS pipe buffer.
    buf.resize(96 * 1024, 0x00);
    buf
}

impl Session for FakeUiSession {
    fn close(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn detach(&mut self) -> Result<Option<Value>, String> {
        Ok(Some(json!({ "alive": true })))
    }

    fn snapshot(
        &mut self,
        _scope_ref: Option<&str>,
        _max_depth: Option<usize>,
        _max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        let secret_val = std::env::var("DA_MODEL_HOOK_SECRET").unwrap_or_default();
        Ok(Some(json!({
            "title": "Fake App",
            "tree": [
                {
                    "ref": "e1",
                    "role": "textbox",
                    "name": "Secret Input",
                    "value": format!("token={secret_val}")
                },
                {
                    "ref": "e2",
                    "role": "button",
                    "name": "Save Order"
                }
            ]
        })))
    }

    fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        self.snapshot(None, None, None)
    }

    fn take_step_metadata(&mut self) -> Option<Value> {
        self.step_meta.take()
    }

    fn masked_viewport_capture(&mut self) -> Result<Option<MaskedViewportCapture>, String> {
        Ok(Some(MaskedViewportCapture {
            png: large_masked_png(),
            width: 800,
            height: 600,
        }))
    }

    fn synthesize_ref(&mut self, ref_id: &str) -> Result<Option<SynthesizedTarget>, String> {
        match ref_id {
            "e1" => Ok(Some(SynthesizedTarget {
                locator: json!({ "role": "textbox", "name": "Secret Input" }),
                fragile: false,
            })),
            "e2" => Ok(Some(SynthesizedTarget {
                locator: json!({ "testid": "save-order-btn" }),
                fragile: false,
            })),
            other => Err(format!("unknown ref `{other}`")),
        }
    }

    fn synthesize_at_point(&mut self, x: f64, y: f64) -> Result<Option<SynthesizedTarget>, String> {
        // Coordinates < (400, 300) hit the addressable button; >= (400, 300) hit an opaque canvas.
        if x < 400.0 && y < 300.0 {
            Ok(Some(SynthesizedTarget {
                locator: json!({ "role": "button", "name": "Save Order" }),
                fragile: false,
            }))
        } else {
            Ok(None)
        }
    }
}

fn fake_ui_registry() -> Registry {
    let mut registry = Registry::new();
    registry
        .register_session(SessionDefinition {
            kind: "browser",
            reattach: |_handle| Ok(Box::new(FakeUiSession::default())),
            dispose_handle: |_| Ok(()),
        })
        .expect("register browser session");

    registry
        .register(StepDefinition {
            name: "browser.open",
            description: "open fake page",
            args_schema: json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": { "type": "string" },
                    "token": { "type": "string" }
                }
            }),
            validate: |_| Ok(()),
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: |args: &Value, sessions: &mut Sessions| {
                sessions
                    .get_or_create("browser", || Ok(FakeUiSession::default()))
                    .map_err(StepError::Environment)?;
                Ok(json!({ "url": args.get("url").cloned().unwrap_or(Value::Null) }))
            },
        })
        .expect("register browser.open");

    registry
        .register(StepDefinition {
            name: "browser.click",
            description: "click fake element",
            args_schema: json!({
                "type": "object",
                "properties": {
                    "testid": { "type": "string" },
                    "role": { "type": "string" },
                    "name": { "type": "string" },
                    "at": { "type": "object" }
                }
            }),
            validate: |args| {
                if args.get("ref").is_some() {
                    return Err("`ref` is not valid in case files".to_string());
                }
                Ok(())
            },
            check_retry: disallow_retry,
            lint_warnings: |args| {
                if args.get("at").is_some() {
                    vec!["explicit coordinates (`at`) are fragile across layouts".to_string()]
                } else {
                    Vec::new()
                }
            },
            run: |args: &Value, sessions: &mut Sessions| {
                let interactive = sessions.is_interactive();
                let session = sessions
                    .get_or_create("browser", || Ok(FakeUiSession::default()))
                    .map_err(StepError::Environment)?;
                if let Some(ref_id) = args.get("ref").and_then(Value::as_str) {
                    let target = session
                        .synthesize_ref(ref_id)
                        .map_err(StepError::Execution)?
                        .expect("synth ref");
                    session.step_meta = Some(json!({
                        "ref": ref_id,
                        "synthesized_locator": target.locator,
                        "fragile": target.fragile,
                    }));
                    return Ok(json!({ "clicked": true }));
                }
                if let Some(at) = args.get("at") {
                    let x = at["x"].as_f64().unwrap_or(0.0);
                    let y = at["y"].as_f64().unwrap_or(0.0);
                    if interactive && let Ok(Some(target)) = session.synthesize_at_point(x, y) {
                        session.step_meta = Some(json!({
                            "synthesized_locator": target.locator,
                            "fragile": target.fragile,
                        }));
                    }
                    return Ok(json!({
                        "x": x,
                        "y": y,
                        "viewport": { "width": 800, "height": 600 }
                    }));
                }
                Ok(json!({ "clicked": true }))
            },
        })
        .expect("register browser.click");

    registry
        .register_session_validator("browser.click", |args| {
            if args.get("ref").is_some()
                || args.get("testid").is_some()
                || args.get("role").is_some()
                || args.get("at").is_some()
            {
                Ok(())
            } else {
                Err("missing target".to_string())
            }
        })
        .expect("register session validator");

    registry
}

#[cfg(feature = "http")]
#[test]
fn cli_judge_redacts_secrets_in_both_directions_records_audit_and_omits_from_export() {
    let server = support::TestServer::start();
    let dir = TempDir::new("hook-cli-judge");
    let hook_exe = mock_hook_exe().to_str().expect("utf8 hook exe");
    let secret = "s3cr3t-judge-token-98765";

    let req_file = dir.path().join("last_request.json");
    let cfg_path = dir.write(
        "hook.cfg",
        &format!(
            "mode=normal\nrequest_file={}\njudge_passed=true\njudge_rationale=Service healthy; echoed {secret}\nmodel=vllm/qwen-judge-14b\nlatency_ms=412\nconfidence=0.96\ntrace_id=tr-{secret}-01\nusage_tag=u-{secret}\n",
            req_file.display()
        ),
    );
    let cfg_str = cfg_path.to_str().unwrap();

    // 1. Start session with persisted `--model-hook` + `--model-hook-arg`.
    let start_out = run_da_env(
        dir.path(),
        &[
            "session",
            "start",
            "--model-hook",
            hook_exe,
            "--model-hook-arg",
            cfg_str,
        ],
        &[("DA_JUDGE_SECRET", secret)],
    );
    assert_eq!(start_out.status.code(), Some(0), "{start_out:?}");
    let started = json_stdout(&start_out);
    let id = started["session_id"].as_str().unwrap().to_string();
    let session_dir = dir.path().join(started["dir"].as_str().unwrap());

    // 2. Run an HTTP step referencing `${env:DA_JUDGE_SECRET}`.
    let step_out = run_da_env(
        dir.path(),
        &[
            "session",
            "step",
            &id,
            "--step",
            &json!({
                "use": "http.request",
                "with": {
                    "method": "POST",
                    "url": server.url("/echo"),
                    "body": "${env:DA_JUDGE_SECRET}"
                },
                "expect": [{ "path": "$.status", "equals": 200 }]
            })
            .to_string(),
        ],
        &[("DA_JUDGE_SECRET", secret)],
    );
    assert_eq!(step_out.status.code(), Some(0), "{step_out:?}");

    // 3. Judge with `passed: true` (`--output json`) and `${env:DA_JUDGE_SECRET}` in rubric.
    let judge_pass = run_da_env(
        dir.path(),
        &[
            "--output",
            "json",
            "session",
            "judge",
            &id,
            "--rubric",
            "Verify payload ${env:DA_JUDGE_SECRET} returned HTTP 200",
        ],
        &[("DA_JUDGE_SECRET", secret)],
    );
    assert_eq!(judge_pass.status.code(), Some(0), "{judge_pass:?}");
    let pass_json = json_stdout(&judge_pass);
    assert_eq!(pass_json["ok"], true, "{pass_json}");
    assert_eq!(pass_json["passed"], true, "{pass_json}");
    assert_eq!(pass_json["model"], "vllm/qwen-judge-14b");
    assert_eq!(pass_json["latency_ms"], 412);
    assert_eq!(pass_json["rationale"], "Service healthy; echoed ***");
    assert_eq!(pass_json["trace_id"], "tr-***-01");
    assert_eq!(pass_json["usage"]["note"], "u-***");

    // Verify outgoing JSON-RPC request sent to the hook NEVER contained the raw secret!
    let raw_hook_stdin = fs::read_to_string(&req_file).expect("hook request file");
    assert!(
        !raw_hook_stdin.contains(secret),
        "hook stdin leaked secret: {raw_hook_stdin}"
    );
    let parsed_req: Value = serde_json::from_str(raw_hook_stdin.trim()).expect("hook req json");
    assert_eq!(parsed_req["jsonrpc"], "2.0");
    assert_eq!(parsed_req["method"], "model/judge");
    assert_eq!(
        parsed_req["params"]["rubric"],
        "Verify payload *** returned HTTP 200"
    );
    assert!(parsed_req["params"]["evidence"]["steps"].is_array());

    // 4. Judge with `--output text` and `passed: false` -> exits 1 (`TestFailure`).
    fs::write(
        &cfg_path,
        format!(
            "mode=normal\nrequest_file={}\njudge_passed=false\njudge_rationale=Missing banner for {secret}\nmodel=vllm/qwen-judge-14b\nlatency_ms=289\nconfidence=0.81\ntrace_id=tr-fail-02\n",
            req_file.display()
        ),
    )
    .expect("update hook cfg");

    let judge_fail = run_da_env(
        dir.path(),
        &[
            "--output",
            "text",
            "session",
            "judge",
            &id,
            "--rubric",
            "Check banner presence",
        ],
        &[("DA_JUDGE_SECRET", secret)],
    );
    assert_eq!(judge_fail.status.code(), Some(1), "{judge_fail:?}");
    let fail_text = String::from_utf8_lossy(&judge_fail.stdout);
    assert!(fail_text.contains("verdict: failed"), "{fail_text}");
    assert!(
        fail_text.contains("rationale: Missing banner for ***"),
        "{fail_text}"
    );
    assert!(!fail_text.contains(secret), "{fail_text}");

    // 5. Inspect `steps.jsonl`: audit fields persisted, zero raw secret anywhere!
    let steps_raw = fs::read_to_string(session_dir.join("steps.jsonl")).expect("steps.jsonl");
    assert!(
        !steps_raw.contains(secret),
        "steps.jsonl leaked secret: {steps_raw}"
    );
    let records = store::load_steps(&session_dir).expect("load_steps");
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].action, "model.judge");
    assert_eq!(records[1].status, "passed");
    assert_eq!(records[1].model.as_deref(), Some("vllm/qwen-judge-14b"));
    assert_eq!(records[1].latency_ms, Some(412));
    assert_eq!(records[1].trace_id.as_deref(), Some("tr-***-01"));
    assert_eq!(
        records[1].usage,
        Some(json!({ "prompt_tokens": 128, "completion_tokens": 24, "note": "u-***" }))
    );
    assert_eq!(records[2].action, "model.judge");
    assert_eq!(records[2].status, "failed");

    // 6. Export omits `model.judge` entries and replays via `da run` with zero hook needed!
    let case_file = dir.path().join("exported.yaml");
    let export_out = run_da_env(
        dir.path(),
        &["session", "export", &id, "-o", case_file.to_str().unwrap()],
        &[("DA_JUDGE_SECRET", secret)],
    );
    assert_eq!(export_out.status.code(), Some(0), "{export_out:?}");
    let exp_json = json_stdout(&export_out);
    assert_eq!(exp_json["exported_steps"], 1, "{exp_json}");
    assert_eq!(
        exp_json["skipped_steps"].as_array().unwrap().len(),
        2,
        "{exp_json}"
    );
    let yaml = fs::read_to_string(&case_file).unwrap();
    assert!(!yaml.contains("model.judge"), "{yaml}");
    assert!(!yaml.contains("vllm"), "{yaml}");

    let lint = run_da_env(
        dir.path(),
        &["lint", case_file.to_str().unwrap()],
        &[("DA_JUDGE_SECRET", secret)],
    );
    assert_eq!(lint.status.code(), Some(0), "{lint:?}");

    let replay = run_da_env(
        dir.path(),
        &["run", case_file.to_str().unwrap(), "--no-bundle"],
        &[("DA_JUDGE_SECRET", secret)],
    );
    assert_eq!(replay.status.code(), Some(0), "{replay:?}");

    let _ = run_da_env(dir.path(), &["session", "stop", &id], &[]);
}

#[test]
fn select_ref_and_locate_write_back_deterministic_locators_and_canvas_coordinate_fallback() {
    let dir = TempDir::new("hook-select-locate");
    let sessions_dir = dir.path().join(".da/sessions");
    let registry = fake_ui_registry();
    let secret = "s3cr3t-ui-value-55555";
    // SAFETY: single-threaded env var dedicated to `DA_MODEL_HOOK_SECRET`.
    // Wait: `std::env::set_var` is `unsafe` in Rust 2024 (`#![forbid(unsafe_code)]`)!
    // Never call `std::env::set_var`; read an existing env var or pass via CLI subprocess!
    let _ = secret;

    let req_file = dir.path().join("rpc_req.json");
    let cfg_path = dir.write(
        "hook.cfg",
        &format!(
            "mode=normal\nrequest_file={}\nselect_ref=e2\nmodel=jev-0.2b\nlatency_ms=11\nconfidence=0.98\ntrace_id=jev-tr-99\n",
            req_file.display()
        ),
    );
    let hook_spec = ModelHookSpec {
        command: mock_hook_exe().to_string_lossy().into_owned(),
        args: vec![cfg_path.to_string_lossy().into_owned()],
    };

    // 1. Start session and open fake UI session.
    let started = session::handle(
        SessionRequest::Start {
            sessions_dir: sessions_dir.clone(),
            idle_timeout_ms: 60_000,
            observe: Vec::new(),
            model_hook: Some(hook_spec.clone()),
            model: None,
            plugins: Vec::new(),
        },
        &registry,
    );
    assert_eq!(started.exit_code, ExitCode::Success);
    let id = started.payload["session_id"].as_str().unwrap().to_string();
    let session_dir = sessions_dir.join(&id);

    let opened = session::handle(
        SessionRequest::Step {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            step: StepInput::Text(
                r#"{"use":"browser.open","with":{"url":"http://127.0.0.1/app"}}"#.to_string(),
            ),
        },
        &registry,
    );
    assert_eq!(opened.exit_code, ExitCode::Success);

    // 2. Reject unknown/hallucinated `ref` from hook with `ExitCode::Environment`.
    let bad_cfg = dir.write("bad_ref.cfg", "mode=normal\nselect_ref=e999\nmodel=jev\n");
    let bad_select = session::handle(
        SessionRequest::SelectRef(SelectRefRequest {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            query: "Click Save Order".to_string(),
            scope_ref: None,
            max_depth: None,
            max_nodes: None,
            hook_override: Some(ModelHookSpec {
                command: mock_hook_exe().to_string_lossy().into_owned(),
                args: vec![bad_cfg.to_string_lossy().into_owned()],
            }),
            model_override: None,
            timeout_ms: 5_000,
            act: None,
        }),
        &registry,
    );
    assert_eq!(
        bad_select.exit_code,
        ExitCode::Environment,
        "{bad_select:?}"
    );

    // 3. `select_ref` picks `e2` -> subsequent `browser.click` step with `ref: e2`!
    let sel = session::handle(
        SessionRequest::SelectRef(SelectRefRequest {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            query: "Click the Save Order button".to_string(),
            scope_ref: None,
            max_depth: None,
            max_nodes: None,
            hook_override: None,
            model_override: None,
            timeout_ms: 5_000,
            act: Some(SelectRefAction {
                action: "browser.click".to_string(),
                with_raw: None,
                args: Vec::new(),
                save: Vec::new(),
            }),
        }),
        &registry,
    );
    assert_eq!(sel.exit_code, ExitCode::Success, "{}", sel.payload);
    assert_eq!(sel.payload["ref"], "e2");
    assert_eq!(
        sel.payload["synthesized_locator"],
        json!({ "testid": "save-order-btn" })
    );
    assert_eq!(sel.payload["model"], "jev-0.2b");
    assert_eq!(sel.payload["latency_ms"], 11);
    assert_eq!(sel.payload["trace_id"], "jev-tr-99");
    assert_eq!(sel.payload["step"]["status"], "passed");

    // 4. `locate` on HTML button `(120, 45)` with `--click` -> synthesizes `{ role: button, name: Save Order }`!
    fs::write(
        &cfg_path,
        format!(
            "mode=normal\nrequest_file={}\nlocate_x=120\nlocate_y=45\nmodel=ui-tars-1.5-7b\nlatency_ms=1821\nconfidence=0.91\ntrace_id=vlm-tr-1\n",
            req_file.display()
        ),
    )
    .unwrap();
    let loc_btn = session::handle(
        SessionRequest::Locate(LocateRequest {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            query: "Click Save Order button".to_string(),
            hook_override: None,
            model_override: None,
            timeout_ms: 5_000,
            click: true,
        }),
        &registry,
    );
    assert_eq!(loc_btn.exit_code, ExitCode::Success, "{}", loc_btn.payload);
    assert_eq!(
        loc_btn.payload["synthesized_locator"],
        json!({ "role": "button", "name": "Save Order" })
    );
    assert_eq!(loc_btn.payload["fragile"], false);

    // Verify `model/locate` sent `mime_type: "image/png"`, `viewport: {width: 800, height: 600}`, and exact masked PNG base64!
    let locate_stdin: Value =
        serde_json::from_str(fs::read_to_string(&req_file).unwrap().trim()).unwrap();
    assert_eq!(locate_stdin["method"], "model/locate");
    assert_eq!(locate_stdin["params"]["mime_type"], "image/png");
    assert_eq!(
        locate_stdin["params"]["viewport"],
        json!({ "width": 800, "height": 600 })
    );
    assert_eq!(
        locate_stdin["params"]["image_base64"],
        base64_encode(&large_masked_png())
    );

    // 5. Out-of-bounds `(900, 45)` outside `800x600` fails with `ExitCode::Environment`.
    fs::write(
        &cfg_path,
        "mode=normal\nlocate_x=900\nlocate_y=45\nmodel=ui-tars\n",
    )
    .unwrap();
    let loc_oob = session::handle(
        SessionRequest::Locate(LocateRequest {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            query: "Out of bounds".to_string(),
            hook_override: None,
            model_override: None,
            timeout_ms: 5_000,
            click: false,
        }),
        &registry,
    );
    assert_eq!(
        loc_oob.exit_code,
        ExitCode::Environment,
        "{}",
        loc_oob.payload
    );

    // 6. `locate` on opaque `<canvas>` `(500, 400)` with `--click` -> falls back to `at: { x: 500, y: 400, viewport: ... }`!
    fs::write(
        &cfg_path,
        "mode=normal\nlocate_x=500\nlocate_y=400\nmodel=ui-tars-1.5-7b\nlatency_ms=1905\ntrace_id=vlm-canvas\n",
    )
    .unwrap();
    let loc_canvas = session::handle(
        SessionRequest::Locate(LocateRequest {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            query: "Click canvas chart node".to_string(),
            hook_override: None,
            model_override: None,
            timeout_ms: 5_000,
            click: true,
        }),
        &registry,
    );
    assert_eq!(
        loc_canvas.exit_code,
        ExitCode::Success,
        "{}",
        loc_canvas.payload
    );
    assert_eq!(loc_canvas.payload["synthesized_locator"], Value::Null);
    assert_eq!(loc_canvas.payload["fragile"], true);

    // 7. Export session -> omits `model.select_ref` and `model.locate`, writes deterministic locators for steps 1-3 and `at:` only for canvas step 4!
    let exported = session::handle(
        SessionRequest::Export {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            options: ExportOptions {
                case_name: Some("Model-assisted exploration".to_string()),
                include_reads: false,
            },
            out_path: None,
        },
        &registry,
    );
    assert_eq!(
        exported.exit_code,
        ExitCode::Success,
        "{}",
        exported.payload
    );
    assert_eq!(
        exported.payload["exported_steps"], 4,
        "{}",
        exported.payload
    );
    let fragile = exported.payload["fragile"].as_array().unwrap();
    assert_eq!(
        fragile.len(),
        1,
        "only the canvas click should be fragile: {}",
        exported.payload
    );
    assert_eq!(fragile[0]["exported_index"], 4);
    assert_eq!(fragile[0]["reason"], "explicit_coordinates");

    let yaml = exported.raw_yaml.unwrap();
    assert!(yaml.contains("testid: save-order-btn"), "{yaml}");
    assert!(yaml.contains("name: Save Order"), "{yaml}");
    assert!(yaml.contains("x: 500"), "{yaml}");
    assert!(yaml.contains("width: 800"), "{yaml}");
    assert!(!yaml.contains("model.select_ref"), "{yaml}");
    assert!(!yaml.contains("model.locate"), "{yaml}");
    assert!(!yaml.contains("ui-tars"), "{yaml}");
    assert!(!yaml.contains("jev"), "{yaml}");

    // Verify audit fields in `steps.jsonl`.
    let steps = store::load_steps(&session_dir).expect("load_steps");
    assert!(steps.iter().any(|s| s.action == "model.select_ref"
        && s.model.as_deref() == Some("jev-0.2b")
        && s.latency_ms == Some(11)));
    assert!(steps.iter().any(|s| s.action == "model.locate"
        && s.model.as_deref() == Some("ui-tars-1.5-7b")
        && s.latency_ms == Some(1821)));

    // Replay exported YAML through `core::runner` with ZERO model hook!
    let case_path = dir.write("exported.yaml", &yaml);
    let parsed_case = do_anything::core::case::load_file(&case_path).expect("load exported case");
    let mut secrets = do_anything::core::secrets::Secrets::new();
    let errors = do_anything::core::runner::preflight(
        std::slice::from_ref(&parsed_case),
        &registry,
        &mut secrets,
    );
    assert!(errors.is_empty(), "preflight errors: {errors:?}");
    let warnings = do_anything::core::runner::preflight_warnings(
        std::slice::from_ref(&parsed_case),
        &registry,
        &mut secrets,
    );
    assert_eq!(
        warnings.len(),
        1,
        "expected 1 coordinate warning for canvas step: {warnings:?}"
    );

    let outcome = do_anything::core::runner::run(
        &[parsed_case],
        &registry,
        &do_anything::core::events::EventBus::new(),
        None,
        &do_anything::core::filter::Filters::default(),
        &mut secrets,
        1,
    );
    assert!(
        outcome.is_ok(),
        "replay must pass without any model hook: {outcome:?}"
    );
}

#[test]
fn hung_crashed_and_malformed_hooks_fail_cleanly_with_zero_orphan_processes() {
    let dir = TempDir::new("hook-failures");
    let hook_exe = mock_hook_exe().to_str().expect("utf8 hook exe");
    let secret = "s3cr3t-crash-stderr-44444";

    let started = json_stdout(&run_da_env(dir.path(), &["session", "start"], &[]));
    let id = started["session_id"].as_str().unwrap().to_string();

    // 1. Missing hook configuration fails with `ExitCode::Usage` (2).
    let no_hook = run_da_env(
        dir.path(),
        &["session", "judge", &id, "--rubric", "check"],
        &[],
    );
    assert_eq!(no_hook.status.code(), Some(2), "{no_hook:?}");

    // 2. Hung hook (`mode=hang`) times out with `ExitCode::Timeout` (5) and leaves ZERO orphan child process.
    let pid_file = dir.path().join("hung.pid");
    let lock_file = dir.path().join("hung.lock");
    let hang_cfg = dir.write(
        "hang.cfg",
        &format!(
            "mode=hang\npid_file={}\nlock_file={}\n",
            pid_file.display(),
            lock_file.display()
        ),
    );
    let hang_out = run_da_env(
        dir.path(),
        &[
            "session",
            "judge",
            &id,
            "--rubric",
            "will hang",
            "--hook",
            hook_exe,
            "--hook-arg",
            hang_cfg.to_str().unwrap(),
            "--timeout-ms",
            "1000",
        ],
        &[],
    );
    assert_eq!(hang_out.status.code(), Some(5), "{hang_out:?}");
    let hang_json = json_stdout(&hang_out);
    assert_eq!(hang_json["error"]["kind"], "timeout", "{hang_json}");
    assert!(
        fs::read_to_string(&pid_file)
            .expect("hung hook must have started and written its PID")
            .trim()
            .parse::<u32>()
            .is_ok()
    );
    // Kernel file lock held by the child process must be immediately acquirable once reaped.
    let probe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_file)
        .expect("open lock probe");
    assert!(
        probe.try_lock().is_ok(),
        "hung child process must be terminated and reaped before da returns"
    );
    let _ = probe.unlock();

    // 3. Hung hook that never reads a >64 KiB `model/locate` stdin payload also times out cleanly without deadlocking.
    let sessions_dir = dir.path().join(".da/sessions-fake");
    let registry = fake_ui_registry();
    let started_fake = session::handle(
        SessionRequest::Start {
            sessions_dir: sessions_dir.clone(),
            idle_timeout_ms: 60_000,
            observe: Vec::new(),
            model_hook: None,
            model: None,
            plugins: Vec::new(),
        },
        &registry,
    );
    let fake_id = started_fake.payload["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = session::handle(
        SessionRequest::Step {
            sessions_dir: sessions_dir.clone(),
            id: fake_id.clone(),
            base_directory: dir.path().to_path_buf(),
            step: StepInput::Text(
                r#"{"use":"browser.open","with":{"url":"http://127.0.0.1/app"}}"#.to_string(),
            ),
        },
        &registry,
    );
    let lock_file2 = dir.path().join("hang_noread.lock");
    let hang_noread_cfg = dir.write(
        "hang_noread.cfg",
        &format!(
            "mode=hang_before_stdin\nlock_file={}\n",
            lock_file2.display()
        ),
    );
    let hang_locate = session::handle(
        SessionRequest::Locate(LocateRequest {
            sessions_dir,
            id: fake_id,
            base_directory: dir.path().to_path_buf(),
            query: "Locate while child ignores stdin".to_string(),
            hook_override: Some(ModelHookSpec {
                command: hook_exe.to_string(),
                args: vec![hang_noread_cfg.to_string_lossy().into_owned()],
            }),
            model_override: None,
            timeout_ms: 1000,
            click: false,
        }),
        &registry,
    );
    assert_eq!(
        hang_locate.exit_code,
        ExitCode::Timeout,
        "{}",
        hang_locate.payload
    );
    let probe2 = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_file2)
        .expect("open lock probe 2");
    assert!(
        probe2.try_lock().is_ok(),
        "unreading hung child must also be reaped"
    );
    let _ = probe2.unlock();

    // 4. Crashed hook (`exit 42` + secret in stderr) fails with `ExitCode::Environment` (4) and redacts stderr.
    let crash_cfg = dir.write(
        "crash.cfg",
        &format!("mode=crash\nstderr=fatal adapter crash token={secret}\n"),
    );
    let crash_out = run_da_env(
        dir.path(),
        &[
            "session",
            "judge",
            &id,
            "--rubric",
            "probe ${env:DA_CRASH_SECRET}",
            "--hook",
            hook_exe,
            "--hook-arg",
            crash_cfg.to_str().unwrap(),
        ],
        &[("DA_CRASH_SECRET", secret)],
    );
    assert_eq!(crash_out.status.code(), Some(4), "{crash_out:?}");
    let crash_stdout = String::from_utf8_lossy(&crash_out.stdout);
    assert!(!crash_stdout.contains(secret), "{crash_stdout}");
    let crash_json = json_stdout(&crash_out);
    assert_eq!(crash_json["error"]["kind"], "environment", "{crash_json}");
    assert!(
        crash_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("token=***"),
        "{crash_json}"
    );

    // 5. Malformed JSON stdout and JSON-RPC error object both fail with `ExitCode::Environment` (4).
    let malformed_cfg = dir.write("malformed.cfg", "mode=malformed\n");
    let malformed_out = run_da_env(
        dir.path(),
        &[
            "session",
            "judge",
            &id,
            "--rubric",
            "check",
            "--hook",
            hook_exe,
            "--hook-arg",
            malformed_cfg.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(malformed_out.status.code(), Some(4), "{malformed_out:?}");

    let rpc_err_cfg = dir.write(
        "rpc_err.cfg",
        "mode=rpc_error\nerror_message=model server overloaded\n",
    );
    let rpc_err_out = run_da_env(
        dir.path(),
        &[
            "session",
            "judge",
            &id,
            "--rubric",
            "check",
            "--hook",
            hook_exe,
            "--hook-arg",
            rpc_err_cfg.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(rpc_err_out.status.code(), Some(4), "{rpc_err_out:?}");
    assert!(
        json_stdout(&rpc_err_out)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("model server overloaded")
    );

    let _ = run_da_env(dir.path(), &["session", "stop", &id], &[]);
}

#[test]
fn judge_screenshot_attaches_masked_png_to_hook_and_requires_visual_session() {
    let dir = TempDir::new("judge-screenshot");
    let hook_exe = mock_hook_exe().to_string_lossy().into_owned();
    let req_file = dir.path().join("judge_req.json");
    let cfg_path = dir.write(
        "judge.cfg",
        &format!(
            "mode=normal\nrequest_file={}\njudge_passed=true\njudge_rationale=Screen verified\nmodel=vlm-judge\nlatency_ms=44\n",
            req_file.display()
        ),
    );

    // 1. CLI `--screenshot` without an active visual session fails with `ExitCode::Environment` (4).
    let cli_started = json_stdout(&run_da_env(dir.path(), &["session", "start"], &[]));
    let cli_id = cli_started["session_id"].as_str().unwrap().to_string();
    let no_visual = run_da_env(
        dir.path(),
        &[
            "session",
            "judge",
            &cli_id,
            "--rubric",
            "Verify visual state",
            "--screenshot",
            "--hook",
            &hook_exe,
            "--hook-arg",
            cfg_path.to_str().unwrap(),
        ],
        &[],
    );
    assert_eq!(no_visual.status.code(), Some(4), "{no_visual:?}");
    let no_visual_json = json_stdout(&no_visual);
    assert_eq!(no_visual_json["error"]["kind"], "environment");
    let _ = run_da_env(dir.path(), &["session", "stop", &cli_id], &[]);

    // 2. With FakeUiSession: `screenshot: false` omits image, `screenshot: true` attaches masked PNG + viewport.
    let sessions_dir = dir.path().join(".da/sessions-fake");
    let registry = fake_ui_registry();
    let hook_spec = ModelHookSpec {
        command: hook_exe,
        args: vec![cfg_path.to_string_lossy().into_owned()],
    };
    let started = session::handle(
        SessionRequest::Start {
            sessions_dir: sessions_dir.clone(),
            idle_timeout_ms: 60_000,
            observe: Vec::new(),
            model_hook: Some(hook_spec),
            model: None,
            plugins: Vec::new(),
        },
        &registry,
    );
    assert_eq!(started.exit_code, ExitCode::Success);
    let id = started.payload["session_id"].as_str().unwrap().to_string();

    let opened = session::handle(
        SessionRequest::Step {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            step: StepInput::Text(
                r#"{"use":"browser.open","with":{"url":"http://127.0.0.1/app"}}"#.to_string(),
            ),
        },
        &registry,
    );
    assert_eq!(opened.exit_code, ExitCode::Success);

    let text_judge = session::handle(
        SessionRequest::Judge(JudgeRequest {
            sessions_dir: sessions_dir.clone(),
            id: id.clone(),
            base_directory: dir.path().to_path_buf(),
            rubric: "Text only check".to_string(),
            evidence: None,
            screenshot: false,
            hook_override: None,
            model_override: None,
            timeout_ms: 5_000,
        }),
        &registry,
    );
    assert_eq!(
        text_judge.exit_code,
        ExitCode::Success,
        "{}",
        text_judge.payload
    );
    let text_req: Value =
        serde_json::from_str(fs::read_to_string(&req_file).unwrap().trim()).unwrap();
    assert_eq!(text_req["method"], "model/judge");
    assert!(
        text_req["params"].get("image_base64").is_none(),
        "screenshot=false must not attach image: {text_req}"
    );

    let visual_judge = session::handle(
        SessionRequest::Judge(JudgeRequest {
            sessions_dir,
            id,
            base_directory: dir.path().to_path_buf(),
            rubric: "Visual check".to_string(),
            evidence: None,
            screenshot: true,
            hook_override: None,
            model_override: None,
            timeout_ms: 5_000,
        }),
        &registry,
    );
    assert_eq!(
        visual_judge.exit_code,
        ExitCode::Success,
        "{}",
        visual_judge.payload
    );
    let visual_req: Value =
        serde_json::from_str(fs::read_to_string(&req_file).unwrap().trim()).unwrap();
    assert_eq!(visual_req["method"], "model/judge");
    assert_eq!(visual_req["params"]["mime_type"], "image/png");
    assert_eq!(
        visual_req["params"]["viewport"],
        json!({ "width": 800, "height": 600 })
    );
    assert_eq!(
        visual_req["params"]["image_base64"],
        base64_encode(&large_masked_png())
    );
}

#[cfg(not(feature = "ai"))]
#[test]
fn builtin_model_without_ai_feature_reports_helpful_usage_error() {
    let dir = TempDir::new("no-ai-feature");
    let out = run_da_env(
        dir.path(),
        &["session", "start", "--model", "openai::gpt-4o-mini"],
        &[],
    );
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let payload = json_stdout(&out);
    assert_eq!(payload["error"]["kind"], "usage");
    let msg = payload["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("ai") && msg.contains("--hook"), "{msg}");
}

#[cfg(feature = "ai")]
mod ai_tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use do_anything::core::case;
    use do_anything::core::events::EventBus;
    use do_anything::core::filter::Filters;
    use do_anything::core::runner;
    use do_anything::core::secrets::Secrets;
    use do_anything::session::model_hook::BuiltinModelSpec;

    #[derive(Debug, Clone)]
    struct RecordedHttpRequest {
        method: String,
        target: String,
        headers: BTreeMap<String, String>,
        body: String,
    }

    #[derive(Debug, Clone)]
    struct MockReply {
        status: u16,
        reason: &'static str,
        body: String,
        delay_ms: u64,
    }

    impl MockReply {
        fn chat_json(model: &str, content: Value) -> Self {
            Self::chat_text(model, &content.to_string())
        }

        fn chat_text(model: &str, content: &str) -> Self {
            let body = json!({
                "id": "chatcmpl-mock-1",
                "object": "chat.completion",
                "created": 1700000000,
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": content,
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 17,
                    "completion_tokens": 9,
                    "total_tokens": 26
                }
            })
            .to_string();
            Self {
                status: 200,
                reason: "OK",
                body,
                delay_ms: 0,
            }
        }

        fn status_text(status: u16, reason: &'static str, message: &str) -> Self {
            Self {
                status,
                reason,
                body: json!({ "error": { "message": message, "type": "mock_error" } }).to_string(),
                delay_ms: 0,
            }
        }
    }

    struct MockModelServer {
        port: u16,
        requests: Arc<Mutex<Vec<RecordedHttpRequest>>>,
        replies: Arc<Mutex<VecDeque<MockReply>>>,
        stop: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl MockModelServer {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock model server");
            let port = listener.local_addr().expect("mock local_addr").port();
            listener
                .set_nonblocking(true)
                .expect("nonblocking mock listener");

            let requests = Arc::new(Mutex::new(Vec::new()));
            let replies = Arc::new(Mutex::new(VecDeque::new()));
            let stop = Arc::new(AtomicBool::new(false));

            let reqs_clone = Arc::clone(&requests);
            let reps_clone = Arc::clone(&replies);
            let stop_clone = Arc::clone(&stop);

            let handle = thread::spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let reqs = Arc::clone(&reqs_clone);
                            let reps = Arc::clone(&reps_clone);
                            thread::spawn(move || handle_http_conn(stream, &reqs, &reps));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });

            Self {
                port,
                requests,
                replies,
                stop,
                handle: Some(handle),
            }
        }

        fn endpoint(&self, suffix: &str) -> String {
            format!("http://127.0.0.1:{}{}", self.port, suffix)
        }

        fn push_reply(&self, reply: MockReply) {
            self.replies.lock().unwrap().push_back(reply);
        }

        fn take_requests(&self) -> Vec<RecordedHttpRequest> {
            let mut guard = self.requests.lock().unwrap();
            std::mem::take(&mut *guard)
        }
    }

    impl Drop for MockModelServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn handle_http_conn(
        stream: TcpStream,
        requests: &Mutex<Vec<RecordedHttpRequest>>,
        replies: &Mutex<VecDeque<MockReply>>,
    ) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let mut reader = BufReader::new(match stream.try_clone() {
            Ok(s) => s,
            Err(_) => return,
        });
        let mut writer = stream;

        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();

        let mut headers = BTreeMap::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim().to_string();
                if key == "content-length" {
                    content_length = val.parse::<usize>().unwrap_or(0);
                }
                headers.insert(key, val);
            }
        }

        let mut body_bytes = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body_bytes).is_err() {
            return;
        }
        let body = String::from_utf8_lossy(&body_bytes).into_owned();
        requests.lock().unwrap().push(RecordedHttpRequest {
            method,
            target,
            headers,
            body,
        });

        let reply = replies.lock().unwrap().pop_front().unwrap_or_else(|| {
            MockReply::status_text(500, "Internal Server Error", "no mock reply queued")
        });

        if reply.delay_ms > 0 {
            thread::sleep(Duration::from_millis(reply.delay_ms));
        }

        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.status,
            reply.reason,
            reply.body.len()
        );
        let _ = writer.write_all(head.as_bytes());
        let _ = writer.write_all(reply.body.as_bytes());
        let _ = writer.flush();
    }

    #[test]
    fn builtin_genai_model_cli_session_secrets_fallback_custom_adapter_and_timeout() {
        let dir = TempDir::new("ai-cli");
        let server = MockModelServer::start();
        let api_secret = "sk-live-secret-998877";
        let url_secret = "tok-url-secret-554433";
        let step_secret = "step-secret-112233";

        // 1. Start session with `--model`, `--model-endpoint` containing `${env:URL_SECRET}`, and `--model-api-key-env`.
        let endpoint_template = format!(
            "{}/v1?token=${{env:DA_TEST_URL_SECRET}}",
            server.endpoint("")
        );
        let start_out = run_da_env(
            dir.path(),
            &[
                "session",
                "start",
                "--model",
                "openai::gpt-4o-mini",
                "--model-endpoint",
                &endpoint_template,
                "--model-api-key-env",
                "DA_TEST_MODEL_KEY",
            ],
            &[
                ("DA_TEST_MODEL_KEY", api_secret),
                ("DA_TEST_URL_SECRET", url_secret),
            ],
        );
        assert_eq!(start_out.status.code(), Some(0), "{start_out:?}");
        let start_stdout = String::from_utf8_lossy(&start_out.stdout);
        assert!(!start_stdout.contains(api_secret), "{start_stdout}");
        assert!(!start_stdout.contains(url_secret), "{start_stdout}");

        let started = json_stdout(&start_out);
        let id = started["session_id"].as_str().unwrap().to_string();
        let session_dir = dir.path().join(started["dir"].as_str().unwrap());

        // `state.json` preserves template `${env:...}` and API key env var name — never raw secret values.
        let state_raw = fs::read_to_string(session_dir.join(store::STATE_FILE)).unwrap();
        assert!(!state_raw.contains(api_secret), "{state_raw}");
        assert!(!state_raw.contains(url_secret), "{state_raw}");
        assert!(state_raw.contains("DA_TEST_MODEL_KEY"), "{state_raw}");
        assert!(
            state_raw.contains("${env:DA_TEST_URL_SECRET}"),
            "{state_raw}"
        );

        // Mutual exclusion check (`--model` + `--hook` -> `ExitCode::Usage` 2).
        let conflict = run_da_env(
            dir.path(),
            &[
                "session",
                "judge",
                &id,
                "--rubric",
                "check",
                "--model",
                "openai::gpt-4o-mini",
                "--hook",
                "missing-hook",
            ],
            &[],
        );
        assert_eq!(conflict.status.code(), Some(2), "{conflict:?}");

        // 2. When `cli` is compiled in, run a CLI step carrying `${env:DA_STEP_SECRET}` first;
        // under `--no-default-features --features ai`, inject the secret via `--evidence`.
        if cfg!(feature = "cli") {
            let step_out = run_da_env(
                dir.path(),
                &[
                    "session",
                    "step",
                    &id,
                    "--step",
                    &json!({
                        "use": "cli.run",
                        "with": {
                            "command": da(),
                            "args": ["--version"],
                            "env": { "TOK": "${env:DA_STEP_SECRET}" }
                        },
                        "expect": [{ "path": "$.exit_code", "equals": 0 }]
                    })
                    .to_string(),
                ],
                &[("DA_STEP_SECRET", step_secret)],
            );
            assert_eq!(step_out.status.code(), Some(0), "{step_out:?}");
        }

        server.push_reply(MockReply::chat_json(
            "gpt-4o-mini-2026",
            json!({
                "passed": true,
                "rationale": format!("Verified cleanly without exposing {api_secret} or {step_secret}"),
                "confidence": 0.94
            }),
        ));
        let mut judge_args = vec![
            "session",
            "judge",
            &id,
            "--rubric",
            "Confirm version printed cleanly without ${env:DA_STEP_SECRET}",
        ];
        if !cfg!(feature = "cli") {
            judge_args.push("--evidence");
            judge_args.push(r#"{"tok":"${env:DA_STEP_SECRET}"}"#);
        }
        let judge_ok = run_da_env(
            dir.path(),
            &judge_args,
            &[
                ("DA_TEST_MODEL_KEY", api_secret),
                ("DA_TEST_URL_SECRET", url_secret),
                ("DA_STEP_SECRET", step_secret),
            ],
        );
        assert_eq!(judge_ok.status.code(), Some(0), "{judge_ok:?}");
        let judge_json = json_stdout(&judge_ok);
        assert_eq!(judge_json["passed"], true, "{judge_json}");
        assert_eq!(judge_json["model"], "gpt-4o-mini-2026");
        assert_eq!(judge_json["usage"]["total_tokens"], 26);
        assert_eq!(
            judge_json["rationale"],
            "Verified cleanly without exposing *** or ***"
        );

        let recorded = server.take_requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].method, "POST");
        assert!(
            recorded[0].target.starts_with("/v1/chat/completions"),
            "{}",
            recorded[0].target
        );
        assert_eq!(
            recorded[0].headers.get("authorization").map(String::as_str),
            Some("Bearer sk-live-secret-998877")
        );
        assert!(
            !recorded[0].body.contains(step_secret),
            "{}",
            recorded[0].body
        );
        assert!(
            !recorded[0].body.contains(api_secret),
            "{}",
            recorded[0].body
        );
        let req_json: Value = serde_json::from_str(&recorded[0].body).unwrap();
        assert_eq!(req_json["response_format"]["type"], "json_schema");

        // 3. `JsonSpec` HTTP 400 fallback + fenced JSON recovery (`passed: false` -> ExitCode::TestFailure 1).
        server.push_reply(MockReply::status_text(
            400,
            "Bad Request",
            "response_format json_schema is unsupported by mock server",
        ));
        server.push_reply(MockReply::chat_text(
            "gpt-4o-mini-2026",
            "```json\n{\"passed\":false,\"rationale\":\"Banner visible\",\"confidence\":0.75}\n```",
        ));
        let judge_fail = run_da_env(
            dir.path(),
            &["session", "judge", &id, "--rubric", "Check fallback"],
            &[
                ("DA_TEST_MODEL_KEY", api_secret),
                ("DA_TEST_URL_SECRET", url_secret),
            ],
        );
        assert_eq!(judge_fail.status.code(), Some(1), "{judge_fail:?}");
        let fail_json = json_stdout(&judge_fail);
        assert_eq!(fail_json["passed"], false, "{fail_json}");
        assert_eq!(fail_json["rationale"], "Banner visible");
        let fallback_reqs = server.take_requests();
        assert_eq!(fallback_reqs.len(), 2);
        let second_req: Value = serde_json::from_str(&fallback_reqs[1].body).unwrap();
        assert!(
            second_req.get("response_format").is_none(),
            "retry must drop response_format: {second_req}"
        );

        // 4. Slow server exceeding `--timeout-ms` returns `ExitCode::Timeout` (5).
        server.push_reply(MockReply {
            delay_ms: 1500,
            ..MockReply::chat_json("gpt-4o-mini", json!({"passed": true, "rationale": "late"}))
        });
        let timeout_out = run_da_env(
            dir.path(),
            &[
                "session",
                "judge",
                &id,
                "--rubric",
                "check timeout",
                "--timeout-ms",
                "300",
            ],
            &[
                ("DA_TEST_MODEL_KEY", api_secret),
                ("DA_TEST_URL_SECRET", url_secret),
            ],
        );
        assert_eq!(timeout_out.status.code(), Some(5), "{timeout_out:?}");
        let _ = server.take_requests();

        // 5. Missing API key env var returns `ExitCode::Environment` (4).
        let missing_key = run_da_env(
            dir.path(),
            &[
                "session",
                "judge",
                &id,
                "--rubric",
                "check missing key",
                "--model",
                "openai::gpt-4o-mini",
                "--endpoint",
                &server.endpoint("/v1"),
                "--api-key-env",
                "DA_UNSET_TEST_KEY",
            ],
            &[],
        );
        assert_eq!(missing_key.status.code(), Some(4), "{missing_key:?}");
        assert!(
            json_stdout(&missing_key)["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("DA_UNSET_TEST_KEY")
        );

        // 6. Export omits `model.judge`; disk files contain zero secrets.
        let exp_out = run_da_env(dir.path(), &["session", "export", &id, "--yaml"], &[]);
        let exported_yaml = String::from_utf8_lossy(&exp_out.stdout);
        if cfg!(feature = "cli") {
            assert_eq!(exp_out.status.code(), Some(0), "{exp_out:?}");
            assert!(exported_yaml.contains("cli.run"), "{exported_yaml}");
        } else {
            assert_eq!(exp_out.status.code(), Some(3), "{exp_out:?}");
        }
        assert!(!exported_yaml.contains("model.judge"), "{exported_yaml}");
        for entry in fs::read_dir(&session_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let text = fs::read_to_string(&path).unwrap();
                assert!(!text.contains(api_secret), "{}", path.display());
                assert!(!text.contains(url_secret), "{}", path.display());
                assert!(!text.contains(step_secret), "{}", path.display());
            }
        }
        let _ = run_da_env(dir.path(), &["session", "stop", &id], &[]);

        // 7. Session without `--model` falls back to `DA_MODEL*` (anonymous `omlx` over `/v1`) AND `genai_1` custom protocol works!
        let bare_start = json_stdout(&run_da_env(dir.path(), &["session", "start"], &[]));
        let bare_id = bare_start["session_id"].as_str().unwrap().to_string();

        server.push_reply(MockReply::chat_json(
            "qwen-local",
            json!({"passed": true, "rationale": "Env fallback ok"}),
        ));
        let env_fallback = run_da_env(
            dir.path(),
            &["session", "judge", &bare_id, "--rubric", "Check DA_MODEL"],
            &[
                ("DA_MODEL", "qwen-local"),
                ("DA_MODEL_ADAPTER", "omlx"),
                ("DA_MODEL_ENDPOINT", &server.endpoint("/v1")),
            ],
        );
        assert_eq!(env_fallback.status.code(), Some(0), "{env_fallback:?}");
        let env_reqs = server.take_requests();
        assert_eq!(env_reqs.len(), 1);
        assert!(
            !env_reqs[0].headers.contains_key("authorization"),
            "omlx anonymous request must omit Authorization header: {:?}",
            env_reqs[0].headers
        );

        server.push_reply(MockReply::chat_json(
            "jev-custom-0.2b",
            json!({"passed": true, "rationale": "Custom genai_1 ok"}),
        ));
        let custom_proto = run_da_env(
            dir.path(),
            &[
                "session",
                "judge",
                &bare_id,
                "--rubric",
                "Check custom protocol",
                "--model",
                "genai_1::jev-custom-0.2b",
                "--endpoint",
                &server.endpoint("/v1"),
            ],
            &[],
        );
        assert_eq!(custom_proto.status.code(), Some(0), "{custom_proto:?}");
        assert_eq!(json_stdout(&custom_proto)["model"], "jev-custom-0.2b");
        let _ = run_da_env(dir.path(), &["session", "stop", &bare_id], &[]);
    }

    #[test]
    fn builtin_genai_select_ref_locate_visual_judge_and_model_free_replay() {
        let dir = TempDir::new("ai-visual-flow");
        let sessions_dir = dir.path().join(".da/sessions");
        let registry = fake_ui_registry();
        let server = Some(MockModelServer::start());
        let srv = server.as_ref().unwrap();
        let model_spec = BuiltinModelSpec {
            model: "local-vlm".to_string(),
            adapter: Some("omlx".to_string()),
            endpoint: Some(srv.endpoint("/v1")),
            api_key_env: None,
        };

        let started = session::handle(
            SessionRequest::Start {
                sessions_dir: sessions_dir.clone(),
                idle_timeout_ms: 60_000,
                observe: Vec::new(),
                model_hook: None,
                model: Some(model_spec),
                plugins: Vec::new(),
            },
            &registry,
        );
        assert_eq!(started.exit_code, ExitCode::Success);
        let id = started.payload["session_id"].as_str().unwrap().to_string();

        let opened = session::handle(
            SessionRequest::Step {
                sessions_dir: sessions_dir.clone(),
                id: id.clone(),
                base_directory: dir.path().to_path_buf(),
                step: StepInput::Text(
                    r#"{"use":"browser.open","with":{"url":"http://127.0.0.1/app"}}"#.to_string(),
                ),
            },
            &registry,
        );
        assert_eq!(opened.exit_code, ExitCode::Success);

        // 1. Hallucinated ref rejected cleanly; valid `e2` synthesized and clicked.
        srv.push_reply(MockReply::chat_json("jev-v1", json!({ "ref": "e999" })));
        let bad_sel = session::handle(
            SessionRequest::SelectRef(SelectRefRequest {
                sessions_dir: sessions_dir.clone(),
                id: id.clone(),
                base_directory: dir.path().to_path_buf(),
                query: "Save".to_string(),
                scope_ref: None,
                max_depth: None,
                max_nodes: None,
                hook_override: None,
                model_override: None,
                timeout_ms: 5_000,
                act: None,
            }),
            &registry,
        );
        assert_eq!(bad_sel.exit_code, ExitCode::Environment, "{bad_sel:?}");
        let _ = srv.take_requests();

        srv.push_reply(MockReply::chat_json(
            "jev-v1",
            json!({ "ref": "e2", "confidence": 0.96, "reason": "Save Order button" }),
        ));
        let ok_sel = session::handle(
            SessionRequest::SelectRef(SelectRefRequest {
                sessions_dir: sessions_dir.clone(),
                id: id.clone(),
                base_directory: dir.path().to_path_buf(),
                query: "Click Save Order".to_string(),
                scope_ref: None,
                max_depth: None,
                max_nodes: None,
                hook_override: None,
                model_override: None,
                timeout_ms: 5_000,
                act: Some(SelectRefAction {
                    action: "browser.click".to_string(),
                    with_raw: None,
                    args: Vec::new(),
                    save: Vec::new(),
                }),
            }),
            &registry,
        );
        assert_eq!(ok_sel.exit_code, ExitCode::Success, "{}", ok_sel.payload);
        assert_eq!(
            ok_sel.payload["synthesized_locator"],
            json!({ "testid": "save-order-btn" })
        );
        let _ = srv.take_requests();

        // 2. Multimodal `locate` sends `image_url` PNG data URL and synthesizes locator (or `at:` on `<canvas>`).
        srv.push_reply(MockReply::chat_json(
            "ui-tars",
            json!({ "x": 120, "y": 45, "confidence": 0.92 }),
        ));
        let ok_loc = session::handle(
            SessionRequest::Locate(LocateRequest {
                sessions_dir: sessions_dir.clone(),
                id: id.clone(),
                base_directory: dir.path().to_path_buf(),
                query: "Click Save button".to_string(),
                hook_override: None,
                model_override: None,
                timeout_ms: 5_000,
                click: true,
            }),
            &registry,
        );
        assert_eq!(ok_loc.exit_code, ExitCode::Success, "{}", ok_loc.payload);
        assert_eq!(
            ok_loc.payload["synthesized_locator"],
            json!({ "role": "button", "name": "Save Order" })
        );
        let expected_data_url = format!(
            "data:image/png;base64,{}",
            base64_encode(&large_masked_png())
        );
        let loc_reqs = srv.take_requests();
        assert_eq!(loc_reqs.len(), 1);
        assert!(
            loc_reqs[0].body.contains(&expected_data_url),
            "locate must attach masked PNG image_url data URL"
        );

        srv.push_reply(MockReply::chat_json(
            "ui-tars",
            json!({ "x": 900, "y": 45 }),
        ));
        let oob_loc = session::handle(
            SessionRequest::Locate(LocateRequest {
                sessions_dir: sessions_dir.clone(),
                id: id.clone(),
                base_directory: dir.path().to_path_buf(),
                query: "Out of bounds".to_string(),
                hook_override: None,
                model_override: None,
                timeout_ms: 5_000,
                click: false,
            }),
            &registry,
        );
        assert_eq!(oob_loc.exit_code, ExitCode::Environment, "{oob_loc:?}");
        let _ = srv.take_requests();

        // 3. Multimodal `judge` with `screenshot: true` ALSO attaches exact `image_url` data URL!
        srv.push_reply(MockReply::chat_json(
            "qwen-vl-judge",
            json!({ "passed": true, "rationale": "Screenshot matches Save Order screen" }),
        ));
        let visual_judge = session::handle(
            SessionRequest::Judge(JudgeRequest {
                sessions_dir: sessions_dir.clone(),
                id: id.clone(),
                base_directory: dir.path().to_path_buf(),
                rubric: "Verify button state on screen".to_string(),
                evidence: None,
                screenshot: true,
                hook_override: None,
                model_override: None,
                timeout_ms: 5_000,
            }),
            &registry,
        );
        assert_eq!(
            visual_judge.exit_code,
            ExitCode::Success,
            "{}",
            visual_judge.payload
        );
        let judge_reqs = srv.take_requests();
        assert_eq!(judge_reqs.len(), 1);
        assert!(
            judge_reqs[0].body.contains(&expected_data_url),
            "judge with screenshot=true must send masked PNG image_url"
        );

        // 4. Export YAML, shut down the mock model server completely, and replay deterministically!
        let out_case = dir.path().join("ai-exported.yaml");
        let exported = session::handle(
            SessionRequest::Export {
                sessions_dir: sessions_dir.clone(),
                id: id.clone(),
                options: ExportOptions {
                    case_name: Some("AI explored flow".to_string()),
                    include_reads: false,
                },
                out_path: Some(out_case.clone()),
            },
            &registry,
        );
        assert_eq!(
            exported.exit_code,
            ExitCode::Success,
            "{}",
            exported.payload
        );
        let _ = session::handle(SessionRequest::Stop { sessions_dir, id }, &registry);
        drop(server);

        let parsed = case::load_file(&out_case).expect("exported YAML parses");
        let mut secrets = Secrets::new();
        let preflight_errors =
            runner::preflight(std::slice::from_ref(&parsed), &registry, &mut secrets);
        assert!(preflight_errors.is_empty(), "{preflight_errors:?}");
        let replay = runner::run(
            &[parsed],
            &registry,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut secrets,
            1,
        );
        assert!(
            replay.is_ok(),
            "replay must pass deterministically after stopping mock model server: {replay:?}"
        );
    }
}
