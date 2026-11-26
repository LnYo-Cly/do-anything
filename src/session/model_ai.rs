//! Built-in multi-provider model access layer backed by `genai` (`ai` feature).
//!
//! Invokes `select_ref`, `locate`, and `judge` across `genai`'s adapters
//! (OpenAI Chat/Responses, Anthropic, Gemini, DeepSeek, Ollama, Omlx, and custom
//! endpoints/`genai_<n>` adapters) using `rustls` with the workspace `ring`
//! crypto provider. Every outgoing payload and incoming field/error passes
//! through [`Secrets`], and calls remain strictly exploration-time only (`da run`
//! stays model-free).

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, ContentPart, JsonSpec,
};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget, webc};
use reqwest::{StatusCode, Url};
use serde_json::{Map, Value, json};

use crate::core::case;
use crate::core::secrets::Secrets;
use crate::core::session::MaskedViewportCapture;
use crate::report::base64_encode;

use super::model_hook::{
    BuiltinModelSpec, HookError, JudgeOutcome, JudgeParams, LocateOutcome, LocateParams,
    METHOD_JUDGE, METHOD_LOCATE, METHOD_SELECT_REF, ModelAudit, SelectRefOutcome, SelectRefParams,
    build_judge_payload, extract_confidence, validate_judge_fields, validate_locate_coords,
    validate_select_ref_choice,
};

const VALID_ADAPTER_HINT: &str = "use a `genai` adapter name such as `openai`, `openai_resp`, `anthropic`, `gemini`, `gemini_ix`, `deepseek`, `ollama`, `ollama_cloud`, `omlx`, `open_router`, or `genai_<n>`";

/// Validates a [`BuiltinModelSpec`] before persisting or invoking it.
pub(crate) fn validate_spec(spec: &BuiltinModelSpec) -> Result<(), String> {
    if spec.model.trim().is_empty() {
        return Err("`--model` must not be empty".to_string());
    }
    if let Some(adapter) = &spec.adapter {
        parse_adapter_kind(adapter)?;
    }
    if let Some(endpoint) = &spec.endpoint {
        let trimmed = endpoint.trim();
        if trimmed.is_empty() {
            return Err("`--model-endpoint` / `--endpoint` must not be empty".to_string());
        }
        if !trimmed.contains("${env:") {
            validate_http_url(trimmed)?;
        }
    }
    if let Some(env_name) = &spec.api_key_env {
        let trimmed = env_name.trim();
        if trimmed.is_empty() || trimmed.contains('=') || trimmed.chars().any(char::is_whitespace) {
            return Err(format!(
                "invalid API key environment variable name `{env_name}`"
            ));
        }
    }
    Ok(())
}

/// Collects environment variable names that should be tracked and redacted by [`Secrets`].
pub(crate) fn collect_env_names(spec: &BuiltinModelSpec, env_names: &mut BTreeSet<String>) {
    let parsed_endpoint = spec
        .endpoint
        .as_deref()
        .and_then(|raw| Url::parse(raw.trim()).ok());
    if let Some(endpoint) = &spec.endpoint {
        env_names.extend(case::env_references(&Value::String(endpoint.clone())));
    }
    if let Some(key_env) = &spec.api_key_env {
        let trimmed = key_env.trim();
        if !trimmed.is_empty() {
            env_names.insert(trimmed.to_string());
        }
    }
    if let Ok(adapter) = select_adapter_kind(spec, parsed_endpoint.as_ref()) {
        if let Some(default_env) = adapter.default_key_env_name() {
            env_names.insert(default_env.to_string());
        }
        if let AdapterKind::Custom(n) = adapter {
            env_names.insert(format!("GENAI_{n}_API_KEY"));
        }
    }
}

/// Chooses the best candidate `ref` via `genai` and validates membership in `candidates`.
pub(crate) fn call_select_ref(
    spec: &BuiltinModelSpec,
    timeout_ms: u64,
    params: SelectRefParams<'_>,
    secrets: &mut Secrets,
) -> Result<SelectRefOutcome, HookError> {
    let valid_refs: BTreeSet<&str> = params
        .candidates
        .iter()
        .filter_map(|c| c.get("ref").and_then(Value::as_str))
        .collect();

    let payload = secrets.redact_json(&json!({
        "session_id": params.session_id,
        "session_kind": params.session_kind,
        "query": params.query,
        "candidates": params.candidates,
    }));
    let system_prompt = "You select the single best matching UI candidate `ref` from `candidates` for the user's `query`. Reply ONLY with JSON matching {\"ref\": \"<exact ref from candidates>\", \"confidence\": <number 0.0..=1.0 or null>}. Never invent a ref not in `candidates`.";
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["ref", "confidence"],
        "properties": {
            "ref": { "type": "string" },
            "confidence": { "type": ["number", "null"] }
        }
    });

    let (obj, audit) = exec_structured_op(
        spec,
        timeout_ms,
        StructuredOp {
            method: METHOD_SELECT_REF,
            schema_name: "select_ref_outcome",
            schema,
            system_prompt,
            user_text: payload.to_string(),
            image: None,
        },
        secrets,
    )?;
    let selected_ref = validate_select_ref_choice(&obj, &valid_refs, secrets)?;
    Ok(SelectRefOutcome {
        selected_ref,
        audit,
    })
}

/// Grounds a visual query on an input-masked PNG via `genai` and validates `(x, y)` bounds.
pub(crate) fn call_locate(
    spec: &BuiltinModelSpec,
    timeout_ms: u64,
    params: LocateParams<'_>,
    secrets: &mut Secrets,
) -> Result<LocateOutcome, HookError> {
    if params.width == 0 || params.height == 0 {
        return Err(HookError::Environment(
            "cannot call `model/locate` with a zero-sized viewport".to_string(),
        ));
    }
    let capture = MaskedViewportCapture {
        png: params.masked_png.to_vec(),
        width: params.width,
        height: params.height,
    };
    let payload = secrets.redact_json(&json!({
        "session_id": params.session_id,
        "session_kind": params.session_kind,
        "query": params.query,
        "viewport": {
            "width": params.width,
            "height": params.height,
        },
    }));
    let system_prompt = format!(
        "You locate UI targets on the attached {}x{} screenshot (sensitive fields are masked in solid black). Return the target center pixel coordinates within 0 <= x < {} and 0 <= y < {}. Reply ONLY with JSON matching {{\"x\": <number>, \"y\": <number>, \"confidence\": <number 0.0..=1.0 or null>}}.",
        params.width, params.height, params.width, params.height
    );
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["x", "y", "confidence"],
        "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" },
            "confidence": { "type": ["number", "null"] }
        }
    });

    let (obj, audit) = exec_structured_op(
        spec,
        timeout_ms,
        StructuredOp {
            method: METHOD_LOCATE,
            schema_name: "locate_outcome",
            schema,
            system_prompt: &system_prompt,
            user_text: payload.to_string(),
            image: Some(&capture),
        },
        secrets,
    )?;
    let (x, y) = validate_locate_coords(&obj, params.width, params.height)?;
    Ok(LocateOutcome { x, y, audit })
}

/// Evaluates a judge rubric over redacted evidence (and optional masked screenshot) via `genai`.
pub(crate) fn call_judge(
    spec: &BuiltinModelSpec,
    timeout_ms: u64,
    params: JudgeParams<'_>,
    secrets: &mut Secrets,
) -> Result<JudgeOutcome, HookError> {
    if let Some(capture) = params.masked_capture
        && (capture.width == 0 || capture.height == 0)
    {
        return Err(HookError::Environment(
            "cannot call `model/judge` with a zero-sized viewport".to_string(),
        ));
    }

    let mut payload = build_judge_payload(&params, secrets);
    if let Some(capture) = params.masked_capture
        && let Value::Object(map) = &mut payload
    {
        map.insert(
            "viewport".to_string(),
            json!({ "width": capture.width, "height": capture.height }),
        );
    }
    let system_prompt = if params.masked_capture.is_some() {
        "You judge whether an interactive UI session satisfies `rubric` using the provided evidence, snapshot, and attached input-masked screenshot (sensitive inputs are blacked out). Reply ONLY with JSON matching {\"passed\": <boolean>, \"rationale\": \"<concise explanation>\", \"confidence\": <number 0.0..=1.0 or null>}."
    } else {
        "You judge whether an interactive test session satisfies `rubric` using the provided evidence and snapshot. Reply ONLY with JSON matching {\"passed\": <boolean>, \"rationale\": \"<concise explanation>\", \"confidence\": <number 0.0..=1.0 or null>}."
    };
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["passed", "rationale", "confidence"],
        "properties": {
            "passed": { "type": "boolean" },
            "rationale": { "type": "string" },
            "confidence": { "type": ["number", "null"] }
        }
    });

    let (obj, audit) = exec_structured_op(
        spec,
        timeout_ms,
        StructuredOp {
            method: METHOD_JUDGE,
            schema_name: "judge_outcome",
            schema,
            system_prompt,
            user_text: payload.to_string(),
            image: params.masked_capture,
        },
        secrets,
    )?;
    let (passed, rationale) = validate_judge_fields(&obj, secrets)?;
    Ok(JudgeOutcome {
        passed,
        rationale,
        audit,
    })
}

struct ResolvedTarget {
    adapter: AdapterKind,
    model_name: String,
    endpoint: Option<String>,
    auth: AuthData,
}

struct StructuredOp<'a> {
    method: &'a str,
    schema_name: &'a str,
    schema: Value,
    system_prompt: &'a str,
    user_text: String,
    image: Option<&'a MaskedViewportCapture>,
}

fn timeout_error(
    spec: &BuiltinModelSpec,
    method: &str,
    timeout_ms: u64,
    secrets: &Secrets,
) -> HookError {
    HookError::Timeout(secrets.redact(&format!(
        "model `{}` (`{method}`) did not finish within {timeout_ms} ms",
        spec.model
    )))
}

fn exec_structured_op(
    spec: &BuiltinModelSpec,
    timeout_ms: u64,
    op: StructuredOp<'_>,
    secrets: &mut Secrets,
) -> Result<(Map<String, Value>, ModelAudit), HookError> {
    let StructuredOp {
        method,
        schema_name,
        schema,
        system_prompt,
        user_text,
        image,
    } = op;
    if timeout_ms == 0 {
        return Err(HookError::Usage(
            "`--timeout-ms` must be at least 1".to_string(),
        ));
    }
    validate_spec(spec).map_err(HookError::Usage)?;

    let resolved = resolve_target(spec, secrets)?;
    let redacted_system = secrets.redact(system_prompt);
    let redacted_user = secrets.redact(&user_text);

    let user_msg = if let Some(capture) = image {
        ChatMessage::user(vec![
            ContentPart::from_text(redacted_user),
            ContentPart::from_binary_base64(
                "image/png",
                base64_encode(&capture.png),
                Some("masked-viewport.png".to_string()),
            ),
        ])
    } else {
        ChatMessage::user(redacted_user)
    };
    let chat_req = ChatRequest::new(vec![ChatMessage::system(redacted_system), user_msg]);

    let _ = rustls::crypto::ring::default_provider().install_default();
    let total_budget = Duration::from_millis(timeout_ms);
    let reqwest_client = reqwest::Client::builder()
        .timeout(total_budget)
        .build()
        .map_err(|err| {
            HookError::Environment(secrets.redact(&format!(
                "could not initialize HTTP client for `{method}`: {err}"
            )))
        })?;

    let endpoint_override = resolved.endpoint.clone().map(Endpoint::from_owned);
    let auth_override = resolved.auth;
    let target_adapter = resolved.adapter;
    let resolver = ServiceTargetResolver::from_resolver_fn(move |mut target: ServiceTarget| {
        if let Some(ep) = &endpoint_override {
            target.endpoint = ep.clone();
        }
        target.auth = auth_override.clone();
        target.model = ModelIden::new(target_adapter, target.model.model_name);
        Ok(target)
    });

    let client = Client::builder()
        .with_reqwest(reqwest_client)
        .with_service_target_resolver(resolver)
        .build()
        .map_err(|err| {
            HookError::Environment(secrets.redact(&format!(
                "could not build `genai` client for `{method}`: {err}"
            )))
        })?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|err| {
            HookError::Environment(format!(
                "could not initialize async runtime for `{method}`: {err}"
            ))
        })?;

    let model_iden = ModelIden::new(resolved.adapter, resolved.model_name.as_str());
    let started = Instant::now();

    let strict_opts = ChatOptions::default()
        .with_normalize_reasoning_content(true)
        .with_response_format(ChatResponseFormat::JsonSpec(JsonSpec::new(
            schema_name,
            schema,
        )));

    let first_attempt = run_with_deadline(
        &rt,
        total_budget,
        client.exec_chat(model_iden.clone(), chat_req.clone(), Some(&strict_opts)),
    );

    let chat_res = match first_attempt {
        Ok(res) => res,
        Err(AttemptError::Timeout) => {
            return Err(timeout_error(spec, method, timeout_ms, secrets));
        }
        Err(AttemptError::Genai(err)) if should_retry_without_json_spec(&err) => {
            let elapsed = started.elapsed();
            let Some(remaining) = total_budget.checked_sub(elapsed) else {
                return Err(timeout_error(spec, method, timeout_ms, secrets));
            };
            if remaining <= Duration::from_millis(50) {
                return Err(timeout_error(spec, method, timeout_ms, secrets));
            }
            let plain_opts = ChatOptions::default().with_normalize_reasoning_content(true);
            match run_with_deadline(
                &rt,
                remaining,
                client.exec_chat(model_iden, chat_req, Some(&plain_opts)),
            ) {
                Ok(res) => res,
                Err(AttemptError::Timeout) => {
                    return Err(timeout_error(spec, method, timeout_ms, secrets));
                }
                Err(AttemptError::Genai(err2)) => {
                    return Err(map_genai_error(err2, spec, method, timeout_ms, secrets));
                }
            }
        }
        Err(AttemptError::Genai(err)) => {
            return Err(map_genai_error(err, spec, method, timeout_ms, secrets));
        }
    };

    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let raw_text = chat_res.first_text().ok_or_else(|| {
        HookError::Environment(secrets.redact(&format!(
            "model `{}` (`{method}`) returned no text content",
            spec.model
        )))
    })?;

    let obj = parse_json_object_text(raw_text).map_err(|reason| {
        HookError::Environment(secrets.redact(&format!(
            "model `{}` (`{method}`) did not return valid JSON object output: {reason}",
            spec.model
        )))
    })?;

    let confidence = extract_confidence(&obj, method)?;
    let reported_model = chat_res.provider_model_iden.model_name.as_str().trim();
    let model = if reported_model.is_empty() {
        secrets.redact(spec.model.trim())
    } else {
        secrets.redact(reported_model)
    };

    let mut usage = chat_res.usage;
    usage.compact_details();
    let usage_json = serde_json::to_value(&usage)
        .ok()
        .filter(|val| val.as_object().is_some_and(|m| !m.is_empty()))
        .map(|val| secrets.redact_json(&val));

    let trace_id = chat_res
        .response_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| secrets.redact(s));

    Ok((
        obj,
        ModelAudit {
            model,
            confidence,
            latency_ms: elapsed_ms,
            usage: usage_json,
            trace_id,
        },
    ))
}

enum AttemptError {
    Timeout,
    Genai(genai::Error),
}

fn run_with_deadline<F>(
    rt: &tokio::runtime::Runtime,
    deadline: Duration,
    fut: F,
) -> Result<genai::chat::ChatResponse, AttemptError>
where
    F: std::future::Future<Output = genai::Result<genai::chat::ChatResponse>>,
{
    match rt.block_on(async move { tokio::time::timeout(deadline, fut).await }) {
        Ok(Ok(res)) => Ok(res),
        Ok(Err(err)) => {
            if is_timeout_error(&err) {
                Err(AttemptError::Timeout)
            } else {
                Err(AttemptError::Genai(err))
            }
        }
        Err(_) => Err(AttemptError::Timeout),
    }
}

fn is_timeout_error(err: &genai::Error) -> bool {
    matches!(
        err,
        genai::Error::WebModelCall {
            webc_error: webc::Error::Reqwest(req_err),
            ..
        } | genai::Error::WebAdapterCall {
            webc_error: webc::Error::Reqwest(req_err),
            ..
        } if req_err.is_timeout()
    )
}

fn should_retry_without_json_spec(err: &genai::Error) -> bool {
    let (status, body) = match err {
        genai::Error::WebModelCall {
            webc_error: webc::Error::ResponseFailedStatus { status, body, .. },
            ..
        } => (*status, body.as_str()),
        genai::Error::HttpError { status, body, .. } => (*status, body.as_str()),
        _ => return false,
    };
    if status != StatusCode::BAD_REQUEST && status != StatusCode::UNPROCESSABLE_ENTITY {
        return false;
    }
    let lowered = body.to_ascii_lowercase();
    lowered.contains("response_format")
        || lowered.contains("json_schema")
        || lowered.contains("structured")
}

fn map_genai_error(
    err: genai::Error,
    spec: &BuiltinModelSpec,
    method: &str,
    timeout_ms: u64,
    secrets: &Secrets,
) -> HookError {
    if is_timeout_error(&err) {
        return HookError::Timeout(secrets.redact(&format!(
            "model `{}` (`{method}`) did not finish within {timeout_ms} ms",
            spec.model
        )));
    }
    let detail = format_genai_error(&err);
    HookError::Environment(secrets.redact(&format!(
        "model `{}` (`{method}`) call failed: {detail}",
        spec.model
    )))
}

fn format_genai_error(err: &genai::Error) -> String {
    match err {
        genai::Error::WebModelCall {
            webc_error: webc::Error::Reqwest(req_err),
            ..
        }
        | genai::Error::WebAdapterCall {
            webc_error: webc::Error::Reqwest(req_err),
            ..
        } => {
            let mut msg = err.to_string();
            let mut cur = std::error::Error::source(req_err);
            while let Some(src) = cur {
                msg.push_str(&format!("\nCause: {src}"));
                cur = src.source();
            }
            msg
        }
        _ => err.to_string(),
    }
}

fn resolve_target(
    spec: &BuiltinModelSpec,
    secrets: &mut Secrets,
) -> Result<ResolvedTarget, HookError> {
    let mut expanded_endpoint = match &spec.endpoint {
        Some(raw) => {
            let expanded = case::expand_env(raw, secrets).map_err(|msg| {
                HookError::Environment(secrets.redact(&format!("`--endpoint`: {msg}")))
            })?;
            let parsed = validate_http_url(&expanded).map_err(HookError::Usage)?;
            Some(parsed)
        }
        None => None,
    };

    let mut adapter =
        select_adapter_kind(spec, expanded_endpoint.as_ref()).map_err(HookError::Usage)?;

    let auth = if let Some(env_name) = &spec.api_key_env {
        let env_name = env_name.trim();
        let val = read_non_empty_env(env_name).ok_or_else(|| {
            HookError::Environment(format!(
                "model API key environment variable `{env_name}` is not set or empty"
            ))
        })?;
        secrets.record(&val);
        AuthData::from_single(val)
    } else if let AdapterKind::Custom(n) = adapter {
        let key_env = format!("GENAI_{n}_API_KEY");
        if let Some(val) = read_non_empty_env(&key_env) {
            secrets.record(&val);
            AuthData::from_single(val)
        } else {
            AuthData::None
        }
    } else if let Some(default_env) = adapter.default_key_env_name() {
        if let Some(val) = read_non_empty_env(default_env) {
            secrets.record(&val);
            AuthData::from_single(val)
        } else if expanded_endpoint.is_some() || matches!(adapter, AdapterKind::Omlx) {
            AuthData::None
        } else {
            return Err(HookError::Environment(format!(
                "model adapter `{}` requires API key environment variable `{default_env}` (or pass `--api-key-env <VAR>`)",
                adapter.as_lower_str()
            )));
        }
    } else {
        AuthData::None
    };

    if let AdapterKind::Custom(n) = adapter
        && expanded_endpoint.is_none()
    {
        if let Some(env_ep) = read_non_empty_env(&format!("GENAI_{n}_ENDPOINT")) {
            expanded_endpoint = Some(validate_http_url(&env_ep).map_err(HookError::Usage)?);
        } else {
            return Err(HookError::Usage(format!(
                "custom adapter `{adapter}` requires `GENAI_{n}_ENDPOINT` or `--endpoint <URL>`"
            )));
        }
    }

    // OpenAIAdapter and CustomAdapter both reject `AuthData::None`, whereas
    // OmlxAdapter shares the identical OpenAI Chat Completions payload while
    // permitting anonymous local/custom OpenAI-compatible endpoints.
    if matches!(auth, AuthData::None)
        && matches!(adapter, AdapterKind::OpenAI | AdapterKind::Custom(_))
        && expanded_endpoint.is_some()
    {
        adapter = AdapterKind::Omlx;
    }

    let endpoint = expanded_endpoint
        .as_ref()
        .map(|url| normalize_endpoint(url, adapter));

    let raw_model = spec.model.trim();
    let (_, bare_model) = split_model_namespace(raw_model);
    let model_name = if spec.adapter.is_some() || expanded_endpoint.is_some() {
        bare_model.to_string()
    } else {
        raw_model.to_string()
    };

    Ok(ResolvedTarget {
        adapter,
        model_name,
        endpoint,
        auth,
    })
}

fn split_model_namespace(model: &str) -> (Option<&str>, &str) {
    match model.split_once("::") {
        Some((ns, name)) => (Some(ns), name),
        None => (None, model),
    }
}

fn select_adapter_kind(
    spec: &BuiltinModelSpec,
    endpoint: Option<&Url>,
) -> Result<AdapterKind, String> {
    if let Some(raw_adapter) = &spec.adapter {
        return parse_adapter_kind(raw_adapter);
    }
    let raw_model = spec.model.trim();
    let (ns, _) = split_model_namespace(raw_model);
    let inferred = AdapterKind::from_model(raw_model)
        .map_err(|err| format!("could not resolve adapter for model `{raw_model}`: {err}"))?;
    if ns.is_none()
        && inferred == AdapterKind::Ollama
        && let Some(url) = endpoint
        && is_openai_v1_path(url.path())
    {
        return Ok(AdapterKind::Omlx);
    }
    Ok(inferred)
}

fn is_openai_v1_path(path: &str) -> bool {
    let trimmed = path.trim_end_matches('/');
    trimmed.ends_with("/v1") || trimmed.ends_with("/v1/chat/completions")
}

fn parse_adapter_kind(raw: &str) -> Result<AdapterKind, String> {
    let lowered = raw.trim().to_ascii_lowercase();
    if lowered.is_empty() {
        return Err("`--model-adapter` / `--adapter` must not be empty".to_string());
    }
    AdapterKind::from_lower_str(&lowered)
        .ok_or_else(|| format!("unknown model adapter `{raw}`; {VALID_ADAPTER_HINT}"))
}

fn validate_http_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim())
        .map_err(|err| format!("invalid model endpoint URL `{raw}`: {err}"))?;
    match url.scheme() {
        "http" | "https" if url.host_str().is_some() => Ok(url),
        _ => Err(format!(
            "model endpoint URL `{raw}` must use `http://` or `https://` with a host"
        )),
    }
}

fn normalize_endpoint(url: &Url, adapter: AdapterKind) -> String {
    let mut normalized = url.clone();
    let path = normalized.path().to_string();
    let stripped = match adapter {
        AdapterKind::OpenAIResp => path.strip_suffix("/responses"),
        AdapterKind::Anthropic => path.strip_suffix("/messages"),
        AdapterKind::Ollama | AdapterKind::OllamaCloud => path.strip_suffix("/api/chat"),
        AdapterKind::Gemini | AdapterKind::GeminiIx => None,
        _ => path.strip_suffix("/chat/completions"),
    };
    if let Some(prefix) = stripped {
        let next = if prefix.is_empty() {
            "/".to_string()
        } else {
            format!("{prefix}/")
        };
        normalized.set_path(&next);
    } else if !path.ends_with('/') {
        let next = if path.is_empty() {
            "/".to_string()
        } else {
            format!("{path}/")
        };
        normalized.set_path(&next);
    }
    normalized.to_string()
}

fn read_non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_json_object_text(raw: &str) -> Result<Map<String, Value>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("model output was empty".to_string());
    }
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(trimmed) {
        return Ok(map);
    }

    let unfenced = strip_markdown_fence(trimmed);
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(unfenced) {
        return Ok(map);
    }

    if let Some(slice) = extract_first_json_object(unfenced)
        && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(slice)
    {
        return Ok(map);
    }

    Err(format!("expected a JSON object, got `{trimmed}`"))
}

fn strip_markdown_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let after_lang = match rest.find('\n') {
        Some(idx) => &rest[idx + 1..],
        None => rest,
    };
    after_lang
        .trim_end()
        .strip_suffix("```")
        .map_or(trimmed, str::trim)
}

fn extract_first_json_object(text: &str) -> Option<&str> {
    let mut start: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' if start.is_some() => in_string = true,
            '{' => {
                if start.is_none() {
                    start = Some(idx);
                }
                depth += 1;
            }
            '}' if start.is_some() => {
                depth -= 1;
                if depth == 0 {
                    return start.map(|s| &text[s..=idx]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_endpoint_preserves_v1_prefix_and_strips_leaf_paths() {
        let cases = [
            (
                "http://127.0.0.1:8000/v1",
                AdapterKind::Omlx,
                "http://127.0.0.1:8000/v1/",
            ),
            (
                "http://127.0.0.1:8000/v1/",
                AdapterKind::OpenAI,
                "http://127.0.0.1:8000/v1/",
            ),
            (
                "http://127.0.0.1:8000/v1/chat/completions",
                AdapterKind::Omlx,
                "http://127.0.0.1:8000/v1/",
            ),
            (
                "http://127.0.0.1:8000/v1/responses",
                AdapterKind::OpenAIResp,
                "http://127.0.0.1:8000/v1/",
            ),
            (
                "http://127.0.0.1:8000/v1/messages",
                AdapterKind::Anthropic,
                "http://127.0.0.1:8000/v1/",
            ),
            (
                "http://127.0.0.1:11434/api/chat",
                AdapterKind::Ollama,
                "http://127.0.0.1:11434/",
            ),
            (
                "https://proxy.example/openai/deployments/d1?api-version=2025-01-01",
                AdapterKind::OpenAI,
                "https://proxy.example/openai/deployments/d1/?api-version=2025-01-01",
            ),
        ];
        for (raw, adapter, expected) in cases {
            let parsed = validate_http_url(raw).expect("valid url");
            assert_eq!(normalize_endpoint(&parsed, adapter), expected, "for {raw}");
        }
    }

    #[test]
    fn select_adapter_kind_defaults_v1_endpoints_for_custom_models_to_omlx() {
        let ep = validate_http_url("http://127.0.0.1:8000/v1").expect("url");
        let spec = BuiltinModelSpec {
            model: "jev-0.2b".to_string(),
            adapter: None,
            endpoint: Some("http://127.0.0.1:8000/v1".to_string()),
            api_key_env: None,
        };
        assert_eq!(
            select_adapter_kind(&spec, Some(&ep)).expect("adapter"),
            AdapterKind::Omlx
        );

        let ollama_ep = validate_http_url("http://127.0.0.1:11434").expect("url");
        assert_eq!(
            select_adapter_kind(&spec, Some(&ollama_ep)).expect("adapter"),
            AdapterKind::Ollama
        );

        let explicit = BuiltinModelSpec {
            model: "custom-model".to_string(),
            adapter: Some("anthropic".to_string()),
            endpoint: Some("http://127.0.0.1:8000/v1".to_string()),
            api_key_env: None,
        };
        assert_eq!(
            select_adapter_kind(&explicit, Some(&ep)).expect("adapter"),
            AdapterKind::Anthropic
        );
    }

    #[test]
    fn parse_json_object_text_handles_plain_fenced_and_prose_wrapped_json() {
        let plain = parse_json_object_text(r#"{"passed":true,"rationale":"ok"}"#).expect("plain");
        assert_eq!(plain["passed"], Value::Bool(true));

        let fenced = parse_json_object_text("```json\n{\"ref\":\"e5\",\"confidence\":0.9}\n```")
            .expect("fenced");
        assert_eq!(fenced["ref"], "e5");

        let wrapped = parse_json_object_text(
            "Verdict below:\n{\"passed\":false,\"rationale\":\"saw {braces} in text\"}\nDone.",
        )
        .expect("wrapped");
        assert_eq!(wrapped["passed"], Value::Bool(false));
        assert_eq!(wrapped["rationale"], "saw {braces} in text");
    }

    #[test]
    fn validate_spec_accepts_genai_custom_adapter_and_rejects_bad_inputs() {
        let ok_spec = BuiltinModelSpec {
            model: "genai_1::jev-0.2b".to_string(),
            adapter: Some("genai_1".to_string()),
            endpoint: Some("${env:MY_ENDPOINT}".to_string()),
            api_key_env: Some("MY_KEY".to_string()),
        };
        assert!(validate_spec(&ok_spec).is_ok());

        let bad_adapter = BuiltinModelSpec {
            model: "m".to_string(),
            adapter: Some("nonexistent_protocol".to_string()),
            endpoint: None,
            api_key_env: None,
        };
        assert!(validate_spec(&bad_adapter).is_err());

        let bad_url = BuiltinModelSpec {
            model: "m".to_string(),
            adapter: None,
            endpoint: Some("ftp://127.0.0.1/v1".to_string()),
            api_key_env: None,
        };
        assert!(validate_spec(&bad_url).is_err());

        let bad_env = BuiltinModelSpec {
            model: "m".to_string(),
            adapter: None,
            endpoint: None,
            api_key_env: Some("KEY=literal".to_string()),
        };
        assert!(validate_spec(&bad_env).is_err());
    }
}
