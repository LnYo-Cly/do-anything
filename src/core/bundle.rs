//! Failure bundles: the evidence a failed run leaves on disk.
//!
//! One run writes one directory per failed case. Each case directory is staged
//! under a temporary name in the same parent and renamed into place, and
//! `meta.json` is the last file written. A directory that contains `meta.json`
//! is therefore complete; a reader must ignore every other directory.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::core::output;
use crate::core::paths;
use crate::core::report::CaseReport;
use crate::core::secrets::Secrets;
use crate::core::session::{EvidenceContent, SessionEvidence, check_kind};

/// Version of the bundle layout this build writes.
pub const SCHEMA_VERSION: u64 = 1;
/// Directory that holds failure bundles unless `--bundle-dir` is given.
pub const DEFAULT_DIRECTORY: &str = ".da/failures";
/// Longest part of a directory name taken from a case name.
const MAX_NAME: usize = 40;

/// Which phase of a case a per-step trace belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StepPhase {
    /// Main `steps` list.
    Steps,
    /// Cleanup `finally` list.
    Finally,
}

impl StepPhase {
    /// Subdirectory name under `<bundle>/trace/`.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Steps => "steps",
            Self::Finally => "finally",
        }
    }
}

/// Per-step session evidence captured immediately after one step executes.
#[derive(Debug, Clone)]
pub struct StepTrace {
    /// Whether the step ran in `steps` or `finally`.
    pub phase: StepPhase,
    /// Zero-based index within that list.
    pub index: usize,
    /// Session evidence groups returned after the step finished.
    pub evidence: Vec<SessionEvidence>,
}

/// How a writer names the directory that holds its case directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One new timestamped run directory under the root, for failures only.
    Run,
    /// The root itself, for a complete run record.
    Record,
}

/// Writes the evidence bundles of a run.
///
/// The run directory is created with the first bundle, so a run where every
/// case passes leaves the file system untouched. A writer created by
/// [`Self::record`] writes directly into its root instead and expects a bundle
/// for every case.
#[derive(Debug)]
pub struct FailureBundles {
    root: PathBuf,
    run: Option<PathBuf>,
    sequence: u64,
    layout: Layout,
}

impl FailureBundles {
    /// Creates a writer that puts its run directories under `root`.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            run: None,
            sequence: 0,
            layout: Layout::Run,
        }
    }

    /// Creates a writer that puts every case directory directly into `root`.
    ///
    /// This is the writer behind `da run --record`: the given directory is the
    /// run directory, so a reader can render it without knowing a timestamp.
    /// [`Self::write`] still writes `meta.json` last.
    pub fn record(root: PathBuf) -> Self {
        Self {
            root,
            run: None,
            sequence: 0,
            layout: Layout::Record,
        }
    }

    /// Whether this writer expects a bundle for a passing case too.
    pub fn records_all(&self) -> bool {
        self.layout == Layout::Record
    }

    /// Writes the bundle of one failed case and returns its final directory.
    ///
    /// `position` is the index of the case in the run, which keeps the
    /// directories in run order and makes their names unique. `secrets` is
    /// applied to every file this function writes; the copy of the case file is
    /// left as written, because it holds `${env:NAME}` references and no values.
    pub fn write(
        &mut self,
        position: usize,
        case: &CaseReport,
        secrets: &Secrets,
    ) -> Result<PathBuf, String> {
        self.write_with_evidence(position, case, secrets, &[])
    }

    /// Writes the bundle of one failed case with the evidence its sessions
    /// returned.
    ///
    /// Each session kind becomes a directory under the bundle. Text evidence is
    /// redacted with `secrets`; bytes are written unchanged. A session that
    /// could not return its evidence leaves a text file naming the reason, so
    /// the bundle still explains the gap.
    pub fn write_with_evidence(
        &mut self,
        position: usize,
        case: &CaseReport,
        secrets: &Secrets,
        evidence: &[SessionEvidence],
    ) -> Result<PathBuf, String> {
        self.write_with_traces(position, case, secrets, evidence, &[])
    }

    /// Writes a case bundle with case-level evidence and optional per-step
    /// trace evidence staged under `trace/{steps,finally}/<idx>/<kind>/`.
    pub fn write_with_traces(
        &mut self,
        position: usize,
        case: &CaseReport,
        secrets: &Secrets,
        evidence: &[SessionEvidence],
        step_traces: &[StepTrace],
    ) -> Result<PathBuf, String> {
        // Checked before anything is created, so a case file that vanished
        // leaves no empty directory behind.
        std::fs::metadata(&case.file).map_err(|error| {
            self.failure_message(
                case,
                format!("could not read `{}`: {error}", paths::display(&case.file)),
            )
        })?;

        let run = self
            .run_directory()
            .map_err(|detail| self.failure_message(case, detail))?;
        let final_dir = run.join(format!("{position}-{}", safe_name(&case.name)));
        let staging = run.join(format!(".tmp-{}-{}", std::process::id(), self.sequence));
        self.sequence += 1;

        // An earlier attempt that failed may have left its staging directory.
        let _ = std::fs::remove_dir_all(&staging);
        if let Err(error) = std::fs::create_dir(&staging) {
            return Err(self.failure_message(
                case,
                format!("could not create `{}`: {error}", paths::display(&staging)),
            ));
        }

        if let Err(detail) = stage(&staging, case, &final_dir, secrets, evidence, step_traces) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(self.failure_message(case, detail));
        }
        if let Err(error) = std::fs::rename(&staging, &final_dir) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(self.failure_message(
                case,
                format!(
                    "could not move `{}` to `{}`: {error}",
                    paths::display(&staging),
                    paths::display(&final_dir)
                ),
            ));
        }
        // The metadata comes last: it is what tells a reader that the rest of
        // the directory is complete.
        if let Err(detail) = write_meta(&final_dir, case, secrets, evidence, step_traces) {
            let _ = std::fs::remove_dir_all(&final_dir);
            return Err(self.failure_message(case, detail));
        }
        Ok(final_dir)
    }

    fn run_directory(&mut self) -> Result<PathBuf, String> {
        if let Some(dir) = &self.run {
            return Ok(dir.clone());
        }
        let dir = match self.layout {
            Layout::Run => create_run_directory(&self.root)?,
            Layout::Record => {
                std::fs::create_dir_all(&self.root).map_err(|error| {
                    format!("could not create `{}`: {error}", paths::display(&self.root))
                })?;
                self.root.clone()
            }
        };
        self.run = Some(dir.clone());
        Ok(dir)
    }

    /// The message a caller reports without changing the run's exit code.
    fn failure_message(&self, case: &CaseReport, detail: String) -> String {
        match self.layout {
            Layout::Run => format!(
                "could not write the failure bundle for `{}`: {detail}; the run result is unchanged, and `--no-bundle` turns failure bundles off",
                case.name
            ),
            Layout::Record => format!(
                "could not write the recorded evidence for `{}`: {detail}; the run result is unchanged, and `--record` names the directory to check",
                case.name
            ),
        }
    }
}

fn stage(
    staging: &Path,
    case: &CaseReport,
    final_dir: &Path,
    secrets: &Secrets,
    evidence: &[SessionEvidence],
    step_traces: &[StepTrace],
) -> Result<(), String> {
    let case_copy = staging.join("case.yaml");
    std::fs::copy(&case.file, &case_copy).map_err(|error| {
        format!(
            "could not copy `{}` to `{}`: {error}",
            paths::display(&case.file),
            paths::display(&case_copy)
        )
    })?;

    // The report is the same object `--output json` shows for this case, so a
    // reader of the bundle and a reader of the output see one shape. Both are
    // redacted the same way, because a bundle is read after the terminal is
    // gone.
    let mut reported = case.clone();
    reported.bundle = Some(final_dir.to_path_buf());
    write_json(
        &staging.join("report.json"),
        &secrets.redact_json(&output::case_json(&reported)),
    )?;
    stage_evidence(staging, evidence, secrets)?;
    for trace in step_traces {
        if trace
            .evidence
            .iter()
            .all(|group| group.files.is_empty() && group.error.is_none())
        {
            continue;
        }
        let step_dir = staging
            .join("trace")
            .join(trace.phase.dir_name())
            .join(trace.index.to_string());
        stage_evidence(&step_dir, &trace.evidence, secrets)?;
    }
    Ok(())
}

/// Writes each session's evidence under a directory named after its kind.
fn stage_evidence(
    staging: &Path,
    evidence: &[SessionEvidence],
    secrets: &Secrets,
) -> Result<(), String> {
    for group in evidence {
        check_kind(&group.kind).map_err(|error| {
            format!(
                "could not write the evidence of session `{}`: {error}",
                group.kind
            )
        })?;
        if group.files.is_empty() && group.error.is_none() {
            continue;
        }
        let directory = staging.join(&group.kind);
        std::fs::create_dir_all(&directory).map_err(|error| {
            format!(
                "could not create the evidence directory `{}`: {error}",
                paths::display(&directory)
            )
        })?;
        for file in &group.files {
            let path = directory.join(file.name());
            let bytes = match file.content() {
                EvidenceContent::Text(text) => secrets.redact(text).into_bytes(),
                EvidenceContent::Bytes(bytes) => bytes.clone(),
            };
            std::fs::write(&path, bytes).map_err(|error| {
                format!(
                    "could not write the evidence file `{}`: {error}",
                    paths::display(&path)
                )
            })?;
        }
        if let Some(error) = &group.error {
            let path = directory.join("evidence-error.txt");
            std::fs::write(&path, secrets.redact(error).into_bytes()).map_err(|write_error| {
                format!(
                    "could not write the evidence error file `{}`: {write_error}",
                    paths::display(&path)
                )
            })?;
        }
    }
    Ok(())
}

fn write_meta(
    dir: &Path,
    case: &CaseReport,
    secrets: &Secrets,
    evidence: &[SessionEvidence],
    step_traces: &[StepTrace],
) -> Result<(), String> {
    let local_only_reason = case.local_only_reason.clone().or_else(|| {
        let mut reasons: Vec<&str> = Vec::new();
        for group in evidence
            .iter()
            .chain(step_traces.iter().flat_map(|t| t.evidence.iter()))
        {
            if let Some(reason) = group.local_only_reason.as_deref()
                && !reasons.contains(&reason)
            {
                reasons.push(reason);
            }
        }
        if reasons.is_empty() {
            None
        } else {
            Some(reasons.join("; "))
        }
    });
    let local_only = case.local_only
        || local_only_reason.is_some()
        || evidence
            .iter()
            .chain(step_traces.iter().flat_map(|t| t.evidence.iter()))
            .any(|group| group.evidence_is_local_only);
    let case_sha256 = match &case.case_sha256 {
        Some(hash) => hash.clone(),
        None => {
            let case_copy = dir.join("case.yaml");
            let bytes = std::fs::read(&case_copy).map_err(|error| {
                format!(
                    "could not read `{}` to hash it: {error}",
                    paths::display(&case_copy)
                )
            })?;
            crate::core::sha256::hex(&bytes)
        }
    };
    let mut meta = json!({
        "schema_version": SCHEMA_VERSION,
        "da_version": env!("CARGO_PKG_VERSION"),
        "case_file": paths::display(&case.file),
        "case_sha256": case_sha256,
        "case_name": case.name,
        "project": case.project,
        "suite": case.suite,
        "layer": case.layer,
        "tags": case.tags,
        "overridden_vars": case.overridden_vars,
        "status": case.status.as_str(),
        "local_only": local_only,
        "created_at": now_millis(),
    });
    if let Some(reason) = local_only_reason {
        meta["local_only_reason"] = Value::String(reason);
    }
    write_json(&dir.join("meta.json"), &secrets.redact_json(&meta))
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("could not render `{}`: {error}", paths::display(path)))?;
    std::fs::write(path, text + "\n")
        .map_err(|error| format!("could not write `{}`: {error}", paths::display(path)))
}

/// Creates `<root>/<millis>-<pid>` and returns it.
///
/// A name that is already taken gets a counter suffix, so a second run in the
/// same millisecond never writes into the directories of the first one.
fn create_run_directory(root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root)
        .map_err(|error| format!("could not create `{}`: {error}", paths::display(root)))?;
    let base = format!("{}-{}", now_millis(), std::process::id());
    for attempt in 1..=100 {
        let candidate = if attempt == 1 {
            root.join(&base)
        } else {
            root.join(format!("{base}-{attempt}"))
        };
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not create `{}`: {error}",
                    paths::display(&candidate)
                ));
            }
        }
    }
    Err(format!(
        "could not find a free run directory name under `{}`",
        paths::display(root)
    ))
}

/// Milliseconds since the Unix epoch, or 0 when the clock is set before it.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Turns a case name into a directory name that every platform accepts.
///
/// Only letters, digits and `_` are kept; every other character becomes a
/// single `-`. The case position prefixes the name, so a case called `con` or
/// `nul` cannot collide with a Windows device name.
fn safe_name(name: &str) -> String {
    let mut output = String::new();
    let mut used = 0;
    let mut after_separator = false;
    for character in name.chars() {
        if used >= MAX_NAME {
            break;
        }
        if character.is_alphanumeric() || character == '_' {
            output.push(character);
            used += 1;
            after_separator = false;
        } else if !after_separator && !output.is_empty() {
            output.push('-');
            used += 1;
            after_separator = true;
        }
    }
    let trimmed = output.trim_matches('-');
    if trimmed.is_empty() {
        "case".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::core::report::{CaseStatus, StepReport, StepStatus};

    fn failing_case(file: PathBuf) -> CaseReport {
        use std::collections::BTreeMap;
        CaseReport {
            file,
            case_sha256: None,
            name: "Wrong exit code".to_string(),
            project: Some("shop".to_string()),
            suite: Some("smoke".to_string()),
            layer: Some("cli".to_string()),
            tags: vec!["smoke".to_string(), "fast".to_string()],
            overridden_vars: Vec::new(),
            status: CaseStatus::Failed,
            duration: Duration::from_millis(3),
            steps: vec![StepReport {
                name: "cli.run".to_string(),
                status: StepStatus::Failed,
                duration: Duration::from_millis(2),
                args: None,
                result: Some(json!({"exit_code": 0})),
                saved: BTreeMap::new(),
                assertions: Vec::new(),
                error: None,
            }],
            finally: Vec::new(),
            error: None,
            close_errors: Vec::new(),
            bundle: None,
            local_only: false,
            local_only_reason: None,
        }
    }

    fn fixture(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("da-core-bundle-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the fixture directory must be created");
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("the directory must be readable")
            .map(|entry| {
                entry
                    .expect("the entry must be readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn writes_the_case_the_report_and_the_metadata() {
        let dir = fixture("complete");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: Wrong exit code\nsteps: []\n")
            .expect("the case file must be written");
        let images = dir.join("failures");
        let mut bundles = FailureBundles::new(images.clone());

        let bundle = bundles
            .write(0, &failing_case(source.clone()), &Secrets::new())
            .expect("the bundle must be written");

        assert!(bundle.starts_with(&images));
        assert_eq!(
            names(&bundle),
            vec!["case.yaml", "meta.json", "report.json"]
        );
        assert_eq!(
            std::fs::read(bundle.join("case.yaml")).expect("case.yaml must be readable"),
            std::fs::read(&source).expect("the case file must be readable")
        );

        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(bundle.join("meta.json")).expect("meta.json must be readable"),
        )
        .expect("meta.json must be JSON");
        assert_eq!(meta["schema_version"], json!(SCHEMA_VERSION));
        assert_eq!(meta["da_version"], json!(env!("CARGO_PKG_VERSION")));
        assert_eq!(meta["case_name"], json!("Wrong exit code"));
        assert_eq!(meta["project"], json!("shop"));
        assert_eq!(meta["suite"], json!("smoke"));
        assert_eq!(meta["layer"], json!("cli"));
        assert_eq!(meta["tags"], json!(["smoke", "fast"]));
        assert_eq!(meta["status"], json!("failed"));
        assert_eq!(meta["local_only"], json!(false));
        assert!(meta.get("local_only_reason").is_none());
        assert!(
            meta["created_at"].as_u64().is_some_and(|millis| millis > 0),
            "got: {meta}"
        );

        let report: Value = serde_json::from_str(
            &std::fs::read_to_string(bundle.join("report.json"))
                .expect("report.json must be readable"),
        )
        .expect("report.json must be JSON");
        assert_eq!(report["status"], json!("failed"));
        assert_eq!(
            report["bundle"],
            json!(paths::display(&bundle)),
            "the bundled report must point at its own directory"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_evidence_is_written_by_kind_and_text_is_redacted() {
        use crate::core::session::Evidence;

        let dir = fixture("evidence");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: Evidence\nsteps: []\n")
            .expect("the case file must be written");
        let mut bundles = FailureBundles::new(dir.join("failures"));
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-value");
        let evidence = vec![
            SessionEvidence {
                kind: "test.evidence".to_string(),
                files: vec![
                    Evidence::text("page.txt", "<p>s3cr3t-value</p>")
                        .expect("the text evidence name is valid"),
                    Evidence::bytes("frame.bin", vec![0x89, b'P', b'N', b'G'])
                        .expect("the binary evidence name is valid"),
                ],
                error: None,
                evidence_is_local_only: false,
                local_only_reason: None,
            },
            SessionEvidence {
                kind: "test.broken".to_string(),
                files: Vec::new(),
                error: Some("could not read s3cr3t-value".to_string()),
                evidence_is_local_only: false,
                local_only_reason: None,
            },
        ];

        let bundle = bundles
            .write_with_evidence(0, &failing_case(source), &secrets, &evidence)
            .expect("the bundle must be written");

        assert_eq!(
            names(&bundle.join("test.evidence")),
            vec!["frame.bin", "page.txt"]
        );
        let text = std::fs::read_to_string(bundle.join("test.evidence/page.txt"))
            .expect("the text evidence must be readable");
        assert_eq!(text, "<p>***</p>");
        assert_eq!(
            std::fs::read(bundle.join("test.evidence/frame.bin"))
                .expect("the binary evidence must be readable"),
            vec![0x89, b'P', b'N', b'G']
        );
        let error = std::fs::read_to_string(bundle.join("test.broken/evidence-error.txt"))
            .expect("the evidence error must be readable");
        assert!(!error.contains("s3cr3t-value"), "the error leaked: {error}");
        assert!(error.contains("***"), "got: {error}");
        assert!(
            bundle.join("meta.json").is_file(),
            "meta.json must be written after the evidence"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_secret_is_redacted_in_the_report_and_the_metadata_but_not_in_the_case_copy() {
        let dir = fixture("redacted");
        let source = dir.join("case.yaml");
        let case_text = "name: Wrong exit code\nsteps:\n  - cli.run:\n      args: [\"${env:DA_BUNDLE_TOKEN}\"]\n";
        std::fs::write(&source, case_text).expect("the case file must be written");
        let mut bundles = FailureBundles::new(dir.join("failures"));
        let mut case = failing_case(source);
        case.name = "Wrong exit code s3cr3t-value".to_string();
        case.steps[0].result = Some(json!({"token": "s3cr3t-value"}));
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-value");

        let bundle = bundles
            .write(0, &case, &secrets)
            .expect("the bundle must be written");

        for name in ["report.json", "meta.json"] {
            let text = std::fs::read_to_string(bundle.join(name))
                .unwrap_or_else(|error| panic!("{name} must be readable: {error}"));
            assert!(!text.contains("s3cr3t-value"), "{name} leaked: {text}");
            assert!(text.contains("***"), "{name} must show the marker: {text}");
        }
        assert_eq!(
            std::fs::read_to_string(bundle.join("case.yaml")).expect("case.yaml must be readable"),
            case_text,
            "the case copy holds references, not values, and stays as written"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_run_directory_holds_only_finished_bundles() {
        let dir = fixture("no-staging");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: One\nsteps: []\n").expect("the case file must be written");
        let mut bundles = FailureBundles::new(dir.join("failures"));

        let first = bundles
            .write(0, &failing_case(source.clone()), &Secrets::new())
            .expect("the first bundle must be written");
        let second = bundles
            .write(1, &failing_case(source), &Secrets::new())
            .expect("the second bundle must be written");

        let run = first.parent().expect("a bundle has a run directory");
        assert_eq!(second.parent(), Some(run));
        assert_eq!(names(run), vec!["0-Wrong-exit-code", "1-Wrong-exit-code"]);
        assert!(
            names(&first).contains(&"meta.json".to_string()),
            "the last file must be the metadata"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_writes_case_directories_into_the_given_directory() {
        let dir = fixture("record");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: One\nsteps: []\n").expect("the case file must be written");
        let record = dir.join("record");
        let mut bundles = FailureBundles::record(record.clone());
        assert!(bundles.records_all());

        let bundle = bundles
            .write(0, &failing_case(source), &Secrets::new())
            .expect("the record must be written");

        assert_eq!(bundle.parent(), Some(record.as_path()));
        assert_eq!(
            bundle.file_name().and_then(|name| name.to_str()),
            Some("0-Wrong-exit-code")
        );
        assert!(bundle.join("meta.json").is_file());
        assert_eq!(names(&record), vec!["0-Wrong-exit-code"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_run_in_the_same_millisecond_gets_its_own_directory() {
        let dir = fixture("unique-run");
        let root = dir.join("failures");
        let first = create_run_directory(&root).expect("the first run directory must be created");
        let second = create_run_directory(&root).expect("the second run directory must be created");

        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
        assert_eq!(names(&root).len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_case_file_that_cannot_be_read_leaves_no_directory_behind() {
        let dir = fixture("missing-source");
        let images = dir.join("failures");
        let mut bundles = FailureBundles::new(images.clone());

        let error = bundles
            .write(0, &failing_case(dir.join("missing.yaml")), &Secrets::new())
            .expect_err("the bundle must fail");

        assert!(error.contains("Wrong exit code"), "got: {error}");
        assert!(error.contains("--no-bundle"), "got: {error}");
        assert!(!images.exists(), "no run directory may be created");
        assert!(!dir.join("failures").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_root_that_is_a_file_is_reported_as_a_bundle_failure() {
        let dir = fixture("blocked-root");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: Blocked\nsteps: []\n")
            .expect("the case file must be written");
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, "not a directory").expect("the blocking file must be written");
        let mut bundles = FailureBundles::new(blocked);

        let error = bundles
            .write(0, &failing_case(source), &Secrets::new())
            .expect_err("the bundle must fail");

        assert!(error.contains("could not create"), "got: {error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_are_reduced_to_safe_characters() {
        assert_eq!(safe_name("Wrong exit code"), "Wrong-exit-code");
        assert_eq!(safe_name("a/b:c*d?"), "a-b-c-d");
        assert_eq!(safe_name("  spaced  "), "spaced");
        assert_eq!(safe_name("***"), "case");
        assert_eq!(safe_name(""), "case");
        assert_eq!(safe_name("登录失败"), "登录失败");
        assert_eq!(safe_name(&"x".repeat(200)).chars().count(), MAX_NAME);
    }

    #[test]
    fn local_only_session_evidence_writes_meta_flag_and_redacted_reason() {
        use crate::core::session::Evidence;

        let dir = fixture("local-only-meta");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: LocalOnly\nsteps: []\n")
            .expect("the case file must be written");
        let mut bundles = FailureBundles::new(dir.join("failures"));
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-token");
        let mut case = failing_case(source);
        case.local_only = true;
        case.local_only_reason =
            Some("connected target s3cr3t-token holds external state".to_string());
        let evidence = vec![SessionEvidence {
            kind: "test.local".to_string(),
            files: vec![Evidence::text("info.txt", "ok").expect("valid name")],
            error: None,
            evidence_is_local_only: true,
            local_only_reason: case.local_only_reason.clone(),
        }];

        let bundle = bundles
            .write_with_evidence(0, &case, &secrets, &evidence)
            .expect("bundle must be written");
        let meta: Value = serde_json::from_str(
            &std::fs::read_to_string(bundle.join("meta.json")).expect("meta must be readable"),
        )
        .expect("meta must be JSON");
        assert_eq!(meta["local_only"], json!(true));
        assert_eq!(
            meta["local_only_reason"],
            json!("connected target *** holds external state")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_with_traces_stages_step_dirs_and_deduplicates_local_only_reason() {
        use crate::core::session::Evidence;

        let dir = fixture("step-traces-meta");
        let source = dir.join("case.yaml");
        std::fs::write(&source, "name: traced\nsteps: []\n").expect("case.yaml");
        let mut bundles = FailureBundles::record(dir.join("record"));
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-tok");
        let case = failing_case(source);
        let step_traces = vec![
            StepTrace {
                phase: StepPhase::Steps,
                index: 0,
                evidence: vec![SessionEvidence {
                    kind: "test.kind".to_string(),
                    files: vec![Evidence::text("step.log", "s3cr3t-tok ok").unwrap()],
                    error: None,
                    evidence_is_local_only: true,
                    local_only_reason: Some("profile s3cr3t-tok".to_string()),
                }],
            },
            StepTrace {
                phase: StepPhase::Finally,
                index: 0,
                evidence: vec![SessionEvidence {
                    kind: "test.kind".to_string(),
                    files: vec![Evidence::text("cleanup.log", "cleanup ok").unwrap()],
                    error: None,
                    evidence_is_local_only: true,
                    local_only_reason: Some("profile s3cr3t-tok".to_string()),
                }],
            },
        ];

        let bundle = bundles
            .write_with_traces(0, &case, &secrets, &[], &step_traces)
            .expect("bundle");
        assert_eq!(
            std::fs::read_to_string(bundle.join("trace/steps/0/test.kind/step.log")).unwrap(),
            "*** ok"
        );
        assert_eq!(
            std::fs::read_to_string(bundle.join("trace/finally/0/test.kind/cleanup.log")).unwrap(),
            "cleanup ok"
        );
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(bundle.join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta["local_only"], json!(true));
        assert_eq!(meta["local_only_reason"], json!("profile ***"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
