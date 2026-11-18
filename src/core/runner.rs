//! Executes loaded cases against a registry.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use jsonpath_rust::JsonPath;
use serde_json::{Map, Value};
use serde_saphyr::Location;

use crate::core::assert::{self, AssertionReport};
use crate::core::bundle::FailureBundles;
use crate::core::case::{self, CaseError, Expectation, LoadedCase, Observation, SavedValue, Step};
use crate::core::events::{Event, EventBus};
use crate::core::filter::{self, Filters};
use crate::core::observer::{Observer, ObserverError};
use crate::core::registry::{Registry, StepDefinition, StepError};
use crate::core::report::{
    CaseReport, CaseStatus, Problem, ProblemKind, RunOutcome, RunReport, StepReport, StepStatus,
};
use crate::core::secrets::Secrets;
use crate::core::session::{SessionEvidence, Sessions};

/// Validates every case, then runs the ones the filters select.
///
/// Validation covers step registration, argument schemas and variable
/// references, and it always covers every discovered case: a filter must not
/// let an invalid file escape the check. If anything is invalid no case runs at
/// all, because a partially executed run would make the result depend on case
/// order.
///
/// `secrets` collects every value that a `${env:NAME}` reference resolved.
/// Validation resolves the references of every case before the first case runs,
/// so the set is complete before the first failure bundle is written.
///
/// When `bundles` is given, every selected case that did not pass gets a
/// failure bundle before the run ends.
///
/// `jobs` is how many cases may run at the same time. One runs them one after
/// another; a value above the number of selected cases is the same as that
/// number, and a value below one is treated as one. Whatever the job count, a
/// case keeps its position in [`RunReport::cases`], so the output does not
/// depend on it. The events of one case stay in order, while events of
/// different cases may interleave.
pub fn run(
    cases: &[LoadedCase],
    registry: &Registry,
    bus: &EventBus,
    bundles: Option<&mut FailureBundles>,
    filters: &Filters,
    secrets: &mut Secrets,
    jobs: usize,
) -> RunOutcome {
    run_with_record(cases, registry, bus, bundles, None, filters, secrets, jobs)
}

/// Runs the selected cases and writes every one of them to `record`.
///
/// `record` is the `--record` writer. Its presence makes the runner collect a
/// passing case's session evidence before the sessions close; `bundles` keeps
/// its own failure-only behavior. A case that fails may therefore be written
/// twice, once to each writer, from the same evidence. A writer error is
/// reported in [`RunReport::bundle_errors`] and never changes the verdict.
#[allow(clippy::too_many_arguments)] // The two writers are independent and must stay explicit.
pub fn run_with_record(
    cases: &[LoadedCase],
    registry: &Registry,
    bus: &EventBus,
    bundles: Option<&mut FailureBundles>,
    record: Option<&mut FailureBundles>,
    filters: &Filters,
    secrets: &mut Secrets,
    jobs: usize,
) -> RunOutcome {
    let errors = preflight(cases, registry, secrets);
    if !errors.is_empty() {
        return RunOutcome::Rejected { errors };
    }

    let selected: Vec<&LoadedCase> = cases
        .iter()
        .filter(|loaded| filters.matches(&loaded.case))
        .collect();
    if selected.is_empty() && !filters.is_empty() {
        return RunOutcome::NoMatch {
            message: filter::no_match_message(filters, &filter::Available::of(cases)),
        };
    }
    let filtered = cases.len() - selected.len();

    let started = Instant::now();
    bus.emit(Event::RunStarted {
        cases: selected.len(),
    });

    // One worker per job, but never more workers than cases: a worker takes the
    // next case with a single atomic increment, so no case runs twice and none
    // is skipped. The report of a case is stored at its own position, which is
    // what keeps the output independent of which case finishes first.
    let next = AtomicUsize::new(0);
    let collected = Mutex::new(Collected::new(selected.len()));
    let secrets = Mutex::new(secrets);
    let bundles = bundles.map(Mutex::new);
    let record = record.map(Mutex::new);
    let workers = jobs.max(1).min(selected.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(index) = take_next(&next, selected.len()) {
                    run_one(
                        index,
                        selected[index],
                        registry,
                        bus,
                        BundleWriters {
                            failures: bundles.as_ref(),
                            record: record.as_ref(),
                        },
                        &collected,
                        &secrets,
                    );
                }
            });
        }
    });

    // A worker that panicked would have resumed its panic when the scope
    // ended, so every slot is filled here.
    let Collected {
        reports,
        mut bundle_errors,
    } = collected
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Bundle errors keep the order of the cases they belong to, not the order
    // in which the workers happened to fail writing them.
    bundle_errors.sort_by_key(|(index, _)| *index);

    let report = RunReport {
        cases: reports.into_iter().flatten().collect(),
        filtered,
        duration: started.elapsed(),
        bundle_errors: bundle_errors
            .into_iter()
            .map(|(_, message)| message)
            .collect(),
    };
    bus.emit(Event::RunFinished {
        summary: report.summary(),
        duration: report.duration,
    });
    RunOutcome::Completed(report)
}

/// What every worker produces and one thread reads after the scope.
struct Collected {
    /// One slot per selected case, filled at the case's discovery position.
    reports: Vec<Option<CaseReport>>,
    /// Bundles that could not be written, with the position of their case.
    bundle_errors: Vec<(usize, String)>,
}

impl Collected {
    fn new(cases: usize) -> Self {
        Self {
            reports: vec![None; cases],
            bundle_errors: Vec::new(),
        }
    }
}

/// The index of the next case to run, or `None` once every case was taken.
fn take_next(next: &AtomicUsize, total: usize) -> Option<usize> {
    let index = next.fetch_add(1, Ordering::Relaxed);
    (index < total).then_some(index)
}

/// Locks a mutex even when another thread panicked while holding it.
///
/// The state behind these locks is the report slots, the bundle writer and the
/// secret values; a panic has no bearing on any of them, and a second panic
/// would replace the run's result.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The two independent bundle writers of a run.
struct BundleWriters<'a> {
    failures: Option<&'a Mutex<&'a mut FailureBundles>>,
    record: Option<&'a Mutex<&'a mut FailureBundles>>,
}

/// Runs one selected case and stores its report at its own position.
fn run_one(
    index: usize,
    loaded: &LoadedCase,
    registry: &Registry,
    bus: &EventBus,
    writers: BundleWriters<'_>,
    collected: &Mutex<Collected>,
    secrets: &Mutex<&mut Secrets>,
) {
    bus.emit(Event::CaseStarted {
        case: index,
        name: loaded.case.name.clone(),
    });
    let CaseRun {
        mut report,
        evidence,
    } = run_case(
        index,
        loaded,
        registry,
        bus,
        secrets,
        writers.failures.is_some(),
        writers.record.is_some(),
    );
    if report.status != CaseStatus::Passed
        && let Some(writer) = writers.failures
    {
        // The bundle writer keeps one run directory and a staging counter, so
        // the workers take turns. The case directories themselves already
        // differ by position, so nothing else is shared.
        let mut writer = lock(writer);
        let secrets = lock(secrets);
        match writer.write_with_evidence(index, &report, &secrets, &evidence) {
            Ok(directory) => report.bundle = Some(directory),
            Err(message) => lock(collected).bundle_errors.push((index, message)),
        }
    }
    if let Some(writer) = writers.record {
        // A recording writer has one directory and a staging counter too, so
        // its workers take turns the same way.
        let mut writer = lock(writer);
        let secrets = lock(secrets);
        if let Err(message) = writer.write_with_evidence(index, &report, &secrets, &evidence) {
            lock(collected).bundle_errors.push((index, message));
        }
    }
    bus.emit(Event::CaseFinished {
        case: index,
        name: report.name.clone(),
        status: report.status,
        duration: report.duration,
    });
    lock(collected).reports[index] = Some(report);
}

/// Checks everything that must hold before any case runs.
///
/// `da lint` calls the same function, so a case that lint accepts is a case
/// the runner would not reject. Validation does not execute a step.
pub fn preflight(
    cases: &[LoadedCase],
    registry: &Registry,
    secrets: &mut Secrets,
) -> Vec<CaseError> {
    let names = registry.names();
    let hint = if names.is_empty() {
        "no steps are registered; build with a driver feature enabled".to_string()
    } else {
        format!(
            "registered steps: {}",
            names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut errors = Vec::new();

    for loaded in cases {
        let mut vars = loaded.case.vars.clone();
        // Observations are declared before the steps and resolve when the case
        // starts, so they are checked first and against the case's own
        // variables only.
        for observation in &loaded.case.observe {
            if let Err(error) = check_observation(loaded, observation, registry, &vars, secrets) {
                errors.push(error);
            }
        }
        for (index, step) in loaded.case.steps.iter().enumerate() {
            let position = index + 1;
            match case::interpolate(&step.args, &vars, secrets) {
                Err(error) => errors.push(positioned(
                    loaded,
                    step.location_of(&error.pointer),
                    format!("step {position}: {}", error.message),
                )),
                Ok(args) => match registry.get(&step.action) {
                    None => errors.push(positioned(
                        loaded,
                        step.location,
                        format!(
                            "step {position}: `{}` is not a registered step; {hint}",
                            step.action
                        ),
                    )),
                    Some(definition) => {
                        if let Err(message) = (definition.validate)(&args) {
                            // Arguments are validated as one mapping, so the
                            // mapping start is the node the message names.
                            errors.push(positioned(
                                loaded,
                                step.args_location,
                                format!("step {position}: `{}` {message}", step.action),
                            ));
                        }
                        if step.retry.is_some()
                            && let Err(message) = (definition.check_retry)(&args)
                        {
                            errors.push(positioned(
                                loaded,
                                step.retry_location,
                                format!("step {position}: `{}` {message}", step.action),
                            ));
                        }
                    }
                },
            }
            for saved in &step.save {
                vars.insert(saved.name.clone(), format!("${{{}}}", saved.name));
            }
        }
    }

    errors
}

/// The names an unknown observer message can offer.
fn observer_hint(registry: &Registry) -> String {
    let names = registry.observer_names();
    if names.is_empty() {
        "no observers are registered; build with a feature that provides one".to_string()
    } else {
        format!(
            "registered observers: {}",
            names
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Checks one declared observation: its arguments expand, its observer exists
/// and its arguments satisfy that observer.
///
/// Every message points at the node in the case file it belongs to, because the
/// parser kept the location and the user has to edit it there.
fn check_observation(
    loaded: &LoadedCase,
    observation: &Observation,
    registry: &Registry,
    vars: &BTreeMap<String, String>,
    secrets: &mut Secrets,
) -> Result<(), CaseError> {
    let name = &observation.name;
    let args = case::interpolate(&observation.args, vars, secrets).map_err(|error| {
        positioned(
            loaded,
            observation.args_location,
            format!("observation `{name}`: {}", error.message),
        )
    })?;
    let Some(definition) = registry.observer(&observation.kind) else {
        return Err(positioned(
            loaded,
            observation.location,
            format!(
                "observation `{name}`: `{}` is not a registered observer; {}",
                observation.kind,
                observer_hint(registry)
            ),
        ));
    };
    (definition.validate)(&args).map_err(|message| {
        positioned(
            loaded,
            observation.args_location,
            format!("observation `{name}`: `{}` {message}", observation.kind),
        )
    })
}

/// Renders a message at the place in the case file it belongs to.
///
/// The snippet needs the file text, which the parser no longer holds, so the
/// file is read again. Loading has just read it; if it is gone now, the line and
/// column are still reported without the snippet.
fn positioned(loaded: &LoadedCase, location: Location, message: String) -> CaseError {
    let message = match std::fs::read_to_string(&loaded.path) {
        Ok(text) => case::render_problem(&text, location, message),
        Err(_) => format!(
            "line {} column {}: {message}",
            location.line(),
            location.column()
        ),
    };
    CaseError {
        file: loaded.path.clone(),
        message,
    }
}

/// One case's report and the evidence its sessions returned.
struct CaseRun {
    report: CaseReport,
    evidence: Vec<SessionEvidence>,
}

fn run_case(
    index: usize,
    loaded: &LoadedCase,
    registry: &Registry,
    bus: &EventBus,
    secrets: &Mutex<&mut Secrets>,
    collect_failure_evidence: bool,
    collect_passing_evidence: bool,
) -> CaseRun {
    let started = Instant::now();
    let mut vars = loaded.case.vars.clone();
    // One set of resources belongs to this case and to no other. It is created
    // here, on the worker that runs the case, so `--jobs` cannot share it.
    let mut sessions = Sessions::new();
    if let Some(directory) = loaded.path.parent() {
        sessions.set_base_directory(directory);
    }
    let mut steps = Vec::with_capacity(loaded.case.steps.len());
    // One observation per declared name belongs to this case and to no other,
    // so two cases that run at the same time never share what they collect.
    let (mut observations, mut stopped) =
        match create_observations(&loaded.case.observe, registry, &vars, secrets) {
            Ok(observations) => (observations, None),
            Err(problem) => (Vec::new(), Some(problem)),
        };

    for (position, step) in loaded.case.steps.iter().enumerate() {
        if stopped.is_some() {
            steps.push(StepReport {
                name: step.action.clone(),
                status: StepStatus::Skipped,
                duration: Duration::ZERO,
                result: None,
                assertions: Vec::new(),
                error: None,
            });
            continue;
        }

        bus.emit(Event::StepStarted {
            case: index,
            step: position,
            name: step.action.clone(),
        });
        let step_started = Instant::now();
        let mut report = StepReport {
            name: step.action.clone(),
            status: StepStatus::Passed,
            duration: Duration::ZERO,
            result: None,
            assertions: Vec::new(),
            error: None,
        };

        match execute_step(
            step,
            registry,
            &mut vars,
            secrets,
            &mut observations,
            &mut sessions,
        ) {
            Ok((result, assertions)) => {
                let failed = assertions.iter().filter(|item| !item.passed).count();
                if failed == 0 {
                    report.status = StepStatus::Passed;
                } else {
                    report.status = StepStatus::Failed;
                    report.error = Some(Problem {
                        kind: ProblemKind::Assertion,
                        message: format!("{failed} of {} assertions failed", assertions.len()),
                    });
                }
                report.result = Some(result);
                report.assertions = assertions;
            }
            Err(problem) => {
                report.status = StepStatus::Error;
                report.error = Some(problem);
            }
        }

        // An optional step that ran but did not reach its assertions is allowed
        // to be absent: it becomes `skipped` and the case continues. The
        // problem stays in the report so a reader still sees why. Only an
        // environment or input error means the case cannot go on, and that is
        // exactly the distinction `ProblemKind` already draws.
        let skipped = step.optional
            && report.error.as_ref().is_some_and(|problem| {
                matches!(
                    problem.kind,
                    ProblemKind::Assertion | ProblemKind::Execution | ProblemKind::Timeout
                )
            });
        if skipped {
            report.status = StepStatus::Skipped;
        }

        report.duration = step_started.elapsed();
        bus.emit(Event::StepFinished {
            case: index,
            step: position,
            name: report.name.clone(),
            status: report.status,
            duration: report.duration,
        });
        if let Some(problem) = &report.error
            && !skipped
        {
            stopped = Some(problem.clone());
        }
        steps.push(report);
    }

    // Evidence is collected before the sessions release their resources. A
    // failure bundle asks only for a case that did not pass; the record writer
    // asks for every case, which is what `--record` adds. An ordinary passing
    // case never asks, even though its sessions are still open here.
    let status_before_close = match stopped.as_ref().map(|problem| problem.kind) {
        None => CaseStatus::Passed,
        Some(ProblemKind::Assertion) => CaseStatus::Failed,
        Some(_) => CaseStatus::Error,
    };
    let evidence = if (collect_failure_evidence && status_before_close != CaseStatus::Passed)
        || collect_passing_evidence
    {
        sessions.collect_evidence()
    } else {
        Vec::new()
    };

    // Every session belongs to this case and is closed here, whatever path the
    // case took. `close_all` removes a session before closing it, so a failing
    // close cannot be attempted twice.
    let close_errors = sessions.close_all();

    let mut status = status_before_close;
    let mut error = stopped;
    // A case that already failed or errored keeps its own verdict; the close
    // errors are attached to the report without replacing the problem. A case
    // that would otherwise pass becomes an environment error, because a
    // resource that could not be released is an environment problem, not a
    // test result.
    if !close_errors.is_empty() && status == CaseStatus::Passed {
        status = CaseStatus::Error;
        error = Some(Problem {
            kind: ProblemKind::Environment,
            message: close_error_message(&close_errors),
        });
    }

    CaseRun {
        report: CaseReport {
            file: loaded.path.clone(),
            name: loaded.case.name.clone(),
            project: loaded.case.project.clone(),
            suite: loaded.case.suite.clone(),
            layer: loaded.case.layer.clone(),
            tags: loaded.case.tags.clone(),
            status,
            duration: started.elapsed(),
            steps,
            error,
            close_errors,
            bundle: None,
        },
        evidence,
    }
}

/// The one problem a case reports when its sessions could not be closed.
fn close_error_message(errors: &[String]) -> String {
    if let [message] = errors {
        return message.clone();
    }
    format!(
        "{} sessions could not be closed: {}",
        errors.len(),
        errors.join("; ")
    )
}

/// One declared observation, ready to start and collect around every step.
struct ObservationSlot {
    /// Name the case gave the observation.
    name: String,
    /// Registered observer name, reported in an error.
    kind: &'static str,
    /// The observation of this case.
    observer: Box<dyn Observer>,
}

/// Creates the observation of one case from its `observe:` entries.
///
/// The arguments are resolved here, once, so every step observes the same
/// target; a value saved by a step is not available to an observation.
fn create_observations(
    declared: &[Observation],
    registry: &Registry,
    vars: &BTreeMap<String, String>,
    secrets: &Mutex<&mut Secrets>,
) -> Result<Vec<ObservationSlot>, Problem> {
    let mut slots = Vec::with_capacity(declared.len());
    for observation in declared {
        let args = {
            let mut secrets = lock(secrets);
            case::interpolate(&observation.args, vars, &mut secrets)
        }
        .map_err(|error| Problem {
            kind: ProblemKind::InvalidInput,
            message: format!("observation `{}`: {}", observation.name, error.message),
        })?;
        let Some(definition) = registry.observer(&observation.kind) else {
            return Err(Problem {
                kind: ProblemKind::InvalidInput,
                message: format!(
                    "observation `{}`: `{}` is not a registered observer",
                    observation.name, observation.kind
                ),
            });
        };
        let observer = (definition.create)(&args).map_err(|error| Problem {
            kind: problem_kind_of(&error),
            message: format!(
                "observation `{}` (`{}`): {}",
                observation.name,
                definition.name,
                error.message()
            ),
        })?;
        slots.push(ObservationSlot {
            name: observation.name.clone(),
            kind: definition.name,
            observer,
        });
    }
    Ok(slots)
}

/// The category of a problem an observer reported.
fn problem_kind_of(error: &ObserverError) -> ProblemKind {
    match error {
        ObserverError::InvalidArguments(_) => ProblemKind::InvalidInput,
        ObserverError::Environment(_) => ProblemKind::Environment,
    }
}

/// A step's arguments and the observations collected around it.
fn execute_step(
    step: &Step,
    registry: &Registry,
    vars: &mut BTreeMap<String, String>,
    secrets: &Mutex<&mut Secrets>,
    observations: &mut [ObservationSlot],
    sessions: &mut Sessions,
) -> Result<(Value, Vec<AssertionReport>), Problem> {
    let definition = registry.get(&step.action).ok_or_else(|| Problem {
        kind: ProblemKind::InvalidInput,
        message: format!("`{}` is not a registered step", step.action),
    })?;

    // Validation resolved every environment reference of every case before the
    // first case ran, so this records nothing new; it still takes the lock,
    // because interpolation is the only place that decides what is a secret.
    // The lock is held while the arguments are expanded and never while a step
    // runs, so one slow step cannot hold the others up.
    let args = {
        let mut secrets = lock(secrets);
        case::interpolate(&step.args, vars, &mut secrets)
    }
    .map_err(|error| Problem {
        kind: ProblemKind::Execution,
        message: error.message,
    })?;

    let Some(policy) = &step.retry else {
        let result = run_attempt(definition, &args, observations, sessions)?;
        let assertions = evaluate_all(&step.expect, &result);
        // A case that already failed an assertion is not allowed to change the
        // variables that later steps would see.
        if assertions.iter().all(|item| item.passed) {
            save_values(&step.save, &result, vars)?;
        }
        return Ok((result, assertions));
    };

    if let Err(message) = (definition.check_retry)(&args) {
        return Err(Problem {
            kind: ProblemKind::InvalidInput,
            message: format!("`{}` {message}", step.action),
        });
    }

    let deadline = Instant::now() + Duration::from_millis(policy.timeout_ms);
    let interval = Duration::from_millis(policy.interval_ms);
    loop {
        match run_attempt(definition, &args, observations, sessions) {
            Ok(result) => {
                let until_assertions = evaluate_all(&policy.until, &result);
                if until_assertions.iter().all(|item| item.passed) {
                    let mut assertions = until_assertions;
                    assertions.extend(evaluate_all(&step.expect, &result));
                    if assertions.iter().all(|item| item.passed) {
                        save_values(&step.save, &result, vars)?;
                    }
                    return Ok((result, assertions));
                }
                let now = Instant::now();
                if now >= deadline {
                    return Ok((result, until_assertions));
                }
                std::thread::sleep(interval.min(deadline.saturating_duration_since(now)));
            }
            Err(problem) => {
                if matches!(problem.kind, ProblemKind::Execution | ProblemKind::Timeout) {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(problem);
                    }
                    std::thread::sleep(interval.min(deadline.saturating_duration_since(now)));
                } else {
                    return Err(problem);
                }
            }
        }
    }
}

fn run_attempt(
    definition: &StepDefinition,
    args: &Value,
    observations: &mut [ObservationSlot],
    sessions: &mut Sessions,
) -> Result<Value, Problem> {
    // Every observation starts before the step, so what it reports is the
    // change this one attempt made.
    for slot in observations.iter_mut() {
        slot.observer
            .start(sessions)
            .map_err(|error| observer_problem(slot, &error))?;
    }

    match (definition.run)(args, sessions) {
        Ok(result) => {
            // Collection happens before assertions are evaluated: an `until` or
            // `expect` entry that names `$.observed.<name>` must see the values of this
            // attempt.
            let observed = collect_observations(observations, sessions)?;
            Ok(merge_observations(result, observed))
        }
        Err(error) => {
            // Drain stateful observers after retryable or optional-skipped failures so
            // buffered events cannot leak into the next attempt or step. On fatal step
            // errors (`Environment` / `InvalidArguments`) the case stops immediately, so
            // preserve the original step error instead of masking it with a broken observer.
            if matches!(error, StepError::Execution(_) | StepError::Timeout(_)) {
                let _ = collect_observations(observations, sessions)?;
            }
            Err(problem_from_step_error(error))
        }
    }
}

fn collect_observations(
    observations: &mut [ObservationSlot],
    sessions: &mut Sessions,
) -> Result<Map<String, Value>, Problem> {
    let mut observed = Map::new();
    for slot in observations.iter_mut() {
        let value = slot
            .observer
            .collect(sessions)
            .map_err(|error| observer_problem(slot, &error))?;
        observed.insert(slot.name.clone(), value);
    }
    Ok(observed)
}

fn evaluate_all(expectations: &[Expectation], result: &Value) -> Vec<AssertionReport> {
    expectations
        .iter()
        .map(|expectation| assert::evaluate(expectation, result))
        .collect()
}

/// Adds the collected values to a step result as `observed.<name>`.
///
/// Every built-in step returns a JSON object, which is what the contract
/// promises. A step of a plugin that returns another value keeps it under
/// `value`, so the observations are not lost and the original result is still
/// reachable at `$.value`.
fn merge_observations(result: Value, observed: Map<String, Value>) -> Value {
    if observed.is_empty() {
        return result;
    }
    match result {
        Value::Object(mut entries) => {
            entries.insert("observed".to_string(), Value::Object(observed));
            Value::Object(entries)
        }
        other => {
            let mut wrapped = Map::new();
            wrapped.insert("value".to_string(), other);
            wrapped.insert("observed".to_string(), Value::Object(observed));
            Value::Object(wrapped)
        }
    }
}

/// The problem an observer that failed around a step produces.
///
/// The message names the observation and the observer kind, because the user
/// chooses the first and may not know which one the second is.
fn observer_problem(slot: &ObservationSlot, error: &ObserverError) -> Problem {
    Problem {
        kind: problem_kind_of(error),
        message: format!(
            "observer `{}` (`{}`): {}",
            slot.name,
            slot.kind,
            error.message()
        ),
    }
}

fn save_values(
    saves: &[SavedValue],
    result: &Value,
    vars: &mut BTreeMap<String, String>,
) -> Result<(), Problem> {
    for saved in saves {
        let selected = result.query(&saved.path).map_err(|error| Problem {
            kind: ProblemKind::Execution,
            message: format!("could not save `{}`: {error}", saved.name),
        })?;
        match selected.as_slice() {
            [value] => {
                vars.insert(saved.name.clone(), variable_text(value));
            }
            [] => {
                return Err(Problem {
                    kind: ProblemKind::Execution,
                    message: format!(
                        "could not save `{}`: `{}` matched no value",
                        saved.name, saved.path
                    ),
                });
            }
            values => {
                return Err(Problem {
                    kind: ProblemKind::Execution,
                    message: format!(
                        "could not save `{}`: `{}` matched {} values, expected one",
                        saved.name,
                        saved.path,
                        values.len()
                    ),
                });
            }
        }
    }
    Ok(())
}

fn variable_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn problem_from_step_error(error: StepError) -> Problem {
    match error {
        StepError::InvalidArguments(message) => Problem {
            kind: ProblemKind::InvalidInput,
            message,
        },
        StepError::Environment(message) => Problem {
            kind: ProblemKind::Environment,
            message,
        },
        StepError::Timeout(message) => Problem {
            kind: ProblemKind::Timeout,
            message,
        },
        StepError::Execution(message) => Problem {
            kind: ProblemKind::Execution,
            message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::case::Case;
    use crate::core::observer::Observer;
    use crate::core::registry::StepDefinition;
    use crate::core::session::{Evidence, Session};
    use serde_json::json;

    /// An observer that reports the arguments it was created with.
    struct Echo(Value);

    impl Observer for Echo {
        fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
            Ok(())
        }

        fn collect(&mut self, _sessions: &mut Sessions) -> Result<Value, ObserverError> {
            Ok(self.0.clone())
        }
    }

    /// An observer that cannot collect, as a file that cannot be read would.
    struct Broken;

    impl Observer for Broken {
        fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
            Ok(())
        }

        fn collect(&mut self, _sessions: &mut Sessions) -> Result<Value, ObserverError> {
            Err(ObserverError::Environment(
                "could not read `app.log`".to_string(),
            ))
        }
    }

    fn test_case(name: &str, steps: Vec<Step>) -> Case {
        Case {
            name: name.to_string(),
            vars: BTreeMap::new(),
            project: None,
            suite: None,
            layer: None,
            tags: Vec::new(),
            observe: Vec::new(),
            steps,
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry
            .register(StepDefinition {
                name: "test.echo",
                description: "Returns the `value` argument.",
                args_schema: json!({"type": "object"}),
                validate: |args: &Value| {
                    if args.get("value").is_some() {
                        Ok(())
                    } else {
                        Err("invalid arguments: `value` is required".to_string())
                    }
                },
                check_retry: crate::core::registry::disallow_retry,
                run: |args: &Value, _sessions| {
                    Ok(args.get("value").cloned().unwrap_or(Value::Null))
                },
            })
            .expect("the test step must register");
        registry
            .register(StepDefinition {
                name: "test.fail",
                description: "Fails the environment.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |_, _| {
                    Err(StepError::Environment(
                        "the target refused the connection".to_string(),
                    ))
                },
            })
            .expect("the failing test step must register");
        registry
            .register(StepDefinition {
                name: "test.wait",
                description: "Waits `ms` milliseconds, then returns `value`.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |args: &Value, _sessions| {
                    let millis = args.get("ms").and_then(Value::as_u64).unwrap_or(0);
                    std::thread::sleep(Duration::from_millis(millis));
                    Ok(args.get("value").cloned().unwrap_or(Value::Null))
                },
            })
            .expect("the waiting test step must register");
        registry
            .register(StepDefinition {
                name: "test.scalar",
                description: "Returns a number.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |_, _| Ok(json!(5)),
            })
            .expect("the scalar test step must register");
        registry
            .register(StepDefinition {
                name: "test.execution",
                description: "Fails its own work.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |_, _| {
                    Err(StepError::Execution(
                        "the step could not finish its work".to_string(),
                    ))
                },
            })
            .expect("the failing test step must register");
        registry
            .register(StepDefinition {
                name: "test.timeout",
                description: "Exceeds its time budget.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |_, _| {
                    Err(StepError::Timeout(
                        "the step did not finish within 10 ms".to_string(),
                    ))
                },
            })
            .expect("the timing-out test step must register");
        registry
            .register(StepDefinition {
                name: "test.invalid",
                description: "Refuses its arguments at run time.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |_, _| {
                    Err(StepError::InvalidArguments(
                        "invalid arguments: `value` is required".to_string(),
                    ))
                },
            })
            .expect("the invalid test step must register");
        registry
            .register_observer(crate::core::observer::ObserverDefinition {
                name: "test.watch",
                description: "Reports its arguments.",
                args_schema: json!({"type": "object"}),
                validate: |args: &Value| {
                    if args.get("note").is_some() {
                        Ok(())
                    } else {
                        Err("invalid arguments: `note` is required".to_string())
                    }
                },
                create: |args| Ok(Box::new(Echo(args.clone()))),
            })
            .expect("the echoing observer must register");
        registry
            .register_observer(crate::core::observer::ObserverDefinition {
                name: "test.broken",
                description: "Cannot collect.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                create: |_| Ok(Box::new(Broken)),
            })
            .expect("the broken observer must register");
        registry
    }

    fn observation(name: &str, kind: &str, args: Value) -> crate::core::case::Observation {
        crate::core::case::Observation {
            name: name.to_string(),
            kind: kind.to_string(),
            args,
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
        }
    }

    /// A step that takes time, so a case can be made to finish after a later one.
    fn wait(millis: u64, value: Value) -> Step {
        Step {
            action: "test.wait".to_string(),
            args: json!({ "ms": millis, "value": value }),
            expect: Vec::new(),
            optional: false,
            retry: None,
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        }
    }

    fn echo(value: Value, expect: Vec<crate::core::case::Expectation>) -> Step {
        Step {
            action: "test.echo".to_string(),
            args: json!({ "value": value }),
            expect,
            optional: false,
            retry: None,
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        }
    }

    fn loaded(steps: Vec<Step>) -> Vec<LoadedCase> {
        vec![LoadedCase {
            path: "case.yaml".into(),
            case: test_case("case", steps),
        }]
    }

    /// A step that calls one of the test steps that fails without arguments.
    fn failing(action: &str) -> Step {
        Step {
            action: action.to_string(),
            args: json!({}),
            optional: false,
            retry: None,
            expect: Vec::new(),
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        }
    }

    /// The same step, marked as optional.
    fn optional(mut step: Step) -> Step {
        step.optional = true;
        step
    }

    /// The single case of a run that must have completed.
    fn one_case(outcome: RunOutcome) -> CaseReport {
        let RunOutcome::Completed(mut report) = outcome else {
            panic!("the run must complete, got: {outcome:?}");
        };
        assert_eq!(report.cases.len(), 1, "{report:?}");
        report.cases.remove(0)
    }

    #[test]
    fn a_passing_case_returns_success() {
        let bus = EventBus::new();
        let outcome = run(
            &loaded(vec![echo(json!(1), Vec::new())]),
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::Success);
        assert!(outcome.is_ok());
    }

    #[test]
    fn a_failed_assertion_stops_the_case_and_skips_the_rest() {
        let bus = EventBus::new();
        let mut failing = echo(json!("wrong"), Vec::new());
        failing.expect = vec![crate::core::case::Expectation {
            path: "$".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!("right")),
        }];
        let outcome = run(
            &loaded(vec![failing, echo(json!(1), Vec::new())]),
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::TestFailure);
        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        assert_eq!(report.cases[0].status, CaseStatus::Failed);
        assert_eq!(report.cases[0].steps[0].status, StepStatus::Failed);
        assert_eq!(report.cases[0].steps[1].status, StepStatus::Skipped);
    }

    #[test]
    fn an_optional_step_that_fails_an_assertion_is_skipped_and_the_case_continues() {
        let mut failing = echo(json!("wrong"), Vec::new());
        failing.optional = true;
        failing.expect = vec![crate::core::case::Expectation {
            path: "$".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!("right")),
        }];

        let outcome = run(
            &loaded(vec![failing, echo(json!("after"), Vec::new())]),
            &registry(),
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(
            outcome.exit_code(),
            crate::exit::ExitCode::Success,
            "a skipped optional step must not fail the run: {outcome:?}"
        );
        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        assert_eq!(report.summary().steps_skipped, 1);
        assert_eq!(report.summary().steps_passed, 1);
        let case = &report.cases[0];
        assert_eq!(case.status, CaseStatus::Passed);
        let skipped = &case.steps[0];
        assert_eq!(skipped.status, StepStatus::Skipped);
        assert_eq!(
            skipped.error.as_ref().map(|problem| problem.kind),
            Some(ProblemKind::Assertion),
            "the problem stays in the report for a reader"
        );
        assert_eq!(skipped.result, Some(json!("wrong")));
        assert_eq!(skipped.assertions.len(), 1);
        assert!(!skipped.assertions[0].passed);
        assert_eq!(
            case.steps[1].status,
            StepStatus::Passed,
            "the step after a skipped optional step must run"
        );
        assert_eq!(case.steps[1].result, Some(json!("after")));
    }

    #[test]
    fn an_optional_step_that_cannot_finish_is_skipped() {
        for action in ["test.execution", "test.timeout"] {
            let outcome = run(
                &loaded(vec![
                    optional(failing(action)),
                    echo(json!("after"), Vec::new()),
                ]),
                &registry(),
                &EventBus::new(),
                None,
                &Filters::default(),
                &mut Secrets::new(),
                1,
            );

            assert_eq!(
                outcome.exit_code(),
                crate::exit::ExitCode::Success,
                "`{action}` on an optional step must be skipped: {outcome:?}"
            );
            let case = one_case(outcome);
            assert_eq!(case.status, CaseStatus::Passed, "{action}");
            assert_eq!(case.steps[0].status, StepStatus::Skipped, "{action}");
            assert!(
                case.steps[0].error.is_some(),
                "`{action}`: the cause must stay in the report"
            );
            assert_eq!(case.steps[1].status, StepStatus::Passed, "{action}");
            assert_eq!(case.steps[1].result, Some(json!("after")), "{action}");
        }
    }

    #[test]
    fn an_optional_step_with_an_environment_error_stops_the_case() {
        let outcome = run(
            &loaded(vec![
                optional(failing("test.fail")),
                echo(json!("after"), Vec::new()),
            ]),
            &registry(),
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(
            outcome.exit_code(),
            crate::exit::ExitCode::Environment,
            "an environment error must not be skipped: {outcome:?}"
        );
        let case = one_case(outcome);
        assert_eq!(case.status, CaseStatus::Error);
        assert_eq!(case.steps[0].status, StepStatus::Error);
        assert_eq!(
            case.error.as_ref().map(|problem| problem.kind),
            Some(ProblemKind::Environment)
        );
        assert_eq!(
            case.steps[1].status,
            StepStatus::Skipped,
            "the case must stop after an environment error"
        );
        assert_eq!(case.steps[1].result, None);
    }

    #[test]
    fn an_optional_step_with_invalid_input_stops_the_case() {
        let outcome = run(
            &loaded(vec![
                optional(failing("test.invalid")),
                echo(json!("after"), Vec::new()),
            ]),
            &registry(),
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(
            outcome.exit_code(),
            crate::exit::ExitCode::InvalidCase,
            "invalid input must not be skipped: {outcome:?}"
        );
        let case = one_case(outcome);
        assert_eq!(case.status, CaseStatus::Error);
        assert_eq!(case.steps[0].status, StepStatus::Error);
        assert_eq!(case.steps[1].status, StepStatus::Skipped);
    }

    #[test]
    fn a_step_without_optional_keeps_its_old_outcome() {
        for (action, exit) in [
            ("test.execution", crate::exit::ExitCode::TestFailure),
            ("test.timeout", crate::exit::ExitCode::Timeout),
            ("test.fail", crate::exit::ExitCode::Environment),
            ("test.invalid", crate::exit::ExitCode::InvalidCase),
        ] {
            let outcome = run(
                &loaded(vec![failing(action), echo(json!("after"), Vec::new())]),
                &registry(),
                &EventBus::new(),
                None,
                &Filters::default(),
                &mut Secrets::new(),
                1,
            );

            assert_eq!(outcome.exit_code(), exit, "`{action}`: {outcome:?}");
            let case = one_case(outcome);
            assert_eq!(case.status, CaseStatus::Error, "`{action}`");
            assert_eq!(case.steps[0].status, StepStatus::Error, "`{action}`");
            assert_eq!(case.steps[1].status, StepStatus::Skipped, "`{action}`");
        }
    }

    #[test]
    fn an_environment_error_wins_over_a_failed_assertion() {
        let bus = EventBus::new();
        let mut failing = echo(json!("wrong"), Vec::new());
        failing.expect = vec![crate::core::case::Expectation {
            path: "$".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!("right")),
        }];
        let broken = Step {
            action: "test.fail".to_string(),
            args: json!({}),
            expect: Vec::new(),
            optional: false,
            retry: None,
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        };
        let mut cases = loaded(vec![failing]);
        cases.push(LoadedCase {
            path: "other.yaml".into(),
            case: test_case("other", vec![broken]),
        });

        let outcome = run(
            &cases,
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::Environment);
    }

    #[test]
    fn invalid_arguments_reject_the_run_before_any_step_runs() {
        let mut bad = echo(json!(1), Vec::new());
        bad.args = json!({});
        let mut cases = loaded(vec![bad]);
        cases.push(LoadedCase {
            path: "other.yaml".into(),
            case: test_case("other", vec![echo(json!(1), Vec::new())]),
        });
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let mut bus = EventBus::new();
        bus.subscribe(move |event| {
            recorder
                .lock()
                .expect("the recording mutex is poisoned")
                .push(format!("{event:?}"));
        });

        let outcome = run(
            &cases,
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::InvalidCase);
        let RunOutcome::Rejected { errors } = outcome else {
            panic!("the run must be rejected");
        };
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].message.contains("value"),
            "got: {}",
            errors[0].message
        );
        assert!(
            seen.lock()
                .expect("the recording mutex is poisoned")
                .is_empty(),
            "no event may be emitted when the run is rejected"
        );
    }

    #[test]
    fn listeners_see_every_event_in_run_order() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let mut bus = EventBus::new();
        bus.subscribe(move |event| {
            let label = match event {
                Event::RunStarted { .. } => "run started".to_string(),
                Event::CaseStarted { .. } => "case started".to_string(),
                Event::StepStarted { name, .. } => format!("step started {name}"),
                Event::StepFinished { name, .. } => format!("step finished {name}"),
                Event::CaseFinished { .. } => "case finished".to_string(),
                Event::RunFinished { .. } => "run finished".to_string(),
            };
            recorder
                .lock()
                .expect("the recording mutex is poisoned")
                .push(label);
        });

        let outcome = run(
            &loaded(vec![echo(json!(1), Vec::new()), echo(json!(2), Vec::new())]),
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert!(outcome.is_ok());
        assert_eq!(
            *seen.lock().expect("the recording mutex is poisoned"),
            vec![
                "run started",
                "case started",
                "step started test.echo",
                "step finished test.echo",
                "step started test.echo",
                "step finished test.echo",
                "case finished",
                "run finished",
            ]
        );
    }

    #[test]
    fn saved_values_feed_later_steps_and_failures_are_reported() {
        let bus = EventBus::new();
        let mut first = echo(json!({"token": "abc"}), Vec::new());
        first.save = vec![SavedValue {
            name: "token".to_string(),
            path: "$.token".to_string(),
        }];
        let second = Step {
            action: "test.echo".to_string(),
            args: json!({ "value": "${token}" }),
            expect: vec![crate::core::case::Expectation {
                path: "$".to_string(),
                matcher: crate::core::case::Matcher::Equals(json!("abc")),
            }],
            optional: false,
            retry: None,
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        };

        let outcome = run(
            &loaded(vec![first, second]),
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
    }

    /// Counts how often the runner asks one kind of session for evidence.
    static EVIDENCE_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct CountingSession;

    impl Session for CountingSession {
        fn close(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
            EVIDENCE_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(vec![
                Evidence::text("notes.txt", "the session was asked").expect("the name is valid"),
            ])
        }
    }

    /// A second counter, because Rust runs tests in one module concurrently.
    static RECORD_EVIDENCE_CALLS: AtomicUsize = AtomicUsize::new(0);

    struct RecordCountingSession;

    impl Session for RecordCountingSession {
        fn close(&mut self) -> Result<(), String> {
            Ok(())
        }

        fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
            RECORD_EVIDENCE_CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(vec![
                Evidence::text("notes.txt", "the session was asked").expect("the name is valid"),
            ])
        }
    }

    #[test]
    fn session_evidence_is_collected_only_for_a_failed_case_with_bundles() {
        let dir = std::env::temp_dir().join(format!("da-runner-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the fixture directory must be created");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: case\nsteps: []\n").expect("the case file must be written");

        let mut registry = registry();
        registry
            .register(StepDefinition {
                name: "test.session",
                description: "Opens a session that counts its evidence requests.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |_, sessions| {
                    sessions
                        .get_or_create("test.session", || Ok(CountingSession))
                        .map_err(StepError::Environment)?;
                    Ok(json!({"opened": true}))
                },
            })
            .expect("the session step must register");

        let session_step = |expect| Step {
            action: "test.session".to_string(),
            args: json!({}),
            expect,
            optional: false,
            retry: None,
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        };
        let failing_expectation = || {
            vec![crate::core::case::Expectation {
                path: "$.opened".to_string(),
                matcher: crate::core::case::Matcher::Equals(json!(false)),
            }]
        };

        let mut cases = loaded(vec![session_step(Vec::new())]);
        cases[0].path = source.clone();
        EVIDENCE_CALLS.store(0, Ordering::SeqCst);
        let mut bundles = FailureBundles::new(dir.join("passing-failures"));
        let outcome = run(
            &cases,
            &registry,
            &EventBus::new(),
            Some(&mut bundles),
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(
            EVIDENCE_CALLS.load(Ordering::SeqCst),
            0,
            "a passing case must not ask for evidence, even when bundles are enabled"
        );

        let mut failing = loaded(vec![session_step(failing_expectation())]);
        failing[0].path = source.clone();
        EVIDENCE_CALLS.store(0, Ordering::SeqCst);
        let outcome = run(
            &failing,
            &registry,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );
        assert_eq!(
            outcome.exit_code(),
            crate::exit::ExitCode::TestFailure,
            "{outcome:?}"
        );
        assert_eq!(
            EVIDENCE_CALLS.load(Ordering::SeqCst),
            0,
            "`--no-bundle` must not ask for evidence"
        );

        EVIDENCE_CALLS.store(0, Ordering::SeqCst);
        let mut bundles = FailureBundles::new(dir.join("failures"));
        let outcome = run(
            &failing,
            &registry,
            &EventBus::new(),
            Some(&mut bundles),
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );
        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        assert_eq!(
            EVIDENCE_CALLS.load(Ordering::SeqCst),
            1,
            "a failing case with bundles must ask once"
        );
        let bundle = report.cases[0]
            .bundle
            .as_ref()
            .expect("the failed case must get a bundle");
        assert_eq!(
            std::fs::read_to_string(bundle.join("test.session/notes.txt"))
                .expect("the evidence must be written"),
            "the session was asked"
        );
        assert!(report.bundle_errors.is_empty(), "{report:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_collects_evidence_for_a_passing_case_and_an_ordinary_run_does_not() {
        let dir = std::env::temp_dir().join(format!("da-runner-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the fixture directory must be created");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: case\nsteps: []\n").expect("the case file must be written");

        let mut registry = registry();
        registry
            .register(StepDefinition {
                name: "test.session",
                description: "Opens a session that counts its evidence requests.",
                args_schema: json!({"type": "object"}),
                validate: |_| Ok(()),
                check_retry: crate::core::registry::disallow_retry,
                run: |_, sessions| {
                    sessions
                        .get_or_create("test.session", || Ok(RecordCountingSession))
                        .map_err(StepError::Environment)?;
                    Ok(json!({"opened": true}))
                },
            })
            .expect("the session step must register");

        let mut cases = loaded(vec![Step {
            action: "test.session".to_string(),
            args: json!({}),
            optional: false,
            retry: None,
            expect: Vec::new(),
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        }]);
        cases[0].path = source;

        RECORD_EVIDENCE_CALLS.store(0, Ordering::SeqCst);
        let mut record = FailureBundles::record(dir.join("record"));
        let outcome = run_with_record(
            &cases,
            &registry,
            &EventBus::new(),
            None,
            Some(&mut record),
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(
            RECORD_EVIDENCE_CALLS.load(Ordering::SeqCst),
            1,
            "a recorded passing case must ask for evidence once"
        );

        let recorded: Vec<_> = std::fs::read_dir(dir.join("record"))
            .expect("the record directory must exist")
            .map(|entry| entry.expect("the entry must be readable").path())
            .collect();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(recorded[0].join("meta.json"))
                .expect("the recorded meta must be readable"),
        )
        .expect("the recorded meta must be JSON");
        assert_eq!(meta["status"], json!("passed"));
        assert_eq!(
            std::fs::read_to_string(recorded[0].join("test.session/notes.txt"))
                .expect("the recorded evidence must be readable"),
            "the session was asked"
        );

        RECORD_EVIDENCE_CALLS.store(0, Ordering::SeqCst);
        let outcome = run(
            &cases,
            &registry,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(
            RECORD_EVIDENCE_CALLS.load(Ordering::SeqCst),
            0,
            "an ordinary passing run must not collect evidence"
        );
        assert!(
            !dir.join("record/test.session").exists(),
            "the ordinary run must not write into the record"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_and_failure_bundle_are_written_independently() {
        let dir =
            std::env::temp_dir().join(format!("da-runner-record-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the fixture directory must be created");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: case\nsteps: []\n").expect("the case file must be written");

        let mut failing = echo(json!("wrong"), Vec::new());
        failing.expect = vec![crate::core::case::Expectation {
            path: "$".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!("right")),
        }];
        let mut cases = loaded(vec![failing]);
        cases[0].path = source;

        let mut bundles = FailureBundles::new(dir.join("failures"));
        let mut record = FailureBundles::record(dir.join("record"));
        let outcome = run_with_record(
            &cases,
            &registry(),
            &EventBus::new(),
            Some(&mut bundles),
            Some(&mut record),
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );
        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        let bundle = report.cases[0]
            .bundle
            .as_ref()
            .expect("the failing case must keep its failure bundle");
        assert!(bundle.starts_with(dir.join("failures")), "{bundle:?}");
        assert!(bundle.join("meta.json").is_file());
        let recorded: Vec<_> = std::fs::read_dir(dir.join("record"))
            .expect("the record directory must exist")
            .map(|entry| entry.expect("the entry must be readable").path())
            .collect();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert!(recorded[0].join("meta.json").is_file());
        assert!(report.bundle_errors.is_empty(), "{report:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_the_failing_case_gets_a_bundle() {
        let dir = std::env::temp_dir().join(format!("da-runner-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the fixture directory must be created");
        let source = dir.join("failing.yaml");
        std::fs::write(&source, "name: case\nsteps: []\n").expect("the case file must be written");

        let mut failing = echo(json!("wrong"), Vec::new());
        failing.expect = vec![crate::core::case::Expectation {
            path: "$".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!("right")),
        }];
        let mut cases = loaded(vec![failing]);
        cases[0].path = source;
        cases.push(LoadedCase {
            path: "other.yaml".into(),
            case: test_case("other", vec![echo(json!(1), Vec::new())]),
        });

        let mut bundles = FailureBundles::new(dir.join("failures"));
        let bus = EventBus::new();
        let outcome = run(
            &cases,
            &registry(),
            &bus,
            Some(&mut bundles),
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        let bundle = report.cases[0]
            .bundle
            .as_ref()
            .expect("the failed case must get a bundle");
        assert!(bundle.join("meta.json").is_file(), "{bundle:?}");
        assert!(report.cases[1].bundle.is_none());
        assert!(report.bundle_errors.is_empty(), "{report:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filters_select_which_cases_run_and_count_the_rest() {
        let mut cases = loaded(vec![echo(json!(1), Vec::new())]);
        cases[0].case.project = Some("shop".to_string());
        cases[0].case.layer = Some("api".to_string());
        cases[0].case.tags = vec!["smoke".to_string()];
        let mut excluded = LoadedCase {
            path: "other.yaml".into(),
            case: test_case("other", vec![echo(json!(2), Vec::new())]),
        };
        excluded.case.layer = Some("cli".to_string());
        cases.push(excluded);
        let filters = Filters {
            project: vec!["shop".to_string()],
            layer: vec!["api".to_string()],
            tag: vec!["smoke".to_string()],
            ..Filters::default()
        };

        let outcome = run(
            &cases,
            &registry(),
            &EventBus::new(),
            None,
            &filters,
            &mut Secrets::new(),
            1,
        );

        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        assert_eq!(report.cases.len(), 1);
        assert_eq!(report.cases[0].name, "case");
        assert_eq!(report.cases[0].layer.as_deref(), Some("api"));
        assert_eq!(report.cases[0].tags, vec!["smoke".to_string()]);
        assert_eq!(report.filtered, 1);
        assert_eq!(report.summary().filtered, 1);
    }

    #[test]
    fn filters_that_select_nothing_are_reported_with_the_available_values() {
        let mut cases = loaded(vec![echo(json!(1), Vec::new())]);
        cases[0].case.layer = Some("api".to_string());
        cases[0].case.tags = vec!["smoke".to_string()];
        let filters = Filters {
            tag: vec!["missing".to_string()],
            ..Filters::default()
        };

        let outcome = run(
            &cases,
            &registry(),
            &EventBus::new(),
            None,
            &filters,
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::Usage);
        let RunOutcome::NoMatch { message } = outcome else {
            panic!("the run must report that nothing matched");
        };
        assert!(message.contains("--tag \"missing\""), "got: {message}");
        assert!(message.contains("`smoke`"), "got: {message}");
    }

    #[test]
    fn a_filter_does_not_hide_an_invalid_case() {
        let mut cases = loaded(vec![echo(json!(1), Vec::new())]);
        cases[0].case.layer = Some("api".to_string());
        let mut broken = LoadedCase {
            path: "broken.yaml".into(),
            case: test_case(
                "broken",
                vec![Step {
                    action: "no.such.step".to_string(),
                    args: json!({}),
                    expect: Vec::new(),
                    optional: false,
                    retry: None,
                    save: Vec::new(),
                    location: Location::UNKNOWN,
                    args_location: Location::UNKNOWN,
                    retry_location: Location::UNKNOWN,
                    arg_locations: Vec::new(),
                }],
            ),
        };
        broken.case.layer = Some("cli".to_string());
        cases.push(broken);
        let filters = Filters {
            layer: vec!["api".to_string()],
            ..Filters::default()
        };

        let outcome = run(
            &cases,
            &registry(),
            &EventBus::new(),
            None,
            &filters,
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::InvalidCase);
        let RunOutcome::Rejected { errors } = outcome else {
            panic!("the invalid case must reject the run");
        };
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].file.ends_with("broken.yaml"),
            "got: {:?}",
            errors[0].file
        );
    }

    #[test]
    fn cases_that_finish_in_another_order_are_reported_in_discovery_order() {
        // The first case waits longest, so under four jobs the last one
        // finishes first: a report collected as the cases finish would come out
        // in the opposite order.
        let mut cases = Vec::new();
        for (name, millis) in [("a", 400_u64), ("b", 200), ("c", 100), ("d", 0)] {
            cases.push(LoadedCase {
                path: format!("{name}.yaml").into(),
                case: test_case(name, vec![wait(millis, json!(name))]),
            });
        }

        let finished = std::sync::Arc::new(Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&finished);
        let mut bus = EventBus::new();
        bus.subscribe(move |event| {
            if let Event::CaseFinished { case, .. } = event {
                recorder
                    .lock()
                    .expect("the recording mutex is poisoned")
                    .push(*case);
            }
        });

        let outcome = run(
            &cases,
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            4,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
        assert_ne!(
            *finished.lock().expect("the recording mutex is poisoned"),
            vec![0, 1, 2, 3],
            "the cases must finish out of order, or this test proves nothing"
        );
        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        let names: Vec<&str> = report.cases.iter().map(|case| case.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["a", "b", "c", "d"],
            "the report must follow the discovery order, not the completion order"
        );
    }

    #[test]
    fn a_job_count_outside_the_range_still_runs_every_case_once() {
        let cases: Vec<LoadedCase> = ["a", "b", "c"]
            .into_iter()
            .map(|name| LoadedCase {
                path: format!("{name}.yaml").into(),
                case: test_case(name, vec![echo(json!(1), Vec::new())]),
            })
            .collect();

        for jobs in [usize::MAX, 4, 1, 0] {
            let outcome = run(
                &cases,
                &registry(),
                &EventBus::new(),
                None,
                &Filters::default(),
                &mut Secrets::new(),
                jobs,
            );
            let RunOutcome::Completed(report) = outcome else {
                panic!("the run must complete with {jobs} jobs");
            };
            assert_eq!(report.cases.len(), 3, "with {jobs} jobs");
            for case in &report.cases {
                assert_eq!(case.steps.len(), 1, "with {jobs} jobs: {case:?}");
            }
            for name in ["a", "b", "c"] {
                let count = report.cases.iter().filter(|case| case.name == name).count();
                assert_eq!(count, 1, "`{name}` must run exactly once with {jobs} jobs");
            }
        }
    }

    #[test]
    fn one_case_keeps_its_events_in_order_when_cases_run_at_the_same_time() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let mut bus = EventBus::new();
        bus.subscribe(move |event| {
            if let Event::StepStarted { case, step, .. } | Event::StepFinished { case, step, .. } =
                event
            {
                recorder
                    .lock()
                    .expect("the recording mutex is poisoned")
                    .push((*case, *step));
            }
        });
        let cases: Vec<LoadedCase> = (0..3)
            .map(|index| LoadedCase {
                path: format!("case-{index}.yaml").into(),
                case: test_case(
                    &format!("case {index}"),
                    vec![wait(20, json!(1)), wait(10, json!(2))],
                ),
            })
            .collect();

        let outcome = run(
            &cases,
            &registry(),
            &bus,
            None,
            &Filters::default(),
            &mut Secrets::new(),
            3,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
        let seen = seen.lock().expect("the recording mutex is poisoned");
        for case in 0..3 {
            let steps: Vec<usize> = seen
                .iter()
                .filter(|(recorded, _)| *recorded == case)
                .map(|(_, step)| *step)
                .collect();
            assert_eq!(
                steps,
                vec![0, 0, 1, 1],
                "case {case} must see its own steps in order, whatever the other cases do"
            );
        }
    }

    #[test]
    fn every_failing_case_of_a_parallel_run_gets_its_own_complete_bundle() {
        let dir =
            std::env::temp_dir().join(format!("da-runner-parallel-bundles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the fixture directory must be created");

        let mut cases = Vec::new();
        for index in 0..3 {
            let source = dir.join(format!("case-{index}.yaml"));
            std::fs::write(&source, "name: case\nsteps: []\n")
                .expect("the case file must be written");
            let mut failing = echo(json!("wrong"), Vec::new());
            failing.expect = vec![crate::core::case::Expectation {
                path: "$".to_string(),
                matcher: crate::core::case::Matcher::Equals(json!("right")),
            }];
            cases.push(LoadedCase {
                path: source,
                case: test_case(&format!("case {index}"), vec![failing]),
            });
        }

        let mut bundles = FailureBundles::new(dir.join("failures"));
        let outcome = run(
            &cases,
            &registry(),
            &EventBus::new(),
            Some(&mut bundles),
            &Filters::default(),
            &mut Secrets::new(),
            3,
        );

        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        assert!(report.bundle_errors.is_empty(), "{report:?}");
        let mut directories = std::collections::BTreeSet::new();
        for case in &report.cases {
            let bundle = case
                .bundle
                .as_ref()
                .expect("every failing case must get a bundle");
            assert!(bundle.join("meta.json").is_file(), "{bundle:?}");
            assert!(bundle.join("report.json").is_file(), "{bundle:?}");
            directories.insert(
                bundle
                    .file_name()
                    .expect("a bundle directory has a name")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        assert_eq!(
            directories,
            ["0-case-0", "1-case-1", "2-case-2"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn run_one_case(case: Case) -> RunOutcome {
        let registry = registry();
        run(
            &[LoadedCase {
                path: "case.yaml".into(),
                case,
            }],
            &registry,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        )
    }

    #[test]
    fn observations_are_merged_into_the_result_before_the_assertions() {
        let mut case = test_case(
            "observed",
            vec![echo(
                json!(1),
                vec![crate::core::case::Expectation {
                    path: "$.observed.app_log.note".to_string(),
                    matcher: crate::core::case::Matcher::Equals(json!("hello")),
                }],
            )],
        );
        case.observe = vec![observation(
            "app_log",
            "test.watch",
            json!({"note": "hello"}),
        )];

        let outcome = run_one_case(case);

        assert!(outcome.is_ok(), "{outcome:?}");
        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        let result = report.cases[0].steps[0]
            .result
            .as_ref()
            .expect("a passed step keeps its result");
        assert_eq!(result["value"], json!(1), "the step result must be kept");
        assert_eq!(
            result["observed"]["app_log"]["note"],
            json!("hello"),
            "the observation must be merged under `observed`"
        );
    }

    #[test]
    fn a_result_that_is_not_an_object_keeps_the_observation_under_value() {
        let mut case = test_case(
            "scalar",
            vec![Step {
                action: "test.scalar".to_string(),
                args: json!({}),
                expect: vec![
                    crate::core::case::Expectation {
                        path: "$.value".to_string(),
                        matcher: crate::core::case::Matcher::Equals(json!(5)),
                    },
                    crate::core::case::Expectation {
                        path: "$.observed.app_log.note".to_string(),
                        matcher: crate::core::case::Matcher::Equals(json!("hello")),
                    },
                ],
                optional: false,
                retry: None,
                save: Vec::new(),
                location: Location::UNKNOWN,
                args_location: Location::UNKNOWN,
                retry_location: Location::UNKNOWN,
                arg_locations: Vec::new(),
            }],
        );
        case.observe = vec![observation(
            "app_log",
            "test.watch",
            json!({"note": "hello"}),
        )];

        let outcome = run_one_case(case);

        assert!(outcome.is_ok(), "{outcome:?}");
    }

    #[test]
    fn an_observer_that_cannot_collect_makes_the_step_an_environment_error() {
        let mut case = test_case("broken", vec![echo(json!(1), Vec::new())]);
        case.observe = vec![observation("app_log", "test.broken", json!({}))];

        let outcome = run_one_case(case);

        assert_eq!(
            outcome.exit_code(),
            crate::exit::ExitCode::Environment,
            "{outcome:?}"
        );
        let RunOutcome::Completed(report) = outcome else {
            panic!("the run must complete");
        };
        let problem = report.cases[0].steps[0]
            .error
            .as_ref()
            .expect("the step must carry the observer's problem");
        assert_eq!(problem.kind, ProblemKind::Environment);
        assert!(problem.message.contains("app_log"), "{problem:?}");
        assert!(problem.message.contains("test.broken"), "{problem:?}");
        assert!(problem.message.contains("app.log"), "{problem:?}");
    }

    #[test]
    fn an_unknown_observation_rejects_the_run_before_any_step_runs() {
        let mut case = test_case("unknown", vec![echo(json!(1), Vec::new())]);
        case.observe = vec![observation("app_log", "test.missing", json!({}))];

        let outcome = run_one_case(case);

        assert_eq!(
            outcome.exit_code(),
            crate::exit::ExitCode::InvalidCase,
            "{outcome:?}"
        );
        let RunOutcome::Rejected { errors } = outcome else {
            panic!("the run must be rejected");
        };
        assert!(
            errors[0].message.contains("test.missing"),
            "{:?}",
            errors[0]
        );
        assert!(
            errors[0]
                .message
                .contains("registered observers: `test.broken`, `test.watch`"),
            "the message must list the registered observers, got: {:?}",
            errors[0]
        );
    }

    #[test]
    fn observation_arguments_that_do_not_validate_reject_the_run() {
        let mut case = test_case("arguments", vec![echo(json!(1), Vec::new())]);
        case.observe = vec![observation("app_log", "test.watch", json!({}))];

        let outcome = run_one_case(case);

        assert_eq!(
            outcome.exit_code(),
            crate::exit::ExitCode::InvalidCase,
            "{outcome:?}"
        );
        let RunOutcome::Rejected { errors } = outcome else {
            panic!("the run must be rejected");
        };
        assert!(errors[0].message.contains("app_log"), "{:?}", errors[0]);
        assert!(errors[0].message.contains("note"), "{:?}", errors[0]);
    }

    #[test]
    fn retry_rejects_steps_whose_check_retry_refuses_in_preflight() {
        let mut step = echo(json!("hi"), Vec::new());
        step.retry = Some(crate::core::case::RetryPolicy {
            until: vec![Expectation {
                path: "$".to_string(),
                matcher: crate::core::case::Matcher::Equals(json!("hi")),
            }],
            timeout_ms: 500,
            interval_ms: 20,
        });
        let outcome = run(
            &loaded(vec![step]),
            &registry(),
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::InvalidCase);
        let RunOutcome::Rejected { errors } = outcome else {
            panic!("check_retry failure must reject before running");
        };
        assert!(errors[0].message.contains("test.echo"), "{:?}", errors[0]);
        assert!(
            errors[0].message.contains("cannot use `retry`"),
            "{:?}",
            errors[0]
        );
    }

    #[test]
    fn retry_retries_execution_and_timeout_errors_then_passes_and_isolates_observations() {
        static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
        static OBSERVER_CALLS: AtomicUsize = AtomicUsize::new(0);

        struct AttemptObserver(usize);

        impl Observer for AttemptObserver {
            fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
                self.0 = OBSERVER_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(())
            }

            fn collect(&mut self, _sessions: &mut Sessions) -> Result<Value, ObserverError> {
                Ok(json!({ "attempt": self.0 }))
            }
        }

        let mut reg = registry();
        reg.register(StepDefinition {
            name: "test.flaky",
            description: "Fails execution, then timeout, then returns ready.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: crate::core::registry::allow_retry,
            run: |_, _| {
                let n = ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
                match n {
                    1 => Err(StepError::Execution("not ready yet".to_string())),
                    2 => Err(StepError::Timeout("timed out once".to_string())),
                    3 => Ok(json!({ "ready": false, "attempt": n })),
                    _ => Ok(json!({ "ready": true, "attempt": n })),
                }
            },
        })
        .expect("flaky step must register");
        reg.register_observer(crate::core::observer::ObserverDefinition {
            name: "test.attempt",
            description: "Records attempt number.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            create: |_| Ok(Box::new(AttemptObserver(0))),
        })
        .expect("attempt observer must register");

        ATTEMPTS.store(0, Ordering::SeqCst);
        OBSERVER_CALLS.store(0, Ordering::SeqCst);

        let mut step = failing("test.flaky");
        step.retry = Some(crate::core::case::RetryPolicy {
            until: vec![
                Expectation {
                    path: "$.ready".to_string(),
                    matcher: crate::core::case::Matcher::Equals(json!(true)),
                },
                Expectation {
                    path: "$.observed.snap.attempt".to_string(),
                    matcher: crate::core::case::Matcher::Equals(json!(4)),
                },
            ],
            timeout_ms: 2000,
            interval_ms: 10,
        });
        step.expect = vec![Expectation {
            path: "$.attempt".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!(4)),
        }];
        step.save = vec![SavedValue {
            name: "attempts".to_string(),
            path: "$.attempt".to_string(),
        }];

        let mut case = test_case("retry", vec![step, echo(json!("${attempts}"), Vec::new())]);
        case.observe = vec![observation("snap", "test.attempt", json!({}))];

        let outcome = run(
            &[LoadedCase {
                path: "retry.yaml".into(),
                case,
            }],
            &reg,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
        let report = one_case(outcome);
        assert_eq!(report.steps[0].assertions.len(), 3);
        assert!(report.steps[0].assertions.iter().all(|a| a.passed));
        let result = report.steps[0]
            .result
            .as_ref()
            .expect("step result must be recorded");
        assert_eq!(result["observed"]["snap"]["attempt"], json!(4));
        assert_eq!(
            report.steps[1].result.as_ref().map(|v| &v["value"]),
            Some(&json!("4"))
        );
    }

    #[test]
    fn retry_aborts_immediately_on_invalid_arguments_and_environment_errors() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);

        let mut reg = registry();
        for (name, fail_env) in [("test.retry_env", true), ("test.retry_invalid", false)] {
            let step_def = if fail_env {
                StepDefinition {
                    name,
                    description: "Fails environment.",
                    args_schema: json!({"type": "object"}),
                    validate: |_| Ok(()),
                    check_retry: crate::core::registry::allow_retry,
                    run: |_, _| {
                        CALLS.fetch_add(1, Ordering::SeqCst);
                        Err(StepError::Environment("down".to_string()))
                    },
                }
            } else {
                StepDefinition {
                    name,
                    description: "Fails invalid arguments.",
                    args_schema: json!({"type": "object"}),
                    validate: |_| Ok(()),
                    check_retry: crate::core::registry::allow_retry,
                    run: |_, _| {
                        CALLS.fetch_add(1, Ordering::SeqCst);
                        Err(StepError::InvalidArguments("bad arg".to_string()))
                    },
                }
            };
            reg.register(step_def).expect("test step must register");
        }

        for (action, expected_exit) in [
            ("test.retry_env", crate::exit::ExitCode::Environment),
            ("test.retry_invalid", crate::exit::ExitCode::InvalidCase),
        ] {
            CALLS.store(0, Ordering::SeqCst);
            let mut step = failing(action);
            step.retry = Some(crate::core::case::RetryPolicy {
                until: vec![Expectation {
                    path: "$".to_string(),
                    matcher: crate::core::case::Matcher::Exists(true),
                }],
                timeout_ms: 2000,
                interval_ms: 10,
            });

            let outcome = run(
                &loaded(vec![step]),
                &reg,
                &EventBus::new(),
                None,
                &Filters::default(),
                &mut Secrets::new(),
                1,
            );

            assert_eq!(outcome.exit_code(), expected_exit, "{action}");
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                1,
                "{action} must stop after the first fatal error"
            );
        }
    }

    /// A session-backed event buffer modeling observers that only drain in `collect`.
    struct BufferedEvents(Vec<String>);

    impl Session for BufferedEvents {
        fn close(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    struct DrainingObserver;

    impl Observer for DrainingObserver {
        fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
            // Deliberately leaves any uncollected entries untouched, matching
            // observers whose `collect` drains the step's completed events.
            Ok(())
        }

        fn collect(&mut self, sessions: &mut Sessions) -> Result<Value, ObserverError> {
            let events = match sessions
                .get::<BufferedEvents>("test.buffered")
                .map_err(ObserverError::Environment)?
            {
                Some(buffer) => std::mem::take(&mut buffer.0),
                None => Vec::new(),
            };
            Ok(json!({ "events": events }))
        }
    }

    #[test]
    fn retry_discards_stateful_observer_state_from_failed_execution_and_timeout_attempts() {
        static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

        let mut reg = registry();
        reg.register(StepDefinition {
            name: "test.buffered_flaky",
            description: "Records buffered events across execution, timeout, and ready attempts.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: crate::core::registry::allow_retry,
            run: |_, sessions| {
                let n = ATTEMPTS.fetch_add(1, Ordering::SeqCst) + 1;
                let buffer = sessions
                    .get_or_create("test.buffered", || Ok(BufferedEvents(Vec::new())))
                    .map_err(StepError::Environment)?;
                match n {
                    1 => {
                        buffer.0.push("attempt-1-exec".to_string());
                        Err(StepError::Execution("not ready yet".to_string()))
                    }
                    2 => {
                        buffer.0.push("attempt-2-timeout".to_string());
                        Err(StepError::Timeout("timed out once".to_string()))
                    }
                    3 => {
                        buffer.0.push("attempt-3-ok".to_string());
                        Ok(json!({ "ready": true, "attempt": n }))
                    }
                    _ => panic!("unexpected extra attempt {n}"),
                }
            },
        })
        .expect("buffered flaky step must register");
        reg.register_observer(crate::core::observer::ObserverDefinition {
            name: "test.draining",
            description: "Drains buffered events on collect.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            create: |_| Ok(Box::new(DrainingObserver)),
        })
        .expect("draining observer must register");

        ATTEMPTS.store(0, Ordering::SeqCst);

        let mut step = failing("test.buffered_flaky");
        step.retry = Some(crate::core::case::RetryPolicy {
            until: vec![Expectation {
                path: "$.ready".to_string(),
                matcher: crate::core::case::Matcher::Equals(json!(true)),
            }],
            timeout_ms: 2000,
            interval_ms: 10,
        });
        step.expect = vec![Expectation {
            path: "$.observed.buf.events".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!(["attempt-3-ok"])),
        }];

        let mut case = test_case("retry-drains-observer", vec![step]);
        case.observe = vec![observation("buf", "test.draining", json!({}))];

        let outcome = run(
            &[LoadedCase {
                path: "retry-drain.yaml".into(),
                case,
            }],
            &reg,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(ATTEMPTS.load(Ordering::SeqCst), 3);
        let report = one_case(outcome);
        let result = report.steps[0]
            .result
            .as_ref()
            .expect("step result must be recorded");
        assert_eq!(
            result["observed"]["buf"]["events"],
            json!(["attempt-3-ok"]),
            "failed attempt events must never leak into the final observation: {result}"
        );
    }

    #[test]
    fn optional_skipped_step_discards_observer_state_before_next_step() {
        let mut reg = registry();
        reg.register(StepDefinition {
            name: "test.emit_and_fail",
            description: "Emits a buffered event then fails execution.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: crate::core::registry::disallow_retry,
            run: |_, sessions| {
                let buffer = sessions
                    .get_or_create("test.buffered", || Ok(BufferedEvents(Vec::new())))
                    .map_err(StepError::Environment)?;
                buffer.0.push("stale-optional".to_string());
                Err(StepError::Execution("optional failure".to_string()))
            },
        })
        .expect("emit-and-fail step must register");
        reg.register(StepDefinition {
            name: "test.emit_and_ok",
            description: "Emits a buffered event then succeeds.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: crate::core::registry::disallow_retry,
            run: |_, sessions| {
                let buffer = sessions
                    .get_or_create("test.buffered", || Ok(BufferedEvents(Vec::new())))
                    .map_err(StepError::Environment)?;
                buffer.0.push("second-step-ok".to_string());
                Ok(json!({ "ok": true }))
            },
        })
        .expect("emit-and-ok step must register");
        reg.register_observer(crate::core::observer::ObserverDefinition {
            name: "test.draining",
            description: "Drains buffered events on collect.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            create: |_| Ok(Box::new(DrainingObserver)),
        })
        .expect("draining observer must register");

        let mut first = failing("test.emit_and_fail");
        first.optional = true;
        let mut second = failing("test.emit_and_ok");
        second.expect = vec![Expectation {
            path: "$.observed.buf.events".to_string(),
            matcher: crate::core::case::Matcher::Equals(json!(["second-step-ok"])),
        }];

        let mut case = test_case("optional-drains-observer", vec![first, second]);
        case.observe = vec![observation("buf", "test.draining", json!({}))];

        let outcome = run(
            &[LoadedCase {
                path: "optional-drain.yaml".into(),
                case,
            }],
            &reg,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert!(outcome.is_ok(), "{outcome:?}");
        let report = one_case(outcome);
        assert_eq!(report.steps[0].status, StepStatus::Skipped);
        let result = report.steps[1]
            .result
            .as_ref()
            .expect("second step result must be recorded");
        assert_eq!(
            result["observed"]["buf"]["events"],
            json!(["second-step-ok"])
        );
    }

    #[test]
    fn retry_aborts_immediately_if_observer_collect_fails_after_retryable_step_error() {
        static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

        let mut reg = registry();
        reg.register(StepDefinition {
            name: "test.retry_exec",
            description: "Fails execution.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: crate::core::registry::allow_retry,
            run: |_, _| {
                ATTEMPTS.fetch_add(1, Ordering::SeqCst);
                Err(StepError::Execution("not ready".to_string()))
            },
        })
        .expect("test step must register");

        ATTEMPTS.store(0, Ordering::SeqCst);
        let mut step = failing("test.retry_exec");
        step.retry = Some(crate::core::case::RetryPolicy {
            until: vec![Expectation {
                path: "$".to_string(),
                matcher: crate::core::case::Matcher::Exists(true),
            }],
            timeout_ms: 2000,
            interval_ms: 10,
        });
        let mut case = test_case("broken-collect-aborts-retry", vec![step]);
        case.observe = vec![observation("app_log", "test.broken", json!({}))];

        let outcome = run(
            &[LoadedCase {
                path: "broken-retry.yaml".into(),
                case,
            }],
            &reg,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        assert_eq!(outcome.exit_code(), crate::exit::ExitCode::Environment);
        assert_eq!(
            ATTEMPTS.load(Ordering::SeqCst),
            1,
            "observer environment failure must stop retry after attempt 1"
        );
    }
}
