//! An interactive, self-contained offline HTML report over recorded runs and sessions.
//!
//! Reads either:
//! - a recorded run or failure-bundle directory (one subdirectory per case with
//!   `meta.json` written last as the completeness marker and `report.json`), or
//! - an interactive exploration session directory (`.da/sessions/<id>/` with
//!   `state.json` and `steps.jsonl`), or a parent directory containing session
//!   and/or recorded-case subdirectories.
//!
//! Every user-supplied value that reaches the document is HTML-escaped and
//! scheme-neutralized (`://` → `&#58;//`). Screenshots are embedded as data URIs
//! only when their size and sharing rules allow it. One static inline `<script>`
//! provides offline timeline scrubbing, keyboard playback (`←`/`→`, `Space`,
//! `f`, `o`), SVG target-overlay toggling, and cross-layer detail tabs without
//! interpolating any untrusted data or making any network requests.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::core::output::ReportSummary;
use crate::core::paths;
use crate::exit::ExitCode;

/// Largest single screenshot embedded in a report.
pub const SINGLE_IMAGE_LIMIT: u64 = 2 * 1024 * 1024;
/// Largest total of screenshots embedded in one report.
pub const TOTAL_IMAGE_LIMIT: u64 = 10 * 1024 * 1024;

/// Bytes of a DOM snapshot shown before the rest is summarized.
const DOM_TEXT_LIMIT: usize = 64 * 1024;
/// Bytes of one other text evidence file shown in the report.
const EVIDENCE_TEXT_LIMIT: usize = 32 * 1024;
/// Log lines shown from one observation.
const MAX_LOG_LINES: usize = 200;

/// The offline policy carried by every report.
const CONTENT_SECURITY_POLICY: &str =
    "default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src 'unsafe-inline'";

/// A rendered report and the counts `da report` prints.
#[derive(Debug)]
pub struct Report {
    html: String,
    summary: ReportSummary,
}

impl Report {
    /// The generated single-file document.
    pub fn html(&self) -> &str {
        &self.html
    }

    /// The counts and size of the document.
    pub fn summary(&self) -> &ReportSummary {
        &self.summary
    }

    /// Writes the document as UTF-8.
    pub fn write(&self, path: &Path) -> Result<(), ReportError> {
        fs::write(path, self.html.as_bytes()).map_err(|error| {
            ReportError::output(format!(
                "could not write the report `{}`: {error}",
                paths::display(path)
            ))
        })
    }
}

/// Why a report could not be read or written.
#[derive(Debug)]
pub struct ReportError {
    message: String,
    exit_code: ExitCode,
}

impl ReportError {
    fn input(message: String) -> Self {
        Self {
            message,
            exit_code: ExitCode::Usage,
        }
    }

    fn output(message: String) -> Self {
        Self {
            message,
            exit_code: ExitCode::Environment,
        }
    }

    /// The message for standard error.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The exit code the command should return.
    pub fn exit_code(&self) -> ExitCode {
        self.exit_code
    }
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReportError {}

/// Reads a record or session directory and renders its complete cases/sessions.
///
/// `shareable` suppresses every screenshot, as does a case whose `meta.json`
/// (or session state) marks `"local_only": true`.
pub fn build(directory: &Path, shareable: bool) -> Result<Report, ReportError> {
    let metadata = fs::metadata(directory).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ReportError::input(format!(
                "the record directory `{}` does not exist; run `da run --record <directory>` first, pass a completed `.da/failures/<run>` directory, or pass a `.da/sessions/<id>` session directory",
                paths::display(directory)
            ))
        } else {
            ReportError::input(format!(
                "could not read the record directory `{}`: {error}",
                paths::display(directory)
            ))
        }
    })?;
    if !metadata.is_dir() {
        return Err(ReportError::input(format!(
            "`{}` is not a directory",
            paths::display(directory)
        )));
    }

    let cases = read_cases(directory)?;
    if cases.is_empty() {
        return Err(ReportError::input(format!(
            "no complete case records or sessions in `{}`; every recorded case directory must contain `meta.json` (written last), or pass a session directory with `state.json`",
            paths::display(directory)
        )));
    }

    let mut budget = ImageBudget::default();
    let html = render_page(&cases, shareable, &mut budget);
    let summary = ReportSummary {
        cases: cases.len(),
        passed: cases
            .iter()
            .filter(|case| case.status() == "passed")
            .count(),
        failed: cases
            .iter()
            .filter(|case| case.status() == "failed")
            .count(),
        errored: cases
            .iter()
            .filter(|case| case.status() != "passed" && case.status() != "failed")
            .count(),
        steps: cases.iter().map(CaseRecord::steps).sum(),
        steps_skipped: cases.iter().map(CaseRecord::skipped_steps).sum(),
        images_embedded: budget.embedded,
        images_omitted: budget.omitted,
        bytes: html.len(),
    };
    Ok(Report { html, summary })
}

/// Aggregated model exploration telemetry across one case/session or the whole report.
#[derive(Debug, Default, Clone)]
struct ModelTelemetry {
    calls: usize,
    calls_by_action: BTreeMap<String, usize>,
    models: BTreeSet<String>,
    total_latency_ms: u64,
    max_latency_ms: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    cached_tokens: u64,
    cache_creation_tokens: u64,
    reasoning_tokens: u64,
    has_usage: bool,
}

impl ModelTelemetry {
    fn merge(&mut self, other: &Self) {
        self.calls = self.calls.saturating_add(other.calls);
        for (action, count) in &other.calls_by_action {
            *self.calls_by_action.entry(action.clone()).or_insert(0) += *count;
        }
        for model in &other.models {
            self.models.insert(model.clone());
        }
        self.total_latency_ms = self.total_latency_ms.saturating_add(other.total_latency_ms);
        self.max_latency_ms = self.max_latency_ms.max(other.max_latency_ms);
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(other.completion_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_tokens = self.cached_tokens.saturating_add(other.cached_tokens);
        self.cache_creation_tokens = self
            .cache_creation_tokens
            .saturating_add(other.cache_creation_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        self.has_usage |= other.has_usage;
    }

    fn collect_from_step(&mut self, step: &Value) {
        let Some(action) = step.get("name").and_then(Value::as_str) else {
            return;
        };
        // Only authoritative model diagnostic records (`model.*` / `model/*`)
        // contribute to aggregate telemetry; synthesized UI steps (`browser.*`,
        // `desktop.*`) may carry provenance fields copied from the model call
        // that chose them and must not be double-counted.
        if !is_model_diagnostic_action(action) {
            return;
        }
        let meta = extract_step_model_meta(step);
        let Some(meta) = meta else {
            return;
        };
        let call_count = model_call_count(action, step);
        self.calls = self.calls.saturating_add(call_count);
        *self.calls_by_action.entry(action.to_string()).or_insert(0) += call_count;
        if let Some(model) = meta.model {
            self.models.insert(model);
        }
        if let Some(lat) = meta.latency_ms {
            self.total_latency_ms = self.total_latency_ms.saturating_add(lat);
            let step_max = step
                .pointer("/result/max_model_latency_ms")
                .and_then(Value::as_u64)
                .unwrap_or(lat);
            self.max_latency_ms = self.max_latency_ms.max(step_max);
        }
        if let Some(usage) = meta.usage.as_ref() {
            let counts = parse_usage_counts(usage);
            if counts.any_present {
                self.has_usage = true;
                self.prompt_tokens = self.prompt_tokens.saturating_add(counts.prompt);
                self.completion_tokens = self.completion_tokens.saturating_add(counts.completion);
                self.total_tokens = self.total_tokens.saturating_add(counts.total);
                self.cached_tokens = self.cached_tokens.saturating_add(counts.cached);
                self.cache_creation_tokens = self
                    .cache_creation_tokens
                    .saturating_add(counts.cache_creation);
                self.reasoning_tokens = self.reasoning_tokens.saturating_add(counts.reasoning);
            }
        }
    }

    fn average_latency_ms(&self) -> u64 {
        if self.calls == 0 {
            0
        } else {
            self.total_latency_ms / u64::try_from(self.calls).unwrap_or(1)
        }
    }
}

fn is_model_diagnostic_action(action: &str) -> bool {
    action.starts_with("model.") || action.starts_with("model/")
}

fn model_call_count(action: &str, step: &Value) -> usize {
    if let Some(calls) = step
        .pointer("/result/model_calls")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n > 0)
    {
        return calls;
    }
    if matches!(action, "model.act" | "model/act")
        && let Some(steps_taken) = step
            .pointer("/result/steps_taken")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    {
        let status = step.pointer("/result/status").and_then(Value::as_str);
        return if status == Some("max_steps_exceeded") {
            steps_taken.max(1)
        } else {
            steps_taken.saturating_add(1)
        };
    }
    1
}

#[derive(Debug, Default)]
struct UsageCounts {
    prompt: u64,
    completion: u64,
    total: u64,
    cached: u64,
    cache_creation: u64,
    reasoning: u64,
    any_present: bool,
}

fn parse_usage_counts(usage: &Value) -> UsageCounts {
    let Some(obj) = usage.as_object() else {
        return UsageCounts::default();
    };
    let read_u64 = |keys: &[&str]| -> Option<u64> {
        for key in keys {
            if let Some(v) = obj.get(*key).and_then(Value::as_u64) {
                return Some(v);
            }
        }
        None
    };
    let prompt = read_u64(&["prompt_tokens", "input_tokens"]);
    let completion = read_u64(&["completion_tokens", "output_tokens"]);
    let total = read_u64(&["total_tokens"]);
    let cached = read_u64(&["cached_tokens", "cache_read_tokens"]).or_else(|| {
        obj.get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
    });
    let cache_creation = read_u64(&["cache_creation_tokens"]).or_else(|| {
        obj.get("prompt_tokens_details")
            .and_then(|d| d.get("cache_creation_tokens"))
            .and_then(Value::as_u64)
    });
    let reasoning = read_u64(&["reasoning_tokens"]).or_else(|| {
        obj.get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64)
    });

    let any_present = prompt.is_some()
        || completion.is_some()
        || total.is_some()
        || cached.is_some()
        || cache_creation.is_some()
        || reasoning.is_some();
    let p = prompt.unwrap_or(0);
    let c = completion.unwrap_or(0);
    let t = total.unwrap_or_else(|| p.saturating_add(c));
    UsageCounts {
        prompt: p,
        completion: c,
        total: t,
        cached: cached.unwrap_or(0),
        cache_creation: cache_creation.unwrap_or(0),
        reasoning: reasoning.unwrap_or(0),
        any_present,
    }
}

#[derive(Debug)]
struct StepModelMeta {
    model: Option<String>,
    latency_ms: Option<u64>,
    usage: Option<Value>,
    trace_id: Option<String>,
    confidence: Option<f64>,
}

fn extract_step_model_meta(step: &Value) -> Option<StepModelMeta> {
    let result = step.get("result");
    let model = step
        .get("model")
        .and_then(Value::as_str)
        .or_else(|| result.and_then(|r| r.get("model")).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let latency_ms = step.get("latency_ms").and_then(Value::as_u64).or_else(|| {
        result
            .and_then(|r| r.get("latency_ms"))
            .and_then(Value::as_u64)
    });
    let usage = step
        .get("usage")
        .filter(|v| !v.is_null())
        .cloned()
        .or_else(|| {
            result
                .and_then(|r| r.get("usage"))
                .filter(|v| !v.is_null())
                .cloned()
        });
    let trace_id = step
        .get("trace_id")
        .and_then(Value::as_str)
        .or_else(|| {
            result
                .and_then(|r| r.get("trace_id"))
                .and_then(Value::as_str)
        })
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let confidence = step.get("confidence").and_then(Value::as_f64).or_else(|| {
        result
            .and_then(|r| r.get("confidence"))
            .and_then(Value::as_f64)
    });

    if model.is_none()
        && latency_ms.is_none()
        && usage.is_none()
        && trace_id.is_none()
        && confidence.is_none()
    {
        return None;
    }
    Some(StepModelMeta {
        model,
        latency_ms,
        usage,
        trace_id,
        confidence,
    })
}

/// One complete case directory or interactive exploration session.
#[derive(Debug)]
struct CaseRecord {
    directory: PathBuf,
    meta: Value,
    report: Value,
    evidence: Vec<EvidenceFile>,
    step_traces: BTreeMap<(bool, usize), Vec<EvidenceFile>>,
    local_only: bool,
    local_only_reason: Option<String>,
    is_exploration: bool,
    telemetry: ModelTelemetry,
}

impl CaseRecord {
    fn name(&self) -> String {
        self.meta
            .get("case_name")
            .and_then(Value::as_str)
            .or_else(|| self.report.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(|| {
                self.directory
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "case".to_string())
            })
    }

    fn status(&self) -> String {
        self.report
            .get("status")
            .and_then(Value::as_str)
            .or_else(|| self.meta.get("status").and_then(Value::as_str))
            .unwrap_or("unknown")
            .to_string()
    }

    fn step_count(&self, field: &str) -> usize {
        self.report
            .get(field)
            .and_then(Value::as_array)
            .map_or(0, Vec::len)
    }

    fn steps(&self) -> usize {
        self.step_count("steps") + self.step_count("finally")
    }

    fn skipped_in(&self, field: &str) -> usize {
        self.report
            .get(field)
            .and_then(Value::as_array)
            .map_or(0, |steps| {
                steps
                    .iter()
                    .filter(|step| step.get("status").and_then(Value::as_str) == Some("skipped"))
                    .count()
            })
    }

    fn skipped_steps(&self) -> usize {
        self.skipped_in("steps") + self.skipped_in("finally")
    }

    fn meta_text(&self, field: &str) -> Option<String> {
        self.meta
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// Returns the most relevant `target_bounds` for case-level failure screenshots
    /// when per-step traces are absent: prefers the last failing/errored UI step,
    /// otherwise the last UI step with `target_bounds`.
    fn fallback_case_target_bounds(&self) -> Option<&Value> {
        let mut last_any: Option<&Value> = None;
        let mut last_failed: Option<&Value> = None;
        for field in ["steps", "finally"] {
            if let Some(steps) = self.report.get(field).and_then(Value::as_array) {
                for step in steps {
                    if let Some(bounds) = step.get("target_bounds").filter(|v| v.is_object()) {
                        last_any = Some(bounds);
                        let status = step
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if matches!(status, "failed" | "error") {
                            last_failed = Some(bounds);
                        }
                    }
                }
            }
        }
        last_failed.or(last_any)
    }
}

/// One file a session left in a case directory.
#[derive(Debug)]
struct EvidenceFile {
    group: String,
    name: String,
    path: PathBuf,
    len: u64,
    kind: EvidenceKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvidenceKind {
    Image(&'static str),
    Dom,
    Text,
    Binary,
}

impl EvidenceFile {
    fn label(&self) -> String {
        format!("{}/{}", self.group, self.name)
    }
}

#[derive(Debug, Default)]
struct ImageBudget {
    total: u64,
    embedded: usize,
    omitted: usize,
}

impl ImageBudget {
    fn embed(&mut self, len: u64) -> Result<(), String> {
        if len > SINGLE_IMAGE_LIMIT {
            self.omitted += 1;
            return Err(format!(
                "{} bytes exceeds the 2 MiB single-image limit",
                thousands(len)
            ));
        }
        if self.total.saturating_add(len) > TOTAL_IMAGE_LIMIT {
            self.omitted += 1;
            return Err(
                "the 10 MiB total image budget was already used by earlier screenshots".to_string(),
            );
        }
        self.total += len;
        self.embedded += 1;
        Ok(())
    }

    fn omit(&mut self) {
        self.omitted += 1;
    }
}

/// Reads every complete recorded case (`meta.json`) or session (`state.json`).
fn read_cases(root: &Path) -> Result<Vec<CaseRecord>, ReportError> {
    if root.join("state.json").is_file() {
        return Ok(vec![read_session_record(root)?]);
    }
    if root.join("meta.json").is_file() && root.join("report.json").is_file() {
        return Ok(vec![read_recorded_case(root)?]);
    }

    let entries = fs::read_dir(root).map_err(|error| {
        ReportError::input(format!(
            "could not read the record directory `{}`: {error}",
            paths::display(root)
        ))
    })?;
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            ReportError::input(format!(
                "could not read an entry in `{}`: {error}",
                paths::display(root)
            ))
        })?;
        let path = entry.path();
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            && (path.join("meta.json").is_file() || path.join("state.json").is_file())
        {
            directories.push(path);
        }
    }
    directories.sort_by(|left, right| {
        match (case_position(left), case_position(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.file_name().cmp(&right.file_name()))
    });

    let mut cases = Vec::with_capacity(directories.len());
    for directory in directories {
        if directory.join("meta.json").is_file() {
            cases.push(read_recorded_case(&directory)?);
        } else if directory.join("state.json").is_file() {
            cases.push(read_session_record(&directory)?);
        }
    }
    Ok(cases)
}

fn read_recorded_case(directory: &Path) -> Result<CaseRecord, ReportError> {
    let meta = read_json(&directory.join("meta.json"))?;
    let report = read_json(&directory.join("report.json"))?;
    let local_only = meta
        .get("local_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let local_only_reason = meta
        .get("local_only_reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string);
    let evidence = read_evidence(directory)?;
    let step_traces = read_step_traces(directory)?;
    let telemetry = collect_report_telemetry(&report);
    let is_exploration = telemetry.calls > 0
        || meta
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| k == "exploration");
    Ok(CaseRecord {
        directory: directory.to_path_buf(),
        meta,
        report,
        evidence,
        step_traces,
        local_only,
        local_only_reason,
        is_exploration,
        telemetry,
    })
}

fn read_session_record(directory: &Path) -> Result<CaseRecord, ReportError> {
    let state = read_json(&directory.join("state.json"))?;
    let session_id = state
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            directory
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "session".to_string())
        });

    let steps_path = directory.join("steps.jsonl");
    let mut step_values = Vec::new();
    let mut has_failed = false;
    let mut has_error = false;
    let mut total_duration_ms = 0_u64;

    if steps_path.is_file() {
        let content = fs::read_to_string(&steps_path).map_err(|error| {
            ReportError::input(format!(
                "could not read `{}`: {error}",
                paths::display(&steps_path)
            ))
        })?;
        for (line_idx, raw_line) in content.lines().enumerate() {
            let trimmed = raw_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let raw: Value = serde_json::from_str(trimmed).map_err(|error| {
                ReportError::input(format!(
                    "`{}` line {} is not valid JSON: {error}",
                    paths::display(&steps_path),
                    line_idx + 1
                ))
            })?;
            let status = raw
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("passed");
            match status {
                "failed" => has_failed = true,
                "error" => has_error = true,
                _ => {}
            }
            let duration_ms = raw.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
            total_duration_ms = total_duration_ms.saturating_add(duration_ms);

            let mut step = Map::new();
            step.insert(
                "name".to_string(),
                json!(raw.get("action").and_then(Value::as_str).unwrap_or("step")),
            );
            step.insert("status".to_string(), json!(status));
            step.insert("duration_ms".to_string(), json!(duration_ms));
            if let Some(args) = raw.get("raw_args") {
                step.insert("args".to_string(), args.clone());
            }
            if let Some(result) = raw.get("result")
                && !result.is_null()
            {
                step.insert("result".to_string(), result.clone());
            }
            if let Some(tb) = raw.get("target_bounds")
                && !tb.is_null()
            {
                step.insert("target_bounds".to_string(), tb.clone());
            }
            if let Some(save) = raw.get("save").and_then(Value::as_object)
                && !save.is_empty()
            {
                step.insert("saved".to_string(), Value::Object(save.clone()));
            }
            if let Some(err_text) = raw.get("error").and_then(Value::as_str) {
                step.insert(
                    "error".to_string(),
                    json!({
                        "kind": if status == "failed" { "assertion" } else { "execution" },
                        "message": err_text,
                    }),
                );
            }
            for key in [
                "model",
                "latency_ms",
                "usage",
                "trace_id",
                "confidence",
                "used_ref",
                "synthesized_locator",
                "fragile",
            ] {
                if let Some(val) = raw.get(key)
                    && !val.is_null()
                {
                    step.insert(key.to_string(), val.clone());
                }
            }
            step_values.push(Value::Object(step));
        }
    }

    let status = if has_error {
        "error"
    } else if has_failed {
        "failed"
    } else {
        "passed"
    };

    let (local_only, local_only_reason) = detect_session_local_only(&state);

    let mut meta = json!({
        "kind": "exploration",
        "case_name": format!("Session {session_id}"),
        "suite": "exploration",
        "status": status,
        "local_only": local_only,
    });
    if let Some(reason) = &local_only_reason {
        meta["local_only_reason"] = json!(reason);
    }
    if let Some(model_cfg) = state
        .get("model")
        .and_then(|m| m.get("model"))
        .and_then(Value::as_str)
    {
        meta["configured_model"] = json!(model_cfg);
    }

    let report = json!({
        "name": format!("Session {session_id}"),
        "status": status,
        "duration_ms": total_duration_ms,
        "steps": step_values,
        "finally": [],
    });

    let mut evidence = read_evidence(directory)?;
    evidence.extend(read_session_root_images(directory)?);
    let step_traces = read_step_traces(directory)?;
    let telemetry = collect_report_telemetry(&report);

    Ok(CaseRecord {
        directory: directory.to_path_buf(),
        meta,
        report,
        evidence,
        step_traces,
        local_only,
        local_only_reason,
        is_exploration: true,
        telemetry,
    })
}

fn detect_session_local_only(state: &Value) -> (bool, Option<String>) {
    if state
        .get("local_only")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let reason = state
            .get("local_only_reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        return (true, reason);
    }
    if let Some(handles) = state.get("handles").and_then(Value::as_object) {
        let mut reasons = Vec::new();
        for (kind, handle) in handles {
            if let Some(reason) = handle
                .get("local_only_reason")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                reasons.push(reason.to_string());
                continue;
            }
            if handle
                .get("connect")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                reasons.push(format!("{kind} attached to an external browser endpoint"));
            } else if handle
                .get("profile_name")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty())
            {
                reasons.push(format!("{kind} used a persistent local profile"));
            }
        }
        if !reasons.is_empty() {
            return (true, Some(reasons.join("; ")));
        }
    }
    (false, None)
}

fn read_session_root_images(directory: &Path) -> Result<Vec<EvidenceFile>, ReportError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        ReportError::input(format!(
            "could not read the session directory `{}`: {error}",
            paths::display(directory)
        ))
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            ReportError::input(format!(
                "could not read an entry in `{}`: {error}",
                paths::display(directory)
            ))
        })?;
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let path = entry.path();
        if let Some(mime) = image_mime(&path) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(EvidenceFile {
                group: "session".to_string(),
                name,
                path,
                len,
                kind: EvidenceKind::Image(mime),
            });
        }
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

fn collect_report_telemetry(report: &Value) -> ModelTelemetry {
    let mut telemetry = ModelTelemetry::default();
    for field in ["steps", "finally"] {
        if let Some(steps) = report.get(field).and_then(Value::as_array) {
            for step in steps {
                telemetry.collect_from_step(step);
            }
        }
    }
    telemetry
}

fn read_json(path: &Path) -> Result<Value, ReportError> {
    let text = fs::read_to_string(path).map_err(|error| {
        ReportError::input(format!(
            "could not read `{}`: {error}",
            paths::display(path)
        ))
    })?;
    serde_json::from_str(&text).map_err(|error| {
        ReportError::input(format!(
            "`{}` is not valid JSON: {error}",
            paths::display(path)
        ))
    })
}

fn case_position(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let digits = name.split('-').next()?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn read_evidence(case: &Path) -> Result<Vec<EvidenceFile>, ReportError> {
    read_evidence_groups(case, &["trace"])
}

fn read_step_traces(
    case: &Path,
) -> Result<BTreeMap<(bool, usize), Vec<EvidenceFile>>, ReportError> {
    let mut traces = BTreeMap::new();
    let trace_root = case.join("trace");
    if !trace_root.is_dir() {
        return Ok(traces);
    }
    for (is_finally, phase_name) in [(false, "steps"), (true, "finally")] {
        let phase_dir = trace_root.join(phase_name);
        if !phase_dir.is_dir() {
            continue;
        }
        let entries = fs::read_dir(&phase_dir).map_err(|error| {
            ReportError::input(format!(
                "could not read the trace directory `{}`: {error}",
                paths::display(&phase_dir)
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                ReportError::input(format!(
                    "could not read a trace entry in `{}`: {error}",
                    paths::display(&phase_dir)
                ))
            })?;
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(step_index) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<usize>().ok())
            else {
                continue;
            };
            let files = read_evidence_groups(&entry.path(), &[])?;
            if !files.is_empty() {
                traces.insert((is_finally, step_index), files);
            }
        }
    }
    Ok(traces)
}

fn read_evidence_groups(
    parent: &Path,
    skip_dirs: &[&str],
) -> Result<Vec<EvidenceFile>, ReportError> {
    let entries = fs::read_dir(parent).map_err(|error| {
        ReportError::input(format!(
            "could not read the case directory `{}`: {error}",
            paths::display(parent)
        ))
    })?;
    let mut groups = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            ReportError::input(format!(
                "could not read an entry in `{}`: {error}",
                paths::display(parent)
            ))
        })?;
        let name = entry.file_name();
        if skip_dirs.iter().any(|skip| name == *skip) {
            continue;
        }
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            groups.push(entry.path());
        }
    }
    groups.sort();

    let mut files = Vec::new();
    for group in groups {
        let group_name = group
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "evidence".to_string());
        let entries = fs::read_dir(&group).map_err(|error| {
            ReportError::input(format!(
                "could not read the evidence directory `{}`: {error}",
                paths::display(&group)
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                ReportError::input(format!(
                    "could not read an evidence entry in `{}`: {error}",
                    paths::display(&group)
                ))
            })?;
            if !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let len = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            let kind = if let Some(mime) = image_mime(&path) {
                EvidenceKind::Image(mime)
            } else if name == "dom.html" {
                EvidenceKind::Dom
            } else if is_text_name(&name) {
                EvidenceKind::Text
            } else {
                EvidenceKind::Binary
            };
            files.push(EvidenceFile {
                group: group_name.clone(),
                name,
                path,
                len,
                kind,
            });
        }
    }
    files.sort_by(|left, right| {
        left.group
            .cmp(&right.group)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(files)
}

fn image_mime(path: &Path) -> Option<&'static str> {
    let mut file = File::open(path).ok()?;
    let mut header = [0_u8; 12];
    let read = file.read(&mut header).ok()?;
    let header = &header[..read];
    if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if header.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if header.len() >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Parses PNG pixel dimensions from an in-memory PNG buffer (IHDR chunk).
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width > 0 && height > 0 {
        Some((width, height))
    } else {
        None
    }
}

fn is_text_name(name: &str) -> bool {
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "txt" | "log" | "html" | "htm" | "json" | "md" | "csv"
    )
}

fn render_page(cases: &[CaseRecord], shareable: bool, budget: &mut ImageBudget) -> String {
    let mut html = String::with_capacity(64 * 1024);
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    let _ = writeln!(
        html,
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{CONTENT_SECURITY_POLICY}\">"
    );
    html.push_str("<title>da run evidence report</title>\n<style>\n");
    html.push_str(STYLE);
    html.push_str(TRACE_STYLE);
    html.push_str(INTERACTIVE_STYLE);
    html.push_str("</style>\n</head>\n<body>\n<main>\n<header class=\"hero\">\n");

    let mut total_telemetry = ModelTelemetry::default();
    let any_exploration = cases.iter().any(|case| case.is_exploration);
    let any_deterministic = cases.iter().any(|case| !case.is_exploration);
    for case in cases {
        total_telemetry.merge(&case.telemetry);
    }

    html.push_str("<div class=\"hero-top\"><p class=\"eyebrow\">da evidence report</p>");
    if any_exploration && any_deterministic {
        html.push_str(
            "<span class=\"mode-badge exploration\">Mixed: Exploration + 0-token Deterministic Replay</span>",
        );
    } else if any_exploration {
        html.push_str("<span class=\"mode-badge exploration\">Exploration Run (da session)</span>");
    } else {
        html.push_str(
            "<span class=\"mode-badge deterministic\">Deterministic Replay · 0 tokens (da run)</span>",
        );
    }
    html.push_str("</div>\n<h1>Run evidence</h1>\n");

    if shareable {
        html.push_str(
            "<p class=\"lede\">This report is shareable: screenshots are not embedded. It is one self-contained offline HTML file with zero network requests.</p>\n",
        );
    } else {
        html.push_str(
            "<p class=\"lede\">An interactive, self-contained offline viewer of recorded cases, steps, SVG action target overlays, and cross-layer evidence. It makes no network requests. Keys: <kbd>&larr;</kbd>/<kbd>&rarr;</kbd> step, <kbd>Space</kbd> play/pause, <kbd>f</kbd> failures only, <kbd>o</kbd> toggle overlays.</p>\n",
        );
    }

    html.push_str("<div class=\"metrics\">\n");
    let passed = cases
        .iter()
        .filter(|case| case.status() == "passed")
        .count();
    let failed = cases
        .iter()
        .filter(|case| case.status() == "failed")
        .count();
    let errored = cases.len() - passed - failed;
    let steps: usize = cases.iter().map(CaseRecord::steps).sum();
    let skipped: usize = cases.iter().map(CaseRecord::skipped_steps).sum();
    metric(&mut html, &cases.len().to_string(), "cases");
    metric(&mut html, &passed.to_string(), "passed");
    metric(&mut html, &failed.to_string(), "failed");
    metric(&mut html, &errored.to_string(), "errors");
    metric(&mut html, &steps.to_string(), "steps");
    metric(&mut html, &skipped.to_string(), "skipped steps");
    html.push_str("</div>\n");

    if total_telemetry.calls > 0 {
        render_telemetry_summary(&mut html, &total_telemetry, "Model exploration telemetry");
    }
    html.push_str("</header>\n");

    for (index, case) in cases.iter().enumerate() {
        render_case(&mut html, index, case, shareable, budget);
    }

    html.push_str("<footer class=\"footer\">Generated by <code>da report</code>. ");
    html.push_str(
        "Every recorded case contained <code>meta.json</code> (or session <code>state.json</code>). ",
    );
    html.push_str(
        "Screenshots are embedded only when the size and sharing rules allow it.</footer>\n",
    );
    html.push_str("</main>\n<script>\n");
    html.push_str(VIEWER_SCRIPT);
    html.push_str("\n</script>\n</body>\n</html>\n");
    html
}

fn metric(html: &mut String, value: &str, label: &str) {
    let _ = writeln!(
        html,
        "<div class=\"metric\"><strong>{}</strong><span>{}</span></div>",
        escape_text(value),
        escape_text(label)
    );
}

fn render_telemetry_summary(html: &mut String, telemetry: &ModelTelemetry, heading: &str) {
    let _ = writeln!(
        html,
        "<section class=\"telemetry-panel\" aria-label=\"{}\">\n<div class=\"telemetry-head\"><h3>{}</h3><span class=\"note\">Exploration-only · <code>da run</code> replays stay 0-token deterministic</span></div>\n<div class=\"telemetry-grid\">",
        escape_text(heading),
        escape_text(heading),
    );
    metric(html, &telemetry.calls.to_string(), "model calls");
    metric(
        html,
        &format!("{} ms", thousands(telemetry.total_latency_ms)),
        "total model latency",
    );
    metric(
        html,
        &format!(
            "{} / {} ms",
            thousands(telemetry.average_latency_ms()),
            thousands(telemetry.max_latency_ms)
        ),
        "avg / max latency",
    );
    if telemetry.has_usage {
        metric(html, &thousands(telemetry.total_tokens), "total tokens");
        metric(
            html,
            &format!(
                "{} / {}",
                thousands(telemetry.prompt_tokens),
                thousands(telemetry.completion_tokens)
            ),
            "prompt / completion",
        );
        let cache_label = if telemetry.reasoning_tokens > 0 {
            format!(
                "{} cached · {} reasoning",
                thousands(telemetry.cached_tokens),
                thousands(telemetry.reasoning_tokens)
            )
        } else if telemetry.cache_creation_tokens > 0 {
            format!(
                "{} read · {} write",
                thousands(telemetry.cached_tokens),
                thousands(telemetry.cache_creation_tokens)
            )
        } else {
            thousands(telemetry.cached_tokens)
        };
        metric(html, &cache_label, "cache / reasoning tokens");
    }
    html.push_str("</div>\n");

    if !telemetry.models.is_empty() || !telemetry.calls_by_action.is_empty() {
        html.push_str("<p class=\"chips telemetry-chips\">");
        for model in &telemetry.models {
            let _ = write!(html, "<span>model: {}</span>", escape_text(model));
        }
        for (action, count) in &telemetry.calls_by_action {
            let _ = write!(html, "<span>{}: {}</span>", escape_text(action), count);
        }
        html.push_str("</p>\n");
    }
    html.push_str("</section>\n");
}

fn render_case(
    html: &mut String,
    index: usize,
    case: &CaseRecord,
    shareable: bool,
    budget: &mut ImageBudget,
) {
    let status = case.status();
    let status_class = status_class(&status);
    let case_id = format!("case-{}", index + 1);
    let _ = write!(
        html,
        "<section class=\"case\" id=\"{case_id}\" data-case-id=\"{case_id}\">\n<header class=\"case-head\">\n<div class=\"status {status_class}\">{}</div>\n<div class=\"case-title\"><h2>{}</h2>\n",
        escape_text(&status.to_uppercase()),
        escape_text(&case.name())
    );
    let mut chips = Vec::new();
    if case.is_exploration {
        chips.push("exploration run".to_string());
    } else {
        chips.push("deterministic da run · 0 tokens".to_string());
    }
    for field in ["project", "suite", "layer", "configured_model"] {
        if let Some(value) = case.meta_text(field) {
            chips.push(escape_text(&format!("{field}: {value}")));
        }
    }
    if let Some(tags) = case.meta.get("tags").and_then(Value::as_array) {
        for tag in tags.iter().filter_map(Value::as_str) {
            chips.push(escape_text(&format!("tag: {tag}")));
        }
    }
    if case.local_only {
        chips.push("local_only".to_string());
    }
    if !chips.is_empty() {
        html.push_str("<p class=\"chips\">");
        for chip in chips {
            let _ = write!(html, "<span>{chip}</span>");
        }
        html.push_str("</p>");
    }
    html.push_str("</div></header>\n");

    if case.local_only {
        let detail = case
            .local_only_reason
            .as_deref()
            .unwrap_or("session used unmanaged external state");
        let _ = writeln!(
            html,
            "<p class=\"note\"><strong>Local-only evidence:</strong> kept local only ({}).</p>",
            escape_text(detail)
        );
    }

    if case.telemetry.calls > 0 {
        render_telemetry_summary(html, &case.telemetry, "Session / case model telemetry");
    }

    if let Some(problem) = case.report.get("error").and_then(Value::as_object) {
        html.push_str("<p class=\"problem\"><strong>Case problem:</strong> ");
        html.push_str(&escape_text(&problem_text(problem)));
        html.push_str("</p>\n");
    }
    if let Some(close_errors) = case.report.get("close_errors").and_then(Value::as_array)
        && !close_errors.is_empty()
    {
        html.push_str("<div class=\"problem\"><strong>Close errors:</strong><ul>");
        for error in close_errors.iter().filter_map(Value::as_str) {
            let _ = write!(html, "<li>{}</li>", escape_text(error));
        }
        html.push_str("</ul></div>\n");
    }

    let main_steps = case
        .report
        .get("steps")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let finally_steps = case
        .report
        .get("finally")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let total_steps = main_steps.len() + finally_steps.len();

    if total_steps > 0 {
        render_timeline_bar(html, main_steps, finally_steps);
    }

    if case.report.get("steps").and_then(Value::as_array).is_some() {
        if main_steps.is_empty() {
            html.push_str("<p class=\"note\">This case recorded no steps.</p>\n");
        } else {
            html.push_str("<div class=\"steps\">\n");
            for (position, step) in main_steps.iter().enumerate() {
                render_step(
                    html, false, position, position, step, case, shareable, budget,
                );
            }
            html.push_str("</div>\n");
        }
    }

    if !finally_steps.is_empty() {
        html.push_str("<div class=\"steps finally\"><h3>Cleanup (finally)</h3>\n");
        for (position, step) in finally_steps.iter().enumerate() {
            render_step(
                html,
                true,
                position,
                main_steps.len() + position,
                step,
                case,
                shareable,
                budget,
            );
        }
        html.push_str("</div>\n");
    }

    render_case_evidence(html, case, shareable, budget);
    html.push_str("</section>\n");
}

fn render_timeline_bar(html: &mut String, main_steps: &[Value], finally_steps: &[Value]) {
    let total = main_steps.len() + finally_steps.len();
    html.push_str(
        "<div class=\"timeline-bar\" role=\"region\" aria-label=\"Step timeline playback\">\n",
    );
    html.push_str("<div class=\"timeline-controls\">");
    html.push_str("<button type=\"button\" class=\"tl-btn\" data-action=\"prev\" title=\"Previous step (Left Arrow)\">&larr; Prev</button>");
    html.push_str("<button type=\"button\" class=\"tl-btn\" data-action=\"play\" aria-pressed=\"false\" title=\"Play or pause timeline (Space)\">Play</button>");
    html.push_str("<button type=\"button\" class=\"tl-btn\" data-action=\"next\" title=\"Next step (Right Arrow)\">Next &rarr;</button>");
    let _ = write!(
        html,
        "<span class=\"tl-counter\" data-role=\"counter\">Step 1 / {total}</span>"
    );
    html.push_str("<button type=\"button\" class=\"tl-btn\" data-action=\"filter-failed\" aria-pressed=\"false\" title=\"Show only failed, errored, or warned steps (f)\">Failures only (f)</button>");
    html.push_str("<button type=\"button\" class=\"tl-btn active\" data-action=\"toggle-overlays\" aria-pressed=\"true\" title=\"Toggle SVG target overlays (o)\">Overlays: ON (o)</button>");
    html.push_str("<button type=\"button\" class=\"tl-btn\" data-action=\"toggle-focus\" aria-pressed=\"false\" title=\"Toggle focused single-step playback vs all steps\">Timeline focus</button>");
    html.push_str("</div>\n<div class=\"timeline-strip\" role=\"tablist\" aria-label=\"Steps\">");

    for (idx, step) in main_steps.iter().enumerate() {
        render_timeline_pill(html, idx, false, idx, step);
    }
    for (idx, step) in finally_steps.iter().enumerate() {
        render_timeline_pill(html, main_steps.len() + idx, true, idx, step);
    }
    html.push_str("</div>\n<p class=\"note tl-empty\" data-role=\"empty-filter\" hidden>No failed, errored, or warned steps in this case.</p>\n</div>\n");
}

fn render_timeline_pill(
    html: &mut String,
    global_idx: usize,
    is_finally: bool,
    local_idx: usize,
    step: &Value,
) {
    let status = step
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let name = step.get("name").and_then(Value::as_str).unwrap_or("step");
    let failed = is_step_attention_worthy(step, status);
    let prefix = if is_finally {
        format!("F{}", local_idx + 1)
    } else {
        format!("#{}", local_idx + 1)
    };
    let active = if global_idx == 0 { " active" } else { "" };
    let _ = write!(
        html,
        "<button type=\"button\" class=\"tl-pill {}{active}\" data-step-idx=\"{global_idx}\" data-status=\"{}\" data-failed=\"{failed}\"><span class=\"tl-num\">{prefix}</span> <span class=\"tl-name\">{}</span></button>",
        status_class(status),
        status_class(status),
        escape_text(name),
    );
}

fn is_step_attention_worthy(step: &Value, status: &str) -> bool {
    matches!(status, "failed" | "error")
        || step
            .get("fragile")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || step
            .get("assertions")
            .and_then(Value::as_array)
            .is_some_and(|list| {
                list.iter()
                    .any(|a| !a.get("passed").and_then(Value::as_bool).unwrap_or(true))
            })
}

#[allow(clippy::too_many_arguments)]
fn render_step(
    html: &mut String,
    is_finally: bool,
    position: usize,
    global_idx: usize,
    step: &Value,
    case: &CaseRecord,
    shareable: bool,
    budget: &mut ImageBudget,
) {
    let status = step
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let name = step.get("name").and_then(Value::as_str).unwrap_or("step");
    let duration = step.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
    let failed = is_step_attention_worthy(step, status);
    let active_cls = if global_idx == 0 { " active" } else { "" };
    let _ = write!(
        html,
        "<article class=\"step {}{active_cls}\" data-step-idx=\"{global_idx}\" data-status=\"{}\" data-failed=\"{failed}\">\n<header class=\"step-head\">\n<span class=\"step-index\">#{}</span><h3>{}</h3><span class=\"status {}\">{}</span><span class=\"duration\">{} ms</span>",
        status_class(status),
        status_class(status),
        position + 1,
        escape_text(name),
        status_class(status),
        escape_text(&status.to_uppercase()),
        duration
    );
    if step
        .get("fragile")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        html.push_str("<span class=\"chip-warn\">fragile</span>");
    }
    if let Some(ref_id) = step.get("used_ref").and_then(Value::as_str) {
        let _ = write!(
            html,
            "<span class=\"chip-meta\">ref: {}</span>",
            escape_text(ref_id)
        );
    }
    html.push_str("\n</header>\n");

    if let Some(problem) = step.get("error").and_then(Value::as_object) {
        html.push_str("<p class=\"problem\">");
        html.push_str(&escape_text(&problem_text(problem)));
        html.push_str("</p>\n");
    }

    let target_bounds = step.get("target_bounds").filter(|v| v.is_object());
    if let Some(bounds) = target_bounds {
        render_target_bounds_summary(html, bounds);
    }

    let assertions = step
        .get("assertions")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty());
    let saved = step
        .get("saved")
        .and_then(Value::as_object)
        .filter(|s| !s.is_empty());
    let result = step.get("result").filter(|v| !v.is_null());
    let has_observed = result
        .and_then(|r| r.get("observed"))
        .and_then(Value::as_object)
        .is_some_and(|o| !o.is_empty());
    let has_data_layer = result.is_some_and(|r| {
        (r.get("headers").is_some_and(Value::is_object) && r.get("status").is_some())
            || (r.get("rows").and_then(Value::as_array).is_some()
                && r.get("columns").and_then(Value::as_array).is_some())
            || (r.get("exit_code").is_some()
                && (r.get("stdout").is_some() || r.get("stderr").is_some()))
    });
    let model_meta = extract_step_model_meta(step);
    let trace_files = case.step_traces.get(&(is_finally, position));
    let has_args = step.get("args").is_some_and(|a| !a.is_null());
    let has_raw = has_args || result.is_some();

    let mut available_tabs: Vec<(&'static str, &'static str)> = Vec::new();
    if assertions.is_some() {
        available_tabs.push(("assertions", "Assertions"));
    }
    if has_observed {
        available_tabs.push(("observed", "Observed"));
    }
    if has_data_layer || saved.is_some() {
        available_tabs.push(("data", "Data"));
    }
    if model_meta.is_some() {
        available_tabs.push(("model", "Model"));
    }
    if trace_files.is_some() {
        available_tabs.push(("trace", "Trace / Visual"));
    }
    if has_raw {
        available_tabs.push(("raw", "Raw"));
    }

    if available_tabs.len() > 1 {
        html.push_str("<div class=\"step-tabs\" role=\"tablist\">");
        html.push_str(
            "<button type=\"button\" class=\"tab-btn active\" data-tab=\"all\">All</button>",
        );
        for (id, label) in &available_tabs {
            let _ = write!(
                html,
                "<button type=\"button\" class=\"tab-btn\" data-tab=\"{id}\">{label}</button>"
            );
        }
        html.push_str("</div>\n");
    }

    if let Some(meta) = &model_meta {
        html.push_str("<div class=\"tab-panel\" data-tab-panel=\"model\">\n");
        render_step_model_card(html, meta);
        html.push_str("</div>\n");
    }

    if let Some(assertions) = assertions {
        html.push_str("<div class=\"tab-panel\" data-tab-panel=\"assertions\">\n");
        render_assertions(html, assertions);
        html.push_str("</div>\n");
    }

    if let Some(saved) = saved {
        html.push_str("<div class=\"tab-panel\" data-tab-panel=\"data\">\n");
        html.push_str("<div class=\"card saved\"><h4>Saved variables</h4><div class=\"table-wrap\"><table><thead><tr><th>Name</th><th>Value</th></tr></thead><tbody>");
        for (var_name, var_val) in saved {
            let _ = write!(
                html,
                "<tr><td><code>{}</code></td><td><code>{}</code></td></tr>",
                escape_text(var_name),
                escape_text(&display_value(var_val))
            );
        }
        html.push_str("</tbody></table></div></div>\n</div>\n");
    }

    if let Some(result) = result {
        if has_data_layer {
            html.push_str(
                "<div class=\"tab-panel\" data-tab-panel=\"data\">\n<div class=\"layers\">\n",
            );
            render_http(html, result);
            render_db(html, result);
            render_cli(html, result);
            html.push_str("</div>\n</div>\n");
        }
        if has_observed {
            html.push_str(
                "<div class=\"tab-panel\" data-tab-panel=\"observed\">\n<div class=\"layers\">\n",
            );
            render_observations(html, result);
            html.push_str("</div>\n</div>\n");
        }
    }

    if let Some(trace_files) = trace_files {
        html.push_str("<div class=\"tab-panel\" data-tab-panel=\"trace\">\n");
        render_evidence_files(
            html,
            trace_files,
            case,
            shareable,
            budget,
            target_bounds,
            EvidenceHeadings {
                images: "Step screenshots",
                doms: "Step DOM snapshots",
                texts: "Step text evidence",
                binaries: "Step binary evidence",
                wrapper_class: "step-evidence",
            },
        );
        html.push_str("</div>\n");
    }

    if has_raw {
        html.push_str("<div class=\"tab-panel\" data-tab-panel=\"raw\">\n");
        if let Some(args) = step.get("args")
            && !args.is_null()
        {
            html.push_str("<details class=\"raw\"><summary>Resolved arguments</summary><pre>");
            html.push_str(&escape_text(&pretty(args)));
            html.push_str("</pre></details>\n");
        }
        if let Some(result) = result {
            html.push_str("<details class=\"raw\"><summary>Raw step result</summary><pre>");
            html.push_str(&escape_text(&pretty(result)));
            html.push_str("</pre></details>\n");
        }
        html.push_str("</div>\n");
    }

    html.push_str("</article>\n");
}

fn render_step_model_card(html: &mut String, meta: &StepModelMeta) {
    html.push_str("<div class=\"card model-card\"><h4>Model telemetry</h4><p class=\"chips\">");
    if let Some(model) = &meta.model {
        let _ = write!(html, "<span>model: {}</span>", escape_text(model));
    }
    if let Some(lat) = meta.latency_ms {
        let _ = write!(html, "<span>latency: {} ms</span>", thousands(lat));
    }
    if let Some(conf) = meta.confidence {
        let _ = write!(html, "<span>confidence: {:.2}</span>", conf);
    }
    if let Some(tid) = &meta.trace_id {
        let _ = write!(html, "<span>trace: {}</span>", escape_text(tid));
    }
    if let Some(usage) = &meta.usage {
        let c = parse_usage_counts(usage);
        if c.any_present {
            let _ = write!(
                html,
                "<span>tokens: {} (in {}, out {}, cached {})</span>",
                thousands(c.total),
                thousands(c.prompt),
                thousands(c.completion),
                thousands(c.cached),
            );
            if c.reasoning > 0 {
                let _ = write!(html, "<span>reasoning: {}</span>", thousands(c.reasoning));
            }
        }
    }
    html.push_str("</p></div>\n");
}

fn render_target_bounds_summary(html: &mut String, bounds: &Value) {
    let mut parts = Vec::new();
    if let (Some(from), Some(to)) = (bounds.get("from"), bounds.get("to"))
        && let (Some(fx), Some(fy), Some(tx), Some(ty)) = (
            from.get("x").and_then(Value::as_f64),
            from.get("y").and_then(Value::as_f64),
            to.get("x").and_then(Value::as_f64),
            to.get("y").and_then(Value::as_f64),
        )
    {
        parts.push(format!(
            "drag ({}, {}) &rarr; ({}, {})",
            format_svg_num(fx),
            format_svg_num(fy),
            format_svg_num(tx),
            format_svg_num(ty)
        ));
    } else {
        if let (Some(x), Some(y), Some(w), Some(h)) = (
            bounds.get("x").and_then(Value::as_f64),
            bounds.get("y").and_then(Value::as_f64),
            bounds.get("width").and_then(Value::as_f64),
            bounds.get("height").and_then(Value::as_f64),
        ) && (w > 0.0 || h > 0.0)
        {
            parts.push(format!(
                "box ({}, {}, {}&times;{})",
                format_svg_num(x),
                format_svg_num(y),
                format_svg_num(w),
                format_svg_num(h)
            ));
        }
        if let Some(pt) = bounds.get("point")
            && let (Some(px), Some(py)) = (
                pt.get("x").and_then(Value::as_f64),
                pt.get("y").and_then(Value::as_f64),
            )
        {
            parts.push(format!(
                "point ({}, {})",
                format_svg_num(px),
                format_svg_num(py)
            ));
        }
    }
    if let Some(vp) = bounds.get("viewport")
        && let (Some(vw), Some(vh)) = (
            vp.get("width").and_then(Value::as_u64),
            vp.get("height").and_then(Value::as_u64),
        )
    {
        parts.push(format!("viewport {vw}&times;{vh}"));
    }
    if !parts.is_empty() {
        let _ = writeln!(
            html,
            "<p class=\"target-meta\"><strong>Target geometry:</strong> {}</p>",
            parts.join(" &middot; ")
        );
    }
}

/// Renders an HTML5 inline SVG overlay for `target_bounds` over a screenshot.
///
/// Omits any XML namespace URL so the document stays free of cleartext URL schemes.
fn render_target_bounds_svg(html: &mut String, bounds: &Value, fallback_dims: Option<(u32, u32)>) {
    let vp = bounds
        .get("viewport")
        .and_then(|v| {
            let w = v.get("width").and_then(Value::as_u64)?;
            let h = v.get("height").and_then(Value::as_u64)?;
            (w > 0 && h > 0).then_some((w as f64, h as f64))
        })
        .or_else(|| fallback_dims.map(|(w, h)| (f64::from(w), f64::from(h))));
    let Some((vw, vh)) = vp else {
        return;
    };
    if !vw.is_finite() || !vh.is_finite() || vw <= 0.0 || vh <= 0.0 {
        return;
    }

    let _ = write!(
        html,
        "<svg class=\"shot-overlay\" viewBox=\"0 0 {} {}\" preserveAspectRatio=\"none\" aria-hidden=\"true\">",
        format_svg_num(vw),
        format_svg_num(vh),
    );

    let r_ring = (vw.min(vh) * 0.018).clamp(8.0, 22.0);
    let r_dot = (r_ring * 0.34).clamp(3.0, 7.0);
    let cross = r_ring * 0.85;

    if let (Some(from), Some(to)) = (bounds.get("from"), bounds.get("to"))
        && let (Some(fx), Some(fy), Some(tx), Some(ty)) = (
            from.get("x").and_then(Value::as_f64),
            from.get("y").and_then(Value::as_f64),
            to.get("x").and_then(Value::as_f64),
            to.get("y").and_then(Value::as_f64),
        )
        && fx.is_finite()
        && fy.is_finite()
        && tx.is_finite()
        && ty.is_finite()
    {
        if let Some(fb) = bounds.get("from_bounds") {
            render_svg_rect(html, fb, "tb-box tb-drag-from");
        }
        if let Some(tb) = bounds.get("to_bounds") {
            render_svg_rect(html, tb, "tb-box tb-drag-to");
        }
        let _ = write!(
            html,
            "<line class=\"tb-drag-line\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
            format_svg_num(fx),
            format_svg_num(fy),
            format_svg_num(tx),
            format_svg_num(ty),
        );
        let dx = tx - fx;
        let dy = ty - fy;
        let dist = dx.hypot(dy);
        if dist > 1.0 {
            let head_len = (r_ring * 1.1).min(dist * 0.4);
            let head_w = head_len * 0.55;
            let ux = dx / dist;
            let uy = dy / dist;
            let bx = tx - ux * head_len;
            let by = ty - uy * head_len;
            let p1x = bx - uy * head_w;
            let p1y = by + ux * head_w;
            let p2x = bx + uy * head_w;
            let p2y = by - ux * head_w;
            let _ = write!(
                html,
                "<polygon class=\"tb-drag-arrow\" points=\"{},{} {},{} {},{}\"/>",
                format_svg_num(tx),
                format_svg_num(ty),
                format_svg_num(p1x),
                format_svg_num(p1y),
                format_svg_num(p2x),
                format_svg_num(p2y),
            );
        }
        let _ = write!(
            html,
            "<circle class=\"tb-drag-start\" cx=\"{}\" cy=\"{}\" r=\"{}\"/>",
            format_svg_num(fx),
            format_svg_num(fy),
            format_svg_num(r_dot * 1.3),
        );
        let _ = write!(
            html,
            "<circle class=\"tb-ring\" cx=\"{}\" cy=\"{}\" r=\"{}\"/><circle class=\"tb-dot\" cx=\"{}\" cy=\"{}\" r=\"{}\"/>",
            format_svg_num(tx),
            format_svg_num(ty),
            format_svg_num(r_ring),
            format_svg_num(tx),
            format_svg_num(ty),
            format_svg_num(r_dot),
        );
    } else {
        render_svg_rect(html, bounds, "tb-box");
        let point = bounds
            .get("point")
            .and_then(|p| {
                Some((
                    p.get("x").and_then(Value::as_f64)?,
                    p.get("y").and_then(Value::as_f64)?,
                ))
            })
            .or_else(|| {
                let x = bounds.get("x").and_then(Value::as_f64)?;
                let y = bounds.get("y").and_then(Value::as_f64)?;
                let w = bounds.get("width").and_then(Value::as_f64).unwrap_or(0.0);
                let h = bounds.get("height").and_then(Value::as_f64).unwrap_or(0.0);
                Some((x + w / 2.0, y + h / 2.0))
            });
        if let Some((px, py)) = point
            && px.is_finite()
            && py.is_finite()
        {
            let _ = write!(
                html,
                "<circle class=\"tb-ring\" cx=\"{}\" cy=\"{}\" r=\"{}\"/><circle class=\"tb-dot\" cx=\"{}\" cy=\"{}\" r=\"{}\"/><line class=\"tb-cross\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/><line class=\"tb-cross\" x1=\"{}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/>",
                format_svg_num(px),
                format_svg_num(py),
                format_svg_num(r_ring),
                format_svg_num(px),
                format_svg_num(py),
                format_svg_num(r_dot),
                format_svg_num(px - cross),
                format_svg_num(py),
                format_svg_num(px + cross),
                format_svg_num(py),
                format_svg_num(px),
                format_svg_num(py - cross),
                format_svg_num(px),
                format_svg_num(py + cross),
            );
        }
    }

    html.push_str("</svg>");
}

fn render_svg_rect(html: &mut String, rect: &Value, class: &str) {
    let Some(x) = rect.get("x").and_then(Value::as_f64) else {
        return;
    };
    let Some(y) = rect.get("y").and_then(Value::as_f64) else {
        return;
    };
    let Some(w) = rect.get("width").and_then(Value::as_f64) else {
        return;
    };
    let Some(h) = rect.get("height").and_then(Value::as_f64) else {
        return;
    };
    if !x.is_finite()
        || !y.is_finite()
        || !w.is_finite()
        || !h.is_finite()
        || (w <= 0.0 && h <= 0.0)
    {
        return;
    }
    let _ = write!(
        html,
        "<rect class=\"{class}\" x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" rx=\"3\"/>",
        format_svg_num(x),
        format_svg_num(y),
        format_svg_num(w.max(0.0)),
        format_svg_num(h.max(0.0)),
    );
}

fn format_svg_num(value: f64) -> String {
    if !value.is_finite() {
        return "0".to_string();
    }
    let rounded = (value * 100.0).round() / 100.0;
    if (rounded - rounded.round()).abs() < 1e-9 {
        format!("{}", rounded.round() as i64)
    } else {
        format!("{rounded:.2}")
    }
}

fn render_assertions(html: &mut String, assertions: &[Value]) {
    if assertions.is_empty() {
        return;
    }
    html.push_str("<div class=\"card assertions\"><h4>Assertions</h4><div class=\"table-wrap\"><table><thead><tr><th>Result</th><th>Path</th><th>Comparison</th><th>Expected</th><th>Actual</th><th>Message</th></tr></thead><tbody>");
    for assertion in assertions {
        let passed = assertion
            .get("passed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let _ = write!(
            html,
            "<tr><td><span class=\"assert {}\">{}</span></td><td><code>{}</code></td><td>{}</td><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
            if passed { "pass" } else { "fail" },
            if passed { "PASS" } else { "FAIL" },
            escape_text(assertion.get("path").and_then(Value::as_str).unwrap_or("")),
            escape_text(
                assertion
                    .get("comparison")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            ),
            escape_text(&json_or_missing(assertion.get("expected"))),
            escape_text(&json_or_missing(assertion.get("actual"))),
            escape_text(
                assertion
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
            )
        );
    }
    html.push_str("</tbody></table></div></div>\n");
}

fn render_http(html: &mut String, result: &Value) {
    if !result.get("headers").is_some_and(Value::is_object) || result.get("status").is_none() {
        return;
    }
    html.push_str("<div class=\"card\"><h4>HTTP response</h4>");
    let _ = write!(
        html,
        "<p class=\"status-line\">Status <strong>{}</strong></p>",
        escape_text(&display_value(result.get("status").unwrap_or(&Value::Null)))
    );
    if let Some(headers) = result.get("headers").and_then(Value::as_object) {
        html.push_str("<div class=\"table-wrap\"><table><thead><tr><th>Header</th><th>Value</th></tr></thead><tbody>");
        for (name, value) in headers {
            let _ = write!(
                html,
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                escape_text(name),
                escape_text(&display_value(value))
            );
        }
        html.push_str("</tbody></table></div>");
    }
    if let Some(body) = result.get("body") {
        html.push_str("<details open><summary>Response body</summary><pre>");
        html.push_str(&escape_text(&pretty(body)));
        html.push_str("</pre></details>");
    }
    html.push_str("</div>\n");
}

fn render_db(html: &mut String, result: &Value) {
    let Some(rows) = result.get("rows").and_then(Value::as_array) else {
        return;
    };
    let Some(columns) = result.get("columns").and_then(Value::as_array) else {
        return;
    };
    html.push_str("<div class=\"card\"><h4>DB query</h4>");
    let row_count = result
        .get("row_count")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| u64::try_from(rows.len()).unwrap_or(u64::MAX));
    let truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let _ = write!(
        html,
        "<p class=\"note\">{} row{}; truncated: {}</p>",
        row_count,
        if row_count == 1 { "" } else { "s" },
        if truncated { "yes" } else { "no" }
    );
    if rows.is_empty() {
        html.push_str("<p class=\"note\">No rows returned.</p>");
    } else {
        html.push_str("<div class=\"table-wrap\"><table><thead><tr>");
        for column in columns {
            let _ = write!(html, "<th>{}</th>", escape_text(&display_value(column)));
        }
        html.push_str("</tr></thead><tbody>");
        for row in rows {
            html.push_str("<tr>");
            for column in columns {
                let cell = row
                    .as_object()
                    .and_then(|object| object.get(display_value(column).as_str()))
                    .unwrap_or(&Value::Null);
                let _ = write!(
                    html,
                    "<td><code>{}</code></td>",
                    escape_text(&display_value(cell))
                );
            }
            html.push_str("</tr>");
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</div>\n");
}

fn render_cli(html: &mut String, result: &Value) {
    if result.get("exit_code").is_none()
        || (result.get("stdout").is_none() && result.get("stderr").is_none())
    {
        return;
    }
    html.push_str("<div class=\"card\"><h4>CLI output</h4>");
    let _ = write!(
        html,
        "<p class=\"status-line\">Exit code <strong>{}</strong></p>",
        escape_text(&display_value(
            result.get("exit_code").unwrap_or(&Value::Null)
        ))
    );
    for (label, field) in [("Standard output", "stdout"), ("Standard error", "stderr")] {
        if let Some(value) = result.get(field) {
            let _ = write!(
                html,
                "<h5>{}</h5><pre>{}</pre>",
                label,
                escape_text(&display_value(value))
            );
        }
    }
    html.push_str("</div>\n");
}

fn render_observations(html: &mut String, result: &Value) {
    let Some(observed) = result.get("observed").and_then(Value::as_object) else {
        return;
    };
    for (name, value) in observed {
        if let Some(requests) = value.get("requests").and_then(Value::as_array) {
            render_network(html, name, value, requests);
        } else if let Some(lines) = value.get("lines").and_then(Value::as_array) {
            render_log(html, name, value, lines);
        } else if let Some(events) = value.get("events").and_then(Value::as_array) {
            render_fs_watch(html, name, value, events);
        } else {
            let _ = writeln!(
                html,
                "<div class=\"card\"><h4>Observation <code>{}</code></h4><pre>{}</pre></div>",
                escape_text(name),
                escape_text(&pretty(value))
            );
        }
    }
}

fn render_fs_watch(html: &mut String, name: &str, value: &Value, events: &[Value]) {
    let truncated = value
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let _ = write!(
        html,
        "<div class=\"card\"><h4>Filesystem watch <code>{}</code></h4><p class=\"note\">{} event{}; truncated: {}</p>",
        escape_text(name),
        events.len(),
        if events.len() == 1 { "" } else { "s" },
        truncated
    );
    if events.is_empty() {
        html.push_str("<p class=\"note\">No filesystem event occurred while this step ran.</p>");
    } else {
        html.push_str("<div class=\"table-wrap\"><table><thead><tr><th>Kind</th><th>Path</th><th>Size</th></tr></thead><tbody>");
        for event in events {
            let _ = write!(
                html,
                "<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>",
                escape_text(event.get("kind").and_then(Value::as_str).unwrap_or("")),
                escape_text(event.get("path").and_then(Value::as_str).unwrap_or("")),
                escape_text(&display_value(event.get("size").unwrap_or(&Value::Null))),
            );
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</div>\n");
}

fn render_network(html: &mut String, name: &str, value: &Value, requests: &[Value]) {
    let _ = write!(
        html,
        "<div class=\"card\"><h4>Network observations <code>{}</code></h4><p class=\"note\">{} request{}; pending: {}; dropped: {}</p>",
        escape_text(name),
        requests.len(),
        if requests.len() == 1 { "" } else { "s" },
        escape_text(&display_value(value.get("pending").unwrap_or(&Value::Null))),
        escape_text(&display_value(value.get("dropped").unwrap_or(&Value::Null)))
    );
    if requests.is_empty() {
        html.push_str("<p class=\"note\">No request finished while this step ran.</p>");
    } else {
        html.push_str("<div class=\"table-wrap\"><table><thead><tr><th>Method</th><th>URL</th><th>Status</th><th>Type</th><th>Error</th></tr></thead><tbody>");
        for request in requests {
            let _ = write!(
                html,
                "<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td></tr>",
                escape_text(request.get("method").and_then(Value::as_str).unwrap_or("")),
                escape_text(&display_value(request.get("url").unwrap_or(&Value::Null))),
                escape_text(&display_value(
                    request.get("status").unwrap_or(&Value::Null)
                )),
                escape_text(request.get("type").and_then(Value::as_str).unwrap_or("")),
                escape_text(&display_value(request.get("error").unwrap_or(&Value::Null)))
            );
        }
        html.push_str("</tbody></table></div>");
    }
    html.push_str("</div>\n");
}

fn render_log(html: &mut String, name: &str, value: &Value, lines: &[Value]) {
    let shown = lines.len().min(MAX_LOG_LINES);
    let _ = write!(
        html,
        "<div class=\"card\"><h4>Log lines <code>{}</code></h4><p class=\"note\">{} line{}; truncated: {}; limited: {}</p>",
        escape_text(name),
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
        value
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        value
            .get("limited")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    );
    if shown == 0 {
        html.push_str("<p class=\"note\">No line was added while this step ran.</p>");
    } else {
        html.push_str("<pre>");
        for line in &lines[..shown] {
            html.push_str(&escape_text(&display_value(line)));
            html.push('\n');
        }
        html.push_str("</pre>");
        if shown < lines.len() {
            let _ = write!(
                html,
                "<p class=\"note\">{} more line(s) are not shown.</p>",
                lines.len() - shown
            );
        }
    }
    html.push_str("</div>\n");
}

struct EvidenceHeadings {
    images: &'static str,
    doms: &'static str,
    texts: &'static str,
    binaries: &'static str,
    wrapper_class: &'static str,
}

fn render_case_evidence(
    html: &mut String,
    case: &CaseRecord,
    shareable: bool,
    budget: &mut ImageBudget,
) {
    let overlay_bounds = if case.step_traces.is_empty() {
        case.fallback_case_target_bounds()
    } else {
        None
    };
    render_evidence_files(
        html,
        &case.evidence,
        case,
        shareable,
        budget,
        overlay_bounds,
        EvidenceHeadings {
            images: "Interface evidence",
            doms: "DOM snapshots",
            texts: "Other text evidence",
            binaries: "Other binary evidence",
            wrapper_class: "case-evidence",
        },
    );
}

fn render_evidence_files(
    html: &mut String,
    files: &[EvidenceFile],
    case: &CaseRecord,
    shareable: bool,
    budget: &mut ImageBudget,
    target_bounds: Option<&Value>,
    headings: EvidenceHeadings,
) {
    let class = headings.wrapper_class;
    let images: Vec<&EvidenceFile> = files
        .iter()
        .filter(|file| matches!(file.kind, EvidenceKind::Image(_)))
        .collect();
    if !images.is_empty() {
        let _ = write!(
            html,
            "<div class=\"{class}\"><h3>{}</h3><div class=\"screenshots\">",
            headings.images
        );
        for image in images {
            let reason = if shareable {
                Some(
                    "the report is shareable (`--shareable`), so screenshots are not embedded"
                        .to_string(),
                )
            } else if case.local_only {
                Some(match case.local_only_reason.as_deref() {
                    Some(reason) => format!(
                        "meta.json marks this case `local_only: true` ({}), so its screenshot is not embedded",
                        escape_text(reason)
                    ),
                    None => "meta.json marks this case `local_only: true`, so its screenshot is not embedded".to_string(),
                })
            } else if image.len > SINGLE_IMAGE_LIMIT {
                budget.omit();
                Some(format!(
                    "{} bytes exceeds the 2 MiB single-image limit",
                    thousands(image.len)
                ))
            } else {
                None
            };
            let label = escape_text(&image.label());
            if let Some(reason) = reason {
                if shareable || case.local_only {
                    budget.omit();
                }
                let _ = write!(
                    html,
                    "<p class=\"omission\"><strong>{label}</strong> ({} bytes): not embedded because {reason}.</p>",
                    thousands(image.len)
                );
                continue;
            }
            match fs::read(&image.path) {
                Ok(bytes) => match budget.embed(image.len) {
                    Ok(()) => {
                        if let EvidenceKind::Image(mime) = image.kind {
                            let png_dims = png_dimensions(&bytes);
                            html.push_str(
                                "<figure class=\"shot-stage\"><div class=\"shot-frame\">",
                            );
                            let _ = write!(
                                html,
                                "<img src=\"data:{mime};base64,{}\" alt=\"{label}\">",
                                base64_encode(&bytes),
                            );
                            if let Some(bounds) = target_bounds {
                                render_target_bounds_svg(html, bounds, png_dims);
                            }
                            let _ = write!(
                                html,
                                "</div><figcaption>{label} · {} bytes</figcaption></figure>",
                                thousands(image.len)
                            );
                        }
                    }
                    Err(reason) => {
                        let _ = write!(
                            html,
                            "<p class=\"omission\"><strong>{label}</strong> ({} bytes): not embedded because {reason}.</p>",
                            thousands(image.len)
                        );
                    }
                },
                Err(_) => {
                    budget.omit();
                    let _ = write!(
                        html,
                        "<p class=\"omission\"><strong>{label}</strong> ({} bytes): not embedded because the file could not be read.</p>",
                        thousands(image.len)
                    );
                }
            }
        }
        html.push_str("</div></div>\n");
    }

    let doms: Vec<&EvidenceFile> = files
        .iter()
        .filter(|file| file.kind == EvidenceKind::Dom)
        .collect();
    if !doms.is_empty() {
        let _ = write!(html, "<div class=\"{class}\"><h3>{}</h3>", headings.doms);
        for dom in doms {
            let _ = write!(
                html,
                "<article class=\"card\"><h4>{}</h4><p class=\"note\">{} bytes</p>",
                escape_text(&dom.label()),
                thousands(dom.len)
            );
            match read_limited(&dom.path, DOM_TEXT_LIMIT) {
                Ok((text, truncated)) => {
                    if truncated {
                        let _ = write!(
                            html,
                            "<p class=\"note\">Showing the first {} of {} bytes.</p>",
                            thousands(DOM_TEXT_LIMIT as u64),
                            thousands(dom.len)
                        );
                    }
                    html.push_str("<details><summary>Show DOM snapshot</summary><pre>");
                    html.push_str(&escape_text(&text));
                    html.push_str("</pre></details>");
                }
                Err(_) => html.push_str("<p class=\"note\">The snapshot could not be read.</p>"),
            }
            html.push_str("</article>");
        }
        html.push_str("</div>\n");
    }

    let texts: Vec<&EvidenceFile> = files
        .iter()
        .filter(|file| file.kind == EvidenceKind::Text)
        .collect();
    if !texts.is_empty() {
        let _ = write!(html, "<div class=\"{class}\"><h3>{}</h3>", headings.texts);
        for text in texts {
            let _ = write!(
                html,
                "<article class=\"card\"><h4>{}</h4><p class=\"note\">{} bytes</p>",
                escape_text(&text.label()),
                thousands(text.len)
            );
            match read_limited(&text.path, EVIDENCE_TEXT_LIMIT) {
                Ok((content, truncated)) => {
                    if truncated {
                        let _ = write!(
                            html,
                            "<p class=\"note\">Showing the first {} of {} bytes.</p>",
                            thousands(EVIDENCE_TEXT_LIMIT as u64),
                            thousands(text.len)
                        );
                    }
                    html.push_str("<pre>");
                    html.push_str(&escape_text(&content));
                    html.push_str("</pre>");
                }
                Err(_) => html.push_str("<p class=\"note\">The file could not be read.</p>"),
            }
            html.push_str("</article>");
        }
        html.push_str("</div>\n");
    }

    let binaries: Vec<&EvidenceFile> = files
        .iter()
        .filter(|file| file.kind == EvidenceKind::Binary)
        .collect();
    if !binaries.is_empty() {
        let _ = write!(
            html,
            "<div class=\"{class}\"><h3>{}</h3><ul class=\"binary-list\">",
            headings.binaries
        );
        for binary in binaries {
            let _ = write!(
                html,
                "<li><code>{}</code> · {} bytes · not embedded</li>",
                escape_text(&binary.label()),
                thousands(binary.len)
            );
        }
        html.push_str("</ul></div>\n");
    }
}

fn read_limited(path: &Path, limit: usize) -> Result<(String, bool), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(limit.saturating_add(1));
    file.take(u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated))
}

fn problem_text(problem: &Map<String, Value>) -> String {
    let kind = problem
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("problem");
    let message = problem
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no message");
    format!("{kind}: {message}")
}

fn status_class(status: &str) -> &'static str {
    match status {
        "passed" => "passed",
        "failed" => "failed",
        "error" => "error",
        "skipped" => "skipped",
        _ => "unknown",
    }
}

fn json_or_missing(value: Option<&Value>) -> String {
    value.map_or_else(|| "<missing>".to_string(), pretty)
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => pretty(other),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

/// Escapes text for HTML text and attribute values.
///
/// `:` after `//` is written as an entity, so the document never contains a
/// cleartext URL scheme even when the evidence quotes one. The reader still sees
/// the original text.
fn escape_text(text: &str) -> String {
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
    escaped.replace("://", "&#58;//")
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

const STYLE: &str = r#":root{color-scheme:light;--paper:#faf7f0;--panel:#fff;--soft:#f7f3ea;--ink:#302c27;--muted:#756d62;--line:#ddd4c7;--accent:#b0713c;--pass:#356346;--fail:#9a3f3f;--error:#8a542c;--blue:#3f6485}*{box-sizing:border-box}body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.55 system-ui,-apple-system,Segoe UI,sans-serif}main{max-width:none;margin:0 auto;padding:0 clamp(16px,2.4vw,32px) 64px}code{font-size:.92em;background:#efe9dd;padding:.04rem .25rem;border-radius:3px}pre{overflow:auto;background:var(--soft);border:1px solid var(--line);padding:.8rem;white-space:pre-wrap;word-break:word;font:12.5px/1.5 ui-monospace,SFMono-Regular,Consolas,monospace}h1,h2,h3,h4,h5{margin:0;line-height:1.2}.hero{padding:2.4rem 0 1.4rem;border-bottom:1px solid var(--line)}.eyebrow{text-transform:uppercase;letter-spacing:.16em;font-size:.75rem;color:var(--accent);font-weight:700;margin:0}.hero h1{font:700 clamp(2.2rem,4vw,4.2rem)/1.02 Georgia,serif;margin:.3rem 0}.lede{max-width:85ch;color:var(--muted);margin:.4rem 0 1.2rem}.metrics{display:grid;grid-template-columns:repeat(6,minmax(0,1fr));gap:10px}.metric{background:var(--panel);border:1px solid var(--line);padding:.7rem .8rem}.metric strong{display:block;font:700 1.8rem/1 Georgia,serif;color:var(--blue)}.metric span{font-size:.82rem;color:var(--muted)}.case{padding:2rem 0;border-bottom:1px solid var(--line)}.case-head{display:flex;gap:1rem;align-items:flex-start}.case-title{min-width:0}.case-title h2{font:700 1.6rem/1.2 Georgia,serif;overflow-wrap:anywhere}.status{display:inline-block;border:1px solid var(--line);border-radius:999px;padding:.2rem .55rem;font-size:.76rem;font-weight:700;letter-spacing:.04em;background:var(--soft);white-space:nowrap}.status.passed{color:var(--pass);border-color:#8fb59b;background:#e5f0e7}.status.failed{color:var(--fail);border-color:#c58d8d;background:#f3e2e2}.status.error{color:var(--error);border-color:#d0a478;background:#f3e3d5}.status.skipped{color:var(--muted);border-color:var(--line);background:var(--soft)}.chips{display:flex;flex-wrap:wrap;gap:6px;margin:.45rem 0 0}.chips span{font-size:.78rem;color:var(--muted);background:var(--soft);border:1px solid var(--line);border-radius:4px;padding:.1rem .45rem}.problem{background:#f7e8e6;border:1px solid #d4a6a2;color:#6e2626;padding:.65rem .8rem;margin:.8rem 0}.steps{display:grid;gap:12px;margin-top:1rem}.step{background:var(--panel);border:1px solid var(--line);padding:.9rem 1rem}.step.failed{border-left:4px solid var(--fail)}.step.error{border-left:4px solid var(--error)}.step.passed{border-left:4px solid var(--pass)}.step.skipped{border-left:4px solid var(--line)}.step-head{display:flex;flex-wrap:wrap;gap:.55rem;align-items:center}.step-index{font:700 .85rem/1 ui-monospace,SFMono-Regular,Consolas,monospace;color:var(--muted)}.step-head h3{font-size:1rem;margin-right:auto}.duration{font-size:.82rem;color:var(--muted);font-variant-numeric:tabular-nums}.layers{display:grid;gap:10px;margin-top:.75rem}.card{background:var(--soft);border:1px solid var(--line);padding:.7rem .8rem;margin-top:.7rem}.card h4{font-size:.9rem;margin-bottom:.4rem}.card h5{font-size:.82rem;color:var(--muted);margin:.6rem 0 .25rem}.table-wrap{overflow:auto}table{width:100%;border-collapse:collapse;font-size:.86rem}th,td{text-align:left;padding:.35rem .5rem;border-bottom:1px solid var(--line);vertical-align:top}th{color:var(--muted);font-weight:600}.assert{display:inline-block;padding:.05rem .35rem;border-radius:3px;font-size:.75rem;font-weight:700}.assert.pass{background:#e5f0e7;color:var(--pass)}.assert.fail{background:#f3e2e2;color:var(--fail)}.raw{margin-top:.65rem}.raw summary,details summary{cursor:pointer;font-weight:600;color:var(--blue)}.case-evidence{margin-top:1.25rem}.case-evidence h3{font-size:1.1rem;margin-bottom:.6rem}.screenshots{display:grid;gap:14px}.screenshots figure{margin:0;background:var(--panel);border:1px solid var(--line);padding:.6rem}.screenshots img{display:block;max-width:100%;height:auto}.screenshots figcaption{margin-top:.45rem;font-size:.82rem;color:var(--muted)}.omission{background:var(--soft);border:1px dashed var(--line);padding:.6rem .8rem;margin:.5rem 0;font-size:.9rem}.note{color:var(--muted);font-size:.88rem;margin:.4rem 0}.binary-list{margin:.4rem 0;padding-left:1.2rem}.footer{margin-top:2rem;color:var(--muted);font-size:.85rem}@media (max-width:720px){.metrics{grid-template-columns:repeat(2,minmax(0,1fr))}}"#;

const TRACE_STYLE: &str = ".step-evidence{margin-top:.8rem}.step-evidence h3{font-size:1rem;margin:.6rem 0 .4rem}.saved table td:first-child{font-weight:600}";

const INTERACTIVE_STYLE: &str = r#"
kbd{display:inline-block;padding:.05rem .32rem;font:600 .78em ui-monospace,SFMono-Regular,Consolas,monospace;background:var(--panel);border:1px solid var(--line);border-radius:3px;color:var(--ink)}
.hero-top{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:.6rem}
.mode-badge{display:inline-block;padding:.2rem .65rem;border-radius:999px;font-size:.78rem;font-weight:700;letter-spacing:.03em;border:1px solid var(--line)}
.mode-badge.deterministic{background:#e5f0e7;border-color:#8fb59b;color:var(--pass)}
.mode-badge.exploration{background:#e8eef6;border-color:#9cb3cc;color:var(--blue)}
.telemetry-panel{margin-top:1rem;background:var(--panel);border:1px solid var(--line);padding:.85rem 1rem}
.telemetry-head{display:flex;flex-wrap:wrap;align-items:baseline;justify-content:space-between;gap:.5rem;margin-bottom:.6rem}
.telemetry-head h3{font-size:.98rem;color:var(--blue)}
.telemetry-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(145px,1fr));gap:8px}
.telemetry-grid .metric strong{font-size:1.3rem}
.telemetry-chips{margin-top:.55rem}
.timeline-bar{margin-top:1rem;background:var(--panel);border:1px solid var(--line);padding:.75rem .85rem;position:sticky;top:0;z-index:5}
.timeline-controls{display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin-bottom:.55rem}
.tl-btn{appearance:none;background:var(--soft);border:1px solid var(--line);color:var(--ink);padding:.28rem .6rem;border-radius:4px;font:600 .8rem/1.2 system-ui,-apple-system,sans-serif;cursor:pointer}
.tl-btn:hover{border-color:var(--accent)}
.tl-btn.active{background:var(--blue);border-color:var(--blue);color:#fff}
.tl-counter{font:600 .82rem/1 ui-monospace,SFMono-Regular,Consolas,monospace;color:var(--muted);padding:0 .35rem;margin-right:auto}
.timeline-strip{display:flex;flex-wrap:nowrap;gap:6px;overflow-x:auto;padding-bottom:.2rem}
.tl-pill{appearance:none;display:inline-flex;align-items:center;gap:4px;background:var(--soft);border:1px solid var(--line);border-radius:4px;padding:.24rem .5rem;font:500 .78rem/1.2 ui-monospace,SFMono-Regular,Consolas,monospace;color:var(--ink);cursor:pointer;white-space:nowrap}
.tl-pill.passed{border-bottom:3px solid var(--pass)}
.tl-pill.failed{border-bottom:3px solid var(--fail)}
.tl-pill.error{border-bottom:3px solid var(--error)}
.tl-pill.skipped{border-bottom:3px solid var(--line);color:var(--muted)}
.tl-pill.active{outline:2px solid var(--blue);outline-offset:1px;background:#eef3f9}
.tl-pill[hidden]{display:none}
.step.active{box-shadow:0 0 0 2px rgba(63,100,133,.28)}
.case.focus-mode .step:not(.active){display:none}
.step[hidden]{display:none}
.chip-warn{font-size:.74rem;font-weight:700;padding:.08rem .4rem;border-radius:4px;background:#f7ead8;border:1px solid #d0a478;color:var(--error)}
.chip-meta{font-size:.74rem;padding:.08rem .4rem;border-radius:4px;background:var(--soft);border:1px solid var(--line);color:var(--muted)}
.target-meta{font-size:.82rem;color:var(--blue);background:#eef3f9;border:1px solid #c3d2e2;padding:.35rem .6rem;border-radius:4px;margin:.55rem 0 0}
.step-tabs{display:flex;flex-wrap:wrap;gap:4px;margin-top:.65rem;border-bottom:1px solid var(--line);padding-bottom:.4rem}
.tab-btn{appearance:none;background:transparent;border:1px solid transparent;border-radius:4px;padding:.2rem .55rem;font:600 .8rem/1.2 system-ui,-apple-system,sans-serif;color:var(--muted);cursor:pointer}
.tab-btn:hover{color:var(--ink);background:var(--soft)}
.tab-btn.active{background:var(--soft);border-color:var(--line);color:var(--blue)}
.tab-panel[hidden]{display:none}
.shot-frame{position:relative;display:inline-block;max-width:100%;line-height:0}
.shot-frame img{display:block;max-width:100%;height:auto}
.shot-overlay{position:absolute;inset:0;width:100%;height:100%;pointer-events:none}
.hide-overlays .shot-overlay{display:none}
.tb-box{fill:rgba(217,56,41,.14);stroke:#d93829;stroke-width:2.5;vector-effect:non-scaling-stroke}
.tb-drag-from{fill:rgba(63,100,133,.14);stroke:#3f6485}
.tb-drag-to{fill:rgba(53,99,70,.14);stroke:#356346}
.tb-ring{fill:rgba(217,56,41,.18);stroke:#d93829;stroke-width:2.5;vector-effect:non-scaling-stroke}
.tb-dot{fill:#d93829;stroke:#fff;stroke-width:1.5;vector-effect:non-scaling-stroke}
.tb-cross{stroke:#d93829;stroke-width:1.6;vector-effect:non-scaling-stroke}
.tb-drag-line{stroke:#d93829;stroke-width:2.5;stroke-dasharray:6 4;vector-effect:non-scaling-stroke}
.tb-drag-arrow{fill:#d93829}
.tb-drag-start{fill:#3f6485;stroke:#fff;stroke-width:1.5;vector-effect:non-scaling-stroke}
.model-card{background:#eef3f9;border-color:#c3d2e2}
"#;

const VIEWER_SCRIPT: &str = r#"(function(){
var cases=Array.prototype.slice.call(document.querySelectorAll('.case'));
if(!cases.length)return;
var activeCase=cases[0];
var stateMap=new WeakMap();
cases.forEach(function(sec){
  var steps=Array.prototype.slice.call(sec.querySelectorAll('.step'));
  var pills=Array.prototype.slice.call(sec.querySelectorAll('.tl-pill'));
  var counter=sec.querySelector('[data-role="counter"]');
  var emptyNote=sec.querySelector('[data-role="empty-filter"]');
  var playBtn=sec.querySelector('[data-action="play"]');
  var filterBtn=sec.querySelector('[data-action="filter-failed"]');
  var overlayBtn=sec.querySelector('[data-action="toggle-overlays"]');
  var focusBtn=sec.querySelector('[data-action="toggle-focus"]');
  var st={sec:sec,steps:steps,pills:pills,counter:counter,emptyNote:emptyNote,playBtn:playBtn,filterBtn:filterBtn,overlayBtn:overlayBtn,focusBtn:focusBtn,index:0,failedOnly:false,focusMode:false,overlays:true,timer:null};
  stateMap.set(sec,st);

  function visibleIndices(){
    var out=[];
    for(var i=0;i<st.steps.length;i++){
      if(!st.failedOnly||st.steps[i].getAttribute('data-failed')==='true')out.push(i);
    }
    return out;
  }
  function stopPlay(){
    if(st.timer){clearInterval(st.timer);st.timer=null;}
    if(st.playBtn){st.playBtn.classList.remove('active');st.playBtn.setAttribute('aria-pressed','false');st.playBtn.textContent='Play';}
  }
  function render(scroll){
    var vis=visibleIndices();
    for(var i=0;i<st.steps.length;i++){
      var show=!st.failedOnly||st.steps[i].getAttribute('data-failed')==='true';
      st.steps[i].hidden=!show;
      if(st.pills[i])st.pills[i].hidden=!show;
      st.steps[i].classList.remove('active');
      if(st.pills[i])st.pills[i].classList.remove('active');
    }
    if(st.emptyNote)st.emptyNote.hidden=vis.length>0;
    if(!vis.length){
      if(st.counter)st.counter.textContent='0 / '+st.steps.length;
      stopPlay();
      return;
    }
    if(vis.indexOf(st.index)===-1)st.index=vis[0];
    var pos=vis.indexOf(st.index)+1;
    st.steps[st.index].classList.add('active');
    if(st.pills[st.index]){
      st.pills[st.index].classList.add('active');
      if(scroll&&st.pills[st.index].scrollIntoView)st.pills[st.index].scrollIntoView({block:'nearest',inline:'nearest'});
    }
    if(scroll&&!st.focusMode&&st.steps[st.index].scrollIntoView){
      st.steps[st.index].scrollIntoView({block:'nearest',behavior:'smooth'});
    }
    if(st.counter)st.counter.textContent='Step '+pos+' / '+vis.length;
  }
  function move(delta){
    var vis=visibleIndices();
    if(!vis.length)return;
    var cur=vis.indexOf(st.index);
    if(cur<0)cur=0;
    var next=(cur+delta+vis.length)%vis.length;
    st.index=vis[next];
    render(true);
  }
  function togglePlay(){
    if(st.timer){stopPlay();return;}
    var vis=visibleIndices();
    if(vis.length<2)return;
    if(st.playBtn){st.playBtn.classList.add('active');st.playBtn.setAttribute('aria-pressed','true');st.playBtn.textContent='Pause';}
    st.timer=setInterval(function(){move(1);},1200);
  }
  function toggleFilter(){
    st.failedOnly=!st.failedOnly;
    if(st.filterBtn){
      st.filterBtn.classList.toggle('active',st.failedOnly);
      st.filterBtn.setAttribute('aria-pressed',st.failedOnly?'true':'false');
    }
    render(false);
  }
  function toggleOverlays(){
    st.overlays=!st.overlays;
    sec.classList.toggle('hide-overlays',!st.overlays);
    if(st.overlayBtn){
      st.overlayBtn.classList.toggle('active',st.overlays);
      st.overlayBtn.setAttribute('aria-pressed',st.overlays?'true':'false');
      st.overlayBtn.textContent=st.overlays?'Overlays: ON (o)':'Overlays: OFF (o)';
    }
  }
  function toggleFocus(){
    st.focusMode=!st.focusMode;
    sec.classList.toggle('focus-mode',st.focusMode);
    if(st.focusBtn){
      st.focusBtn.classList.toggle('active',st.focusMode);
      st.focusBtn.setAttribute('aria-pressed',st.focusMode?'true':'false');
      st.focusBtn.textContent=st.focusMode?'Show all steps':'Timeline focus';
    }
    render(false);
  }
  st.move=move;st.togglePlay=togglePlay;st.toggleFilter=toggleFilter;st.toggleOverlays=toggleOverlays;
  sec.addEventListener('click',function(ev){
    activeCase=sec;
    var btn=ev.target.closest('button');
    if(!btn)return;
    var act=btn.getAttribute('data-action');
    if(act==='prev'){stopPlay();move(-1);return;}
    if(act==='next'){stopPlay();move(1);return;}
    if(act==='play'){togglePlay();return;}
    if(act==='filter-failed'){toggleFilter();return;}
    if(act==='toggle-overlays'){toggleOverlays();return;}
    if(act==='toggle-focus'){toggleFocus();return;}
    if(btn.classList.contains('tl-pill')){
      stopPlay();
      st.index=parseInt(btn.getAttribute('data-step-idx'),10)||0;
      render(true);
      return;
    }
    if(btn.classList.contains('tab-btn')){
      var stepEl=btn.closest('.step');
      if(!stepEl)return;
      var tab=btn.getAttribute('data-tab');
      Array.prototype.forEach.call(stepEl.querySelectorAll('.tab-btn'),function(b){
        b.classList.toggle('active',b===btn);
      });
      Array.prototype.forEach.call(stepEl.querySelectorAll('.tab-panel'),function(p){
        p.hidden=tab!=='all'&&p.getAttribute('data-tab-panel')!==tab;
      });
    }
  });
  render(false);
});
document.addEventListener('keydown',function(ev){
  var tag=(ev.target&&ev.target.tagName)||'';
  if(tag==='INPUT'||tag==='TEXTAREA'||tag==='SELECT'||(ev.target&&ev.target.isContentEditable))return;
  var st=stateMap.get(activeCase);
  if(!st)return;
  if(ev.key==='ArrowLeft'){ev.preventDefault();st.move(-1);}
  else if(ev.key==='ArrowRight'){ev.preventDefault();st.move(1);}
  else if(ev.key===' '||ev.code==='Space'){ev.preventDefault();st.togglePlay();}
  else if(ev.key==='f'||ev.key==='F'){ev.preventDefault();st.toggleFilter();}
  else if(ev.key==='o'||ev.key==='O'){ev.preventDefault();st.toggleOverlays();}
});
})();"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_uses_standard_padding() {
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn escaping_covers_every_html_special_and_neutralizes_schemes() {
        let escaped = escape_text("<img src='https://evil.example/x?a=1&b=2'>");
        assert_eq!(
            escaped,
            "&lt;img src=&#39;https&#58;//evil.example/x?a=1&amp;b=2&#39;&gt;"
        );
    }

    #[test]
    fn a_case_position_is_read_from_its_directory_name() {
        assert_eq!(case_position(Path::new("0-login")), Some(0));
        assert_eq!(case_position(Path::new("12-login")), Some(12));
        assert_eq!(case_position(Path::new("login")), None);
        assert_eq!(case_position(Path::new("x-login")), None);
    }

    #[test]
    fn viewer_assets_contain_zero_cleartext_url_schemes() {
        assert!(!STYLE.contains("http://") && !STYLE.contains("https://"));
        assert!(!TRACE_STYLE.contains("http://") && !TRACE_STYLE.contains("https://"));
        assert!(!INTERACTIVE_STYLE.contains("http://") && !INTERACTIVE_STYLE.contains("https://"));
        assert!(!VIEWER_SCRIPT.contains("http://") && !VIEWER_SCRIPT.contains("https://"));
        assert!(!VIEWER_SCRIPT.contains("</script"));
    }

    #[test]
    fn svg_overlay_renders_box_reticle_and_drag_arrow_without_schemes() {
        let mut box_svg = String::new();
        render_target_bounds_svg(
            &mut box_svg,
            &json!({
                "x": 100,
                "y": 50,
                "width": 120,
                "height": 40,
                "point": { "x": 160, "y": 70 },
                "viewport": { "width": 1280, "height": 720 }
            }),
            None,
        );
        assert!(box_svg.contains("viewBox=\"0 0 1280 720\""), "{box_svg}");
        assert!(box_svg.contains("class=\"tb-box\""), "{box_svg}");
        assert!(box_svg.contains("class=\"tb-ring\""), "{box_svg}");
        assert!(!box_svg.contains("http://") && !box_svg.contains("https://"));

        let mut drag_svg = String::new();
        render_target_bounds_svg(
            &mut drag_svg,
            &json!({
                "x": 20,
                "y": 30,
                "width": 180,
                "height": 120,
                "from": { "x": 20, "y": 30 },
                "to": { "x": 200, "y": 150 },
                "viewport": { "width": 800, "height": 600 }
            }),
            None,
        );
        assert!(drag_svg.contains("class=\"tb-drag-line\""), "{drag_svg}");
        assert!(drag_svg.contains("class=\"tb-drag-arrow\""), "{drag_svg}");
        assert!(!drag_svg.contains("http://") && !drag_svg.contains("https://"));
    }

    #[test]
    fn telemetry_aggregates_tokens_cache_and_latency() {
        let mut t = ModelTelemetry::default();
        t.collect_from_step(&json!({
            "name": "model/select_ref",
            "model": "gpt-test",
            "latency_ms": 120,
            "usage": {
                "prompt_tokens": 500,
                "completion_tokens": 50,
                "total_tokens": 550,
                "prompt_tokens_details": { "cached_tokens": 200 }
            }
        }));
        t.collect_from_step(&json!({
            "name": "model/judge",
            "model": "gpt-test",
            "latency_ms": 280,
            "usage": {
                "input_tokens": 800,
                "output_tokens": 80,
                "cache_read_tokens": 300,
                "reasoning_tokens": 40
            }
        }));
        assert_eq!(t.calls, 2);
        assert_eq!(t.total_latency_ms, 400);
        assert_eq!(t.average_latency_ms(), 200);
        assert_eq!(t.max_latency_ms, 280);
        assert_eq!(t.prompt_tokens, 1300);
        assert_eq!(t.completion_tokens, 130);
        assert_eq!(t.total_tokens, 1430);
        assert_eq!(t.cached_tokens, 500);
        assert_eq!(t.reasoning_tokens, 40);
    }
}
