//! End-to-end and runner-level tests for the `fs.watch` observer.

#![cfg(feature = "fs")]
#![allow(dead_code, unused_imports)]

mod support;

use std::fs::{self, File, FileTimes};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};

use do_anything::core::bundle::FailureBundles;
use do_anything::core::case::{Case, Expectation, LoadedCase, Matcher, Observation, Step};
use do_anything::core::events::EventBus;
use do_anything::core::filter::Filters;
use do_anything::core::registry::{
    Registry, StepDefinition, StepError, disallow_retry, no_lint_warnings,
};
use do_anything::core::report::{CaseStatus, ProblemKind, RunOutcome};
use do_anything::core::runner;
use do_anything::core::secrets::Secrets;
use do_anything::core::session::Sessions;
use do_anything::drivers;
use do_anything::exit::ExitCode;
use serde_json::{Value, json};
use serde_saphyr::Location;
use support::TempDir;

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(da())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run the da binary")
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("test paths must be valid UTF-8")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout must contain exactly one JSON value")
}

fn fs_registry() -> Registry {
    let mut registry = Registry::new();
    drivers::register_observers(&mut registry).expect("built-in observers must register");
    registry
        .register(StepDefinition {
            name: "test.mutate",
            description: "Applies deterministic filesystem mutations for fs.watch tests.",
            args_schema: json!({"type": "object"}),
            validate: |_| Ok(()),
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_mutate,
        })
        .expect("test.mutate must register");
    registry
}

fn run_mutate(args: &Value, _sessions: &mut Sessions) -> Result<Value, StepError> {
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| StepError::InvalidArguments("missing `op`".to_string()))?;
    let root = PathBuf::from(
        args.get("dir")
            .and_then(Value::as_str)
            .ok_or_else(|| StepError::InvalidArguments("missing `dir`".to_string()))?,
    );

    match op {
        "create_modify_delete" => {
            fs::create_dir_all(root.join("sub")).expect("sub dir must be created");
            fs::write(root.join("alpha.txt"), "new-alpha\n").expect("alpha must write");
            fs::write(root.join("sub").join("beta.txt"), "new-beta\n").expect("beta must write");
            fs::write(root.join("modify_me.txt"), "after-much-longer-content\n")
                .expect("modify_me must update");
            fs::remove_file(root.join("delete_me.txt")).expect("delete_me must remove");
            fs::remove_file(root.join("sub").join("nested_del.txt"))
                .expect("nested_del must remove");
        }
        "step2_create" => {
            fs::write(root.join("gamma.txt"), "gamma\n").expect("gamma must write");
        }
        "nested_and_top" => {
            fs::create_dir_all(root.join("sub")).expect("sub dir must be created");
            fs::write(root.join("top.txt"), "top\n").expect("top must write");
            fs::write(root.join("sub").join("nested.txt"), "nested\n").expect("nested must write");
        }
        "same_size_same_mtime" => {
            let file = root.join("same.txt");
            let orig_mtime = fs::metadata(&file)
                .expect("metadata must exist")
                .modified()
                .expect("mtime must be readable");
            fs::write(&file, "bbbb").expect("overwrite must succeed");
            let handle = File::options()
                .write(true)
                .open(&file)
                .expect("file must open for timestamp restore");
            handle
                .set_times(FileTimes::new().set_modified(orig_mtime))
                .expect("must restore exact mtime");
        }
        "same_content_new_mtime" => {
            let file = root.join("same.txt");
            let orig_mtime = fs::metadata(&file)
                .expect("metadata must exist")
                .modified()
                .expect("mtime must be readable");
            fs::write(&file, "aaaa").expect("rewrite must succeed");
            let bumped = orig_mtime + Duration::from_secs(5);
            let handle = File::options()
                .write(true)
                .open(&file)
                .expect("file must open for timestamp update");
            handle
                .set_times(FileTimes::new().set_modified(bumped))
                .expect("must bump mtime");
        }
        "create_dir_and_file" => {
            fs::create_dir_all(root.join("nested")).expect("nested dir must create");
            fs::write(root.join("nested").join("created.txt"), "ok\n")
                .expect("created file must write");
        }
        "remove_root" => {
            fs::remove_dir_all(&root).expect("root must remove");
        }
        "replace_root_with_file" => {
            fs::remove_dir_all(&root).expect("root must remove");
            fs::write(&root, "now-a-file\n").expect("file must replace dir");
        }
        "exceed_limit" => {
            fs::write(root.join("d.txt"), "4\n").expect("fourth file must write");
        }
        "noop" => {}
        other => panic!("unknown test op `{other}`"),
    }
    Ok(json!({"ok": true}))
}

fn loc() -> Location {
    Location::UNKNOWN
}

fn obs_fs(alias: &str, args: Value) -> Observation {
    Observation {
        name: alias.to_string(),
        kind: "fs.watch".to_string(),
        args,
        location: loc(),
        args_location: loc(),
    }
}

fn step_mutate(dir: &Path, op: &str, expect: Vec<Expectation>) -> Step {
    Step {
        action: "test.mutate".to_string(),
        args: json!({
            "dir": dir.to_string_lossy(),
            "op": op,
        }),
        optional: false,
        retry: None,
        expect,
        save: Vec::new(),
        location: loc(),
        args_location: loc(),
        retry_location: loc(),
        arg_locations: Vec::new(),
    }
}

fn eq(path: &str, expected: Value) -> Expectation {
    Expectation {
        path: path.to_string(),
        matcher: Matcher::Equals(expected),
    }
}

fn run_case(
    dir: &TempDir,
    observe: Vec<Observation>,
    steps: Vec<Step>,
    bundles: Option<&mut FailureBundles>,
    secrets: &mut Secrets,
) -> do_anything::core::report::RunReport {
    let source = dir.write("case.yaml", "name: fs-watch-test\nsteps: []\n");
    let case = Case {
        name: "fs-watch-test".to_string(),
        vars: Default::default(),
        project: None,
        suite: None,
        layer: None,
        tags: Vec::new(),
        observe,
        steps,
        finally: Vec::new(),
    };
    let outcome = runner::run(
        &[LoadedCase { path: source, case }],
        &fs_registry(),
        &EventBus::new(),
        bundles,
        &Filters::default(),
        secrets,
        1,
    );
    match outcome {
        RunOutcome::Completed(report) => report,
        other => panic!("expected Completed run, got: {other:?}"),
    }
}

#[test]
fn detects_sorted_created_modified_deleted_and_isolates_steps() {
    let dir = TempDir::new("fs-watch-crud");
    let watched = dir.path().join("watched");
    fs::create_dir_all(watched.join("sub")).expect("sub dir must exist");
    fs::write(watched.join("keep.txt"), "stay\n").expect("keep must write");
    fs::write(watched.join("modify_me.txt"), "before\n").expect("modify_me must write");
    fs::write(watched.join("delete_me.txt"), "bye\n").expect("delete_me must write");
    fs::write(watched.join("sub").join("nested_del.txt"), "bye2\n").expect("nested_del must write");

    let observe = vec![obs_fs("fs", json!({ "path": watched.to_string_lossy() }))];
    let steps = vec![
        step_mutate(
            &watched,
            "create_modify_delete",
            vec![
                eq(
                    "$.observed.fs.created",
                    json!(["alpha.txt", "sub/beta.txt"]),
                ),
                eq("$.observed.fs.modified", json!(["modify_me.txt"])),
                eq(
                    "$.observed.fs.deleted",
                    json!(["delete_me.txt", "sub/nested_del.txt"]),
                ),
            ],
        ),
        step_mutate(
            &watched,
            "step2_create",
            vec![
                eq("$.observed.fs.created", json!(["gamma.txt"])),
                eq("$.observed.fs.modified", json!([])),
                eq("$.observed.fs.deleted", json!([])),
            ],
        ),
    ];

    let report = run_case(&dir, observe, steps, None, &mut Secrets::new());
    assert_eq!(
        report.cases[0].status,
        CaseStatus::Passed,
        "case failed: {:?}",
        report.cases[0]
    );
}

#[test]
fn recursive_false_ignores_nested_changes() {
    let dir = TempDir::new("fs-watch-nonrec");
    let watched = dir.path().join("watched");
    fs::create_dir_all(&watched).expect("watched dir must exist");

    let observe = vec![obs_fs(
        "fs",
        json!({ "path": watched.to_string_lossy(), "recursive": false }),
    )];
    let steps = vec![step_mutate(
        &watched,
        "nested_and_top",
        vec![
            eq("$.observed.fs.created", json!(["top.txt"])),
            eq("$.observed.fs.modified", json!([])),
            eq("$.observed.fs.deleted", json!([])),
        ],
    )];

    let report = run_case(&dir, observe, steps, None, &mut Secrets::new());
    assert_eq!(
        report.cases[0].status,
        CaseStatus::Passed,
        "case failed: {:?}",
        report.cases[0]
    );
}

#[test]
fn hash_detects_same_size_same_mtime_change_and_ignores_identical_rewrite() {
    let dir = TempDir::new("fs-watch-hash");
    let watched_meta = dir.path().join("meta");
    let watched_hash = dir.path().join("hashed");
    fs::create_dir_all(&watched_meta).expect("meta dir must exist");
    fs::create_dir_all(&watched_hash).expect("hashed dir must exist");
    fs::write(watched_meta.join("same.txt"), "aaaa").expect("meta file must write");
    fs::write(watched_hash.join("same.txt"), "aaaa").expect("hash file must write");

    // Control: metadata-only (`hash: false`) misses a same-length overwrite with restored mtime.
    let report_meta = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": watched_meta.to_string_lossy(), "hash": false }),
        )],
        vec![step_mutate(
            &watched_meta,
            "same_size_same_mtime",
            vec![eq("$.observed.fs.modified", json!([]))],
        )],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(report_meta.cases[0].status, CaseStatus::Passed);

    // With `hash: true`, the same-size + same-mtime content change IS detected.
    let report_hash = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": watched_hash.to_string_lossy(), "hash": true }),
        )],
        vec![
            step_mutate(
                &watched_hash,
                "same_size_same_mtime",
                vec![eq("$.observed.fs.modified", json!(["same.txt"]))],
            ),
            // Wait: same.txt now holds "bbbb"; test identical rewrite check on another file below.
        ],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(
        report_hash.cases[0].status,
        CaseStatus::Passed,
        "hash mode must detect content change: {:?}",
        report_hash.cases[0]
    );

    // Rewriting identical bytes with a bumped mtime is NOT flagged under `hash: true`.
    let watched_idem = dir.path().join("idem");
    fs::create_dir_all(&watched_idem).expect("idem dir must exist");
    fs::write(watched_idem.join("same.txt"), "aaaa").expect("idem file must write");
    let report_idem = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": watched_idem.to_string_lossy(), "hash": true }),
        )],
        vec![step_mutate(
            &watched_idem,
            "same_content_new_mtime",
            vec![eq("$.observed.fs.modified", json!([]))],
        )],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(
        report_idem.cases[0].status,
        CaseStatus::Passed,
        "hash mode must ignore identical bytes with bumped mtime: {:?}",
        report_idem.cases[0]
    );
}

#[test]
fn max_entries_cap_enforced_at_start_and_collect() {
    let dir = TempDir::new("fs-watch-limit");
    let watched = dir.path().join("watched");
    fs::create_dir_all(&watched).expect("watched dir must exist");
    for name in ["a.txt", "b.txt", "c.txt"] {
        fs::write(watched.join(name), "1\n").expect("fixture file must write");
    }

    // Boundary: exactly `max_entries` (3) succeeds.
    let ok_report = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": watched.to_string_lossy(), "max_entries": 3 }),
        )],
        vec![step_mutate(
            &watched,
            "noop",
            vec![eq("$.observed.fs.created", json!([]))],
        )],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(ok_report.cases[0].status, CaseStatus::Passed);

    // Crossing `max_entries` during step fails in `collect` with Environment error.
    let over_collect = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": watched.to_string_lossy(), "max_entries": 3 }),
        )],
        vec![step_mutate(&watched, "exceed_limit", vec![])],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(over_collect.cases[0].status, CaseStatus::Error);
    let err = over_collect.cases[0]
        .error
        .as_ref()
        .expect("exceeding max_entries must error");
    assert_eq!(err.kind, ProblemKind::Environment);
    assert!(
        err.message.contains("max_entries") && err.message.contains("3"),
        "error must explain max_entries cap, got: {}",
        err.message
    );

    // Starting over `max_entries` fails immediately in `start` with Environment error.
    let over_start = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": watched.to_string_lossy(), "max_entries": 3 }),
        )],
        vec![step_mutate(&watched, "noop", vec![])],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(over_start.cases[0].status, CaseStatus::Error);
    let start_err = over_start.cases[0]
        .error
        .as_ref()
        .expect("starting above max_entries must error");
    assert_eq!(start_err.kind, ProblemKind::Environment);
    assert!(start_err.message.contains("max_entries"));
}

#[test]
fn missing_root_is_read_only_and_non_directory_is_environment_error() {
    let dir = TempDir::new("fs-watch-missing");
    let absent = dir.path().join("never-created");

    let empty_report = run_case(
        &dir,
        vec![obs_fs("fs", json!({ "path": absent.to_string_lossy() }))],
        vec![step_mutate(
            &absent,
            "noop",
            vec![
                eq("$.observed.fs.created", json!([])),
                eq("$.observed.fs.modified", json!([])),
                eq("$.observed.fs.deleted", json!([])),
            ],
        )],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(empty_report.cases[0].status, CaseStatus::Passed);
    assert!(
        !absent.exists(),
        "fs.watch must never create the watched directory"
    );

    // Step creates the directory and writes a file inside it.
    let late_dir = dir.path().join("created-later");
    let created_report = run_case(
        &dir,
        vec![obs_fs("fs", json!({ "path": late_dir.to_string_lossy() }))],
        vec![
            step_mutate(
                &late_dir,
                "create_dir_and_file",
                vec![eq("$.observed.fs.created", json!(["nested/created.txt"]))],
            ),
            step_mutate(
                &late_dir,
                "remove_root",
                vec![eq("$.observed.fs.deleted", json!(["nested/created.txt"]))],
            ),
        ],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(
        created_report.cases[0].status,
        CaseStatus::Passed,
        "late directory creation and deletion failed: {:?}",
        created_report.cases[0]
    );

    // Watching a regular file is an Environment error.
    let file_target = dir.write("not-a-dir.txt", "hello\n");
    let file_report = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": file_target.to_string_lossy() }),
        )],
        vec![step_mutate(&file_target, "noop", vec![])],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(file_report.cases[0].status, CaseStatus::Error);
    let err = file_report.cases[0]
        .error
        .as_ref()
        .expect("regular file target must fail");
    assert_eq!(err.kind, ProblemKind::Environment);
    assert!(
        err.message.contains("not a directory"),
        "got: {}",
        err.message
    );

    // Replacing directory with a regular file mid-step is also an Environment error.
    let replaced = dir.path().join("becomes-file");
    fs::create_dir_all(&replaced).expect("dir must exist");
    let replaced_report = run_case(
        &dir,
        vec![obs_fs("fs", json!({ "path": replaced.to_string_lossy() }))],
        vec![step_mutate(&replaced, "replace_root_with_file", vec![])],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(replaced_report.cases[0].status, CaseStatus::Error);
    assert_eq!(
        replaced_report.cases[0].error.as_ref().map(|e| e.kind),
        Some(ProblemKind::Environment)
    );
}

#[test]
fn symlink_directories_are_not_followed_into_cycles() {
    let dir = TempDir::new("fs-watch-symlink");
    let watched = dir.path().join("watched");
    fs::create_dir_all(&watched).expect("watched dir must exist");
    fs::write(watched.join("real.txt"), "ok\n").expect("real file must write");

    let loop_link = watched.join("loop");
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&watched, &loop_link).is_ok();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_dir(&watched, &loop_link).is_ok();
    #[cfg(not(any(unix, windows)))]
    let linked = false;

    if !linked {
        // Unprivileged Windows environments may disallow symlink creation; regular file check still runs.
        return;
    }

    let report = run_case(
        &dir,
        vec![obs_fs(
            "fs",
            json!({ "path": watched.to_string_lossy(), "max_entries": 10 }),
        )],
        vec![step_mutate(
            &watched,
            "noop",
            vec![
                eq("$.observed.fs.created", json!([])),
                eq("$.observed.fs.modified", json!([])),
                eq("$.observed.fs.deleted", json!([])),
            ],
        )],
        None,
        &mut Secrets::new(),
    );
    assert_eq!(
        report.cases[0].status,
        CaseStatus::Passed,
        "symlink loop must not recurse or fail: {:?}",
        report.cases[0]
    );
}

#[test]
fn lint_and_run_reject_invalid_fs_watch_arguments_with_location() {
    let dir = TempDir::new("fs-watch-invalid");
    let step_yaml = if cfg!(feature = "proc") {
        "  - proc.stop: {}"
    } else if cfg!(feature = "cli") {
        "  - cli.run:\n      command: da"
    } else {
        return;
    };

    for (label, args, fragment) in [
        ("empty-path", "path: \"\"", "path"),
        ("zero-max", "path: out\n      max_entries: 0", "max_entries"),
        (
            "unknown-arg",
            "path: out\n      follow_symlinks: true",
            "follow_symlinks",
        ),
    ] {
        let case = dir.write(
            &format!("{label}.yaml"),
            &format!(
                "name: Bad fs.watch\nobserve:\n  fs:\n    fs.watch:\n      {args}\nsteps:\n{step_yaml}\n"
            ),
        );
        let lint = run_da_in(dir.path(), &["lint", path_str(&case)]);
        assert_eq!(
            lint.status.code(),
            Some(3),
            "{label}: stdout={:?} stderr={:?}",
            stdout(&lint),
            stderr(&lint)
        );
        assert!(
            stdout(&lint).contains("fs") && stdout(&lint).contains(fragment),
            "{label}: got: {}",
            stdout(&lint)
        );

        let run = run_da_in(dir.path(), &["run", path_str(&case), "--no-bundle"]);
        assert_eq!(
            run.status.code(),
            Some(3),
            "{label}: stderr={:?}",
            stderr(&run)
        );
    }
}

#[cfg(feature = "proc")]
fn mutator_bin() -> &'static Path {
    static BIN: LazyLock<(TempDir, PathBuf)> = LazyLock::new(|| {
        let dir = TempDir::new("fs-watch-helper");
        let src = dir.write(
            "mutator.rs",
            r#"
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let mut args = env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("dir"));
    let phase = args.next().expect("phase");
    match phase.as_str() {
        "step1" => {
            fs::create_dir_all(dir.join("nested")).unwrap();
            fs::write(dir.join("created.txt"), "hello\n").unwrap();
            fs::write(dir.join("nested").join("deep.txt"), "deep\n").unwrap();
            fs::write(dir.join("edit.txt"), "updated-longer-contents\n").unwrap();
            fs::remove_file(dir.join("gone.txt")).unwrap();
        }
        "limit" => {
            for idx in 0..5 {
                fs::write(dir.join(format!("f{idx}.txt")), "x\n").unwrap();
            }
        }
        other => panic!("unknown phase {other}"),
    }
    println!("MUTATE_DONE");
}
"#,
        );
        let exe = dir.path().join(if cfg!(windows) {
            "mutator.exe"
        } else {
            "mutator"
        });
        let status = Command::new("rustc")
            .arg("--edition=2024")
            .arg(&src)
            .arg("-o")
            .arg(&exe)
            .status()
            .expect("rustc must run to build fs_watch test helper");
        assert!(status.success(), "rustc failed building fs_watch helper");
        (dir, exe)
    });
    &BIN.1
}

#[cfg(feature = "proc")]
#[test]
fn da_binary_observes_created_modified_deleted_and_max_entries_error() {
    let helper = mutator_bin();
    let dir = TempDir::new("fs-watch-e2e");
    let watched = dir.path().join("out");
    fs::create_dir_all(&watched).expect("watched dir must exist");
    fs::write(watched.join("edit.txt"), "init\n").expect("edit.txt must write");
    fs::write(watched.join("gone.txt"), "gone\n").expect("gone.txt must write");

    let pass_case = dir.write(
        "pass.yaml",
        &format!(
            r#"
name: Watch output directory across steps
observe:
  out_fs:
    fs.watch:
      path: '{watched}'
steps:
  - proc.start:
      command: '{helper}'
      args: ['{watched}', 'step1']
      ready_pattern: "MUTATE_DONE"
    expect:
      - path: $.observed.out_fs.created
        equals: ["created.txt", "nested/deep.txt"]
      - path: $.observed.out_fs.modified
        equals: ["edit.txt"]
      - path: $.observed.out_fs.deleted
        equals: ["gone.txt"]
  - proc.stop: {{}}
    expect:
      - path: $.observed.out_fs.created
        equals: []
      - path: $.observed.out_fs.modified
        equals: []
      - path: $.observed.out_fs.deleted
        equals: []
"#,
            watched = watched.display(),
            helper = helper.display(),
        ),
    );

    let pass_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&pass_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        pass_out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&pass_out),
        stderr(&pass_out)
    );

    // Exceeding `max_entries` through the real `da` binary exits 4 (Environment).
    let limited_dir = dir.path().join("limited");
    fs::create_dir_all(&limited_dir).expect("limited dir must exist");
    let limit_case = dir.write(
        "limit.yaml",
        &format!(
            r#"
name: Exceed max_entries
observe:
  out_fs:
    fs.watch:
      path: '{limited}'
      max_entries: 3
steps:
  - proc.start:
      command: '{helper}'
      args: ['{limited}', 'limit']
      ready_pattern: "MUTATE_DONE"
"#,
            limited = limited_dir.display(),
            helper = helper.display(),
        ),
    );
    let limit_out = run_da_in(
        dir.path(),
        &[
            "--output",
            "json",
            "run",
            path_str(&limit_case),
            "--no-bundle",
        ],
    );
    assert_eq!(
        limit_out.status.code(),
        Some(4),
        "stdout: {}\nstderr: {}",
        stdout(&limit_out),
        stderr(&limit_out)
    );
    let report = json_stdout(&limit_out);
    assert_eq!(
        report["cases"][0]["steps"][0]["error"]["kind"],
        "environment"
    );
}
