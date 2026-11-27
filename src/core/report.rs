//! Result types shared by the runner, the event bus and the renderer.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::core::assert::AssertionReport;
use crate::core::case::{CaseError, CaseErrorKind};
use crate::exit::ExitCode;

/// The state of a finished case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseStatus {
    /// Every step ran and every assertion passed.
    Passed,
    /// An assertion did not pass.
    Failed,
    /// A step could not run to completion.
    Error,
}

impl CaseStatus {
    /// The name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
        }
    }
}

/// The state of a finished step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// The step ran and every assertion passed.
    Passed,
    /// The step ran and an assertion did not pass.
    Failed,
    /// The step could not run to completion.
    Error,
    /// The case stopped before this step.
    Skipped,
}

impl StepStatus {
    /// The name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }
}

/// The category of a problem, which decides the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    /// An assertion did not pass.
    Assertion,
    /// The case or a step's arguments are invalid.
    InvalidInput,
    /// The environment failed, for example an unreachable target.
    Environment,
    /// A step exceeded its time budget.
    Timeout,
    /// The step ran but could not finish its work.
    Execution,
}

impl ProblemKind {
    /// The name used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Assertion => "assertion",
            Self::InvalidInput => "invalid_input",
            Self::Environment => "environment",
            Self::Timeout => "timeout",
            Self::Execution => "execution",
        }
    }
}

/// What went wrong and why it matters.
#[derive(Debug, Clone)]
pub struct Problem {
    /// Category that decides the exit code.
    pub kind: ProblemKind,
    /// Message that tells the user what happened.
    pub message: String,
}

/// One step of a case report.
#[derive(Debug, Clone)]
pub struct StepReport {
    /// Registered step name.
    pub name: String,
    /// Step state.
    pub status: StepStatus,
    /// How long the step took.
    pub duration: Duration,
    /// Redacted resolved arguments passed to this step, when interpolation succeeded.
    pub args: Option<Value>,
    /// The JSON result, when the step produced one.
    pub result: Option<Value>,
    /// Redacted UI action geometry (`{ x, y, width, height, viewport, ... }`), when recorded.
    pub target_bounds: Option<Value>,
    /// Redacted variables saved by `save:` from this step's result.
    pub saved: BTreeMap<String, String>,
    /// Every assertion evaluated for this step.
    pub assertions: Vec<AssertionReport>,
    /// The problem that stopped the case, if this step caused it.
    pub error: Option<Problem>,
}

/// Returns the Draft 2020-12 JSON Schema for a step's optional `target_bounds` payload.
pub fn target_bounds_schema() -> Value {
    json!({
        "$schema": crate::CASE_SCHEMA_DIALECT,
        "title": "da step target_bounds",
        "description": "UI target geometry recorded when a step resolves an element or executes explicit pointer coordinates.",
        "type": "object",
        "additionalProperties": false,
        "required": ["x", "y", "width", "height"],
        "properties": {
            "x": { "type": "number" },
            "y": { "type": "number" },
            "width": { "type": "number", "minimum": 0 },
            "height": { "type": "number", "minimum": 0 },
            "point": { "$ref": "#/$defs/point" },
            "viewport": { "$ref": "#/$defs/viewport" },
            "from": { "$ref": "#/$defs/point" },
            "to": { "$ref": "#/$defs/point" },
            "from_bounds": { "$ref": "#/$defs/rect" },
            "to_bounds": { "$ref": "#/$defs/rect" }
        },
        "$defs": {
            "point": {
                "type": "object",
                "additionalProperties": false,
                "required": ["x", "y"],
                "properties": {
                    "x": { "type": "number" },
                    "y": { "type": "number" }
                }
            },
            "rect": {
                "type": "object",
                "additionalProperties": false,
                "required": ["x", "y", "width", "height"],
                "properties": {
                    "x": { "type": "number" },
                    "y": { "type": "number" },
                    "width": { "type": "number", "minimum": 0 },
                    "height": { "type": "number", "minimum": 0 }
                }
            },
            "viewport": {
                "type": "object",
                "additionalProperties": false,
                "required": ["width", "height"],
                "properties": {
                    "width": { "type": "integer", "minimum": 1 },
                    "height": { "type": "integer", "minimum": 1 }
                }
            }
        }
    })
}

/// Validates a `target_bounds` value against [`target_bounds_schema`].
pub fn validate_target_bounds(value: &Value) -> Result<(), String> {
    let Some(obj) = value.as_object() else {
        return Err("target_bounds must be a JSON object".to_string());
    };
    for key in obj.keys() {
        if !matches!(
            key.as_str(),
            "x" | "y"
                | "width"
                | "height"
                | "point"
                | "viewport"
                | "from"
                | "to"
                | "from_bounds"
                | "to_bounds"
        ) {
            return Err(format!("unexpected target_bounds field `{key}`"));
        }
    }
    validate_rect_fields(obj, "target_bounds")?;
    for field in ["point", "from", "to"] {
        if let Some(pt) = obj.get(field) {
            validate_point_value(pt, field)?;
        }
    }
    for field in ["from_bounds", "to_bounds"] {
        if let Some(rect) = obj.get(field) {
            let Some(rect_obj) = rect.as_object() else {
                return Err(format!("target_bounds.{field} must be an object"));
            };
            for key in rect_obj.keys() {
                if !matches!(key.as_str(), "x" | "y" | "width" | "height") {
                    return Err(format!("unexpected field `{key}` in target_bounds.{field}"));
                }
            }
            validate_rect_fields(rect_obj, field)?;
        }
    }
    if let Some(viewport) = obj.get("viewport") {
        let Some(vp) = viewport.as_object() else {
            return Err("target_bounds.viewport must be an object".to_string());
        };
        for key in vp.keys() {
            if !matches!(key.as_str(), "width" | "height") {
                return Err(format!(
                    "unexpected field `{key}` in target_bounds.viewport"
                ));
            }
        }
        for dim in ["width", "height"] {
            let Some(v) = vp.get(dim).and_then(Value::as_u64) else {
                return Err(format!(
                    "target_bounds.viewport.{dim} must be a positive integer"
                ));
            };
            if v < 1 {
                return Err(format!("target_bounds.viewport.{dim} must be at least 1"));
            }
        }
    }
    Ok(())
}

fn validate_rect_fields(obj: &serde_json::Map<String, Value>, context: &str) -> Result<(), String> {
    for coord in ["x", "y"] {
        let Some(n) = obj.get(coord).and_then(Value::as_f64) else {
            return Err(format!("{context}.{coord} must be a finite number"));
        };
        if !n.is_finite() {
            return Err(format!("{context}.{coord} must be finite"));
        }
    }
    for dim in ["width", "height"] {
        let Some(n) = obj.get(dim).and_then(Value::as_f64) else {
            return Err(format!("{context}.{dim} must be a non-negative number"));
        };
        if !n.is_finite() || n < 0.0 {
            return Err(format!("{context}.{dim} must be >= 0"));
        }
    }
    Ok(())
}

fn validate_point_value(value: &Value, context: &str) -> Result<(), String> {
    let Some(obj) = value.as_object() else {
        return Err(format!("target_bounds.{context} must be an object"));
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "x" | "y") {
            return Err(format!(
                "unexpected field `{key}` in target_bounds.{context}"
            ));
        }
    }
    for coord in ["x", "y"] {
        let Some(n) = obj.get(coord).and_then(Value::as_f64) else {
            return Err(format!("target_bounds.{context}.{coord} must be a number"));
        };
        if !n.is_finite() {
            return Err(format!("target_bounds.{context}.{coord} must be finite"));
        }
    }
    Ok(())
}

/// One case of a run.
#[derive(Debug, Clone)]
pub struct CaseReport {
    /// File the case came from.
    pub file: PathBuf,
    /// Lowercase hex SHA-256 of the source case file bytes, when known.
    pub case_sha256: Option<String>,
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
    /// Sorted names of CLI or `--vars-file` variables supplied for this run.
    pub overridden_vars: Vec<String>,
    /// Case state.
    pub status: CaseStatus,
    /// How long the case took.
    pub duration: Duration,
    /// Every step, including the ones skipped after a failure.
    pub steps: Vec<StepReport>,
    /// Cleanup steps executed after `steps`.
    pub finally: Vec<StepReport>,
    /// The problem that made the case fail, if any.
    pub error: Option<Problem>,
    /// Sessions that could not be closed, in the order they were closed.
    ///
    /// A case that passed becomes an environment error when this list is not
    /// empty. A case that already failed or errored keeps its own status and
    /// problem; these messages are attached to the report without changing the
    /// verdict.
    pub close_errors: Vec<String>,
    /// Failure bundle directory, when one was written for this case.
    pub bundle: Option<PathBuf>,
    /// Whether any session used unmanaged state whose evidence must stay local-only.
    pub local_only: bool,
    /// Why evidence for this case must stay local-only, if applicable.
    pub local_only_reason: Option<String>,
}

/// Every case of a run.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// Case reports in execution order.
    pub cases: Vec<CaseReport>,
    /// Cases that the filters excluded from this run.
    pub filtered: usize,
    /// How long the run took.
    pub duration: Duration,
    /// Failure bundles that could not be written.
    ///
    /// A bundle is evidence for the reader, not part of the verdict, so these
    /// are reported without changing the run's exit code.
    pub bundle_errors: Vec<String>,
}

/// Counts derived from a [`RunReport`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Number of cases.
    pub cases: usize,
    /// Cases that the filters excluded from this run.
    pub filtered: usize,
    /// Cases that passed.
    pub cases_passed: usize,
    /// Cases that failed an assertion.
    pub cases_failed: usize,
    /// Cases that could not run to completion.
    pub cases_errored: usize,
    /// Number of steps, including skipped ones.
    pub steps: usize,
    /// Steps that passed.
    pub steps_passed: usize,
    /// Steps that failed an assertion.
    pub steps_failed: usize,
    /// Steps that could not run to completion.
    pub steps_errored: usize,
    /// Steps skipped after a failure.
    pub steps_skipped: usize,
}

impl RunReport {
    /// Derives the counts reported to the user.
    pub fn summary(&self) -> Summary {
        let mut summary = Summary {
            cases: self.cases.len(),
            filtered: self.filtered,
            ..Summary::default()
        };
        for case in &self.cases {
            match case.status {
                CaseStatus::Passed => summary.cases_passed += 1,
                CaseStatus::Failed => summary.cases_failed += 1,
                CaseStatus::Error => summary.cases_errored += 1,
            }
            for step in case.steps.iter().chain(&case.finally) {
                summary.steps += 1;
                match step.status {
                    StepStatus::Passed => summary.steps_passed += 1,
                    StepStatus::Failed => summary.steps_failed += 1,
                    StepStatus::Error => summary.steps_errored += 1,
                    StepStatus::Skipped => summary.steps_skipped += 1,
                }
            }
        }
        summary
    }

    /// Collects every non-skipped problem kind recorded across cases and steps.
    pub(crate) fn all_problem_kinds(&self) -> Vec<ProblemKind> {
        self.problem_kinds()
    }

    fn problem_kinds(&self) -> Vec<ProblemKind> {
        let mut kinds = Vec::new();
        for case in &self.cases {
            if let Some(problem) = &case.error {
                kinds.push(problem.kind);
            }
            for step in case.steps.iter().chain(&case.finally) {
                // A skipped step keeps the problem that skipped it, so that the
                // reader can see why an optional step did not run. That problem
                // is not part of the run's verdict: the case continued past it.
                if step.status == StepStatus::Skipped {
                    continue;
                }
                if let Some(problem) = &step.error {
                    kinds.push(problem.kind);
                }
            }
        }
        kinds
    }
}

/// What `da run` produced.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// Nothing ran because at least one case is invalid.
    Rejected {
        /// One entry per invalid case file.
        errors: Vec<CaseError>,
    },
    /// Nothing ran because the filters selected no case.
    NoMatch {
        /// Names the filters and the values the discovered cases use.
        message: String,
    },
    /// Every selected case ran.
    Completed(RunReport),
}

impl RunOutcome {
    /// The exit code for this outcome.
    ///
    /// When several problems occur in one run the most fundamental one wins:
    /// invalid input, then environment, then timeout, then a failed assertion.
    pub fn exit_code(&self) -> ExitCode {
        let report = match self {
            Self::Rejected { errors } => return rejected_exit_code(errors),
            Self::NoMatch { .. } => return ExitCode::Usage,
            Self::Completed(report) => report,
        };
        let kinds = report.problem_kinds();
        if kinds.contains(&ProblemKind::InvalidInput) {
            return ExitCode::InvalidCase;
        }
        if kinds.contains(&ProblemKind::Environment) {
            return ExitCode::Environment;
        }
        if kinds.contains(&ProblemKind::Timeout) {
            return ExitCode::Timeout;
        }
        if !kinds.is_empty() {
            return ExitCode::TestFailure;
        }
        ExitCode::Success
    }

    /// Whether every case ran and passed.
    pub fn is_ok(&self) -> bool {
        self.exit_code() == ExitCode::Success
    }
}

/// Maps rejected load or preflight errors to the process exit code using the
/// same precedence as [`RunOutcome::exit_code`]: invalid input, then
/// environment, then timeout.
pub fn rejected_exit_code(errors: &[CaseError]) -> ExitCode {
    if errors
        .iter()
        .any(|error| error.kind == CaseErrorKind::InvalidInput)
    {
        return ExitCode::InvalidCase;
    }
    if errors
        .iter()
        .any(|error| error.kind == CaseErrorKind::Environment)
    {
        return ExitCode::Environment;
    }
    if errors
        .iter()
        .any(|error| error.kind == CaseErrorKind::Timeout)
    {
        return ExitCode::Timeout;
    }
    ExitCode::InvalidCase
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_with(kind: ProblemKind) -> RunReport {
        RunReport {
            cases: vec![CaseReport {
                file: PathBuf::from("case.yaml"),
                case_sha256: None,
                name: "case".to_string(),
                project: None,
                suite: None,
                layer: None,
                tags: Vec::new(),
                overridden_vars: Vec::new(),
                status: CaseStatus::Error,
                duration: Duration::ZERO,
                steps: vec![StepReport {
                    name: "test.step".to_string(),
                    status: StepStatus::Error,
                    duration: Duration::ZERO,
                    args: None,
                    result: None,
                    target_bounds: None,
                    saved: BTreeMap::new(),
                    assertions: Vec::new(),
                    error: Some(Problem {
                        kind,
                        message: "problem".to_string(),
                    }),
                }],
                finally: Vec::new(),
                error: Some(Problem {
                    kind,
                    message: "problem".to_string(),
                }),
                close_errors: Vec::new(),
                bundle: None,
                local_only: false,
                local_only_reason: None,
            }],
            filtered: 0,
            duration: Duration::ZERO,
            bundle_errors: Vec::new(),
        }
    }

    #[test]
    fn exit_code_prefers_invalid_input_over_other_problems() {
        let mut report = report_with(ProblemKind::Environment);
        report
            .cases
            .push(report_with(ProblemKind::Timeout).cases.remove(0));
        report
            .cases
            .push(report_with(ProblemKind::InvalidInput).cases.remove(0));

        assert_eq!(
            RunOutcome::Completed(report).exit_code(),
            ExitCode::InvalidCase
        );
    }

    #[test]
    fn exit_code_prefers_environment_over_timeout_and_assertion() {
        let mut report = report_with(ProblemKind::Assertion);
        report
            .cases
            .push(report_with(ProblemKind::Timeout).cases.remove(0));
        report
            .cases
            .push(report_with(ProblemKind::Environment).cases.remove(0));

        assert_eq!(
            RunOutcome::Completed(report).exit_code(),
            ExitCode::Environment
        );
    }

    #[test]
    fn exit_code_prefers_timeout_over_a_failed_assertion() {
        let mut report = report_with(ProblemKind::Assertion);
        report
            .cases
            .push(report_with(ProblemKind::Timeout).cases.remove(0));

        assert_eq!(RunOutcome::Completed(report).exit_code(), ExitCode::Timeout);
    }

    #[test]
    fn an_empty_report_is_a_success() {
        let report = RunReport {
            cases: Vec::new(),
            filtered: 0,
            duration: Duration::ZERO,
            bundle_errors: Vec::new(),
        };

        assert_eq!(RunOutcome::Completed(report).exit_code(), ExitCode::Success);
    }

    #[test]
    fn a_skipped_step_does_not_decide_the_exit_code() {
        // An optional step that did not reach its assertions keeps its problem
        // in the report, so the reader can see why it was skipped. That problem
        // must not turn a passing case into a failed run.
        let mut case = report_with(ProblemKind::Assertion).cases.remove(0);
        case.status = CaseStatus::Passed;
        case.error = None;
        case.steps[0].status = StepStatus::Skipped;
        let report = RunReport {
            cases: vec![case],
            filtered: 0,
            duration: Duration::ZERO,
            bundle_errors: Vec::new(),
        };

        assert_eq!(
            report.summary().steps_skipped,
            1,
            "the skipped step is still counted"
        );
        assert_eq!(RunOutcome::Completed(report).exit_code(), ExitCode::Success);
    }

    #[test]
    fn filters_that_match_nothing_are_a_usage_error() {
        let outcome = RunOutcome::NoMatch {
            message: "no case matched --tag \"smoke\"".to_string(),
        };

        assert_eq!(outcome.exit_code(), ExitCode::Usage);
        assert!(!outcome.is_ok());
    }

    #[test]
    fn the_summary_carries_the_filtered_count() {
        let mut report = report_with(ProblemKind::Assertion);
        report.filtered = 3;

        let summary = report.summary();

        assert_eq!(summary.filtered, 3);
        assert_eq!(summary.cases, 1, "filtered cases are not run cases");
    }

    #[test]
    fn summary_and_exit_code_include_finally_steps() {
        let mut report = report_with(ProblemKind::Assertion);
        report.cases[0].finally = vec![
            StepReport {
                name: "test.cleanup".to_string(),
                status: StepStatus::Passed,
                duration: Duration::ZERO,
                args: None,
                result: None,
                target_bounds: None,
                saved: BTreeMap::new(),
                assertions: Vec::new(),
                error: None,
            },
            StepReport {
                name: "test.broken_cleanup".to_string(),
                status: StepStatus::Error,
                duration: Duration::ZERO,
                args: None,
                result: None,
                target_bounds: None,
                saved: BTreeMap::new(),
                assertions: Vec::new(),
                error: Some(Problem {
                    kind: ProblemKind::Environment,
                    message: "cleanup target down".to_string(),
                }),
            },
            StepReport {
                name: "test.optional_cleanup".to_string(),
                status: StepStatus::Skipped,
                duration: Duration::ZERO,
                args: None,
                result: None,
                target_bounds: None,
                saved: BTreeMap::new(),
                assertions: Vec::new(),
                error: Some(Problem {
                    kind: ProblemKind::Execution,
                    message: "optional skip".to_string(),
                }),
            },
        ];

        let summary = report.summary();
        assert_eq!(summary.steps, 4);
        assert_eq!(summary.steps_passed, 1);
        assert_eq!(summary.steps_errored, 2);
        assert_eq!(summary.steps_skipped, 1);
        assert_eq!(
            RunOutcome::Completed(report).exit_code(),
            ExitCode::Environment
        );
    }

    #[test]
    fn rejected_exit_code_follows_invalid_input_then_environment_then_timeout() {
        let timeout = CaseError {
            file: PathBuf::from("a.yaml"),
            message: "timed out".to_string(),
            kind: CaseErrorKind::Timeout,
        };
        let environment = CaseError {
            file: PathBuf::from("b.yaml"),
            message: "crashed".to_string(),
            kind: CaseErrorKind::Environment,
        };
        let invalid = CaseError::invalid(PathBuf::from("c.yaml"), "bad arg");

        assert_eq!(
            RunOutcome::Rejected {
                errors: vec![timeout.clone()],
            }
            .exit_code(),
            ExitCode::Timeout
        );
        assert_eq!(
            RunOutcome::Rejected {
                errors: vec![timeout.clone(), environment.clone()],
            }
            .exit_code(),
            ExitCode::Environment
        );
        assert_eq!(
            RunOutcome::Rejected {
                errors: vec![timeout, environment, invalid],
            }
            .exit_code(),
            ExitCode::InvalidCase
        );
    }
}
