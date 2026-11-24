//! Static assertion-strength comparison between candidate and baseline cases.
//!
//! An automated repair loop can make a failing suite pass by deleting cases,
//! removing assertions, loosening matchers, marking steps `optional: true`,
//! excluding cases via tags, widening retry budgets, or replacing structured
//! locators with fragile arguments that emit lint warnings. This module
//! compares parsed cases without running any step and reports every reduction
//! in contract strength.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::core::case::{self, Case, Expectation, LoadedCase, Matcher, Step};
use crate::core::filter::Filters;
use crate::core::paths;
use crate::core::registry::Registry;
use crate::core::secrets::Secrets;

/// Weight of one preserved expectation path when aligning baseline and candidate steps.
const EXPECT_PATH_WEIGHT: u64 = 1_000;

/// One static contract-strength reduction detected against a baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrengthFinding {
    /// Candidate file (or baseline file when the entire case was deleted).
    pub file: PathBuf,
    /// Baseline file paired with this finding.
    pub baseline_file: PathBuf,
    /// Case name declared by the case.
    pub case_name: String,
    /// Machine-readable category identifier.
    pub kind: &'static str,
    /// Actionable description of what was weakened.
    pub message: String,
    /// Whether `--allow-weaken` explicitly acknowledged this finding.
    pub acknowledged: bool,
}

/// Parameters for comparing candidate cases against a baseline set.
pub struct StrengthCheck<'a> {
    /// Baseline paths passed to `--against`.
    pub baseline_inputs: &'a [PathBuf],
    /// Parsed baseline cases.
    pub baseline_cases: &'a [LoadedCase],
    /// Candidate paths passed to `da lint`.
    pub candidate_inputs: &'a [PathBuf],
    /// Parsed candidate cases.
    pub candidate_cases: &'a [LoadedCase],
    /// Step registry used to evaluate step lint warnings.
    pub registry: &'a Registry,
    /// Active CLI filters (`--tag`, `--skip-tag`, `--project`, `--suite`, `--layer`).
    pub filters: &'a Filters,
    /// Whether `--allow-weaken <reason>` was supplied.
    pub acknowledged: bool,
}

/// Compares `candidate_cases` against `baseline_cases` and returns every
/// detected reduction in assertion or coverage strength.
pub fn compare_strength(check: &StrengthCheck<'_>, secrets: &mut Secrets) -> Vec<StrengthFinding> {
    let mut findings = Vec::new();
    let pairs = pair_cases(
        check.baseline_inputs,
        check.baseline_cases,
        check.candidate_inputs,
        check.candidate_cases,
    );

    for pair in pairs {
        match pair {
            CasePair::Deleted(baseline) => {
                findings.push(StrengthFinding {
                    file: baseline.path.clone(),
                    baseline_file: baseline.path.clone(),
                    case_name: baseline.case.name.clone(),
                    kind: "case_deleted",
                    message: format!(
                        "case `{}` present in baseline `{}` is missing from candidate cases",
                        baseline.case.name,
                        paths::display(&baseline.path)
                    ),
                    acknowledged: check.acknowledged,
                });
            }
            CasePair::Matched {
                baseline,
                candidate,
            } => {
                compare_paired_case(
                    baseline,
                    candidate,
                    check.registry,
                    check.filters,
                    check.acknowledged,
                    secrets,
                    &mut findings,
                );
            }
        }
    }

    findings
}

enum CasePair<'a> {
    Matched {
        baseline: &'a LoadedCase,
        candidate: &'a LoadedCase,
    },
    Deleted(&'a LoadedCase),
}

fn pair_cases<'a>(
    baseline_inputs: &[PathBuf],
    baseline_cases: &'a [LoadedCase],
    candidate_inputs: &[PathBuf],
    candidate_cases: &'a [LoadedCase],
) -> Vec<CasePair<'a>> {
    // When both sides name one file directly, pair those two files even if
    // their filenames or declared case names differ.
    if baseline_inputs.len() == 1
        && candidate_inputs.len() == 1
        && baseline_cases.len() == 1
        && candidate_cases.len() == 1
        && !baseline_inputs[0].is_dir()
        && !candidate_inputs[0].is_dir()
    {
        return vec![CasePair::Matched {
            baseline: &baseline_cases[0],
            candidate: &candidate_cases[0],
        }];
    }

    let base_keys: Vec<String> = baseline_cases
        .iter()
        .map(|loaded| relative_case_key(&loaded.path, baseline_inputs))
        .collect();
    let cand_keys: Vec<String> = candidate_cases
        .iter()
        .map(|loaded| relative_case_key(&loaded.path, candidate_inputs))
        .collect();

    let mut cand_used = vec![false; candidate_cases.len()];
    let mut matched: Vec<Option<&'a LoadedCase>> = vec![None; baseline_cases.len()];

    // Pass 1: exact (relative_path, case_name) match.
    for (b_idx, baseline) in baseline_cases.iter().enumerate() {
        if let Some(c_idx) = candidate_cases.iter().enumerate().position(|(idx, cand)| {
            !cand_used[idx]
                && cand_keys[idx] == base_keys[b_idx]
                && cand.case.name == baseline.case.name
        }) {
            cand_used[c_idx] = true;
            matched[b_idx] = Some(&candidate_cases[c_idx]);
        }
    }

    // Pass 2: same relative file path when `name:` inside the file changed.
    for (b_idx, slot) in matched.iter_mut().enumerate() {
        if slot.is_some() {
            continue;
        }
        if let Some(c_idx) = candidate_cases
            .iter()
            .enumerate()
            .position(|(idx, _)| !cand_used[idx] && cand_keys[idx] == base_keys[b_idx])
        {
            cand_used[c_idx] = true;
            *slot = Some(&candidate_cases[c_idx]);
        }
    }

    baseline_cases
        .iter()
        .zip(matched)
        .map(|(baseline, candidate)| match candidate {
            Some(candidate) => CasePair::Matched {
                baseline,
                candidate,
            },
            None => CasePair::Deleted(baseline),
        })
        .collect()
}

fn relative_case_key(file: &Path, roots: &[PathBuf]) -> String {
    let mut best: Option<&Path> = None;
    for root in roots {
        if root.is_dir()
            && let Ok(stripped) = file.strip_prefix(root)
            && best.is_none_or(|prev| stripped.as_os_str().len() < prev.as_os_str().len())
        {
            best = Some(stripped);
        }
    }
    if let Some(stripped) = best {
        return normalize_key(&paths::display(stripped));
    }
    file.file_name()
        .map(|name| normalize_key(&name.to_string_lossy()))
        .unwrap_or_else(|| normalize_key(&paths::display(file)))
}

fn normalize_key(raw: &str) -> String {
    let mut text = raw.replace('\\', "/");
    while let Some(rest) = text.strip_prefix("./") {
        text = rest.to_string();
    }
    text
}

#[allow(clippy::too_many_arguments)] // Keeps per-case comparison inputs explicit without an intermediate wrapper.
fn compare_paired_case(
    baseline: &LoadedCase,
    candidate: &LoadedCase,
    registry: &Registry,
    filters: &Filters,
    acknowledged: bool,
    secrets: &mut Secrets,
    findings: &mut Vec<StrengthFinding>,
) {
    check_filters(baseline, candidate, filters, acknowledged, findings);

    let (base_step_warnings, base_finally_warnings) =
        collect_case_step_warnings(&baseline.case, registry, secrets);
    let (cand_step_warnings, cand_finally_warnings) =
        collect_case_step_warnings(&candidate.case, registry, secrets);

    compare_step_sequence(
        baseline,
        candidate,
        "step",
        &baseline.case.steps,
        &candidate.case.steps,
        &base_step_warnings,
        &cand_step_warnings,
        acknowledged,
        findings,
    );
    compare_step_sequence(
        baseline,
        candidate,
        "finally step",
        &baseline.case.finally,
        &candidate.case.finally,
        &base_finally_warnings,
        &cand_finally_warnings,
        acknowledged,
        findings,
    );
}

fn check_filters(
    baseline: &LoadedCase,
    candidate: &LoadedCase,
    filters: &Filters,
    acknowledged: bool,
    findings: &mut Vec<StrengthFinding>,
) {
    let base_tags: BTreeSet<&str> = baseline.case.tags.iter().map(String::as_str).collect();
    let cand_tags: BTreeSet<&str> = candidate.case.tags.iter().map(String::as_str).collect();

    // Any newly added tag that matches an active `--skip-tag` excludes the case.
    for skip in &filters.skip_tag {
        if !base_tags.contains(skip.as_str()) && cand_tags.contains(skip.as_str()) {
            findings.push(StrengthFinding {
                file: candidate.path.clone(),
                baseline_file: baseline.path.clone(),
                case_name: candidate.case.name.clone(),
                kind: "filter_excluded",
                message: format!(
                    "case `{}` added tag `{skip}` matching `--skip-tag` exclusion",
                    candidate.case.name
                ),
                acknowledged,
            });
            return;
        }
    }

    if !filters.is_empty() && filters.matches(&baseline.case) && !filters.matches(&candidate.case) {
        findings.push(StrengthFinding {
            file: candidate.path.clone(),
            baseline_file: baseline.path.clone(),
            case_name: candidate.case.name.clone(),
            kind: "filter_excluded",
            message: format!(
                "case `{}` matched baseline filters ({}) but candidate classification/tags exclude it",
                candidate.case.name,
                filters.describe()
            ),
            acknowledged,
        });
    }
}

fn collect_case_step_warnings(
    case: &Case,
    registry: &Registry,
    secrets: &mut Secrets,
) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let mut vars = case.vars.clone();
    let steps = collect_list_warnings(&case.steps, &mut vars, registry, secrets);
    let finally = collect_list_warnings(&case.finally, &mut vars, registry, secrets);
    (steps, finally)
}

fn collect_list_warnings(
    steps: &[Step],
    vars: &mut BTreeMap<String, String>,
    registry: &Registry,
    secrets: &mut Secrets,
) -> Vec<Vec<String>> {
    let mut list = Vec::with_capacity(steps.len());
    for step in steps {
        let mut step_warnings = Vec::new();
        if let Ok(args) = case::interpolate(&step.args, vars, secrets)
            && let Some(definition) = registry.get(&step.action)
            && definition.validate(&args).is_ok()
        {
            step_warnings = definition.lint_warnings(&args);
        }
        for saved in &step.save {
            vars.insert(saved.name.clone(), format!("${{{}}}", saved.name));
        }
        list.push(step_warnings);
    }
    list
}

#[allow(clippy::too_many_arguments)] // Step-sequence comparison needs both step slices and their warning slices.
fn compare_step_sequence(
    baseline: &LoadedCase,
    candidate: &LoadedCase,
    phase_label: &str,
    base_steps: &[Step],
    cand_steps: &[Step],
    base_warnings: &[Vec<String>],
    cand_warnings: &[Vec<String>],
    acknowledged: bool,
    findings: &mut Vec<StrengthFinding>,
) {
    let anchors = lcs_step_anchors(base_steps, cand_steps);
    let mut prev_b = 0usize;
    let mut prev_c = 0usize;

    for (anchor_b, anchor_c) in anchors
        .into_iter()
        .chain(std::iter::once((base_steps.len(), cand_steps.len())))
    {
        let b_gap = anchor_b - prev_b;
        let c_gap = anchor_c - prev_c;
        // Pair only steps that still call the same action. A different action
        // in the same position is a deletion of the baseline step plus an
        // inserted candidate step, not a modification of the old contract.
        let mut paired = 0;
        while paired < b_gap.min(c_gap)
            && base_steps[prev_b + paired].action == cand_steps[prev_c + paired].action
        {
            paired += 1;
        }

        for offset in 0..paired {
            let b_idx = prev_b + offset;
            let c_idx = prev_c + offset;
            compare_paired_step(
                baseline,
                candidate,
                phase_label,
                b_idx,
                c_idx,
                &base_steps[b_idx],
                &cand_steps[c_idx],
                &base_warnings[b_idx],
                &cand_warnings[c_idx],
                acknowledged,
                findings,
            );
        }

        for (b_idx, step) in base_steps
            .iter()
            .enumerate()
            .take(anchor_b)
            .skip(prev_b + paired)
        {
            findings.push(StrengthFinding {
                file: candidate.path.clone(),
                baseline_file: baseline.path.clone(),
                case_name: candidate.case.name.clone(),
                kind: "step_deleted",
                message: format!(
                    "baseline {phase_label} {} (`{}`) was deleted",
                    b_idx + 1,
                    step.action
                ),
                acknowledged,
            });
        }

        for c_idx in (prev_c + paired)..anchor_c {
            for warning in &cand_warnings[c_idx] {
                findings.push(StrengthFinding {
                    file: candidate.path.clone(),
                    baseline_file: baseline.path.clone(),
                    case_name: candidate.case.name.clone(),
                    kind: "fragile_warning_added",
                    message: format!(
                        "{phase_label} {} (`{}`): newly introduced fragile step argument: {warning}",
                        c_idx + 1,
                        cand_steps[c_idx].action
                    ),
                    acknowledged,
                });
            }
        }

        if anchor_b < base_steps.len() && anchor_c < cand_steps.len() {
            compare_paired_step(
                baseline,
                candidate,
                phase_label,
                anchor_b,
                anchor_c,
                &base_steps[anchor_b],
                &cand_steps[anchor_c],
                &base_warnings[anchor_b],
                &cand_warnings[anchor_c],
                acknowledged,
                findings,
            );
        }

        prev_b = anchor_b + 1;
        prev_c = anchor_c + 1;
    }
}

fn lcs_step_anchors(base: &[Step], cand: &[Step]) -> Vec<(usize, usize)> {
    let m = base.len();
    let n = cand.len();
    if m == 0 || n == 0 {
        return Vec::new();
    }

    // Score each matched step heavily (1_000_000) so the common subsequence
    // length is always maximized first. The tie-breaker prefers a step that
    // kept the baseline's expectation paths: inserting an earlier setup step
    // with the same action must not make the later, still-asserted step look
    // deleted. At most 500 overlapping paths keep the tie-breaker below one
    // step's weight.
    const STEP_WEIGHT: u64 = 1_000_000;
    let mut dp = vec![vec![0u64; n + 1]; m + 1];

    for i in (0..m).rev() {
        for j in (0..n).rev() {
            let skip = dp[i + 1][j].max(dp[i][j + 1]);
            if base[i].action == cand[j].action {
                let bonus = expect_path_overlap(&base[i].expect, &cand[j].expect);
                let take = STEP_WEIGHT + bonus + dp[i + 1][j + 1];
                dp[i][j] = take.max(skip);
            } else {
                dp[i][j] = skip;
            }
        }
    }

    let mut anchors = Vec::new();
    let mut i = 0;
    let mut j = 0;
    while i < m && j < n {
        if base[i].action == cand[j].action {
            let bonus = expect_path_overlap(&base[i].expect, &cand[j].expect);
            if STEP_WEIGHT + bonus + dp[i + 1][j + 1] >= dp[i + 1][j].max(dp[i][j + 1]) {
                anchors.push((i, j));
                i += 1;
                j += 1;
                continue;
            }
        }
        if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    anchors
}

fn expect_path_overlap(base: &[Expectation], candidate: &[Expectation]) -> u64 {
    let overlap = base
        .iter()
        .filter(|baseline| candidate.iter().any(|step| step.path == baseline.path))
        .count();
    u64::try_from(overlap).unwrap_or(u64::MAX).min(500) * EXPECT_PATH_WEIGHT
}

#[allow(clippy::too_many_arguments)] // Paired step comparison checks optionality, retry, expectations, and warnings together.
fn compare_paired_step(
    baseline: &LoadedCase,
    candidate: &LoadedCase,
    phase_label: &str,
    _base_index: usize,
    cand_index: usize,
    base_step: &Step,
    cand_step: &Step,
    base_warnings: &[String],
    cand_warnings: &[String],
    acknowledged: bool,
    findings: &mut Vec<StrengthFinding>,
) {
    let step_desc = format!("{phase_label} {} (`{}`)", cand_index + 1, cand_step.action);

    if !base_step.optional && cand_step.optional {
        findings.push(StrengthFinding {
            file: candidate.path.clone(),
            baseline_file: baseline.path.clone(),
            case_name: candidate.case.name.clone(),
            kind: "optional_added",
            message: format!("{step_desc}: added `optional: true` to a required step"),
            acknowledged,
        });
    }

    compare_expectations(
        baseline,
        candidate,
        &step_desc,
        "`expect`",
        &base_step.expect,
        &cand_step.expect,
        acknowledged,
        findings,
    );

    if let Some(base_retry) = &base_step.retry {
        match &cand_step.retry {
            Some(cand_retry) => {
                if cand_retry.timeout_ms > base_retry.timeout_ms {
                    findings.push(StrengthFinding {
                        file: candidate.path.clone(),
                        baseline_file: baseline.path.clone(),
                        case_name: candidate.case.name.clone(),
                        kind: "retry_timeout_widened",
                        message: format!(
                            "{step_desc}: `retry.timeout_ms` widened from {} ms to {} ms",
                            base_retry.timeout_ms, cand_retry.timeout_ms
                        ),
                        acknowledged,
                    });
                }
                compare_expectations(
                    baseline,
                    candidate,
                    &step_desc,
                    "`retry.until`",
                    &base_retry.until,
                    &cand_retry.until,
                    acknowledged,
                    findings,
                );
            }
            None => {
                // Removing `retry` is safe only if every `retry.until` assertion
                // moved into immediate `expect` without weakening.
                compare_expectations(
                    baseline,
                    candidate,
                    &step_desc,
                    "`retry.until`",
                    &base_retry.until,
                    &cand_step.expect,
                    acknowledged,
                    findings,
                );
            }
        }
    }

    if cand_warnings.len() > base_warnings.len() {
        for warning in cand_warnings {
            if !base_warnings.contains(warning) {
                findings.push(StrengthFinding {
                    file: candidate.path.clone(),
                    baseline_file: baseline.path.clone(),
                    case_name: candidate.case.name.clone(),
                    kind: "fragile_warning_added",
                    message: format!("{step_desc}: introduced fragile step argument: {warning}"),
                    acknowledged,
                });
            }
        }
        // Fallback when duplicate warning text increased count.
        if cand_warnings.iter().all(|w| base_warnings.contains(w)) {
            findings.push(StrengthFinding {
                file: candidate.path.clone(),
                baseline_file: baseline.path.clone(),
                case_name: candidate.case.name.clone(),
                kind: "fragile_warning_added",
                message: format!(
                    "{step_desc}: fragile lint warnings increased from {} to {}",
                    base_warnings.len(),
                    cand_warnings.len()
                ),
                acknowledged,
            });
        }
    } else {
        for warning in cand_warnings {
            if !base_warnings.contains(warning) {
                findings.push(StrengthFinding {
                    file: candidate.path.clone(),
                    baseline_file: baseline.path.clone(),
                    case_name: candidate.case.name.clone(),
                    kind: "fragile_warning_added",
                    message: format!("{step_desc}: introduced fragile step argument: {warning}"),
                    acknowledged,
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)] // Shares expectation diffing between `expect` and `retry.until`.
fn compare_expectations(
    baseline: &LoadedCase,
    candidate: &LoadedCase,
    step_desc: &str,
    list_name: &str,
    base_exps: &[Expectation],
    cand_exps: &[Expectation],
    acknowledged: bool,
    findings: &mut Vec<StrengthFinding>,
) {
    let mut base_done = vec![false; base_exps.len()];
    let mut cand_used = vec![false; cand_exps.len()];

    // Pass 1: exact expectations remain unchanged.
    for (b_idx, base_exp) in base_exps.iter().enumerate() {
        if let Some(c_idx) = cand_exps
            .iter()
            .enumerate()
            .position(|(idx, cand_exp)| !cand_used[idx] && cand_exp == base_exp)
        {
            base_done[b_idx] = true;
            cand_used[c_idx] = true;
        }
    }

    // Pass 2: expectations targeting the same JSONPath.
    for (b_idx, base_exp) in base_exps.iter().enumerate() {
        if base_done[b_idx] {
            continue;
        }
        if let Some(c_idx) = cand_exps
            .iter()
            .enumerate()
            .position(|(idx, cand_exp)| !cand_used[idx] && cand_exp.path == base_exp.path)
        {
            base_done[b_idx] = true;
            cand_used[c_idx] = true;
            if let Some(detail) = matcher_weakened(&base_exp.matcher, &cand_exps[c_idx].matcher) {
                findings.push(StrengthFinding {
                    file: candidate.path.clone(),
                    baseline_file: baseline.path.clone(),
                    case_name: candidate.case.name.clone(),
                    kind: "matcher_weakened",
                    message: format!(
                        "{step_desc}: {list_name} assertion on `{}` weakened ({detail})",
                        base_exp.path
                    ),
                    acknowledged,
                });
            }
        }
    }

    // Pass 3: baseline expectations whose path disappeared completely.
    for (b_idx, base_exp) in base_exps.iter().enumerate() {
        if base_done[b_idx] {
            continue;
        }
        findings.push(StrengthFinding {
            file: candidate.path.clone(),
            baseline_file: baseline.path.clone(),
            case_name: candidate.case.name.clone(),
            kind: "assertion_deleted",
            message: format!(
                "{step_desc}: deleted {list_name} assertion on `{}` (`{}`)",
                base_exp.path,
                base_exp.matcher.comparison()
            ),
            acknowledged,
        });
    }
}

fn matcher_weakened(base: &Matcher, cand: &Matcher) -> Option<String> {
    match (base, cand) {
        (Matcher::Equals(_), Matcher::Contains(_) | Matcher::Exists(_) | Matcher::NotEquals(_)) => {
            Some(format!("`equals` -> `{}`", cand.comparison()))
        }
        (Matcher::Contains(_), Matcher::Exists(_) | Matcher::NotEquals(_)) => {
            Some(format!("`contains` -> `{}`", cand.comparison()))
        }
        (Matcher::Contains(Value::String(b)), Matcher::Contains(Value::String(c)))
            if b != c && b.contains(c.as_str()) =>
        {
            Some(format!(
                "`contains` substring shortened from {b:?} to {c:?}"
            ))
        }
        (Matcher::NotEquals(_), Matcher::Exists(_)) => Some("`not_equals` -> `exists`".to_string()),
        (Matcher::Exists(true), Matcher::Exists(false)) => {
            Some("`exists: true` -> `exists: false`".to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::core::registry::{StepDefinition, StepError, allow_retry, no_lint_warnings};

    fn registry() -> Registry {
        let mut reg = Registry::new();
        reg.register(StepDefinition {
            name: "sample.read",
            description: "Reads a sample target.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: allow_retry,
            lint_warnings: |args| {
                if args.get("at").is_some() {
                    vec!["uses explicit coordinates `at`".to_string()]
                } else {
                    Vec::new()
                }
            },
            run: |_, _| Err(StepError::Execution("unused".to_string())),
        })
        .expect("sample.read must register");
        reg.register(StepDefinition {
            name: "sample.wait",
            description: "Waits for state.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: allow_retry,
            lint_warnings: no_lint_warnings,
            run: |_, _| Ok(Value::Null),
        })
        .expect("sample.wait must register");
        reg
    }

    fn loaded(path: &str, yaml: &str) -> LoadedCase {
        let case = case::parse(yaml).expect("fixture YAML must parse");
        LoadedCase::new(PathBuf::from(path), case)
    }

    #[test]
    fn inserting_a_wait_step_before_an_asserted_step_is_not_flagged() {
        let reg = registry();
        let base = loaded(
            "cases/flow.yaml",
            "name: Flow\nsteps:\n  - sample.read:\n      id: btn\n  - sample.read:\n      id: status\n    expect:\n      - path: $.text\n        equals: Saved\n",
        );
        let cand = loaded(
            "cases/flow.yaml",
            "name: Flow\nsteps:\n  - sample.read:\n      id: btn\n  - sample.wait:\n      id: status\n  - sample.read:\n      id: status\n    expect:\n      - path: $.text\n        equals: Saved\n",
        );
        let inputs = [PathBuf::from("cases/flow.yaml")];
        let mut secrets = Secrets::new();
        let findings = compare_strength(
            &StrengthCheck {
                baseline_inputs: &inputs,
                baseline_cases: &[base],
                candidate_inputs: &inputs,
                candidate_cases: &[cand],
                registry: &reg,
                filters: &Filters::default(),
                acknowledged: false,
            },
            &mut secrets,
        );
        assert!(findings.is_empty(), "got: {findings:?}");
    }

    #[test]
    fn detects_matcher_optional_retry_and_fragile_coordinate_weakenings() {
        let reg = registry();
        let base = loaded(
            "cases/flow.yaml",
            "name: Flow\nsteps:\n  - sample.read:\n      id: status\n    retry:\n      timeout_ms: 1000\n      until:\n        - path: $.ready\n          equals: true\n    expect:\n      - path: $.text\n        equals: Saved\n      - path: $.code\n        equals: 200\n",
        );
        let cand = loaded(
            "cases/flow.yaml",
            "name: Flow\nsteps:\n  - sample.read:\n      at: { x: 10, y: 20 }\n    optional: true\n    retry:\n      timeout_ms: 5000\n      until:\n        - path: $.ready\n          exists: true\n    expect:\n      - path: $.text\n        contains: Saved\n",
        );
        let inputs = [PathBuf::from("cases/flow.yaml")];
        let mut secrets = Secrets::new();
        let findings = compare_strength(
            &StrengthCheck {
                baseline_inputs: &inputs,
                baseline_cases: &[base],
                candidate_inputs: &inputs,
                candidate_cases: &[cand],
                registry: &reg,
                filters: &Filters::default(),
                acknowledged: false,
            },
            &mut secrets,
        );
        let kinds: Vec<&str> = findings.iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&"optional_added"), "got: {findings:?}");
        assert!(
            kinds.contains(&"retry_timeout_widened"),
            "got: {findings:?}"
        );
        assert!(kinds.contains(&"matcher_weakened"), "got: {findings:?}");
        assert!(kinds.contains(&"assertion_deleted"), "got: {findings:?}");
        assert!(
            kinds.contains(&"fragile_warning_added"),
            "got: {findings:?}"
        );
    }

    #[test]
    fn replacing_a_step_action_reports_the_baseline_step_deleted() {
        let reg = registry();
        let base = loaded(
            "cases/flow.yaml",
            "name: Flow\nsteps:\n  - sample.wait:\n      id: ready\n",
        );
        let cand = loaded(
            "cases/flow.yaml",
            "name: Flow\nsteps:\n  - sample.read:\n      id: ready\n",
        );
        let inputs = [PathBuf::from("cases/flow.yaml")];
        let mut secrets = Secrets::new();
        let findings = compare_strength(
            &StrengthCheck {
                baseline_inputs: &inputs,
                baseline_cases: &[base],
                candidate_inputs: &inputs,
                candidate_cases: &[cand],
                registry: &reg,
                filters: &Filters::default(),
                acknowledged: false,
            },
            &mut secrets,
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.kind == "step_deleted"),
            "replacing an action must not hide the deleted step: {findings:?}"
        );
    }
}
