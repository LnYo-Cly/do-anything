//! Regression comparison across two recorded evidence directories.
//!
//! `da run --record <dir>` names each case subdirectory `{position}-{safe_name}`.
//! When cases are added or removed between runs, those numeric prefixes shift.
//! `da compare <baseline_dir> <candidate_dir>` requires every case
//! subdirectory in both records to contain `meta.json` (rejecting partial or
//! interrupted runs), reads `meta.json` and `report.json`, pairs cases by
//! stable `(case_file, case_name)` identity independent of directory
//! numbering, and classifies every case into `regressed`, `fixed`,
//! `still_failing`, `still_passing`, `added`, or `removed`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::output::{
    AssertionCounts, CaseComparison, CompareClassification, CompareReport, CompareSummary,
    StepCounts, StepDiff,
};
use crate::core::paths;
use crate::exit::ExitCode;

/// Why two recorded directories could not be compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareError {
    message: String,
    exit_code: ExitCode,
}

impl CompareError {
    fn usage(message: String) -> Self {
        Self {
            message,
            exit_code: ExitCode::Usage,
        }
    }

    fn invalid_case(message: String) -> Self {
        Self {
            message,
            exit_code: ExitCode::InvalidCase,
        }
    }

    /// Error category string used in `--output json` error payloads.
    pub fn kind(&self) -> &'static str {
        match self.exit_code {
            ExitCode::InvalidCase => "invalid_case",
            _ => "usage",
        }
    }

    /// Actionable diagnostic message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Exit code `da compare` must return for this failure.
    pub fn exit_code(&self) -> ExitCode {
        self.exit_code
    }
}

impl std::fmt::Display for CompareError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CompareError {}

/// Compares `candidate_dir` against `baseline_dir` and returns the structured
/// regression report.
pub fn compare(baseline_dir: &Path, candidate_dir: &Path) -> Result<CompareReport, CompareError> {
    let baseline_cases = read_recorded_run(baseline_dir, "baseline")?;
    let candidate_cases = read_recorded_run(candidate_dir, "candidate")?;

    let mut base_groups: BTreeMap<(String, String), VecDeque<RecordedCase>> = BTreeMap::new();
    let mut cand_groups: BTreeMap<(String, String), VecDeque<RecordedCase>> = BTreeMap::new();
    let mut keys: BTreeSet<(String, String)> = BTreeSet::new();

    for case in baseline_cases {
        let key = (case.case_file.clone(), case.case_name.clone());
        keys.insert(key.clone());
        base_groups.entry(key).or_default().push_back(case);
    }
    for case in candidate_cases {
        let key = (case.case_file.clone(), case.case_name.clone());
        keys.insert(key.clone());
        cand_groups.entry(key).or_default().push_back(case);
    }

    let mut summary = CompareSummary::default();
    let mut comparisons = Vec::new();

    for key in keys {
        let mut base_queue = base_groups.remove(&key).unwrap_or_default();
        let mut cand_queue = cand_groups.remove(&key).unwrap_or_default();

        loop {
            match (base_queue.pop_front(), cand_queue.pop_front()) {
                (Some(base), Some(cand)) => {
                    let classification = match (base.is_passed(), cand.is_passed()) {
                        (true, false) => CompareClassification::Regressed,
                        (false, true) => CompareClassification::Fixed,
                        (false, false) => CompareClassification::StillFailing,
                        (true, true) => CompareClassification::StillPassing,
                    };
                    let case_changed = match (&base.case_sha256, &cand.case_sha256) {
                        (Some(left), Some(right)) => left != right,
                        _ => false,
                    };
                    let duration_delta_ms =
                        Some(signed_u64_delta(base.duration_ms, cand.duration_ms));
                    let step_diffs = diff_steps(&base, &cand);

                    match classification {
                        CompareClassification::Regressed => summary.regressed += 1,
                        CompareClassification::Fixed => summary.fixed += 1,
                        CompareClassification::StillFailing => summary.still_failing += 1,
                        CompareClassification::StillPassing => summary.still_passing += 1,
                        CompareClassification::Added | CompareClassification::Removed => {}
                    }
                    if case_changed {
                        summary.case_changed += 1;
                    }
                    summary.total += 1;
                    summary.baseline_duration_ms = summary
                        .baseline_duration_ms
                        .saturating_add(base.duration_ms);
                    summary.candidate_duration_ms = summary
                        .candidate_duration_ms
                        .saturating_add(cand.duration_ms);

                    comparisons.push(CaseComparison {
                        case_file: key.0.clone(),
                        case_name: key.1.clone(),
                        classification,
                        baseline_status: Some(base.status),
                        candidate_status: Some(cand.status),
                        baseline_bundle: Some(base.directory),
                        candidate_bundle: Some(cand.directory),
                        baseline_sha256: base.case_sha256,
                        candidate_sha256: cand.case_sha256,
                        case_changed,
                        baseline_duration_ms: Some(base.duration_ms),
                        candidate_duration_ms: Some(cand.duration_ms),
                        duration_delta_ms,
                        baseline_steps: Some(base.step_counts),
                        candidate_steps: Some(cand.step_counts),
                        baseline_assertions: Some(base.assertion_counts),
                        candidate_assertions: Some(cand.assertion_counts),
                        step_diffs,
                    });
                }
                (None, Some(cand)) => {
                    summary.added += 1;
                    summary.total += 1;
                    summary.candidate_duration_ms = summary
                        .candidate_duration_ms
                        .saturating_add(cand.duration_ms);
                    let step_diffs = diff_added_steps(&cand);
                    comparisons.push(CaseComparison {
                        case_file: key.0.clone(),
                        case_name: key.1.clone(),
                        classification: CompareClassification::Added,
                        baseline_status: None,
                        candidate_status: Some(cand.status),
                        baseline_bundle: None,
                        candidate_bundle: Some(cand.directory),
                        baseline_sha256: None,
                        candidate_sha256: cand.case_sha256,
                        case_changed: false,
                        baseline_duration_ms: None,
                        candidate_duration_ms: Some(cand.duration_ms),
                        duration_delta_ms: None,
                        baseline_steps: None,
                        candidate_steps: Some(cand.step_counts),
                        baseline_assertions: None,
                        candidate_assertions: Some(cand.assertion_counts),
                        step_diffs,
                    });
                }
                (Some(base), None) => {
                    summary.removed += 1;
                    summary.total += 1;
                    summary.baseline_duration_ms = summary
                        .baseline_duration_ms
                        .saturating_add(base.duration_ms);
                    comparisons.push(CaseComparison {
                        case_file: key.0.clone(),
                        case_name: key.1.clone(),
                        classification: CompareClassification::Removed,
                        baseline_status: Some(base.status),
                        candidate_status: None,
                        baseline_bundle: Some(base.directory),
                        candidate_bundle: None,
                        baseline_sha256: base.case_sha256,
                        candidate_sha256: None,
                        case_changed: false,
                        baseline_duration_ms: Some(base.duration_ms),
                        candidate_duration_ms: None,
                        duration_delta_ms: None,
                        baseline_steps: Some(base.step_counts),
                        candidate_steps: None,
                        baseline_assertions: Some(base.assertion_counts),
                        candidate_assertions: None,
                        step_diffs: Vec::new(),
                    });
                }
                (None, None) => break,
            }
        }
    }

    summary.duration_delta_ms =
        signed_u64_delta(summary.baseline_duration_ms, summary.candidate_duration_ms);

    Ok(CompareReport {
        baseline_dir: baseline_dir.to_path_buf(),
        candidate_dir: candidate_dir.to_path_buf(),
        summary,
        cases: comparisons,
    })
}

#[derive(Debug, Clone)]
struct RecordedStep {
    name: String,
    status: String,
    duration_ms: u64,
    assertions: AssertionCounts,
    error_message: Option<String>,
}

#[derive(Debug, Clone)]
struct RecordedCase {
    directory: PathBuf,
    case_file: String,
    case_name: String,
    case_sha256: Option<String>,
    status: String,
    duration_ms: u64,
    steps: Vec<RecordedStep>,
    finally: Vec<RecordedStep>,
    step_counts: StepCounts,
    assertion_counts: AssertionCounts,
}

impl RecordedCase {
    fn is_passed(&self) -> bool {
        self.status == "passed"
    }
}

fn read_recorded_run(directory: &Path, role: &str) -> Result<Vec<RecordedCase>, CompareError> {
    let metadata = fs::metadata(directory).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            CompareError::usage(format!(
                "the {role} record directory `{}` does not exist; run `da run --record <directory>` first",
                paths::display(directory)
            ))
        } else {
            CompareError::usage(format!(
                "could not read the {role} record directory `{}`: {error}",
                paths::display(directory)
            ))
        }
    })?;
    if !metadata.is_dir() {
        return Err(CompareError::usage(format!(
            "the {role} path `{}` is not a directory",
            paths::display(directory)
        )));
    }

    let entries = fs::read_dir(directory).map_err(|error| {
        CompareError::usage(format!(
            "could not list the {role} record directory `{}`: {error}",
            paths::display(directory)
        ))
    })?;

    let mut case_dirs = Vec::new();
    let mut incomplete_dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            CompareError::usage(format!(
                "could not read an entry in `{}`: {error}",
                paths::display(directory)
            ))
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|error| {
            CompareError::usage(format!(
                "could not inspect `{}`: {error}",
                paths::display(&path)
            ))
        })?;
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
        if !is_dir {
            continue;
        }
        if path.join("meta.json").is_file() {
            case_dirs.push(path);
        } else {
            incomplete_dirs.push(path);
        }
    }

    sort_case_dirs(&mut case_dirs);
    sort_case_dirs(&mut incomplete_dirs);

    if case_dirs.is_empty() && directory.join("attempt-1").is_dir() {
        return Err(CompareError::usage(format!(
            "`{}` is a `--repeat` record directory; pass one attempt directory such as `{}/attempt-1`",
            paths::display(directory),
            paths::display(directory)
        )));
    }

    if let Some(incomplete) = incomplete_dirs.first() {
        return Err(CompareError::usage(format!(
            "the {role} record directory `{}` contains an incomplete case directory `{}` without `meta.json`; re-run `da run --record` to produce a complete record",
            paths::display(directory),
            paths::display(incomplete)
        )));
    }

    if case_dirs.is_empty() {
        return Err(CompareError::usage(format!(
            "no complete case records in `{}`; every case directory must contain `meta.json`, which is written last",
            paths::display(directory)
        )));
    }

    let mut cases = Vec::with_capacity(case_dirs.len());
    for case_dir in case_dirs {
        cases.push(parse_recorded_case(&case_dir)?);
    }
    Ok(cases)
}

fn sort_case_dirs(dirs: &mut [PathBuf]) {
    dirs.sort_by(|left, right| {
        match (case_position(left), case_position(right)) {
            (Some(l), Some(r)) => l.cmp(&r),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.file_name().cmp(&right.file_name()))
    });
}

fn parse_recorded_case(case_dir: &Path) -> Result<RecordedCase, CompareError> {
    let meta_path = case_dir.join("meta.json");
    let meta = read_json_object(&meta_path)?;

    let raw_case_file = required_non_empty_str(&meta, "case_file", &meta_path)?;
    let case_name = required_non_empty_str(&meta, "case_name", &meta_path)?;
    let meta_status = required_case_status(&meta, "status", &meta_path)?;

    let case_sha256 = match meta.get("case_sha256") {
        None | Some(Value::Null) => None,
        Some(Value::String(hash)) if !hash.is_empty() => Some(hash.clone()),
        Some(_) => {
            return Err(CompareError::invalid_case(format!(
                "`{}` has an invalid `case_sha256` field; expected a non-empty hex string",
                paths::display(&meta_path)
            )));
        }
    };

    let report_path = case_dir.join("report.json");
    let report = read_json_object(&report_path)?;
    let report_status = required_case_status(&report, "status", &report_path)?;
    // If `report.json` carries `error.kind == "timeout"`, surface `"timeout"`
    // when neither status already says so, or preserve `"passed"`/`"failed"`/`"error"`/`"timeout"`.
    let report_says_timeout = report_status == "error"
        && report
            .get("error")
            .and_then(|err| err.get("kind"))
            .and_then(Value::as_str)
            == Some("timeout");
    let status = if report_says_timeout || meta_status == "timeout" {
        "timeout".to_string()
    } else {
        report_status
    };

    let duration_ms = match report.get("duration_ms") {
        Some(Value::Number(num)) => num.as_u64().ok_or_else(|| {
            CompareError::invalid_case(format!(
                "`{}` has an invalid `duration_ms`; expected a non-negative integer",
                paths::display(&report_path)
            ))
        })?,
        None => 0,
        Some(_) => {
            return Err(CompareError::invalid_case(format!(
                "`{}` has an invalid `duration_ms`; expected a number",
                paths::display(&report_path)
            )));
        }
    };

    let steps_val = report
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            CompareError::invalid_case(format!(
                "`{}` is missing a `steps` array",
                paths::display(&report_path)
            ))
        })?;
    let steps = parse_steps(steps_val, &report_path)?;

    let finally = match report.get("finally") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => parse_steps(items, &report_path)?,
        Some(_) => {
            return Err(CompareError::invalid_case(format!(
                "`{}` has a non-array `finally` field",
                paths::display(&report_path)
            )));
        }
    };

    let mut step_counts = StepCounts::default();
    let mut assertion_counts = AssertionCounts::default();
    for step in steps.iter().chain(&finally) {
        step_counts.total += 1;
        match step.status.as_str() {
            "passed" => step_counts.passed += 1,
            "failed" => step_counts.failed += 1,
            "error" => step_counts.errored += 1,
            "skipped" => step_counts.skipped += 1,
            _ => {}
        }
        assertion_counts.total += step.assertions.total;
        assertion_counts.passed += step.assertions.passed;
        assertion_counts.failed += step.assertions.failed;
    }

    Ok(RecordedCase {
        directory: case_dir.to_path_buf(),
        case_file: normalize_case_file(&raw_case_file),
        case_name,
        case_sha256,
        status,
        duration_ms,
        steps,
        finally,
        step_counts,
        assertion_counts,
    })
}

fn parse_steps(items: &[Value], report_path: &Path) -> Result<Vec<RecordedStep>, CompareError> {
    let mut out = Vec::with_capacity(items.len());
    for (idx, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(CompareError::invalid_case(format!(
                "`{}` step {} is not a JSON object",
                paths::display(report_path),
                idx + 1
            )));
        };
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CompareError::invalid_case(format!(
                    "`{}` step {} is missing a string `name`",
                    paths::display(report_path),
                    idx + 1
                ))
            })?
            .to_string();
        let status = obj.get("status").and_then(Value::as_str).ok_or_else(|| {
            CompareError::invalid_case(format!(
                "`{}` step {} is missing a string `status`",
                paths::display(report_path),
                idx + 1
            ))
        })?;
        if !matches!(status, "passed" | "failed" | "error" | "skipped") {
            return Err(CompareError::invalid_case(format!(
                "`{}` step {} has unknown status `{status}`",
                paths::display(report_path),
                idx + 1
            )));
        }
        let duration_ms = obj.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);

        let mut assertions = AssertionCounts::default();
        if let Some(raw_assertions) = obj.get("assertions")
            && !raw_assertions.is_null()
        {
            let Some(list) = raw_assertions.as_array() else {
                return Err(CompareError::invalid_case(format!(
                    "`{}` step {} `assertions` is not an array",
                    paths::display(report_path),
                    idx + 1
                )));
            };
            for assertion in list {
                let Some(passed) = assertion.get("passed").and_then(Value::as_bool) else {
                    return Err(CompareError::invalid_case(format!(
                        "`{}` step {} has an assertion without boolean `passed`",
                        paths::display(report_path),
                        idx + 1
                    )));
                };
                assertions.total += 1;
                if passed {
                    assertions.passed += 1;
                } else {
                    assertions.failed += 1;
                }
            }
        }

        let error_message = obj
            .get("error")
            .and_then(|err| err.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string);

        out.push(RecordedStep {
            name,
            status: status.to_string(),
            duration_ms,
            assertions,
            error_message,
        });
    }
    Ok(out)
}

fn diff_steps(base: &RecordedCase, cand: &RecordedCase) -> Vec<StepDiff> {
    let mut diffs = Vec::new();
    diff_phase("steps", &base.steps, &cand.steps, &mut diffs);
    diff_phase("finally", &base.finally, &cand.finally, &mut diffs);
    diffs
}

fn diff_added_steps(cand: &RecordedCase) -> Vec<StepDiff> {
    let mut diffs = Vec::new();
    for (phase, list) in [("steps", &cand.steps), ("finally", &cand.finally)] {
        for (idx, step) in list.iter().enumerate() {
            if step.status != "passed" {
                diffs.push(StepDiff {
                    phase,
                    step_number: idx + 1,
                    baseline_name: None,
                    candidate_name: Some(step.name.clone()),
                    baseline_status: None,
                    candidate_status: Some(step.status.clone()),
                    baseline_duration_ms: None,
                    candidate_duration_ms: Some(step.duration_ms),
                    duration_delta_ms: None,
                    baseline_assertions: None,
                    candidate_assertions: Some(step.assertions),
                    candidate_error: step.error_message.clone(),
                });
            }
        }
    }
    diffs
}

fn diff_phase(
    phase: &'static str,
    base_steps: &[RecordedStep],
    cand_steps: &[RecordedStep],
    diffs: &mut Vec<StepDiff>,
) {
    let max_len = base_steps.len().max(cand_steps.len());
    for idx in 0..max_len {
        let b = base_steps.get(idx);
        let c = cand_steps.get(idx);
        let changed = match (b, c) {
            (Some(bs), Some(cs)) => {
                bs.name != cs.name || bs.status != cs.status || bs.assertions != cs.assertions
            }
            _ => true,
        };
        if changed {
            diffs.push(StepDiff {
                phase,
                step_number: idx + 1,
                baseline_name: b.map(|s| s.name.clone()),
                candidate_name: c.map(|s| s.name.clone()),
                baseline_status: b.map(|s| s.status.clone()),
                candidate_status: c.map(|s| s.status.clone()),
                baseline_duration_ms: b.map(|s| s.duration_ms),
                candidate_duration_ms: c.map(|s| s.duration_ms),
                duration_delta_ms: match (b, c) {
                    (Some(bs), Some(cs)) => Some(signed_u64_delta(bs.duration_ms, cs.duration_ms)),
                    _ => None,
                },
                baseline_assertions: b.map(|s| s.assertions),
                candidate_assertions: c.map(|s| s.assertions),
                candidate_error: c.and_then(|s| s.error_message.clone()),
            });
        }
    }
}

fn read_json_object(path: &Path) -> Result<serde_json::Map<String, Value>, CompareError> {
    let text = fs::read_to_string(path).map_err(|error| {
        CompareError::invalid_case(format!(
            "could not read `{}`: {error}",
            paths::display(path)
        ))
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|error| {
        CompareError::invalid_case(format!(
            "`{}` is not valid JSON: {error}",
            paths::display(path)
        ))
    })?;
    value.as_object().cloned().ok_or_else(|| {
        CompareError::invalid_case(format!(
            "`{}` must contain a JSON object",
            paths::display(path)
        ))
    })
}

fn required_non_empty_str(
    map: &serde_json::Map<String, Value>,
    field: &str,
    path: &Path,
) -> Result<String, CompareError> {
    map.get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            CompareError::invalid_case(format!(
                "`{}` is missing a non-empty string `{field}`",
                paths::display(path)
            ))
        })
}

fn required_case_status(
    map: &serde_json::Map<String, Value>,
    field: &str,
    path: &Path,
) -> Result<String, CompareError> {
    let status = required_non_empty_str(map, field, path)?;
    if !matches!(status.as_str(), "passed" | "failed" | "error" | "timeout") {
        return Err(CompareError::invalid_case(format!(
            "`{}` has invalid `{field}` value `{status}`; expected passed, failed, error, or timeout",
            paths::display(path)
        )));
    }
    Ok(status)
}

/// Normalizes separator style and redundant `./` segments so bundles recorded
/// on Windows and Unix match cleanly by `(case_file, case_name)`.
pub fn normalize_case_file(raw: &str) -> String {
    let replaced = raw.replace('\\', "/");
    let has_leading_slash = replaced.starts_with('/');
    let mut parts = Vec::new();
    for segment in replaced.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        parts.push(segment);
    }
    if has_leading_slash {
        format!("/{}", parts.join("/"))
    } else {
        parts.join("/")
    }
}

fn case_position(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    let digits = name.split('-').next()?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn signed_u64_delta(before: u64, after: u64) -> i64 {
    let diff = i128::from(after) - i128::from(before);
    i64::try_from(diff).unwrap_or(if diff < 0 { i64::MIN } else { i64::MAX })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_windows_and_unix_case_file_paths() {
        assert_eq!(
            normalize_case_file(r".\cases\auth\login.yaml"),
            "cases/auth/login.yaml"
        );
        assert_eq!(
            normalize_case_file("./cases//auth/login.yaml"),
            "cases/auth/login.yaml"
        );
        assert_eq!(
            normalize_case_file("/workspace/cases/login.yaml"),
            "/workspace/cases/login.yaml"
        );
    }
}
