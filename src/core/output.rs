//! Renders run outcomes for people and for machines.
//!
//! This is the only place the JSON report is built, and the only place result
//! text is formatted, so the two views cannot drift apart.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::core::assert::AssertionReport;
use crate::core::case::{CaseError, CaseWarning, LoadedCase};
use crate::core::paths;
use crate::core::repeat::{RepeatOutcome, RepeatReport};
use crate::core::report::{
    CaseReport, CaseStatus, Problem, RunOutcome, StepReport, StepStatus, Summary,
};
use crate::core::secrets::Secrets;
use crate::core::strength::StrengthFinding;

/// How results are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// One line per case, assertion details on failure, then a summary line.
    Text,
    /// Exactly one JSON object.
    Json,
}

/// The text `da` writes to standard output and standard error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    /// Results.
    pub stdout: String,
    /// Diagnostics.
    pub stderr: String,
}

/// Counts and size of one rendered HTML report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportSummary {
    /// Complete case records the report contains.
    pub cases: usize,
    /// Cases that passed.
    pub passed: usize,
    /// Cases that failed an assertion.
    pub failed: usize,
    /// Cases that could not run to completion.
    pub errored: usize,
    /// Steps across every case.
    pub steps: usize,
    /// Steps that were skipped, including optional steps that did not finish.
    pub steps_skipped: usize,
    /// Screenshots embedded as data URIs.
    pub images_embedded: usize,
    /// Screenshots left out, with a reason in the report.
    pub images_omitted: usize,
    /// Size of the generated HTML in bytes.
    pub bytes: usize,
}

/// Renders the result of `da report`.
pub fn render_report(summary: &ReportSummary, path: &std::path::Path, format: Format) -> Rendered {
    match format {
        Format::Json => Rendered {
            stdout: format!(
                "{}\n",
                json!({
                    "ok": true,
                    "report": paths::display(path),
                    "cases": summary.cases,
                    "passed": summary.passed,
                    "failed": summary.failed,
                    "errors": summary.errored,
                    "steps": summary.steps,
                    "steps_skipped": summary.steps_skipped,
                    "images_embedded": summary.images_embedded,
                    "images_omitted": summary.images_omitted,
                    "bytes": summary.bytes,
                })
            ),
            stderr: String::new(),
        },
        Format::Text => Rendered {
            stdout: format!(
                "Report: {}\nCases: {} {} ({} passed, {} failed, {} errors), {} {} ({} skipped); images: {} embedded, {} omitted; {} bytes.\n",
                paths::display(path),
                summary.cases,
                noun(summary.cases, "case", "cases"),
                summary.passed,
                summary.failed,
                summary.errored,
                summary.steps,
                noun(summary.steps, "step", "steps"),
                summary.steps_skipped,
                summary.images_embedded,
                summary.images_omitted,
                summary.bytes,
            ),
            stderr: String::new(),
        },
    }
}

/// What `da lint` checked and what it found.
#[derive(Debug, Clone, Default)]
pub struct LintReport {
    /// Number of case files the paths expanded to.
    pub files: usize,
    /// Every problem found, across all files.
    pub errors: Vec<CaseError>,
    /// Every valid case that relies on something fragile.
    ///
    /// A warning never changes the verdict: lint still exits 0 for a case that
    /// only warns.
    pub warnings: Vec<CaseWarning>,
    /// Every case that parsed, so a reader can discover the classification
    /// values without running anything.
    pub cases: Vec<LintCase>,
    /// Optional baseline path when `--against` was supplied.
    pub against: Option<PathBuf>,
    /// Assertion-strength reductions found relative to `against`.
    pub strength_findings: Vec<StrengthFinding>,
    /// Non-empty reason supplied via `--allow-weaken`, when given.
    pub acknowledged_reason: Option<String>,
}

/// One parsed case of a lint report.
#[derive(Debug, Clone, Default)]
pub struct LintCase {
    /// File the case came from.
    pub file: PathBuf,
    /// Case name.
    pub name: String,
    /// Project the case declares.
    pub project: Option<String>,
    /// Suite the case declares.
    pub suite: Option<String>,
    /// Layer the case declares.
    pub layer: Option<String>,
    /// Tags the case declares.
    pub tags: Vec<String>,
}

impl LintCase {
    /// Copies the classification of a loaded case.
    pub fn new(loaded: &LoadedCase) -> Self {
        Self {
            file: loaded.path.clone(),
            name: loaded.case.name.clone(),
            project: loaded.case.project.clone(),
            suite: loaded.case.suite.clone(),
            layer: loaded.case.layer.clone(),
            tags: loaded.case.tags.clone(),
        }
    }
}

impl LintReport {
    /// Whether every checked case would pass the runner's preflight and any
    /// active `--against` strength gate.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
            && (self.strength_findings.is_empty() || self.acknowledged_reason.is_some())
    }
}

/// Renders a lint result.
pub fn render_lint(report: &LintReport, format: Format, secrets: &Secrets) -> Rendered {
    match format {
        Format::Json => Rendered {
            stdout: format!("{}\n", secrets.redact_json(&json_lint(report))),
            stderr: String::new(),
        },
        Format::Text => text_lint(report, secrets),
    }
}

fn text_lint(report: &LintReport, secrets: &Secrets) -> Rendered {
    let mut stdout = String::new();
    for error in &report.errors {
        // The parser's message carries a source snippet with its own line
        // breaks. Text output keeps it as written: the caret line is what makes
        // the reported location readable, and every error still starts with its
        // file name at the beginning of a line.
        stdout.push_str(&format!(
            "{}: {}\n",
            paths::display(&error.file),
            error.message
        ));
    }
    if report.acknowledged_reason.is_none() {
        for finding in &report.strength_findings {
            stdout.push_str(&format!(
                "{}: strength [{}]: {}\n",
                paths::display(&finding.file),
                finding.kind,
                finding.message
            ));
        }
    }
    let total_warnings = if report.acknowledged_reason.is_some() {
        report.warnings.len() + report.strength_findings.len()
    } else {
        report.warnings.len()
    };
    let unacknowledged_strength = if report.acknowledged_reason.is_none() {
        report.strength_findings.len()
    } else {
        0
    };

    let mut counts = format!(
        "{} {}",
        report.errors.len(),
        noun(report.errors.len(), "error", "errors")
    );
    if total_warnings > 0 {
        counts.push_str(&format!(
            ", {} {}",
            total_warnings,
            noun(total_warnings, "warning", "warnings")
        ));
    }
    if unacknowledged_strength > 0 {
        counts.push_str(&format!(
            ", {} {}",
            unacknowledged_strength,
            noun(
                unacknowledged_strength,
                "strength reduction",
                "strength reductions"
            )
        ));
    }
    stdout.push_str(&format!(
        "Checked {} {}: {counts}.\n",
        report.files,
        noun(report.files, "file", "files"),
    ));

    // Warnings leave the process on standard error, leaving standard output as
    // the result a caller reads. They still name file and location so a reader
    // can find the step that triggered them.
    let mut stderr = String::new();
    for warning in &report.warnings {
        stderr.push_str(&format!(
            "{}: warning: {}\n",
            paths::display(&warning.file),
            warning.message
        ));
    }
    if let Some(reason) = &report.acknowledged_reason {
        for finding in &report.strength_findings {
            stderr.push_str(&format!(
                "{}: warning [acknowledged: {reason}] [{}]: {}\n",
                paths::display(&finding.file),
                finding.kind,
                finding.message
            ));
        }
    }
    Rendered {
        stdout: secrets.redact(&stdout),
        stderr: secrets.redact(&stderr),
    }
}

fn json_lint(report: &LintReport) -> Value {
    let mut value = json!({
        "ok": report.is_ok(),
        "files": report.files,
        "errors": report
            .errors
            .iter()
            .map(|error| {
                json!({
                    "file": paths::display(&error.file),
                    "message": error.message,
                })
            })
            .collect::<Vec<Value>>(),
        "cases": report
            .cases
            .iter()
            .map(|case| {
                json!({
                    "file": paths::display(&case.file),
                    "name": case.name,
                    "project": case.project,
                    "suite": case.suite,
                    "layer": case.layer,
                    "tags": case.tags,
                })
            })
            .collect::<Vec<Value>>(),
    });
    let mut warning_items: Vec<Value> = report
        .warnings
        .iter()
        .map(|warning| {
            json!({
                "file": paths::display(&warning.file),
                "message": warning.message,
            })
        })
        .collect();
    if let Some(reason) = &report.acknowledged_reason {
        for finding in &report.strength_findings {
            warning_items.push(json!({
                "file": paths::display(&finding.file),
                "baseline_file": paths::display(&finding.baseline_file),
                "case_name": finding.case_name,
                "kind": finding.kind,
                "message": finding.message,
                "acknowledged": true,
                "acknowledged_reason": reason,
            }));
        }
    }
    // No warnings means no `warnings` key: a case that never warned keeps
    // exactly the object earlier releases wrote.
    if !warning_items.is_empty()
        && let Some(entries) = value.as_object_mut()
    {
        entries.insert("warnings".to_string(), Value::Array(warning_items));
    }
    if let Some(against) = &report.against
        && let Some(entries) = value.as_object_mut()
    {
        entries.insert("against".to_string(), json!(paths::display(against)));
        entries.insert(
            "strength_findings".to_string(),
            Value::Array(
                report
                    .strength_findings
                    .iter()
                    .map(|finding| {
                        let mut item = Map::new();
                        item.insert("file".to_string(), json!(paths::display(&finding.file)));
                        item.insert(
                            "baseline_file".to_string(),
                            json!(paths::display(&finding.baseline_file)),
                        );
                        item.insert("case_name".to_string(), json!(finding.case_name));
                        item.insert("kind".to_string(), json!(finding.kind));
                        item.insert("message".to_string(), json!(finding.message));
                        item.insert("acknowledged".to_string(), json!(finding.acknowledged));
                        if let Some(reason) = &report.acknowledged_reason {
                            item.insert("acknowledged_reason".to_string(), json!(reason));
                        }
                        Value::Object(item)
                    })
                    .collect(),
            ),
        );
        if let Some(reason) = &report.acknowledged_reason {
            entries.insert("acknowledged_reason".to_string(), json!(reason));
        }
    }
    value
}

/// Renders a JSON Schema value for standard output.
pub fn render_schema(schema: &Value, format: Format) -> Rendered {
    let text = match format {
        Format::Json => schema.to_string(),
        // A person reads this mode, so make the nesting visible. Serializing a
        // `Value` cannot fail in practice; the compact form is a safe fallback.
        Format::Text => serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string()),
    };
    Rendered {
        stdout: format!("{text}\n"),
        stderr: String::new(),
    }
}

/// Renders an outcome in the requested format.
///
/// Every string that leaves the process goes through `secrets` here, so a
/// value that came from the environment cannot reach the terminal even when a
/// step, an assertion or an error message carries it. A failure bundle applies
/// the same redaction before it writes its files.
pub fn render(outcome: &RunOutcome, format: Format, secrets: &Secrets) -> Rendered {
    match format {
        Format::Json => Rendered {
            stdout: format!("{}\n", secrets.redact_json(&json_report(outcome))),
            // Diagnostics never go to standard output, so a bundle that could
            // not be written is repeated on standard error in both formats.
            stderr: match outcome {
                RunOutcome::Completed(report) => secrets.redact(&bundle_errors(report)),
                RunOutcome::Rejected { .. } => String::new(),
                RunOutcome::NoMatch { message } => format!("error: {}\n", secrets.redact(message)),
            },
        },
        Format::Text => text_report(outcome, secrets),
    }
}

fn text_report(outcome: &RunOutcome, secrets: &Secrets) -> Rendered {
    let report = match outcome {
        RunOutcome::Completed(report) => report,
        RunOutcome::NoMatch { message } => {
            return Rendered {
                stdout: String::new(),
                stderr: format!("error: {}\n", secrets.redact(message)),
            };
        }
        RunOutcome::Rejected { errors } => {
            let mut stderr = String::new();
            for error in errors {
                stderr.push_str(&format!(
                    "error: {}: {}\n",
                    paths::display(&error.file),
                    error.message
                ));
            }
            return Rendered {
                stdout: String::new(),
                stderr: secrets.redact(&stderr),
            };
        }
    };

    let mut stdout = String::new();
    for case in &report.cases {
        stdout.push_str(&format!(
            "{} {} [{}] {} ms\n",
            label(case.status),
            paths::display(&case.file),
            case.name,
            millis(case.duration)
        ));
        for step in &case.steps {
            render_step_text(&mut stdout, step, false);
        }
        for step in &case.finally {
            render_step_text(&mut stdout, step, true);
        }
        for close_error in &case.close_errors {
            stdout.push_str(&format!("  close: {close_error}\n"));
        }
        if let Some(bundle) = &case.bundle {
            stdout.push_str(&format!("  bundle: {}\n", paths::display(bundle)));
        }
        if case.local_only {
            let reason = case
                .local_only_reason
                .as_deref()
                .unwrap_or("session holds unmanaged external state");
            stdout.push_str(&format!(
                "  local-only: evidence is kept local only ({reason})\n"
            ));
        }
    }

    let summary = report.summary();
    stdout.push_str(&format!(
        "Summary: {} {} ({} passed, {} failed, {} errors, {} filtered), {} {} ({} passed, {} failed, {} errors, {} skipped), {} ms\n",
        summary.cases,
        noun(summary.cases, "case", "cases"),
        summary.cases_passed,
        summary.cases_failed,
        summary.cases_errored,
        summary.filtered,
        summary.steps,
        noun(summary.steps, "step", "steps"),
        summary.steps_passed,
        summary.steps_failed,
        summary.steps_errored,
        summary.steps_skipped,
        millis(report.duration),
    ));

    Rendered {
        stdout: secrets.redact(&stdout),
        stderr: secrets.redact(&bundle_errors(report)),
    }
}

/// Renders one step's diagnostic lines (its problem and any failed assertions).
fn render_step_text(stdout: &mut String, step: &StepReport, is_finally: bool) {
    if let Some(problem) = &step.error {
        // A skipped step keeps the problem that skipped it. The note says the
        // case went on, so a reader does not take the line for a failure of the
        // run. A step that never ran has no problem and still prints nothing.
        let prefix = if is_finally { "finally " } else { "" };
        let note = if step.status == StepStatus::Skipped {
            " (optional)"
        } else {
            ""
        };
        stdout.push_str(&format!(
            "  {prefix}{}{note}: {}\n",
            step.name, problem.message
        ));
    }
    for assertion in &step.assertions {
        if !assertion.passed {
            stdout.push_str(&format!("    {}\n", describe(assertion)));
        }
    }
}

/// Renders the bundles that could not be written as diagnostics.
fn bundle_errors(report: &crate::core::report::RunReport) -> String {
    let mut stderr = String::new();
    for error in &report.bundle_errors {
        stderr.push_str(&format!("error: {error}\n"));
    }
    stderr
}

/// Picks the singular or plural form for a count.
fn noun(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

fn label(status: CaseStatus) -> &'static str {
    match status {
        CaseStatus::Passed => "PASS",
        CaseStatus::Failed => "FAIL",
        CaseStatus::Error => "ERROR",
    }
}

fn describe(assertion: &AssertionReport) -> String {
    let actual = assertion
        .actual
        .as_ref()
        .map_or_else(|| "<missing>".to_string(), compact);
    let mut line = format!(
        "{} {} {} (actual {})",
        assertion.path,
        assertion.comparison,
        compact(&assertion.expected),
        actual
    );
    if let Some(message) = &assertion.message {
        line.push_str(": ");
        line.push_str(message);
    }
    line
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_string())
}

fn json_report(outcome: &RunOutcome) -> Value {
    let mut object = Map::new();
    object.insert("ok".to_string(), Value::Bool(outcome.is_ok()));

    let (summary, duration, bundle_errors) = match outcome {
        RunOutcome::Completed(report) => (
            report.summary(),
            report.duration,
            report.bundle_errors.as_slice(),
        ),
        RunOutcome::Rejected { .. } | RunOutcome::NoMatch { .. } => {
            (Summary::default(), std::time::Duration::ZERO, &[][..])
        }
    };
    object.insert("summary".to_string(), summary_value(&summary, duration));
    object.insert(
        "bundle_errors".to_string(),
        Value::Array(
            bundle_errors
                .iter()
                .map(|error| Value::String(error.clone()))
                .collect(),
        ),
    );

    match outcome {
        RunOutcome::Completed(report) => {
            object.insert(
                "cases".to_string(),
                Value::Array(report.cases.iter().map(case_json).collect()),
            );
            object.insert("errors".to_string(), Value::Array(Vec::new()));
        }
        RunOutcome::Rejected { errors } => {
            object.insert("cases".to_string(), Value::Array(Vec::new()));
            object.insert(
                "errors".to_string(),
                Value::Array(
                    errors
                        .iter()
                        .map(|error| {
                            json!({
                                "file": paths::display(&error.file),
                                "message": error.message,
                                "kind": error.kind.as_str(),
                            })
                        })
                        .collect(),
                ),
            );
        }
        RunOutcome::NoMatch { message } => {
            object.insert("cases".to_string(), Value::Array(Vec::new()));
            object.insert("errors".to_string(), Value::Array(Vec::new()));
            // The filters are valid but selected nothing, so this is neither a
            // case error nor a test failure; the message says what to change.
            object.insert("filter_error".to_string(), Value::String(message.clone()));
        }
    }

    Value::Object(object)
}

fn summary_value(summary: &Summary, duration: std::time::Duration) -> Value {
    json!({
        "cases": summary.cases,
        "filtered": summary.filtered,
        "cases_passed": summary.cases_passed,
        "cases_failed": summary.cases_failed,
        "cases_errored": summary.cases_errored,
        "steps": summary.steps,
        "steps_passed": summary.steps_passed,
        "steps_failed": summary.steps_failed,
        "steps_errored": summary.steps_errored,
        "steps_skipped": summary.steps_skipped,
        "duration_ms": millis(duration),
    })
}

/// Renders one case the way `--output json` shows it.
///
/// A failure bundle uses this for its `report.json`, so the bundle and the
/// command output cannot describe the same case differently.
pub fn case_json(case: &CaseReport) -> Value {
    let mut object = Map::new();
    object.insert("file".to_string(), json!(paths::display(&case.file)));
    object.insert("name".to_string(), json!(case.name));
    object.insert("project".to_string(), json!(case.project));
    object.insert("suite".to_string(), json!(case.suite));
    object.insert("layer".to_string(), json!(case.layer));
    object.insert("tags".to_string(), json!(case.tags));
    object.insert("status".to_string(), json!(case.status.as_str()));
    object.insert("duration_ms".to_string(), json!(millis(case.duration)));
    object.insert(
        "bundle".to_string(),
        json!(case.bundle.as_ref().map(|path| paths::display(path))),
    );
    object.insert(
        "steps".to_string(),
        Value::Array(case.steps.iter().map(step_value).collect()),
    );
    if !case.finally.is_empty() {
        object.insert(
            "finally".to_string(),
            Value::Array(case.finally.iter().map(step_value).collect()),
        );
    }
    object.insert(
        "error".to_string(),
        json!(case.error.as_ref().map(problem_value)),
    );
    object.insert("close_errors".to_string(), json!(case.close_errors));
    if case.local_only {
        object.insert("local_only".to_string(), json!(true));
        if let Some(reason) = &case.local_only_reason {
            object.insert("local_only_reason".to_string(), json!(reason));
        }
    }
    Value::Object(object)
}

/// Renders one step report the way `report.json` shows it.
pub fn step_json(step: &StepReport) -> Value {
    step_value(step)
}

fn step_value(step: &StepReport) -> Value {
    json!({
        "name": step.name,
        "status": step.status.as_str(),
        "duration_ms": millis(step.duration),
        "args": step.args,
        "result": step.result,
        "saved": step.saved,
        "assertions": step.assertions.iter().map(assertion_value).collect::<Vec<Value>>(),
        "error": step.error.as_ref().map(problem_value),
    })
}

fn assertion_value(assertion: &AssertionReport) -> Value {
    json!({
        "path": assertion.path,
        "comparison": assertion.comparison,
        "expected": assertion.expected,
        "actual": assertion.actual,
        "passed": assertion.passed,
        "message": assertion.message,
    })
}

fn problem_value(problem: &Problem) -> Value {
    json!({
        "kind": problem.kind.as_str(),
        "message": problem.message,
    })
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Classification of one case between a baseline and candidate `--record` run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareClassification {
    /// Passed in baseline and failed, errored, or timed out in candidate.
    Regressed,
    /// Failed, errored, or timed out in baseline and passed in candidate.
    Fixed,
    /// Did not pass in either baseline or candidate.
    StillFailing,
    /// Passed in both baseline and candidate.
    StillPassing,
    /// Absent in baseline and present in candidate.
    Added,
    /// Present in baseline and absent in candidate.
    Removed,
}

impl CompareClassification {
    /// Machine-readable string used in JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Regressed => "regressed",
            Self::Fixed => "fixed",
            Self::StillFailing => "still_failing",
            Self::StillPassing => "still_passing",
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }

    /// Uppercase prefix used in human-readable text output.
    pub fn label(self) -> &'static str {
        match self {
            Self::Regressed => "REGRESSED",
            Self::Fixed => "FIXED",
            Self::StillFailing => "STILL_FAILING",
            Self::StillPassing => "STILL_PASSING",
            Self::Added => "ADDED",
            Self::Removed => "REMOVED",
        }
    }
}

/// Step status counts for one recorded case execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepCounts {
    /// Total steps across `steps` and `finally`.
    pub total: usize,
    /// Steps with `"passed"` status.
    pub passed: usize,
    /// Steps with `"failed"` status.
    pub failed: usize,
    /// Steps with `"error"` status.
    pub errored: usize,
    /// Steps with `"skipped"` status.
    pub skipped: usize,
}

/// Assertion outcome counts for one recorded case or step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssertionCounts {
    /// Total evaluated assertions.
    pub total: usize,
    /// Assertions that passed.
    pub passed: usize,
    /// Assertions that failed.
    pub failed: usize,
}

/// Per-step difference between baseline and candidate executions of a case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepDiff {
    /// `"steps"` or `"finally"`.
    pub phase: &'static str,
    /// 1-based step number within `phase`.
    pub step_number: usize,
    /// Step name in baseline, when the step existed there.
    pub baseline_name: Option<String>,
    /// Step name in candidate, when the step existed there.
    pub candidate_name: Option<String>,
    /// Step status in baseline, when present.
    pub baseline_status: Option<String>,
    /// Step status in candidate, when present.
    pub candidate_status: Option<String>,
    /// Step duration in baseline (`ms`), when present.
    pub baseline_duration_ms: Option<u64>,
    /// Step duration in candidate (`ms`), when present.
    pub candidate_duration_ms: Option<u64>,
    /// Signed candidate minus baseline step duration (`ms`), when both exist.
    pub duration_delta_ms: Option<i64>,
    /// Assertion counts in baseline for this step.
    pub baseline_assertions: Option<AssertionCounts>,
    /// Assertion counts in candidate for this step.
    pub candidate_assertions: Option<AssertionCounts>,
    /// Candidate step error message, when present.
    pub candidate_error: Option<String>,
}

/// Comparison result for one paired, added, or removed case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseComparison {
    /// Normalized `/` case file path.
    pub case_file: String,
    /// Case name.
    pub case_name: String,
    /// Outcome transition classification.
    pub classification: CompareClassification,
    /// Baseline status (`"passed"`, `"failed"`, `"error"`, `"timeout"`), when present.
    pub baseline_status: Option<String>,
    /// Candidate status, when present.
    pub candidate_status: Option<String>,
    /// Baseline bundle directory path, when present.
    pub baseline_bundle: Option<PathBuf>,
    /// Candidate bundle directory path, when present.
    pub candidate_bundle: Option<PathBuf>,
    /// Baseline `case_sha256` from `meta.json`, when present.
    pub baseline_sha256: Option<String>,
    /// Candidate `case_sha256` from `meta.json`, when present.
    pub candidate_sha256: Option<String>,
    /// `true` when paired `case_sha256` digests both exist and differ.
    pub case_changed: bool,
    /// Baseline case duration in milliseconds, when present.
    pub baseline_duration_ms: Option<u64>,
    /// Candidate case duration in milliseconds, when present.
    pub candidate_duration_ms: Option<u64>,
    /// Signed candidate minus baseline duration (`ms`), when paired.
    pub duration_delta_ms: Option<i64>,
    /// Baseline step counts, when present.
    pub baseline_steps: Option<StepCounts>,
    /// Candidate step counts, when present.
    pub candidate_steps: Option<StepCounts>,
    /// Baseline assertion counts, when present.
    pub baseline_assertions: Option<AssertionCounts>,
    /// Candidate assertion counts, when present.
    pub candidate_assertions: Option<AssertionCounts>,
    /// Individual steps whose presence, name, status, or assertion counts changed.
    pub step_diffs: Vec<StepDiff>,
}

/// Aggregate summary of a `da compare` invocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompareSummary {
    /// Total compared cases (paired + added + removed).
    pub total: usize,
    /// Cases classified as `regressed`.
    pub regressed: usize,
    /// Cases classified as `fixed`.
    pub fixed: usize,
    /// Cases classified as `still_failing`.
    pub still_failing: usize,
    /// Cases classified as `still_passing`.
    pub still_passing: usize,
    /// Cases classified as `added`.
    pub added: usize,
    /// Cases classified as `removed`.
    pub removed: usize,
    /// Paired cases whose `case_sha256` changed.
    pub case_changed: usize,
    /// Sum of baseline case durations (`ms`).
    pub baseline_duration_ms: u64,
    /// Sum of candidate case durations (`ms`).
    pub candidate_duration_ms: u64,
    /// Signed candidate minus baseline total duration (`ms`).
    pub duration_delta_ms: i64,
}

/// Complete comparison between two `--record` directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareReport {
    /// Baseline record directory.
    pub baseline_dir: PathBuf,
    /// Candidate record directory.
    pub candidate_dir: PathBuf,
    /// Aggregate classification counts.
    pub summary: CompareSummary,
    /// Deterministic per-case comparisons.
    pub cases: Vec<CaseComparison>,
}

impl CompareReport {
    /// Returns `true` when zero cases regressed.
    pub fn is_ok(&self) -> bool {
        self.summary.regressed == 0
    }
}

/// Renders a completed `da compare` report.
pub fn render_compare(report: &CompareReport, format: Format) -> Rendered {
    match format {
        Format::Json => Rendered {
            stdout: format!("{}\n", json_compare(report)),
            stderr: String::new(),
        },
        Format::Text => Rendered {
            stdout: text_compare(report),
            stderr: String::new(),
        },
    }
}

/// Renders an input/validation error from `da compare` in text or JSON mode.
pub fn render_compare_error(kind: &'static str, message: &str, format: Format) -> Rendered {
    match format {
        Format::Json => Rendered {
            stdout: format!(
                "{}\n",
                json!({
                    "ok": false,
                    "error": {
                        "kind": kind,
                        "message": message,
                    }
                })
            ),
            stderr: format!("error: {message}\n"),
        },
        Format::Text => Rendered {
            stdout: String::new(),
            stderr: format!("error: {message}\n"),
        },
    }
}

fn text_compare(report: &CompareReport) -> String {
    let mut stdout = String::new();
    for case in &report.cases {
        let transition = match (&case.baseline_status, &case.candidate_status) {
            (Some(base), Some(cand)) => format!("{base} -> {cand}"),
            (None, Some(cand)) => format!("none -> {cand}"),
            (Some(base), None) => format!("{base} -> none"),
            (None, None) => "none -> none".to_string(),
        };
        let mut notes = Vec::new();
        if let Some(delta) = case.duration_delta_ms {
            notes.push(format!("{delta:+} ms"));
        } else if let Some(cand_ms) = case.candidate_duration_ms {
            notes.push(format!("{cand_ms} ms"));
        } else if let Some(base_ms) = case.baseline_duration_ms {
            notes.push(format!("{base_ms} ms"));
        }
        if case.case_changed {
            notes.push("case_changed".to_string());
        }
        let suffix = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join(", "))
        };
        stdout.push_str(&format!(
            "{} {} [{}] {transition}{suffix}\n",
            case.classification.label(),
            case.case_file,
            case.case_name
        ));
        for diff in &case.step_diffs {
            let phase_prefix = if diff.phase == "finally" {
                "finally step"
            } else {
                "step"
            };
            let name = diff
                .candidate_name
                .as_deref()
                .or(diff.baseline_name.as_deref())
                .unwrap_or("step");
            let base_status = diff.baseline_status.as_deref().unwrap_or("absent");
            let cand_status = diff.candidate_status.as_deref().unwrap_or("absent");
            let mut line = format!(
                "  {phase_prefix} {} {name}: {base_status} -> {cand_status}",
                diff.step_number
            );
            if let (Some(ba), Some(ca)) = (diff.baseline_assertions, diff.candidate_assertions)
                && (ba != ca)
            {
                line.push_str(&format!(
                    " (assertions {}/{} -> {}/{})",
                    ba.passed, ba.total, ca.passed, ca.total
                ));
            }
            if let Some(err) = &diff.candidate_error {
                line.push_str(": ");
                line.push_str(err);
            }
            line.push('\n');
            stdout.push_str(&line);
        }
        if matches!(
            case.classification,
            CompareClassification::Regressed | CompareClassification::StillFailing
        ) && let Some(bundle) = &case.candidate_bundle
        {
            stdout.push_str(&format!("  candidate bundle: {}\n", paths::display(bundle)));
        }
    }

    let s = &report.summary;
    stdout.push_str(&format!(
        "Summary: {} {} ({} regressed, {} fixed, {} still_failing, {} still_passing, {} added, {} removed, {} case_changed), {:+} ms\n",
        s.total,
        noun(s.total, "case", "cases"),
        s.regressed,
        s.fixed,
        s.still_failing,
        s.still_passing,
        s.added,
        s.removed,
        s.case_changed,
        s.duration_delta_ms,
    ));
    stdout
}

fn json_compare(report: &CompareReport) -> Value {
    let s = &report.summary;
    json!({
        "ok": report.is_ok(),
        "baseline_dir": paths::display(&report.baseline_dir),
        "candidate_dir": paths::display(&report.candidate_dir),
        "summary": {
            "total": s.total,
            "regressed": s.regressed,
            "fixed": s.fixed,
            "still_failing": s.still_failing,
            "still_passing": s.still_passing,
            "added": s.added,
            "removed": s.removed,
            "case_changed": s.case_changed,
            "baseline_duration_ms": s.baseline_duration_ms,
            "candidate_duration_ms": s.candidate_duration_ms,
            "duration_delta_ms": s.duration_delta_ms,
        },
        "cases": report.cases.iter().map(case_comparison_value).collect::<Vec<Value>>(),
    })
}

fn case_comparison_value(case: &CaseComparison) -> Value {
    let steps_delta = match (case.baseline_steps, case.candidate_steps) {
        (Some(b), Some(c)) => json!({
            "total": signed_usize_delta(b.total, c.total),
            "passed": signed_usize_delta(b.passed, c.passed),
            "failed": signed_usize_delta(b.failed, c.failed),
            "errored": signed_usize_delta(b.errored, c.errored),
            "skipped": signed_usize_delta(b.skipped, c.skipped),
        }),
        _ => Value::Null,
    };
    let assertions_delta = match (case.baseline_assertions, case.candidate_assertions) {
        (Some(b), Some(c)) => json!({
            "total": signed_usize_delta(b.total, c.total),
            "passed": signed_usize_delta(b.passed, c.passed),
            "failed": signed_usize_delta(b.failed, c.failed),
        }),
        _ => Value::Null,
    };

    json!({
        "case_file": case.case_file,
        "case_name": case.case_name,
        "classification": case.classification.as_str(),
        "baseline_status": case.baseline_status,
        "candidate_status": case.candidate_status,
        "baseline_bundle": case.baseline_bundle.as_ref().map(|p| paths::display(p)),
        "candidate_bundle": case.candidate_bundle.as_ref().map(|p| paths::display(p)),
        "baseline_sha256": case.baseline_sha256,
        "candidate_sha256": case.candidate_sha256,
        "case_changed": case.case_changed,
        "baseline_duration_ms": case.baseline_duration_ms,
        "candidate_duration_ms": case.candidate_duration_ms,
        "duration_delta_ms": case.duration_delta_ms,
        "steps": {
            "baseline": case.baseline_steps.map(step_counts_value),
            "candidate": case.candidate_steps.map(step_counts_value),
            "delta": steps_delta,
            "diffs": case.step_diffs.iter().map(step_diff_value).collect::<Vec<Value>>(),
        },
        "assertions": {
            "baseline": case.baseline_assertions.map(assertion_counts_value),
            "candidate": case.candidate_assertions.map(assertion_counts_value),
            "delta": assertions_delta,
        },
    })
}

fn step_counts_value(counts: StepCounts) -> Value {
    json!({
        "total": counts.total,
        "passed": counts.passed,
        "failed": counts.failed,
        "errored": counts.errored,
        "skipped": counts.skipped,
    })
}

fn assertion_counts_value(counts: AssertionCounts) -> Value {
    json!({
        "total": counts.total,
        "passed": counts.passed,
        "failed": counts.failed,
    })
}

fn step_diff_value(diff: &StepDiff) -> Value {
    json!({
        "phase": diff.phase,
        "step_number": diff.step_number,
        "baseline_name": diff.baseline_name,
        "candidate_name": diff.candidate_name,
        "baseline_status": diff.baseline_status,
        "candidate_status": diff.candidate_status,
        "baseline_duration_ms": diff.baseline_duration_ms,
        "candidate_duration_ms": diff.candidate_duration_ms,
        "duration_delta_ms": diff.duration_delta_ms,
        "baseline_assertions": diff.baseline_assertions.map(assertion_counts_value),
        "candidate_assertions": diff.candidate_assertions.map(assertion_counts_value),
        "candidate_error": diff.candidate_error,
    })
}

fn signed_usize_delta(before: usize, after: usize) -> i64 {
    let diff = i128::from(u64::try_from(after).unwrap_or(u64::MAX))
        - i128::from(u64::try_from(before).unwrap_or(u64::MAX));
    i64::try_from(diff).unwrap_or(if diff < 0 { i64::MIN } else { i64::MAX })
}

/// Builds the redacted JSON value for a `da run --repeat N` (`N > 1`) outcome.
pub fn repeat_json(outcome: &RepeatOutcome, secrets: &Secrets) -> Value {
    secrets.redact_json(&raw_repeat_json(outcome))
}

/// Renders a `da run --repeat N` (`N > 1`) outcome in text or JSON format.
pub fn render_repeat(outcome: &RepeatOutcome, format: Format, secrets: &Secrets) -> Rendered {
    match format {
        Format::Json => Rendered {
            stdout: format!("{}\n", repeat_json(outcome, secrets)),
            stderr: match outcome {
                RepeatOutcome::Completed(report) => secrets.redact(&repeat_bundle_errors(report)),
                RepeatOutcome::Rejected { .. } => String::new(),
                RepeatOutcome::NoMatch { message, .. } => {
                    format!("error: {}\n", secrets.redact(message))
                }
            },
        },
        Format::Text => text_repeat(outcome, secrets),
    }
}

fn repeat_bundle_errors(report: &RepeatReport) -> String {
    let mut stderr = String::new();
    for error in &report.bundle_errors {
        stderr.push_str(&format!("error: {error}\n"));
    }
    stderr
}

fn text_repeat(outcome: &RepeatOutcome, secrets: &Secrets) -> Rendered {
    let report = match outcome {
        RepeatOutcome::Completed(report) => report,
        RepeatOutcome::NoMatch { message, .. } => {
            return Rendered {
                stdout: String::new(),
                stderr: format!("error: {}\n", secrets.redact(message)),
            };
        }
        RepeatOutcome::Rejected { errors, .. } => {
            let mut stderr = String::new();
            for error in errors {
                stderr.push_str(&format!(
                    "error: {}: {}\n",
                    paths::display(&error.file),
                    error.message
                ));
            }
            return Rendered {
                stdout: String::new(),
                stderr: secrets.redact(&stderr),
            };
        }
    };

    let mut stdout = String::new();
    for case in &report.cases {
        stdout.push_str(&format!(
            "{} {} [{}] {}/{} passed ({} failed) {} ms\n",
            case.classification.label(),
            paths::display(&case.file),
            case.name,
            case.passed_runs,
            case.repeat_count,
            case.failed_runs,
            millis(case.duration),
        ));
        for (idx, attempt) in case.attempts.iter().enumerate() {
            let attempt_no = idx + 1;
            if let Some(problem) = &attempt.error {
                stdout.push_str(&format!(
                    "  attempt {attempt_no}: {} ({} ms): {}\n",
                    attempt.status.as_str(),
                    millis(attempt.duration),
                    problem.message
                ));
            } else {
                stdout.push_str(&format!(
                    "  attempt {attempt_no}: {} ({} ms)\n",
                    attempt.status.as_str(),
                    millis(attempt.duration),
                ));
            }
            for step in &attempt.steps {
                for assertion in &step.assertions {
                    if !assertion.passed {
                        stdout.push_str(&format!("    {}\n", describe(assertion)));
                    }
                }
            }
            if let Some(bundle) = &attempt.bundle {
                stdout.push_str(&format!("    bundle: {}\n", paths::display(bundle)));
            }
        }
    }

    let s = report.summary();
    stdout.push_str(&format!(
        "Summary (repeat {}): {} {} ({} stable_pass, {} flaky, {} stable_fail, {} filtered), {}/{} attempts passed, {} ms\n",
        s.repeat_count,
        s.cases,
        noun(s.cases, "case", "cases"),
        s.stable_pass,
        s.flaky,
        s.stable_fail,
        s.filtered,
        s.passed_attempts,
        s.total_attempts,
        millis(report.duration),
    ));

    Rendered {
        stdout: secrets.redact(&stdout),
        stderr: secrets.redact(&repeat_bundle_errors(report)),
    }
}

fn raw_repeat_json(outcome: &RepeatOutcome) -> Value {
    let mut object = Map::new();
    object.insert("ok".to_string(), Value::Bool(outcome.is_ok()));

    match outcome {
        RepeatOutcome::Completed(report) => {
            let s = report.summary();
            object.insert("repeat_count".to_string(), json!(report.repeat_count));
            object.insert(
                "summary".to_string(),
                json!({
                    "repeat_count": s.repeat_count,
                    "cases": s.cases,
                    "filtered": s.filtered,
                    "stable_pass": s.stable_pass,
                    "stable_fail": s.stable_fail,
                    "flaky": s.flaky,
                    "total_attempts": s.total_attempts,
                    "passed_attempts": s.passed_attempts,
                    "failed_attempts": s.failed_attempts,
                    "duration_ms": millis(report.duration),
                }),
            );
            object.insert(
                "bundle_errors".to_string(),
                Value::Array(
                    report
                        .bundle_errors
                        .iter()
                        .map(|e| Value::String(e.clone()))
                        .collect(),
                ),
            );
            object.insert(
                "cases".to_string(),
                Value::Array(
                    report
                        .cases
                        .iter()
                        .map(|case| {
                            json!({
                                "file": paths::display(&case.file),
                                "name": case.name,
                                "project": case.project,
                                "suite": case.suite,
                                "layer": case.layer,
                                "tags": case.tags,
                                "classification": case.classification.as_str(),
                                "repeat_count": case.repeat_count,
                                "passed_runs": case.passed_runs,
                                "failed_runs": case.failed_runs,
                                "duration_ms": millis(case.duration),
                                "attempts": case
                                    .attempts
                                    .iter()
                                    .enumerate()
                                    .map(|(idx, attempt)| {
                                        let mut attempt_obj = case_json(attempt);
                                        if let Some(map) = attempt_obj.as_object_mut() {
                                            map.insert("attempt".to_string(), json!(idx + 1));
                                        }
                                        attempt_obj
                                    })
                                    .collect::<Vec<Value>>(),
                            })
                        })
                        .collect(),
                ),
            );
            object.insert("errors".to_string(), Value::Array(Vec::new()));
        }
        RepeatOutcome::Rejected {
            repeat_count,
            errors,
        } => {
            object.insert("repeat_count".to_string(), json!(repeat_count));
            object.insert(
                "summary".to_string(),
                json!({
                    "repeat_count": repeat_count,
                    "cases": 0,
                    "filtered": 0,
                    "stable_pass": 0,
                    "stable_fail": 0,
                    "flaky": 0,
                    "total_attempts": 0,
                    "passed_attempts": 0,
                    "failed_attempts": 0,
                    "duration_ms": 0,
                }),
            );
            object.insert("bundle_errors".to_string(), Value::Array(Vec::new()));
            object.insert("cases".to_string(), Value::Array(Vec::new()));
            object.insert(
                "errors".to_string(),
                Value::Array(
                    errors
                        .iter()
                        .map(|error| {
                            json!({
                                "file": paths::display(&error.file),
                                "message": error.message,
                                "kind": "invalid_input",
                            })
                        })
                        .collect(),
                ),
            );
        }
        RepeatOutcome::NoMatch {
            repeat_count,
            message,
        } => {
            object.insert("repeat_count".to_string(), json!(repeat_count));
            object.insert(
                "summary".to_string(),
                json!({
                    "repeat_count": repeat_count,
                    "cases": 0,
                    "filtered": 0,
                    "stable_pass": 0,
                    "stable_fail": 0,
                    "flaky": 0,
                    "total_attempts": 0,
                    "passed_attempts": 0,
                    "failed_attempts": 0,
                    "duration_ms": 0,
                }),
            );
            object.insert("bundle_errors".to_string(), Value::Array(Vec::new()));
            object.insert("cases".to_string(), Value::Array(Vec::new()));
            object.insert("errors".to_string(), Value::Array(Vec::new()));
            object.insert("filter_error".to_string(), Value::String(message.clone()));
        }
    }

    Value::Object(object)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::core::report::{ProblemKind, StepStatus};

    fn failing_outcome() -> RunOutcome {
        RunOutcome::Completed(crate::core::report::RunReport {
            cases: vec![CaseReport {
                file: PathBuf::from("cases/login.yaml"),
                case_sha256: None,
                name: "Login".to_string(),
                project: Some("shop".to_string()),
                suite: Some("smoke".to_string()),
                layer: Some("api".to_string()),
                tags: vec!["smoke".to_string()],
                overridden_vars: Vec::new(),
                status: CaseStatus::Failed,
                duration: Duration::from_millis(3),
                steps: vec![StepReport {
                    name: "sample.call".to_string(),
                    status: StepStatus::Failed,
                    duration: Duration::from_millis(2),
                    args: None,
                    result: Some(json!({"status": 200})),
                    saved: BTreeMap::new(),
                    assertions: vec![AssertionReport {
                        path: "$.status".to_string(),
                        comparison: "equals".to_string(),
                        expected: json!(201),
                        actual: Some(json!(200)),
                        passed: false,
                        message: None,
                    }],
                    error: Some(Problem {
                        kind: ProblemKind::Assertion,
                        message: "1 of 1 assertions failed".to_string(),
                    }),
                }],
                finally: Vec::new(),
                error: Some(Problem {
                    kind: ProblemKind::Assertion,
                    message: "1 of 1 assertions failed".to_string(),
                }),
                close_errors: Vec::new(),
                bundle: Some(PathBuf::from(".da/failures/17-42/0-Login")),
                local_only: false,
                local_only_reason: None,
            }],
            filtered: 2,
            duration: Duration::from_millis(4),
            bundle_errors: Vec::new(),
        })
    }

    /// A passing case whose first, optional step could not find its target.
    fn optional_skip_outcome() -> RunOutcome {
        RunOutcome::Completed(crate::core::report::RunReport {
            cases: vec![CaseReport {
                file: PathBuf::from("cases/banner.yaml"),
                case_sha256: None,
                name: "Banner".to_string(),
                project: None,
                suite: None,
                layer: None,
                tags: Vec::new(),
                overridden_vars: Vec::new(),
                status: CaseStatus::Passed,
                duration: Duration::from_millis(5),
                steps: vec![
                    StepReport {
                        name: "sample.click".to_string(),
                        status: StepStatus::Skipped,
                        duration: Duration::from_millis(2),
                        args: None,
                        result: Some(json!({"found": false})),
                        saved: BTreeMap::new(),
                        assertions: vec![AssertionReport {
                            path: "$.found".to_string(),
                            comparison: "equals".to_string(),
                            expected: json!(true),
                            actual: Some(json!(false)),
                            passed: false,
                            message: None,
                        }],
                        error: Some(Problem {
                            kind: ProblemKind::Assertion,
                            message: "1 of 1 assertions failed".to_string(),
                        }),
                    },
                    StepReport {
                        name: "sample.after".to_string(),
                        status: StepStatus::Passed,
                        duration: Duration::from_millis(1),
                        args: None,
                        result: Some(json!({"ok": true})),
                        saved: BTreeMap::new(),
                        assertions: Vec::new(),
                        error: None,
                    },
                ],
                finally: Vec::new(),
                error: None,
                close_errors: Vec::new(),
                bundle: None,
                local_only: false,
                local_only_reason: None,
            }],
            filtered: 0,
            duration: Duration::from_millis(5),
            bundle_errors: Vec::new(),
        })
    }

    #[test]
    fn a_skipped_optional_step_keeps_its_evidence_in_json() {
        let rendered = render(&optional_skip_outcome(), Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&rendered.stdout).expect("stdout must be JSON");

        let case = &value["cases"][0];
        assert_eq!(case["status"], "passed");
        assert_eq!(case["error"], Value::Null);
        assert_eq!(case["steps"][0]["status"], "skipped");
        assert_eq!(
            case["steps"][0]["error"]["kind"], "assertion",
            "the reason it was skipped must stay readable, got: {case}"
        );
        assert_eq!(case["steps"][0]["result"], json!({"found": false}));
        assert_eq!(case["steps"][0]["assertions"][0]["passed"], false);
        assert_eq!(case["steps"][0]["assertions"][0]["expected"], true);
        assert_eq!(value["summary"]["steps_skipped"], 1);
        assert_eq!(value["summary"]["cases_passed"], 1);
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn text_names_an_optional_step_that_was_skipped() {
        let rendered = render(&optional_skip_outcome(), Format::Text, &Secrets::new());

        assert!(
            rendered
                .stdout
                .contains("  sample.click (optional): 1 of 1 assertions failed"),
            "got: {}",
            rendered.stdout
        );
        assert!(
            rendered
                .stdout
                .contains("    $.found equals true (actual false)"),
            "the failed assertion must stay visible, got: {}",
            rendered.stdout
        );
        assert!(
            rendered.stdout.contains("1 skipped"),
            "the summary must count the skipped step, got: {}",
            rendered.stdout
        );
        assert!(
            !rendered.stdout.contains("sample.after"),
            "a step that passed prints no line, got: {}",
            rendered.stdout
        );
    }

    #[test]
    fn text_stays_quiet_about_a_step_that_never_ran() {
        let RunOutcome::Completed(mut report) = failing_outcome() else {
            panic!("the fixture must be a completed run");
        };
        report.cases[0].steps.push(StepReport {
            name: "sample.never".to_string(),
            status: StepStatus::Skipped,
            duration: Duration::ZERO,
            args: None,
            result: None,
            saved: BTreeMap::new(),
            assertions: Vec::new(),
            error: None,
        });
        let rendered = render(
            &RunOutcome::Completed(report),
            Format::Text,
            &Secrets::new(),
        );

        assert!(
            !rendered.stdout.contains("sample.never"),
            "an unexecuted step prints nothing, got: {}",
            rendered.stdout
        );
        assert!(
            !rendered.stdout.contains("(optional)"),
            "only a skipped optional step carries the note, got: {}",
            rendered.stdout
        );
    }

    #[test]
    fn text_lists_the_path_the_expected_value_and_the_actual_value() {
        let rendered = render(&failing_outcome(), Format::Text, &Secrets::new());

        assert!(rendered.stderr.is_empty());
        assert!(
            rendered
                .stdout
                .contains("FAIL cases/login.yaml [Login] 3 ms")
        );
        assert!(
            rendered.stdout.contains("$.status equals 201 (actual 200)"),
            "got: {}",
            rendered.stdout
        );
        assert!(
            rendered
                .stdout
                .contains("  bundle: .da/failures/17-42/0-Login"),
            "the bundle path belongs under the failed case, got: {}",
            rendered.stdout
        );
        assert!(rendered.stdout.contains("Summary: 1 case"));
        assert!(
            rendered.stdout.contains("0 errors, 2 filtered"),
            "the summary must count the filtered cases, got: {}",
            rendered.stdout
        );
    }

    #[test]
    fn json_is_one_object_with_the_documented_fields() {
        let rendered = render(&failing_outcome(), Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&rendered.stdout).expect("stdout must be JSON");

        assert_eq!(value["ok"], false);
        assert_eq!(value["summary"]["cases"], 1);
        assert_eq!(value["summary"]["filtered"], 2);
        assert_eq!(value["summary"]["cases_failed"], 1);
        assert_eq!(value["summary"]["steps_failed"], 1);
        assert_eq!(value["cases"][0]["project"], "shop");
        assert_eq!(value["cases"][0]["suite"], "smoke");
        assert_eq!(value["cases"][0]["layer"], "api");
        assert_eq!(value["cases"][0]["tags"], json!(["smoke"]));
        assert_eq!(value["cases"][0]["duration_ms"], 3);
        assert_eq!(value["cases"][0]["bundle"], ".da/failures/17-42/0-Login");
        assert_eq!(value["cases"][0]["steps"][0]["status"], "failed");
        assert_eq!(
            value["cases"][0]["steps"][0]["assertions"][0]["passed"],
            false
        );
        assert_eq!(
            value["cases"][0]["steps"][0]["assertions"][0]["actual"],
            200
        );
        assert_eq!(value["cases"][0]["steps"][0]["error"]["kind"], "assertion");
        assert_eq!(value["errors"], json!([]));
        assert_eq!(value["bundle_errors"], json!([]));
    }

    #[test]
    fn reported_paths_use_forward_slashes_on_every_platform() {
        let RunOutcome::Completed(mut report) = failing_outcome() else {
            panic!("the fixture must be a completed run");
        };
        report.cases[0].file = PathBuf::from(r"G:\cases\login.yaml");
        report.cases[0].bundle = Some(PathBuf::from(r"G:\cases\.da\failures\1-0-Login"));
        let outcome = RunOutcome::Completed(report);

        let json = render(&outcome, Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert_eq!(value["cases"][0]["file"], "G:/cases/login.yaml");
        assert_eq!(
            value["cases"][0]["bundle"],
            "G:/cases/.da/failures/1-0-Login"
        );

        let text = render(&outcome, Format::Text, &Secrets::new());
        assert!(
            !text.stdout.contains('\\'),
            "text output must not mix separators, got: {}",
            text.stdout
        );
    }

    #[test]
    fn a_bundle_that_could_not_be_written_is_reported_without_changing_the_verdict() {
        let RunOutcome::Completed(mut report) = failing_outcome() else {
            panic!("the fixture must be a completed run");
        };
        report
            .bundle_errors
            .push("could not write the bundle".to_string());
        report.cases[0].bundle = None;
        let outcome = RunOutcome::Completed(report);

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::TestFailure);

        let text = render(&outcome, Format::Text, &Secrets::new());
        assert!(
            text.stderr.contains("could not write the bundle"),
            "got: {}",
            text.stderr
        );
        assert!(!text.stdout.contains("bundle:"), "got: {}", text.stdout);

        let json = render(&outcome, Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert_eq!(value["bundle_errors"][0], "could not write the bundle");
        assert_eq!(value["cases"][0]["bundle"], Value::Null);
        assert!(
            json.stderr.contains("could not write the bundle"),
            "got: {}",
            json.stderr
        );
    }

    #[test]
    fn rejected_input_is_reported_on_stderr_and_as_json_errors() {
        let outcome = RunOutcome::Rejected {
            errors: vec![crate::core::case::CaseError::invalid(
                PathBuf::from("broken.yaml"),
                "line 2: unknown field",
            )],
        };

        let text = render(&outcome, Format::Text, &Secrets::new());
        assert!(text.stdout.is_empty());
        assert!(text.stderr.contains("broken.yaml"), "got: {}", text.stderr);

        let json = render(&outcome, Format::Json, &Secrets::new());
        assert!(json.stderr.is_empty());
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert_eq!(value["ok"], false);
        assert_eq!(value["cases"], json!([]));
        assert_eq!(value["errors"][0]["file"], "broken.yaml");
    }

    #[test]
    fn a_filter_that_matches_nothing_is_a_diagnostic_and_a_json_error() {
        let outcome = RunOutcome::NoMatch {
            message: "no case matched --tag \"missing\"; the discovered cases use tags: `smoke`"
                .to_string(),
        };

        let text = render(&outcome, Format::Text, &Secrets::new());
        assert!(text.stdout.is_empty());
        assert!(
            text.stderr.contains("--tag \"missing\""),
            "got: {}",
            text.stderr
        );

        let json = render(&outcome, Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert_eq!(value["ok"], false);
        assert_eq!(value["summary"]["cases"], 0);
        assert_eq!(value["cases"], json!([]));
        assert_eq!(value["errors"], json!([]));
        assert!(
            value["filter_error"]
                .as_str()
                .is_some_and(|message| message.contains("`smoke`")),
            "got: {value}"
        );
        assert!(
            json.stderr.contains("--tag \"missing\""),
            "got: {}",
            json.stderr
        );
    }

    #[test]
    fn lint_text_puts_one_error_per_line_and_ends_with_a_summary() {
        let report = LintReport {
            warnings: Vec::new(),
            files: 2,
            errors: vec![
                CaseError::invalid(PathBuf::from("a.yaml"), "line 2: unknown field"),
                CaseError::invalid(PathBuf::from("b.yaml"), "step 1: undefined variable"),
            ],
            cases: Vec::new(),
            ..LintReport::default()
        };

        let rendered = render_lint(&report, Format::Text, &Secrets::new());

        assert!(rendered.stderr.is_empty());
        assert_eq!(
            rendered.stdout,
            "a.yaml: line 2: unknown field\nb.yaml: step 1: undefined variable\nChecked 2 files: 2 errors.\n"
        );
    }

    #[test]
    fn lint_text_prints_warnings_on_stderr_and_counts_them() {
        let report = LintReport {
            warnings: vec![crate::core::case::CaseWarning {
                file: PathBuf::from("coords.yaml"),
                message: "line 7 column 5: step 2: `browser.click` uses explicit coordinates"
                    .to_string(),
            }],
            files: 1,
            errors: Vec::new(),
            cases: Vec::new(),
            ..LintReport::default()
        };

        let rendered = render_lint(&report, Format::Text, &Secrets::new());

        assert_eq!(
            rendered.stdout, "Checked 1 file: 0 errors, 1 warning.\n",
            "the summary must count a warning that never fails the case"
        );
        assert_eq!(
            rendered.stderr,
            "coords.yaml: warning: line 7 column 5: step 2: `browser.click` uses explicit coordinates\n"
        );
    }

    #[test]
    fn lint_json_reports_warnings_only_when_there_are_some() {
        let without = LintReport {
            files: 1,
            errors: Vec::new(),
            warnings: Vec::new(),
            cases: Vec::new(),
            ..LintReport::default()
        };
        let rendered = render_lint(&without, Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&rendered.stdout).expect("stdout must be JSON");
        assert!(
            value.get("warnings").is_none(),
            "a report without warnings must keep the earlier object shape: {value}"
        );

        let with = LintReport {
            warnings: vec![crate::core::case::CaseWarning {
                file: PathBuf::from("coords.yaml"),
                message: "line 7 column 5: step 2: uses explicit coordinates".to_string(),
            }],
            ..without
        };
        let rendered = render_lint(&with, Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&rendered.stdout).expect("stdout must be JSON");
        assert_eq!(
            value["warnings"],
            json!([{
                "file": "coords.yaml",
                "message": "line 7 column 5: step 2: uses explicit coordinates",
            }])
        );
        assert_eq!(value["ok"], true, "a warning never fails lint");
    }

    #[test]
    fn lint_text_keeps_the_line_breaks_of_a_parser_message() {
        let report = LintReport {
            warnings: Vec::new(),
            files: 1,
            errors: vec![CaseError::invalid(
                PathBuf::from("broken.yaml"),
                "line 2 column 3: unclosed bracket\n --> <input>\n  |",
            )],
            cases: Vec::new(),
            ..LintReport::default()
        };

        let rendered = render_lint(&report, Format::Text, &Secrets::new());

        assert_eq!(
            rendered.stdout,
            "broken.yaml: line 2 column 3: unclosed bracket\n --> <input>\n  |\nChecked 1 file: 1 error.\n"
        );
    }

    #[test]
    fn lint_json_has_ok_files_errors_and_the_case_classification() {
        let report = LintReport {
            warnings: Vec::new(),
            files: 1,
            errors: vec![CaseError::invalid(
                PathBuf::from("broken.yaml"),
                "line 2: unknown field",
            )],
            cases: vec![LintCase {
                file: PathBuf::from("cases/login.yaml"),
                name: "Login".to_string(),
                project: Some("shop".to_string()),
                suite: None,
                layer: Some("api".to_string()),
                tags: vec!["smoke".to_string()],
            }],
            ..LintReport::default()
        };

        let rendered = render_lint(&report, Format::Json, &Secrets::new());
        let value: Value = serde_json::from_str(&rendered.stdout).expect("stdout must be JSON");

        assert_eq!(
            value,
            json!({
                "ok": false,
                "files": 1,
                "errors": [{"file": "broken.yaml", "message": "line 2: unknown field"}],
                "cases": [{
                    "file": "cases/login.yaml",
                    "name": "Login",
                    "project": "shop",
                    "suite": null,
                    "layer": "api",
                    "tags": ["smoke"],
                }],
            })
        );
    }

    #[test]
    fn schema_text_is_pretty_and_json_is_compact() {
        let schema = json!({"type": "object", "properties": {"name": {"type": "string"}}});

        let text = render_schema(&schema, Format::Text);
        assert!(text.stderr.is_empty());
        assert!(
            text.stdout.contains("\n  \"type\""),
            "text mode must indent, got: {:?}",
            text.stdout
        );
        let parsed: Value = serde_json::from_str(&text.stdout).expect("stdout must be JSON");
        assert_eq!(parsed, schema);

        let compact = render_schema(&schema, Format::Json);
        assert_eq!(compact.stdout, format!("{schema}\n"));
    }

    #[test]
    fn a_secret_is_redacted_in_text_and_json_output() {
        let RunOutcome::Completed(mut report) = failing_outcome() else {
            panic!("the fixture must be a completed run");
        };
        report.cases[0].steps[0].result = Some(json!({"token": "s3cr3t-value"}));
        report.cases[0].steps[0].assertions[0].actual = Some(json!("s3cr3t-value"));
        report.cases[0].error = Some(Problem {
            kind: ProblemKind::Assertion,
            message: "s3cr3t-value did not match".to_string(),
        });
        let outcome = RunOutcome::Completed(report);
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-value");

        let text = render(&outcome, Format::Text, &secrets);
        assert!(!text.stdout.contains("s3cr3t-value"), "{}", text.stdout);
        assert!(text.stdout.contains("***"), "{}", text.stdout);

        let json = render(&outcome, Format::Json, &secrets);
        assert!(!json.stdout.contains("s3cr3t-value"), "{}", json.stdout);
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert_eq!(value["cases"][0]["steps"][0]["result"]["token"], "***");
        assert_eq!(value["cases"][0]["error"]["message"], "*** did not match");
    }

    #[test]
    fn a_rejected_run_redacts_its_error_messages() {
        let outcome = RunOutcome::Rejected {
            errors: vec![crate::core::case::CaseError::invalid(
                PathBuf::from("bad.yaml"),
                "`url` must start with a scheme, found `nope://s3cr3t-value`",
            )],
        };
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-value");

        let text = render(&outcome, Format::Text, &secrets);
        assert!(!text.stderr.contains("s3cr3t-value"), "{}", text.stderr);
        assert!(text.stderr.contains("***"), "{}", text.stderr);

        let json = render(&outcome, Format::Json, &secrets);
        assert!(!json.stdout.contains("s3cr3t-value"), "{}", json.stdout);
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert!(
            value["errors"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("***")),
            "got: {value}"
        );
    }

    #[test]
    fn a_lint_error_is_redacted_in_text_and_json() {
        let report = LintReport {
            warnings: Vec::new(),
            files: 1,
            errors: vec![CaseError::invalid(
                PathBuf::from("bad.yaml"),
                "step 1: environment variable `s3cr3t-value` is not set",
            )],
            cases: Vec::new(),
            ..LintReport::default()
        };
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-value");

        let text = render_lint(&report, Format::Text, &secrets);
        assert!(!text.stdout.contains("s3cr3t-value"), "{}", text.stdout);
        assert!(text.stdout.contains("***"), "{}", text.stdout);

        let json = render_lint(&report, Format::Json, &secrets);
        assert!(!json.stdout.contains("s3cr3t-value"), "{}", json.stdout);
        assert!(json.stdout.contains("***"), "{}", json.stdout);
    }

    #[test]
    fn json_and_text_render_finally_steps_and_redact_secrets() {
        let RunOutcome::Completed(mut report) = failing_outcome() else {
            panic!("the fixture must be a completed run");
        };
        let without_finally = render(
            &RunOutcome::Completed(report.clone()),
            Format::Json,
            &Secrets::new(),
        );
        let without_value: Value =
            serde_json::from_str(&without_finally.stdout).expect("stdout must be JSON");
        assert!(
            without_value["cases"][0].get("finally").is_none(),
            "empty finally list should stay omitted for compatibility: {without_value}"
        );

        report.cases[0].finally = vec![
            StepReport {
                name: "sample.cleanup".to_string(),
                status: StepStatus::Error,
                duration: Duration::from_millis(2),
                args: None,
                result: Some(json!({ "token": "s3cr3t-value" })),
                saved: BTreeMap::new(),
                assertions: Vec::new(),
                error: Some(Problem {
                    kind: ProblemKind::Execution,
                    message: "cleanup failed for s3cr3t-value".to_string(),
                }),
            },
            StepReport {
                name: "sample.optional_cleanup".to_string(),
                status: StepStatus::Skipped,
                duration: Duration::from_millis(1),
                args: None,
                result: None,
                saved: BTreeMap::new(),
                assertions: Vec::new(),
                error: Some(Problem {
                    kind: ProblemKind::Execution,
                    message: "missing id".to_string(),
                }),
            },
        ];
        let outcome = RunOutcome::Completed(report);
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-value");

        let text = render(&outcome, Format::Text, &secrets);
        assert!(!text.stdout.contains("s3cr3t-value"), "{}", text.stdout);
        assert!(
            text.stdout
                .contains("  finally sample.cleanup: cleanup failed for ***"),
            "got: {}",
            text.stdout
        );
        assert!(
            text.stdout
                .contains("  finally sample.optional_cleanup (optional): missing id"),
            "got: {}",
            text.stdout
        );

        let json = render(&outcome, Format::Json, &secrets);
        assert!(!json.stdout.contains("s3cr3t-value"), "{}", json.stdout);
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert_eq!(value["cases"][0]["finally"][0]["result"]["token"], "***");
        assert_eq!(
            value["cases"][0]["finally"][0]["error"]["message"],
            "cleanup failed for ***"
        );
        assert_eq!(value["cases"][0]["finally"][1]["status"], "skipped");
    }

    #[test]
    fn local_only_case_prints_notice_and_emits_redacted_json_fields() {
        let RunOutcome::Completed(mut report) = failing_outcome() else {
            panic!("the fixture must be a completed run");
        };
        report.cases[0].local_only = true;
        report.cases[0].local_only_reason =
            Some("external handle s3cr3t-value may hold unmanaged state".to_string());
        let outcome = RunOutcome::Completed(report);
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-value");

        let text = render(&outcome, Format::Text, &secrets);
        assert!(!text.stdout.contains("s3cr3t-value"), "{}", text.stdout);
        assert!(
            text.stdout.contains("local-only: evidence is kept local only (external handle *** may hold unmanaged state)"),
            "{}",
            text.stdout
        );

        let json = render(&outcome, Format::Json, &secrets);
        assert!(!json.stdout.contains("s3cr3t-value"), "{}", json.stdout);
        let value: Value = serde_json::from_str(&json.stdout).expect("stdout must be JSON");
        assert_eq!(value["cases"][0]["local_only"], true);
        assert_eq!(
            value["cases"][0]["local_only_reason"],
            "external handle *** may hold unmanaged state"
        );
    }
}
