//! A static, self-contained HTML report over recorded case directories.
//!
//! The reader follows the failure-bundle layout: one directory per case,
//! `meta.json` written last as the completeness marker, and evidence grouped in
//! subdirectories named after the session kind. Every value that reaches the
//! document is escaped; screenshots are embedded as data URIs only when their
//! size and the sharing mode allow it.
//!
//! The module deliberately has no template engine or front-end dependency. The
//! document contains inline CSS, no script, and a Content-Security-Policy that
//! forbids every network request.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

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

/// Reads a record directory and renders its complete cases.
///
/// `shareable` suppresses every screenshot, as does a case whose `meta.json`
/// carries `"local_only": true`.
pub fn build(directory: &Path, shareable: bool) -> Result<Report, ReportError> {
    let metadata = fs::metadata(directory).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ReportError::input(format!(
                "the record directory `{}` does not exist; run `da run --record <directory>` first or pass a completed `.da/failures/<run>` directory",
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
            "no complete case records in `{}`; every case directory must contain `meta.json`, which is written last",
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

/// One complete case directory.
#[derive(Debug)]
struct CaseRecord {
    directory: PathBuf,
    meta: Value,
    report: Value,
    evidence: Vec<EvidenceFile>,
    local_only: bool,
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

/// Reads every directory with `meta.json` and ignores every incomplete one.
fn read_cases(root: &Path) -> Result<Vec<CaseRecord>, ReportError> {
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
            && path.join("meta.json").is_file()
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
        let meta = read_json(&directory.join("meta.json"))?;
        let report_path = directory.join("report.json");
        let report = read_json(&report_path)?;
        let local_only = meta
            .get("local_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let evidence = read_evidence(&directory)?;
        cases.push(CaseRecord {
            local_only,
            directory,
            meta,
            report,
            evidence,
        });
    }
    Ok(cases)
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
    let entries = fs::read_dir(case).map_err(|error| {
        ReportError::input(format!(
            "could not read the case directory `{}`: {error}",
            paths::display(case)
        ))
    })?;
    let mut groups = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            ReportError::input(format!(
                "could not read an entry in `{}`: {error}",
                paths::display(case)
            ))
        })?;
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
    html.push_str("</style>\n</head>\n<body>\n<main>\n<header class=\"hero\">\n");
    html.push_str("<p class=\"eyebrow\">da evidence report</p>\n<h1>Run evidence</h1>\n");
    if shareable {
        html.push_str(
            "<p class=\"lede\">This report is shareable: screenshots are not embedded. It is one static file with no network access.</p>\n",
        );
    } else {
        html.push_str(
            "<p class=\"lede\">A static, self-contained view of recorded cases, their steps and the evidence each step left behind. It makes no network requests.</p>\n",
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
    html.push_str("</div>\n</header>\n");

    for (index, case) in cases.iter().enumerate() {
        render_case(&mut html, index, case, shareable, budget);
    }

    html.push_str("<footer class=\"footer\">Generated by <code>da report</code>. ");
    html.push_str("Every recorded directory in this report contained <code>meta.json</code>. ");
    html.push_str(
        "Screenshots are embedded only when the size and sharing rules allow it.</footer>\n",
    );
    html.push_str("</main>\n</body>\n</html>\n");
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

fn render_case(
    html: &mut String,
    index: usize,
    case: &CaseRecord,
    shareable: bool,
    budget: &mut ImageBudget,
) {
    let status = case.status();
    let status_class = status_class(&status);
    let _ = write!(
        html,
        "<section class=\"case\" id=\"case-{}\">\n<header class=\"case-head\">\n<div class=\"status {status_class}\">{}</div>\n<div class=\"case-title\"><h2>{}</h2>\n",
        index + 1,
        escape_text(&status.to_uppercase()),
        escape_text(&case.name())
    );
    let mut chips = Vec::new();
    for field in ["project", "suite", "layer"] {
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

    if let Some(steps) = case.report.get("steps").and_then(Value::as_array) {
        if steps.is_empty() {
            html.push_str("<p class=\"note\">This case recorded no steps.</p>\n");
        } else {
            html.push_str("<div class=\"steps\">\n");
            for (position, step) in steps.iter().enumerate() {
                render_step(html, position, step);
            }
            html.push_str("</div>\n");
        }
    }

    if let Some(finally) = case.report.get("finally").and_then(Value::as_array)
        && !finally.is_empty()
    {
        html.push_str("<div class=\"steps finally\"><h3>Cleanup (finally)</h3>\n");
        for (position, step) in finally.iter().enumerate() {
            render_step(html, position, step);
        }
        html.push_str("</div>\n");
    }

    render_case_evidence(html, case, shareable, budget);
    html.push_str("</section>\n");
}

fn render_step(html: &mut String, position: usize, step: &Value) {
    let status = step
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let name = step.get("name").and_then(Value::as_str).unwrap_or("step");
    let duration = step.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
    let _ = write!(
        html,
        "<article class=\"step {}\">\n<header class=\"step-head\">\n<span class=\"step-index\">#{}</span><h3>{}</h3><span class=\"status {}\">{}</span><span class=\"duration\">{} ms</span>\n</header>\n",
        status_class(status),
        position + 1,
        escape_text(name),
        status_class(status),
        escape_text(&status.to_uppercase()),
        duration
    );
    if let Some(problem) = step.get("error").and_then(Value::as_object) {
        html.push_str("<p class=\"problem\">");
        html.push_str(&escape_text(&problem_text(problem)));
        html.push_str("</p>\n");
    }
    if let Some(assertions) = step.get("assertions").and_then(Value::as_array) {
        render_assertions(html, assertions);
    }
    if let Some(result) = step.get("result") {
        html.push_str("<div class=\"layers\">\n");
        render_http(html, result);
        render_db(html, result);
        render_cli(html, result);
        render_observations(html, result);
        html.push_str("</div>\n");
        html.push_str("<details class=\"raw\"><summary>Raw step result</summary><pre>");
        html.push_str(&escape_text(&pretty(result)));
        html.push_str("</pre></details>\n");
    }
    html.push_str("</article>\n");
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

fn render_case_evidence(
    html: &mut String,
    case: &CaseRecord,
    shareable: bool,
    budget: &mut ImageBudget,
) {
    let images: Vec<&EvidenceFile> = case
        .evidence
        .iter()
        .filter(|file| matches!(file.kind, EvidenceKind::Image(_)))
        .collect();
    if !images.is_empty() {
        html.push_str(
            "<div class=\"case-evidence\"><h3>Interface evidence</h3><div class=\"screenshots\">",
        );
        for image in images {
            let reason = if shareable {
                Some(
                    "the report is shareable (`--shareable`), so screenshots are not embedded"
                        .to_string(),
                )
            } else if case.local_only {
                Some(
                    "meta.json marks this case `local_only: true`, so its screenshot is not embedded"
                        .to_string(),
                )
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
                            let _ = write!(
                                html,
                                "<figure><img src=\"data:{mime};base64,{}\" alt=\"{label}\"><figcaption>{label} · {} bytes</figcaption></figure>",
                                base64_encode(&bytes),
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

    let doms: Vec<&EvidenceFile> = case
        .evidence
        .iter()
        .filter(|file| file.kind == EvidenceKind::Dom)
        .collect();
    if !doms.is_empty() {
        html.push_str("<div class=\"case-evidence\"><h3>DOM snapshots</h3>");
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

    let texts: Vec<&EvidenceFile> = case
        .evidence
        .iter()
        .filter(|file| file.kind == EvidenceKind::Text)
        .collect();
    if !texts.is_empty() {
        html.push_str("<div class=\"case-evidence\"><h3>Other text evidence</h3>");
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

    let binaries: Vec<&EvidenceFile> = case
        .evidence
        .iter()
        .filter(|file| file.kind == EvidenceKind::Binary)
        .collect();
    if !binaries.is_empty() {
        html.push_str(
            "<div class=\"case-evidence\"><h3>Other binary evidence</h3><ul class=\"binary-list\">",
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

const STYLE: &str = r#":root{color-scheme:light;--paper:#faf7f0;--panel:#fff;--soft:#f7f3ea;--ink:#302c27;--muted:#756d62;--line:#ddd4c7;--accent:#b0713c;--pass:#356346;--fail:#9a3f3f;--error:#8a542c;--blue:#3f6485}*{box-sizing:border-box}body{margin:0;background:var(--paper);color:var(--ink);font:15px/1.55 system-ui,-apple-system,Segoe UI,sans-serif}main{max-width:none;margin:0 auto;padding:0 clamp(16px,2.4vw,32px) 64px}code{font-size:.92em;background:#efe9dd;padding:.04rem .25rem;border-radius:3px}pre{overflow:auto;background:var(--soft);border:1px solid var(--line);padding:.8rem;white-space:pre-wrap;word-break:break-word;font:12.5px/1.5 ui-monospace,SFMono-Regular,Consolas,monospace}h1,h2,h3,h4,h5{margin:0;line-height:1.2}.hero{padding:2.4rem 0 1.4rem;border-bottom:1px solid var(--line)}.eyebrow{text-transform:uppercase;letter-spacing:.16em;font-size:.75rem;color:var(--accent);font-weight:700;margin:0}.hero h1{font:700 clamp(2.2rem,4vw,4.2rem)/1.02 Georgia,serif;margin:.3rem 0}.lede{max-width:85ch;color:var(--muted);margin:.4rem 0 1.2rem}.metrics{display:grid;grid-template-columns:repeat(6,minmax(0,1fr));gap:10px}.metric{background:var(--panel);border:1px solid var(--line);padding:.7rem .8rem}.metric strong{display:block;font:700 1.8rem/1 Georgia,serif;color:var(--blue)}.metric span{font-size:.82rem;color:var(--muted)}.case{padding:2rem 0;border-bottom:1px solid var(--line)}.case-head{display:flex;gap:1rem;align-items:flex-start}.case-title{min-width:0}.case-title h2{font:700 1.6rem/1.2 Georgia,serif;overflow-wrap:anywhere}.status{display:inline-block;border:1px solid var(--line);border-radius:999px;padding:.2rem .55rem;font-size:.76rem;font-weight:700;letter-spacing:.04em;background:var(--soft);white-space:nowrap}.status.passed{color:var(--pass);border-color:#8fb59b;background:#e5f0e7}.status.failed{color:var(--fail);border-color:#c58d8d;background:#f3e2e2}.status.error{color:var(--error);border-color:#d0a478;background:#f3e3d5}.status.skipped{color:var(--muted);border-color:#bbb1a2;background:#f0ece4}.step.skipped{border-style:dashed}.chips{display:flex;gap:.35rem;flex-wrap:wrap;margin:.55rem 0 0}.chips span{border:1px solid var(--line);background:var(--soft);border-radius:999px;padding:.08rem .45rem;font-size:.76rem;color:var(--muted)}.claim{font-weight:700}.steps{margin-top:1.2rem}.step{border:1px solid var(--line);background:var(--panel);margin:1rem 0;padding:1rem}.step-head{display:flex;gap:.6rem;align-items:center;flex-wrap:wrap;border-bottom:1px solid var(--line);padding-bottom:.55rem}.step-head h3{font-size:1.05rem;overflow-wrap:anywhere}.step-index{color:var(--muted);font:700 .9rem ui-monospace,Consolas,monospace}.duration{color:var(--muted);font-size:.82rem;margin-left:auto}.problem{border-left:4px solid var(--fail);background:#f7eaea;padding:.55rem .7rem;margin:.7rem 0;overflow-wrap:anywhere}.layers{display:grid;grid-template-columns:repeat(auto-fit,minmax(320px,1fr));gap:.8rem;margin-top:.8rem}.card{border:1px solid var(--line);background:var(--soft);padding:.8rem;margin:.7rem 0;min-width:0}.card h4{font-size:.98rem;color:var(--blue);margin-bottom:.45rem}.card h5{margin:.55rem 0 .2rem;font-size:.8rem;color:var(--muted);text-transform:uppercase;letter-spacing:.05em}.note{color:var(--muted);font-size:.8rem;margin:.2rem 0}.status-line{margin:.2rem 0 .4rem}.table-wrap{overflow:auto}table{width:100%;border-collapse:collapse;font-size:.82rem}th,td{text-align:left;vertical-align:top;border-bottom:1px solid var(--line);padding:.35rem .45rem;overflow-wrap:anywhere}th{color:var(--blue);font-size:.76rem;text-transform:uppercase;letter-spacing:.03em}td code{white-space:pre-wrap;word-break:break-word}.assert{font-weight:700;font-size:.76rem}.assert.pass{color:var(--pass)}.assert.fail{color:var(--fail)}.raw,.card details{margin-top:.55rem}.raw summary,.card summary{cursor:pointer;color:var(--blue);font-size:.85rem}.case-evidence{margin-top:1.1rem}.case-evidence>h3{font:700 1.15rem/1.2 Georgia,serif;margin:.3rem 0}.screenshots{display:grid;grid-template-columns:repeat(auto-fit,minmax(280px,1fr));gap:.8rem}.screenshots figure{margin:0;border:1px solid var(--line);background:var(--panel);padding:.55rem}.screenshots img{display:block;max-width:100%;height:auto;background:#fff}.screenshots figcaption{font-size:.8rem;color:var(--muted);margin-top:.35rem;overflow-wrap:anywhere}.omission{background:#f3e3d5;border-left:4px solid var(--accent);padding:.55rem .7rem;margin:.45rem 0;overflow-wrap:anywhere}.binary-list{font-size:.85rem;color:var(--muted)}.footer{padding:1.6rem 0;color:var(--muted);font-size:.82rem}@media(max-width:760px){.metrics{grid-template-columns:repeat(2,minmax(0,1fr))}.case-head{display:block}.case-head .status{margin-bottom:.35rem}.duration{margin-left:0}}"#;

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
}
