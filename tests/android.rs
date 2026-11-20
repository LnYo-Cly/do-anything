#![cfg(feature = "android")]

//! Tests for the `android.*` steps.
//!
//! The schema, lint, missing-adb and deadline tests need no device, so they run
//! on every machine: a missing adb binary must be an error rather than a skip,
//! the message must name the setting the user can change, and a step that
//! outlives its deadline must be killed and reported. The deadline tests point
//! `DA_ADB` at a script that waits instead of answering, because only a program
//! that never answers can prove a deadline. The tests that drive a device are
//! gated on `DA_TEST_ANDROID`: when the device cannot be reached they print why
//! and skip, and with `DA_TEST_ANDROID=1` they fail instead, which is what the
//! device job sets.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::TempDir;

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

/// Runs the binary with `dir` as its working directory.
fn run_da_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(da())
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run the da binary")
}

/// Runs the binary with extra environment variables.
fn run_da_with_env(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Output {
    let mut command = Command::new(da());
    command.current_dir(dir).args(args);
    for (name, value) in envs {
        command.env(name, value);
    }
    command.output().expect("failed to run the da binary")
}

fn exit_code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("test paths must be valid UTF-8")
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).expect("stdout must contain exactly one JSON value")
}

/// Writes a diagnostic without a print macro.
///
/// The project denies `print_stderr` because a print macro panics when the
/// reader closed the pipe; the skip message must not replace the test result.
fn note(message: &str) {
    use std::io::Write;
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{message}");
}

/// Writes a fake adb that runs `unix` or `windows` instead of talking to a
/// device.
///
/// A script is the only way to hold an adb command open without a device: the
/// real binary either answers in milliseconds or fails at once, and both hide
/// the deadline. The platform decides how a text file becomes a program `da` can
/// start — a `#!/bin/sh` script on Unix, a `.cmd` on Windows — but never what the
/// test asserts, because both scripts do the same thing.
fn fake_adb(dir: &TempDir, name: &str, unix: &str, windows: &str) -> String {
    let (file, contents) = if cfg!(windows) {
        (format!("{name}.cmd"), windows.replace('\n', "\r\n"))
    } else {
        (name.to_string(), format!("#!/bin/sh\n{unix}"))
    };
    let path = dir.write(&file, &contents);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .expect("the fake adb must be readable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("the fake adb must be executable");
    }
    path_str(&path).to_string()
}

/// The path an error names after `kind` between backticks, for example
/// `screenshot \``.
fn named_path(message: &str, kind: &str) -> Option<PathBuf> {
    let start = message.find(kind)? + kind.len();
    let rest = &message[start..];
    let end = rest.find('`')?;
    Some(PathBuf::from(&rest[..end]))
}

/// Serializes the tests that drive the device: one case at a time keeps several
/// cases from interleaving taps and dumps on one screen.
static ANDROID_TESTS: Mutex<()> = Mutex::new(());

/// Whether a device answers, probed once per test process.
fn device_ready() -> bool {
    static READY: OnceLock<Result<(), String>> = OnceLock::new();
    match READY.get_or_init(probe_device) {
        Ok(()) => true,
        Err(message) => {
            if std::env::var("DA_TEST_ANDROID").as_deref() == Ok("1") {
                panic!("DA_TEST_ANDROID=1 requires a reachable Android device: {message}");
            }
            note(&format!(
                "skipping android device test: {message}; set DA_TEST_ANDROID=1 to fail instead of skipping"
            ));
            false
        }
    }
}

/// Takes the device-test lock, or returns `None` when no device is reachable.
fn device_test() -> Option<MutexGuard<'static, ()>> {
    if !device_ready() {
        return None;
    }
    Some(
        ANDROID_TESTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// Dumps the hierarchy once, so a missing adb or device is reported before any
/// test body runs.
fn probe_device() -> Result<(), String> {
    let dir = TempDir::new("android-probe");
    let case = dir.write(
        "probe.yaml",
        "name: android probe\nsteps:\n  - android.dump: {}\n",
    );
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    if exit_code(&output) == Some(0) {
        return Ok(());
    }
    let detail = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| {
            value["cases"][0]["error"]["message"]
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("{}{}", stdout(&output), stderr(&output)));
    Err(format!("exit {:?}: {detail}", exit_code(&output)))
}

/// Runs a case that must pass and returns the parsed report.
fn run_passing_case(dir: &TempDir, name: &str, case: &str) -> Value {
    let path = dir.write(name, case);
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&path), "--no-bundle"],
    );
    assert_eq!(
        exit_code(&output),
        Some(0),
        "the case `{name}` must pass\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    json_stdout(&output)
}

/// The steps of the only case a report carries.
fn steps(report: &Value) -> &Vec<Value> {
    report["cases"][0]["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("the report must carry the steps: {report}"))
}

/// The nodes a dump step returned.
fn nodes(step: &Value) -> &Vec<Value> {
    step["result"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("a dump must return nodes: {step}"))
}

/// How long a step took, in milliseconds, as the report records it.
fn duration_ms(step: &Value) -> u64 {
    step["duration_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("every step carries its duration: {step}"))
}

/// A text or resource id that names exactly one node of `nodes`, with its centre.
///
/// A dump is the only place a device tells a case which target it can resolve,
/// so the device tests ask the same question a case author would.
fn unique_target(nodes: &[Value]) -> Option<(String, String, i64, i64)> {
    for field in ["text", "resource_id"] {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for node in nodes {
            let Some(value) = node[field].as_str() else {
                continue;
            };
            if value.trim().is_empty() {
                continue;
            }
            match counts.iter_mut().find(|(seen, _)| seen == value) {
                Some((_, count)) => *count += 1,
                None => counts.push((value.to_string(), 1)),
            }
        }
        for (value, count) in counts {
            if count != 1 {
                continue;
            }
            let Some(node) = nodes
                .iter()
                .find(|node| node[field].as_str() == Some(value.as_str()))
            else {
                continue;
            };
            let (Some(x), Some(y)) = (node["center"]["x"].as_i64(), node["center"]["y"].as_i64())
            else {
                continue;
            };
            return Some((field.to_string(), value, x, y));
        }
    }
    None
}

#[test]
fn the_schema_describes_every_android_step() {
    let work = TempDir::new("android-schema");
    let output = run_da_in(work.path(), &["--output", "json", "schema"]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "`da schema` must exit 0, stderr: {:?}",
        stderr(&output)
    );
    let schema = json_stdout(&output);
    let branches = schema["properties"]["steps"]["items"]["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("the step schema must be a oneOf: {schema}"));

    for name in [
        "android.launch",
        "android.tap",
        "android.input_text",
        "android.screenshot",
        "android.dump",
        "android.tap_on",
    ] {
        let definition = &schema["$defs"][name];
        assert_eq!(
            definition["type"], "object",
            "`{name}` must be a step definition: {definition}"
        );
        assert!(
            definition["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "`{name}` must keep its description: {definition}"
        );
        let reference = format!("#/$defs/{name}");
        assert!(
            branches.iter().any(|branch| {
                branch["properties"][name]["$ref"].as_str() == Some(reference.as_str())
            }),
            "the steps schema must reference `{name}`: {branches:?}"
        );
    }

    assert_eq!(
        schema["$defs"]["android.tap"]["properties"]["x"]["minimum"], 0,
        "a pixel is never negative: {}",
        schema["$defs"]["android.tap"]
    );
    assert_eq!(
        schema["$defs"]["android.tap_on"]["oneOf"],
        json!([{ "required": ["text"] }, { "required": ["resource_id"] }]),
        "a tap has exactly one target: {}",
        schema["$defs"]["android.tap_on"]
    );
}

#[test]
fn lint_accepts_a_case_that_uses_the_android_steps() {
    let work = TempDir::new("android-lint-ok");
    let case = work.write(
        "device.yaml",
        r#"name: Android surface
steps:
  - android.launch:
      package: com.android.settings
  - android.tap:
      x: 360
      "y": 640
  - android.input_text:
      text: hello world
  - android.screenshot:
      path: screens/device.png
  - android.dump:
      text: Search settings
  - android.tap_on:
      resource_id: com.android.settings:id/search_bar
"#,
    );
    let output = run_da_in(work.path(), &["lint", path_str(&case)]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "`da lint` must accept the case, stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn lint_refuses_an_argument_the_device_could_not_act_on() {
    let work = TempDir::new("android-lint-bad");
    let case = work.write(
        "bad.yaml",
        "name: Android bad tap\nsteps:\n  - android.tap:\n      x: -1\n      y: 640\n",
    );
    let output = run_da_in(work.path(), &["lint", path_str(&case)]);
    assert_eq!(
        exit_code(&output),
        Some(3),
        "an invalid step argument is an invalid case, stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        text.contains("android.tap"),
        "the message must name the step: {text}"
    );
    assert!(
        text.contains("invalid arguments"),
        "the message must say what is wrong: {text}"
    );
}

#[test]
fn lint_refuses_a_deadline_that_cannot_work() {
    let work = TempDir::new("android-lint-deadline");
    for (timeout, expected) in [("0", "must be at least 1"), ("600001", "must be at most")] {
        let case = work.write(
            &format!("deadline-{timeout}.yaml"),
            &format!(
                "name: Android deadline out of range\nsteps:\n  - android.tap:\n      x: 1\n      \"y\": 2\n      timeout_ms: {timeout}\n"
            ),
        );
        let output = run_da_in(work.path(), &["lint", path_str(&case)]);
        assert_eq!(
            exit_code(&output),
            Some(3),
            "a deadline of {timeout} ms is an invalid case, stdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        let text = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            text.contains("timeout_ms"),
            "the message must name the argument: {text}"
        );
        assert!(
            text.contains(expected),
            "the message must give the bound: {text}"
        );
    }
}

#[test]
fn an_unquoted_y_key_stays_a_coordinate() {
    let work = TempDir::new("android-lint-y");
    // The case format keeps argument keys as text, so an unquoted `y` is the
    // coordinate rather than the YAML 1.1 boolean; this test pins that a case
    // written the natural way lints and validates.
    let case = work.write(
        "unquoted.yaml",
        "name: Android unquoted y
steps:
  - android.tap:
      x: 360
      y: 640
",
    );
    let output = run_da_in(work.path(), &["lint", path_str(&case)]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "an unquoted y must be a coordinate, stdout:
{}
stderr:
{}",
        stdout(&output),
        stderr(&output)
    );
    let text = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        !text.contains("boolean"),
        "nothing may read the coordinate key as a boolean: {text}"
    );
}

#[test]
fn a_case_without_adb_fails_instead_of_skipping() {
    let work = TempDir::new("android-no-adb");
    let case = work.write(
        "tap.yaml",
        "name: Android tap without adb\nsteps:\n  - android.tap:\n      x: 1\n      \"y\": 2\n",
    );
    let missing = work.path().join("no-such-adb");
    let output = run_da_with_env(
        work.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
        &[("DA_ADB", path_str(&missing).to_string())],
    );

    assert_eq!(
        exit_code(&output),
        Some(4),
        "a missing adb binary is an environment failure, stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let report = json_stdout(&output);
    assert_eq!(
        report["cases"][0]["status"], "error",
        "the case must error rather than skip: {report}"
    );
    let message = report["cases"][0]["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the case must carry an error: {report}"));
    assert!(message.contains("could not run adb"), "got: {message}");
    assert!(message.contains("DA_ADB"), "got: {message}");
    assert!(
        message.contains("adb executable"),
        "the message must say what to set: {message}"
    );
    assert!(
        !message.contains("no-such-adb"),
        "the configured path must not leak into the output: {message}"
    );
}

#[test]
fn a_step_that_outlives_its_deadline_is_killed_and_named() {
    let work = TempDir::new("android-deadline");
    // The script waits and only then writes the marker: a process that outlives
    // the deadline writes it, a process that was killed never does. The wait is
    // a busy loop on Windows because `ping` takes ten seconds on some hosts and
    // leaves a child of its own, while a `for /l` loop is a cmd builtin that
    // ends with the shell.
    let marker = work.path().join("survived.txt");
    let marker_text = path_str(&marker).to_string();
    let adb = fake_adb(
        &work,
        "slow-adb",
        &format!("sleep 2\necho survived > '{marker_text}'\n"),
        &format!(
            "setlocal\nfor /l %%i in (1,1,2200000) do @rem\necho survived> \"{marker_text}\"\n"
        ),
    );

    // Time the fake adb once: the test has to know how long the work would have
    // taken on this machine, and this run is the positive control for the
    // marker, which must appear when nothing kills the script.
    let natural = {
        let started = Instant::now();
        let status = Command::new(&adb)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("the fake adb must run outside da as well");
        assert!(
            status.success(),
            "the fake adb must finish when nothing kills it: {status}"
        );
        let natural = started.elapsed();
        assert!(
            marker.is_file(),
            "the unchallenged fake adb must write {}, or the marker proves nothing",
            marker.display()
        );
        std::fs::remove_file(&marker).expect("the calibration marker must be removable");
        natural
    };

    let case = work.write(
        "slow.yaml",
        "name: Android step against a device that never answers\nsteps:\n  - android.tap:\n      x: 1\n      \"y\": 2\n      timeout_ms: 400\n",
    );

    let started = Instant::now();
    let output = run_da_with_env(
        work.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
        &[("DA_ADB", adb.clone())],
    );
    let elapsed = started.elapsed();

    assert_eq!(
        exit_code(&output),
        Some(5),
        "a missed deadline is a timeout, stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let report = json_stdout(&output);
    let message = report["cases"][0]["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the case must carry the deadline: {report}"));
    assert!(
        message.contains("`android.tap`"),
        "the step must be named: {message}"
    );
    assert!(
        message.contains("400 ms"),
        "the deadline must be named: {message}"
    );
    assert!(
        message.contains("`adb shell input tap 1 2`"),
        "the command must be named: {message}"
    );
    assert!(
        message.contains("timeout_ms"),
        "the message must say what to raise: {message}"
    );
    assert!(
        !message.contains(&adb) && !message.contains("slow-adb"),
        "the adb path must not leak into the error: {message}"
    );

    let killed = message
        .split("was killed after ")
        .nth(1)
        .and_then(|rest| rest.split(" ms").next())
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("the error must say when the command was killed: {message}"));
    assert!(
        killed >= 400,
        "no command can be killed before its 400 ms deadline: {killed} ms"
    );
    assert!(
        killed < natural.as_millis() as u64,
        "the kill must come before the work would have finished: killed after {killed} ms, the work takes {natural:?}"
    );
    assert!(
        elapsed < natural + Duration::from_secs(1),
        "the run must end before the fake adb would have: {elapsed:?} against {natural:?}"
    );

    // Wait past the time the work would have taken. A process that survived the
    // deadline writes its marker in this window; nothing else can write it.
    if let Some(remaining) = (natural + Duration::from_secs(1)).checked_sub(started.elapsed()) {
        std::thread::sleep(remaining);
    }
    assert!(
        !marker.exists(),
        "the killed adb must not outlive the step and write {}",
        marker.display()
    );
    note(&format!(
        "android.tap was killed after {killed} ms of its 400 ms deadline; the fake adb takes {natural:?} when nothing kills it and the run took {elapsed:?}"
    ));
}

#[test]
fn a_device_that_never_answers_records_no_evidence() {
    let work = TempDir::new("android-unreachable");
    let adb = fake_adb(
        &work,
        "unreachable-adb",
        "echo 'error: no devices/emulators found' >&2\nexit 1\n",
        "echo error: no devices/emulators found 1>&2\nexit /b 1\n",
    );
    let case = work.write(
        "dump.yaml",
        "name: Android dump without a device\nsteps:\n  - android.dump: {}\n",
    );

    let output = run_da_with_env(
        work.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
        &[("DA_ADB", adb)],
    );

    assert_eq!(
        exit_code(&output),
        Some(1),
        "a device that never answers fails the step, stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let report = json_stdout(&output);
    let message = report["cases"][0]["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the case must carry an error: {report}"));
    assert!(
        message.contains("could not dump the UI hierarchy"),
        "the step must say what it could not do: {message}"
    );
    assert!(
        message.contains("no devices/emulators found"),
        "a failure must keep the output adb printed: {message}"
    );
    assert!(
        !message.contains("recorded in the case directory"),
        "an unreachable device has nothing to record: {message}"
    );

    let written: Vec<String> = std::fs::read_dir(work.path())
        .expect("the case directory must be readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("android-failure-"))
        .collect();
    assert!(
        written.is_empty(),
        "a device that never answered must not make the step write evidence: {written:?}"
    );
}

#[test]
fn a_device_launches_a_package_and_dumps_its_tree() {
    let Some(_guard) = device_test() else {
        return;
    };
    let dir = TempDir::new("android-device-dump");
    let report = run_passing_case(
        &dir,
        "dump.yaml",
        r#"name: Android launch and dump
steps:
  - android.launch:
      package: com.android.settings
  - android.dump: {}
"#,
    );
    let run_steps = steps(&report);

    assert_eq!(
        run_steps[0]["result"]["package"], "com.android.settings",
        "the launch must report the package it started: {}",
        run_steps[0]
    );
    let dump = &run_steps[1]["result"];
    let count = dump["count"]
        .as_u64()
        .unwrap_or_else(|| panic!("a dump must count its nodes: {dump}"));
    assert!(count > 0, "a dump must return nodes: {dump}");
    assert_eq!(
        nodes(&run_steps[1]).len() as u64,
        count,
        "the node list must match the count: {dump}"
    );
    assert!(
        nodes(&run_steps[1])
            .iter()
            .any(|node| node["bounds"].is_object() && node["center"].is_object()),
        "a node must carry its rectangle and centre: {dump}"
    );
    note(&format!(
        "android.dump took {} ms for {count} nodes",
        duration_ms(&run_steps[1])
    ));
}

#[test]
fn tap_on_resolves_a_node_and_costs_a_dump_while_tap_costs_one_call() {
    let Some(_guard) = device_test() else {
        return;
    };
    let dir = TempDir::new("android-device-tap");

    // The target comes from the device's own dump, so the test asks the same
    // question a case author asks.
    let first = run_passing_case(
        &dir,
        "dump.yaml",
        "name: Android dump for a target\nsteps:\n  - android.launch:\n      package: com.android.settings\n  - android.dump: {}\n",
    );
    let run_steps = steps(&first);
    let Some((field, value, x, y)) = unique_target(nodes(&run_steps[1])) else {
        note(
            "skipping the tap test: the device screen has no text or resource id that names exactly one node",
        );
        return;
    };

    let case = format!(
        r#"name: Android tap on a node
steps:
  - android.launch:
      package: com.android.settings
  - android.tap_on:
      {field}: {value}
  - android.tap:
      x: {x}
      "y": {y}
"#,
        value = json!(value)
    );
    let report = run_passing_case(&dir, "tap.yaml", &case);
    let run_steps = steps(&report);

    assert_eq!(
        run_steps[1]["result"]["center"],
        json!({ "x": x, "y": y }),
        "`android.tap_on` must tap the centre the dump reported for `{field}` `{value}`: {}",
        run_steps[1]
    );
    assert_eq!(
        run_steps[2]["result"],
        json!({ "x": x, "y": y }),
        "`android.tap` must tap the pixels it was given: {}",
        run_steps[2]
    );

    let tap_on = duration_ms(&run_steps[1]);
    let tap = duration_ms(&run_steps[2]);
    note(&format!(
        "android.tap_on took {tap_on} ms, android.tap took {tap} ms"
    ));
    assert!(
        tap < tap_on,
        "a tap must not pay the dump: tap {tap} ms against tap_on {tap_on} ms"
    );
}

#[test]
fn a_screenshot_lands_in_the_case_directory() {
    let Some(_guard) = device_test() else {
        return;
    };
    let dir = TempDir::new("android-device-screenshot");
    let started = Instant::now();
    let report = run_passing_case(
        &dir,
        "shot.yaml",
        "name: Android screenshot\nsteps:\n  - android.screenshot:\n      path: screens/device.png\n",
    );
    let elapsed = started.elapsed().as_millis();
    let step = &steps(&report)[0];

    let path = step["result"]["path"]
        .as_str()
        .unwrap_or_else(|| panic!("the screenshot must report its path: {step}"));
    assert!(
        path.ends_with("screens/device.png"),
        "the reported path must be the file it wrote: {path}"
    );
    let bytes = step["result"]["bytes"]
        .as_u64()
        .unwrap_or_else(|| panic!("the screenshot must report its size: {step}"));
    let written = dir.path().join("screens").join("device.png");
    let content = std::fs::read(&written).unwrap_or_else(|error| {
        panic!(
            "the screenshot must be written to {}: {error}",
            written.display()
        )
    });
    assert_eq!(
        content.len() as u64,
        bytes,
        "the reported size must be the bytes on disk"
    );
    assert!(
        content.starts_with(b"\x89PNG\r\n\x1a\n"),
        "a screenshot is a PNG"
    );
    note(&format!(
        "android.screenshot took {} ms for {bytes} bytes ({elapsed} ms including the run)",
        duration_ms(step)
    ));
}

#[test]
fn a_failing_step_records_the_screen_and_the_tree() {
    let Some(_guard) = device_test() else {
        return;
    };
    let dir = TempDir::new("android-device-evidence");
    // A filter no screen can resolve: the dump itself succeeds, so the device
    // still answers, which is exactly the failure that must leave evidence.
    let missing = format!("da-no-such-node-{}", std::process::id());
    let case = dir.write(
        "fail.yaml",
        &format!(
            "name: Android failure evidence\nsteps:\n  - android.dump:\n      text: \"{missing}\"\n"
        ),
    );

    let started = Instant::now();
    let output = run_da_in(
        dir.path(),
        &["--output", "json", "run", path_str(&case), "--no-bundle"],
    );
    let elapsed = started.elapsed();

    assert_eq!(
        exit_code(&output),
        Some(1),
        "a filter that names no node fails the case, stdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let report = json_stdout(&output);
    let message = report["cases"][0]["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("the case must carry an error: {report}"));
    assert!(
        message.contains(&missing),
        "the failure must keep its own reason: {message}"
    );
    assert!(
        message.contains("the device still answered"),
        "the error must say the failure was recorded: {message}"
    );

    let screenshot = named_path(message, "screenshot `")
        .unwrap_or_else(|| panic!("the error must name the screenshot: {message}"));
    let tree = named_path(message, "UI tree `")
        .unwrap_or_else(|| panic!("the error must name the UI tree: {message}"));

    assert_eq!(
        screenshot.parent(),
        Some(dir.path()),
        "the evidence screenshot belongs beside the case: {}",
        screenshot.display()
    );
    assert_eq!(
        tree.parent(),
        Some(dir.path()),
        "the evidence tree belongs beside the case: {}",
        tree.display()
    );

    let png = std::fs::read(&screenshot).unwrap_or_else(|error| {
        panic!(
            "the evidence screenshot must be written to {}: {error}",
            screenshot.display()
        )
    });
    assert!(!png.is_empty(), "the evidence screenshot must not be empty");
    assert!(
        png.starts_with(b"\x89PNG\r\n\x1a\n"),
        "the evidence screenshot is a PNG"
    );
    let xml = std::fs::read_to_string(&tree).unwrap_or_else(|error| {
        panic!(
            "the evidence tree must be written to {}: {error}",
            tree.display()
        )
    });
    assert!(!xml.is_empty(), "the evidence tree must not be empty");
    assert!(
        xml.contains("<hierarchy") && xml.contains("<node"),
        "the evidence tree must be the dump document: {:.200}",
        xml
    );
    note(&format!(
        "a failing android.dump wrote {} bytes of PNG and {} bytes of XML in {elapsed:?}: {} and {}",
        png.len(),
        xml.len(),
        screenshot.display(),
        tree.display()
    ));
}

#[test]
fn typed_text_reaches_the_field_that_has_focus() {
    let Some(_guard) = device_test() else {
        return;
    };
    let dir = TempDir::new("android-device-type");
    let token = format!("da-{}", std::process::id());

    // Find a field to type into: the settings search field is behind the search
    // bar on most builds, so the test opens it when the first screen has none.
    let first = run_passing_case(
        &dir,
        "dump.yaml",
        "name: Android dump for a field\nsteps:\n  - android.launch:\n      package: com.android.settings\n  - android.dump: {}\n",
    );
    let mut field = editable_node(nodes(&steps(&first)[1])).cloned();
    if field.is_none()
        && let Some((x, y)) = search_target(nodes(&steps(&first)[1]))
    {
        let case = format!(
            "name: Android open the search field\nsteps:\n  - android.launch:\n      package: com.android.settings\n  - android.tap:\n      x: {x}\n      \"y\": {y}\n  - android.dump: {{}}\n"
        );
        let second = run_passing_case(&dir, "search.yaml", &case);
        field = editable_node(nodes(&steps(&second)[2])).cloned();
    }
    let Some(field) = field else {
        note("skipping the typing test: the device screen has no editable field to type into");
        return;
    };
    let (Some(x), Some(y)) = (field["center"]["x"].as_i64(), field["center"]["y"].as_i64()) else {
        note("skipping the typing test: the editable field has no centre");
        return;
    };

    let case = format!(
        r#"name: Android type
steps:
  - android.launch:
      package: com.android.settings
  - android.tap:
      x: {x}
      "y": {y}
  - android.input_text:
      text: {token}
  - android.dump: {{}}
"#
    );
    let report = run_passing_case(&dir, "type.yaml", &case);
    let run_steps = steps(&report);

    if !nodes(&run_steps[3]).iter().any(|node| {
        node["class"].as_str().is_some_and(|class| {
            class.contains("EditText") || class.contains("AutoCompleteTextView")
        })
    }) {
        note("skipping the typing assertion: this device exposes no editable field after the tap");
        return;
    }
    assert_eq!(
        run_steps[2]["result"]["characters"],
        json!(token.chars().count()),
        "the step must report what it sent: {}",
        run_steps[2]
    );
    let typed = nodes(&run_steps[3]).iter().any(|node| {
        node["text"]
            .as_str()
            .is_some_and(|text| text.contains(&token))
    });
    assert!(
        typed,
        "the typed text must appear in the field's dump: {}",
        run_steps[3]["result"]
    );
}

/// The first node that accepts typing, as a dump describes it.
fn editable_node(nodes: &[Value]) -> Option<&Value> {
    nodes.iter().find(|node| {
        node["class"].as_str().is_some_and(|class| {
            class.contains("EditText") || class.contains("AutoCompleteTextView")
        }) && node["center"].is_object()
    })
}

/// The point that opens a search field, from the node that names the search.
fn search_target(nodes: &[Value]) -> Option<(i64, i64)> {
    let node = nodes.iter().find(|node| {
        ["resource_id", "content_desc", "text"].iter().any(|field| {
            node[*field]
                .as_str()
                .is_some_and(|value| value.to_lowercase().contains("search"))
        })
    })?;
    Some((node["center"]["x"].as_i64()?, node["center"]["y"].as_i64()?))
}
