//! Whole-case repeat execution model and flakiness classification.
//!
//! Running a suite once cannot distinguish a deterministic assertion failure
//! from a timing flake. When `--repeat N` (`N > 1`) is requested, the runner
//! executes the selected cases `N` times and classifies each case as
//! `stable_pass`, `stable_fail`, or `flaky`.

use std::path::PathBuf;
use std::time::Duration;

use crate::core::case::CaseError;
use crate::core::report::{CaseReport, CaseStatus, ProblemKind, RunReport};
use crate::exit::ExitCode;

/// Flakiness verdict for a case executed across multiple attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatClassification {
    /// Passed on every attempt (`N` of `N`).
    StablePass,
    /// Did not pass on any attempt (`0` of `N`).
    StableFail,
    /// Passed on at least one attempt and failed/errored on at least one (`1..N-1` of `N`).
    Flaky,
}

impl RepeatClassification {
    /// Machine-readable classification identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StablePass => "stable_pass",
            Self::StableFail => "stable_fail",
            Self::Flaky => "flaky",
        }
    }

    /// Uppercase label used in human-readable text reports.
    pub fn label(self) -> &'static str {
        match self {
            Self::StablePass => "STABLE_PASS",
            Self::StableFail => "STABLE_FAIL",
            Self::Flaky => "FLAKY",
        }
    }
}

/// Aggregated multi-attempt outcome for one selected case.
#[derive(Debug, Clone)]
pub struct RepeatedCaseReport {
    /// File the case came from.
    pub file: PathBuf,
    /// Case name.
    pub name: String,
    /// Declared project, if any.
    pub project: Option<String>,
    /// Declared suite, if any.
    pub suite: Option<String>,
    /// Declared layer, if any.
    pub layer: Option<String>,
    /// Declared tags.
    pub tags: Vec<String>,
    /// Total number of attempts executed (`N`).
    pub repeat_count: usize,
    /// Number of attempts that passed.
    pub passed_runs: usize,
    /// Number of attempts that failed or errored.
    pub failed_runs: usize,
    /// Flakiness classification derived from `passed_runs` and `repeat_count`.
    pub classification: RepeatClassification,
    /// Sum of attempt durations for this case.
    pub duration: Duration,
    /// Per-attempt case reports in execution order (`1..=N`).
    pub attempts: Vec<CaseReport>,
}

/// Counts derived from a [`RepeatReport`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepeatSummary {
    /// Number of whole-suite attempts requested.
    pub repeat_count: usize,
    /// Number of selected cases.
    pub cases: usize,
    /// Number of discovered cases excluded by filters.
    pub filtered: usize,
    /// Selected cases classified as `stable_pass`.
    pub stable_pass: usize,
    /// Selected cases classified as `stable_fail`.
    pub stable_fail: usize,
    /// Selected cases classified as `flaky`.
    pub flaky: usize,
    /// Total individual case executions (`cases * repeat_count`).
    pub total_attempts: usize,
    /// Individual case executions that passed.
    pub passed_attempts: usize,
    /// Individual case executions that failed or errored.
    pub failed_attempts: usize,
}

/// Completed report of a `da run --repeat N` (`N > 1`) invocation.
#[derive(Debug, Clone)]
pub struct RepeatReport {
    /// Number of attempts executed.
    pub repeat_count: usize,
    /// Selected cases in deterministic discovery order.
    pub cases: Vec<RepeatedCaseReport>,
    /// Number of discovered cases excluded by filters.
    pub filtered: usize,
    /// Total wall-clock duration across all attempts.
    pub duration: Duration,
    /// Any bundle or summary write errors encountered across attempts.
    pub bundle_errors: Vec<String>,
    /// Worst failure problem kind observed across all attempts.
    problem_kinds: Vec<ProblemKind>,
}

impl RepeatReport {
    /// Aggregates `repeat_count` completed [`RunReport`]s into one [`RepeatReport`].
    pub fn from_attempts(
        repeat_count: usize,
        reports: Vec<RunReport>,
        duration: Duration,
        mut bundle_errors: Vec<String>,
    ) -> Self {
        let filtered = reports.first().map_or(0, |report| report.filtered);
        let case_count = reports.first().map_or(0, |report| report.cases.len());

        let mut problem_kinds = Vec::new();
        for report in &reports {
            problem_kinds.extend(report.all_problem_kinds());
            bundle_errors.extend(report.bundle_errors.iter().cloned());
        }

        let mut cases = Vec::with_capacity(case_count);
        for case_idx in 0..case_count {
            let mut attempts = Vec::with_capacity(reports.len());
            let mut passed_runs = 0usize;
            let mut case_duration = Duration::ZERO;

            for report in &reports {
                if let Some(case_report) = report.cases.get(case_idx) {
                    if case_report.status == CaseStatus::Passed {
                        passed_runs += 1;
                    }
                    case_duration += case_report.duration;
                    attempts.push(case_report.clone());
                }
            }

            let total_runs = attempts.len();
            let failed_runs = total_runs.saturating_sub(passed_runs);
            let classification = if passed_runs == total_runs {
                RepeatClassification::StablePass
            } else if passed_runs == 0 {
                RepeatClassification::StableFail
            } else {
                RepeatClassification::Flaky
            };

            if let Some(first) = attempts.first() {
                cases.push(RepeatedCaseReport {
                    file: first.file.clone(),
                    name: first.name.clone(),
                    project: first.project.clone(),
                    suite: first.suite.clone(),
                    layer: first.layer.clone(),
                    tags: first.tags.clone(),
                    repeat_count,
                    passed_runs,
                    failed_runs,
                    classification,
                    duration: case_duration,
                    attempts,
                });
            }
        }

        Self {
            repeat_count,
            cases,
            filtered,
            duration,
            bundle_errors,
            problem_kinds,
        }
    }

    /// Derives aggregate counts across all repeated cases.
    pub fn summary(&self) -> RepeatSummary {
        let mut summary = RepeatSummary {
            repeat_count: self.repeat_count,
            cases: self.cases.len(),
            filtered: self.filtered,
            ..RepeatSummary::default()
        };
        for case in &self.cases {
            match case.classification {
                RepeatClassification::StablePass => summary.stable_pass += 1,
                RepeatClassification::StableFail => summary.stable_fail += 1,
                RepeatClassification::Flaky => summary.flaky += 1,
            }
            summary.total_attempts += case.attempts.len();
            summary.passed_attempts += case.passed_runs;
            summary.failed_attempts += case.failed_runs;
        }
        summary
    }

    /// Computes the process exit code across all attempts.
    ///
    /// Returns [`ExitCode::Success`] only when every selected case is
    /// [`RepeatClassification::StablePass`]; otherwise preserves the highest
    /// precedence failure exit code (`InvalidCase` > `Environment` > `Timeout`
    /// > `TestFailure`).
    pub fn exit_code(&self) -> ExitCode {
        if self.problem_kinds.contains(&ProblemKind::InvalidInput) {
            return ExitCode::InvalidCase;
        }
        if self.problem_kinds.contains(&ProblemKind::Environment) {
            return ExitCode::Environment;
        }
        if self.problem_kinds.contains(&ProblemKind::Timeout) {
            return ExitCode::Timeout;
        }
        if !self.problem_kinds.is_empty()
            || self
                .cases
                .iter()
                .any(|case| case.classification != RepeatClassification::StablePass)
        {
            return ExitCode::TestFailure;
        }
        ExitCode::Success
    }
}

/// Top-level outcome of `da run --repeat N` (`N > 1`).
#[derive(Debug, Clone)]
pub enum RepeatOutcome {
    /// Nothing ran because at least one case failed preflight or parsing.
    Rejected {
        /// Requested repeat count.
        repeat_count: usize,
        /// Validation errors.
        errors: Vec<CaseError>,
    },
    /// Nothing ran because the filters selected no case.
    NoMatch {
        /// Requested repeat count.
        repeat_count: usize,
        /// Actionable filter mismatch message.
        message: String,
    },
    /// All `N` attempts executed.
    Completed(RepeatReport),
}

impl RepeatOutcome {
    /// Exit code for this repeated run outcome.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Rejected { .. } => ExitCode::InvalidCase,
            Self::NoMatch { .. } => ExitCode::Usage,
            Self::Completed(report) => report.exit_code(),
        }
    }

    /// Whether every selected case achieved `stable_pass`.
    pub fn is_ok(&self) -> bool {
        self.exit_code() == ExitCode::Success
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::core::report::{Problem, StepReport, StepStatus};

    fn sample_case(name: &str, status: CaseStatus, kind: Option<ProblemKind>) -> CaseReport {
        let error = kind.map(|k| Problem {
            kind: k,
            message: "problem".to_string(),
        });
        CaseReport {
            file: PathBuf::from(format!("cases/{name}.yaml")),
            case_sha256: None,
            name: name.to_string(),
            project: None,
            suite: None,
            layer: None,
            tags: Vec::new(),
            overridden_vars: Vec::new(),
            status,
            duration: Duration::from_millis(2),
            steps: vec![StepReport {
                name: "sample.step".to_string(),
                status: match status {
                    CaseStatus::Passed => StepStatus::Passed,
                    CaseStatus::Failed => StepStatus::Failed,
                    CaseStatus::Error => StepStatus::Error,
                },
                duration: Duration::from_millis(2),
                args: None,
                result: None,
                target_bounds: None,
                saved: BTreeMap::new(),
                assertions: Vec::new(),
                error: error.clone(),
            }],
            finally: Vec::new(),
            error,
            close_errors: Vec::new(),
            bundle: None,
            local_only: false,
            local_only_reason: None,
        }
    }

    #[test]
    fn classifies_stable_pass_flaky_and_stable_fail_and_exits_nonzero_on_flaky() {
        let attempt_1 = RunReport {
            cases: vec![
                sample_case("pass", CaseStatus::Passed, None),
                sample_case("flaky", CaseStatus::Passed, None),
                sample_case("fail", CaseStatus::Failed, Some(ProblemKind::Assertion)),
            ],
            filtered: 1,
            duration: Duration::from_millis(6),
            bundle_errors: Vec::new(),
        };
        let attempt_2 = RunReport {
            cases: vec![
                sample_case("pass", CaseStatus::Passed, None),
                sample_case("flaky", CaseStatus::Failed, Some(ProblemKind::Assertion)),
                sample_case("fail", CaseStatus::Failed, Some(ProblemKind::Assertion)),
            ],
            filtered: 1,
            duration: Duration::from_millis(6),
            bundle_errors: Vec::new(),
        };
        let attempt_3 = attempt_2.clone();

        let report = RepeatReport::from_attempts(
            3,
            vec![attempt_1, attempt_2, attempt_3],
            Duration::from_millis(18),
            Vec::new(),
        );
        let summary = report.summary();
        assert_eq!(summary.cases, 3);
        assert_eq!(summary.stable_pass, 1);
        assert_eq!(summary.flaky, 1);
        assert_eq!(summary.stable_fail, 1);
        assert_eq!(
            report.cases[0].classification,
            RepeatClassification::StablePass
        );
        assert_eq!(report.cases[1].classification, RepeatClassification::Flaky);
        assert_eq!(report.cases[1].passed_runs, 1);
        assert_eq!(report.cases[1].failed_runs, 2);
        assert_eq!(
            report.cases[2].classification,
            RepeatClassification::StableFail
        );
        assert_eq!(report.exit_code(), ExitCode::TestFailure);
    }

    #[test]
    fn preserves_worst_failure_exit_code_across_attempts() {
        let attempt_1 = RunReport {
            cases: vec![sample_case("flaky", CaseStatus::Passed, None)],
            filtered: 0,
            duration: Duration::from_millis(2),
            bundle_errors: Vec::new(),
        };
        let attempt_2 = RunReport {
            cases: vec![sample_case(
                "flaky",
                CaseStatus::Error,
                Some(ProblemKind::Timeout),
            )],
            filtered: 0,
            duration: Duration::from_millis(2),
            bundle_errors: Vec::new(),
        };
        let report = RepeatReport::from_attempts(
            2,
            vec![attempt_1, attempt_2],
            Duration::from_millis(4),
            Vec::new(),
        );
        assert_eq!(report.exit_code(), ExitCode::Timeout);
    }
}
