//! End-to-end tests for run recording and the single-file HTML report.

mod support;

use std::fs;
#[cfg(feature = "browser")]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use support::TempDir;

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

/// Runs the binary with a temporary working directory.
fn run_da(args: &[&str]) -> Output {
    let work = TempDir::new("report-cwd");
    Command::new(da())
        .current_dir(work.path())
        .args(args)
        .output()
        .expect("failed to run the da binary")
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

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| {
        panic!("could not read {}: {error}", path.display());
    })
}

/// A complete one-pixel PNG, so an embedded data URI is a real image.
const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D', b'R',
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xae,
    0x42, 0x60, 0x82,
];

fn write_case(root: &Path, directory: &str, meta: &Value, report: &Value, png: &[u8]) -> PathBuf {
    let case = root.join(directory);
    fs::create_dir_all(case.join("browser")).expect("the case directory must be created");
    fs::write(case.join("case.yaml"), "name: fixture\nsteps: []\n")
        .expect("the case copy must be written");
    fs::write(
        case.join("meta.json"),
        serde_json::to_vec_pretty(meta).expect("meta must serialize"),
    )
    .expect("meta.json must be written");
    fs::write(
        case.join("report.json"),
        serde_json::to_vec_pretty(report).expect("report must serialize"),
    )
    .expect("report.json must be written");
    fs::write(case.join("browser/screenshot.png"), png).expect("the screenshot must be written");
    fs::write(
        case.join("browser/dom.html"),
        "<!doctype html><html><body><p id=\"secret\">***</p>\
         <pre>&lt;img src=\"https://evil.example/x\"&gt;</pre></body></html>",
    )
    .expect("the DOM snapshot must be written");
    fs::write(
        case.join("browser/events.log"),
        "line &lt;b&gt;one&lt;/b&gt; with ***\n",
    )
    .expect("the text evidence must be written");
    case
}

fn malicious_report() -> Value {
    json!({
        "status": "failed",
        "duration_ms": 12,
        "steps": [
            {
                "name": "http.request <b>step</b>",
                "status": "failed",
                "duration_ms": 3,
                "result": {
                    "status": 200,
                    "headers": {"content-type": "application/json"},
                    "body": {"ok": true, "note": "<script>alert(1)</script>"},
                    "observed": {
                        "api_log": {
                            "lines": ["line <b>one</b>"],
                            "text": "line <b>one</b>\n",
                            "truncated": false,
                            "limited": false
                        },
                        "page_network": {
                            "requests": [{
                                "method": "GET",
                                "url": "http://127.0.0.1:1/health?next=<img src=\"https://evil.example/x\">",
                                "status": 200,
                                "type": "Fetch",
                                "error": null
                            }],
                            "pending": 0,
                            "dropped": 0
                        }
                    }
                },
                "assertions": [{
                    "path": "$.status",
                    "comparison": "equals",
                    "expected": 201,
                    "actual": 200,
                    "passed": false,
                    "message": "<img src=\"https://evil.example/x\">"
                }],
                "error": {
                    "kind": "assertion",
                    "message": "1 of 1 assertions failed"
                }
            },
            {
                "name": "db.query",
                "status": "passed",
                "duration_ms": 2,
                "result": {
                    "columns": ["id", "<script>column</script>"],
                    "rows": [{"id": 1, "<script>column</script>": "<b>value</b>"}],
                    "row_count": 1,
                    "truncated": false
                },
                "assertions": [],
                "error": null
            },
            {
                "name": "cli.run",
                "status": "passed",
                "duration_ms": 1,
                "result": {
                    "exit_code": 0,
                    "stdout": "hello <world>",
                    "stderr": ""
                },
                "assertions": [],
                "error": null
            },
            {
                "name": "browser.click",
                "status": "skipped",
                "duration_ms": 1,
                "result": null,
                "assertions": [],
                "error": {
                    "kind": "assertion",
                    "message": "the optional target was absent"
                }
            }
        ],
        "error": {"kind": "assertion", "message": "1 of 1 assertions failed"},
        "close_errors": []
    })
}

fn malicious_meta(case_name: &str) -> Value {
    json!({
        "schema_version": 1,
        "da_version": "0.0.0-test",
        "case_file": "C:/private/cases/fixture.yaml",
        "case_name": case_name,
        "project": "shop <script>project</script>",
        "suite": "smoke",
        "layer": "ui",
        "tags": ["<b>fast</b>"],
        "status": "failed",
        "created_at": 1
    })
}

#[test]
#[cfg(feature = "cli")]
fn a_passing_case_is_recorded_and_report_writes_the_default_file() {
    let dir = TempDir::new("record-passing");
    let case = dir.write(
        "passing.yaml",
        &format!(
            "name: A passing case\nsteps:\n  - cli.run:\n      command: \"{}\"\n      args: [\"--version\"]\n    expect:\n      - path: $.exit_code\n        equals: 0\n",
            da().replace('\\', "\\\\")
        ),
    );
    let record = dir.path().join("record");

    let output = run_da(&["run", path_str(&case), "--record", path_str(&record)]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let cases: Vec<PathBuf> = fs::read_dir(&record)
        .expect("the record directory must exist")
        .map(|entry| entry.expect("the entry must be readable").path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(cases.len(), 1, "got: {cases:?}");
    let case_dir = &cases[0];
    for name in ["case.yaml", "report.json", "meta.json"] {
        assert!(case_dir.join(name).is_file(), "missing {name}");
    }
    let meta: Value =
        serde_json::from_str(&read(&case_dir.join("meta.json"))).expect("meta.json must be JSON");
    assert_eq!(meta["status"], "passed");
    let recorded: Value = serde_json::from_str(&read(&case_dir.join("report.json")))
        .expect("report.json must be JSON");
    assert_eq!(recorded["status"], "passed");

    let output = run_da(&["report", path_str(&record)]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let report_path = record.join("report.html");
    assert!(report_path.is_file(), "the default report must be written");
    let text = stdout(&output);
    assert!(text.contains("report.html"), "got: {text}");
    assert!(text.contains("1 case"), "got: {text}");
    let html = read(&report_path);
    assert!(html.contains("A passing case"), "{html}");
    assert!(!html.contains("data:image/png;base64,"), "no screenshot");
}

#[test]
#[cfg(feature = "cli")]
fn a_skipped_optional_step_is_recorded_and_counted_in_the_report() {
    let dir = TempDir::new("record-skipped");
    let case = dir.write(
        "skipped.yaml",
        &format!(
            r#"name: An optional step is skipped
steps:
  - cli.run:
      command: "{da}"
      args: ["--version"]
    optional: true
    expect:
      - path: $.exit_code
        equals: 99
  - cli.run:
      command: "{da}"
      args: ["--version"]
    expect:
      - path: $.exit_code
        equals: 0
"#,
            da = da().replace('\\', "\\\\")
        ),
    );
    let record = dir.path().join("record");

    let output = run_da(&["run", path_str(&case), "--record", path_str(&record)]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let case_dir = fs::read_dir(&record)
        .expect("the record directory must exist")
        .map(|entry| entry.expect("the entry must be readable").path())
        .find(|path| path.is_dir())
        .expect("the passing case must be recorded");
    let recorded: Value = serde_json::from_str(&read(&case_dir.join("report.json")))
        .expect("report.json must be JSON");
    assert_eq!(recorded["status"], "passed");
    assert_eq!(recorded["steps"][0]["status"], "skipped");
    assert_eq!(recorded["steps"][1]["status"], "passed");

    let output = run_da(&["--output", "json", "report", path_str(&record)]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let value: Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be one JSON value");
    assert_eq!(value["steps"], 2);
    assert_eq!(value["steps_skipped"], 1);
    let html = read(&record.join("report.html"));
    assert!(html.contains("SKIPPED"));

    let output = run_da(&["report", path_str(&record)]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("2 steps (1 skipped)"),
        "got: {}",
        stdout(&output)
    );
}

#[test]
fn cases_are_ordered_by_the_numeric_directory_prefix() {
    let dir = TempDir::new("report-order");
    let record = dir.path().join("record");
    write_case(
        &record,
        "10-ten",
        &malicious_meta("Ten"),
        &malicious_report(),
        PNG,
    );
    write_case(
        &record,
        "2-two",
        &malicious_meta("Two"),
        &malicious_report(),
        PNG,
    );

    let output = run_da(&["report", path_str(&record)]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let html = read(&record.join("report.html"));
    let two = html.find("Two").expect("the two case must be present");
    let ten = html.find("Ten").expect("the ten case must be present");
    assert!(
        two < ten,
        "numeric prefixes must order 2 before 10, not lexicographically"
    );
}

#[test]
fn the_report_escapes_content_neutralizes_schemes_and_renders_every_layer() {
    let dir = TempDir::new("report-escape");
    let record = dir.path().join("record");
    let case = write_case(
        &record,
        "0-malicious",
        &malicious_meta("<script>alert('case')</script>"),
        &malicious_report(),
        PNG,
    );
    assert!(case.join("meta.json").is_file());

    let output = run_da(&["report", path_str(&record)]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let html = read(&record.join("report.html"));
    assert!(
        html.contains(
            "default-src 'none'; img-src data:; style-src 'unsafe-inline'; script-src 'unsafe-inline'"
        ),
        "the report must carry the offline policy"
    );
    assert!(!html.contains("http://"), "no cleartext scheme may remain");
    assert!(!html.contains("https://"), "no cleartext scheme may remain");
    assert_eq!(
        html.matches("<script>").count(),
        1,
        "the report must contain only the single static offline viewer script"
    );
    assert_eq!(
        html.matches("</script>").count(),
        1,
        "no user-injected script end tag may appear"
    );
    assert!(
        !html.contains("<img src=\"https://evil.example/x\">"),
        "page markup must never be emitted as a tag"
    );
    assert!(
        !html.contains("<script>alert"),
        "page script text must never be emitted as a tag"
    );
    assert!(
        html.contains("&lt;script&gt;"),
        "the text must be visibly escaped"
    );
    assert!(
        html.contains("https&#58;//evil.example/x"),
        "the URL text must remain readable even though the scheme is entity-encoded"
    );
    assert!(html.contains("data:image/png;base64,"));
    assert!(html.contains("HTTP response"), "missing the HTTP layer");
    assert!(
        html.contains("Network observations"),
        "missing the network layer"
    );
    assert!(html.contains("Log lines"), "missing the log layer");
    assert!(html.contains("DB query"), "missing the database layer");
    assert!(html.contains("CLI output"), "missing the CLI layer");
    assert!(html.contains("Assertions"), "missing the assertions");
    assert!(html.contains("SKIPPED"), "missing the skipped-step status");
    assert!(
        html.contains("the optional target was absent"),
        "missing the reason the optional step was skipped"
    );
    assert!(html.contains("201"), "missing the expected value");
    assert!(html.contains("200"), "missing the actual value");
    assert!(html.contains("DOM snapshot"), "missing the DOM snapshot");
    assert!(
        html.contains("line &lt;b&gt;one&lt;/b&gt;"),
        "log text must be escaped"
    );
    assert!(
        !html.contains(path_str(dir.path())),
        "the report must not contain the local record path"
    );
}

#[test]
fn report_json_names_the_file_and_counts_the_records() {
    let dir = TempDir::new("report-json");
    let record = dir.path().join("record");
    write_case(
        &record,
        "0-one",
        &malicious_meta("One"),
        &malicious_report(),
        PNG,
    );
    let output_path = dir.path().join("out/report.html");
    fs::create_dir_all(output_path.parent().expect("the output has a parent"))
        .expect("the output directory must be created");

    let output = run_da(&[
        "--output",
        "json",
        "report",
        path_str(&record),
        "-o",
        path_str(&output_path),
    ]);
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let value: Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be one JSON value");
    assert_eq!(value["ok"], true);
    assert_eq!(value["cases"], 1);
    assert_eq!(value["failed"], 1);
    assert_eq!(value["steps"], 4);
    assert_eq!(value["steps_skipped"], 1);
    assert_eq!(value["images_embedded"], 1);
    assert_eq!(value["images_omitted"], 0);
    assert!(
        value["report"]
            .as_str()
            .is_some_and(|path| path.ends_with("out/report.html")),
        "got: {value}"
    );
    assert!(output_path.is_file());
}

#[test]
fn shareable_and_local_only_records_omit_the_screenshot_with_a_reason() {
    let dir = TempDir::new("report-shareable");
    let record = dir.path().join("record");
    write_case(
        &record,
        "0-one",
        &malicious_meta("One"),
        &malicious_report(),
        PNG,
    );

    let shareable = dir.path().join("shareable.html");
    let output = run_da(&[
        "report",
        path_str(&record),
        "-o",
        path_str(&shareable),
        "--shareable",
    ]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let html = read(&shareable);
    assert!(
        !html.contains("data:image/"),
        "shareable report embedded an image"
    );
    assert!(
        html.contains("--shareable"),
        "the omission reason must name the flag"
    );
    assert!(
        html.contains("not embedded"),
        "the omission must be explicit"
    );

    let mut meta = malicious_meta("One");
    meta["local_only"] = json!(true);
    fs::write(
        record.join("0-one/meta.json"),
        serde_json::to_vec_pretty(&meta).expect("meta must serialize"),
    )
    .expect("meta.json must be updated");
    let local = dir.path().join("local.html");
    let output = run_da(&["report", path_str(&record), "-o", path_str(&local)]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let html = read(&local);
    assert!(
        !html.contains("data:image/"),
        "local_only embedded the screenshot"
    );
    assert!(
        html.contains("local_only"),
        "the omission reason must name the marker"
    );
}

/// A PNG signature followed by zero bytes, long enough to exercise a limit.
fn png_of_size(bytes: usize) -> Vec<u8> {
    let mut png = vec![0_u8; bytes];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    png
}

#[test]
fn oversized_screenshots_are_omitted_with_a_reason() {
    let dir = TempDir::new("report-limit");
    let record = dir.path().join("record");
    let case = write_case(
        &record,
        "0-one",
        &malicious_meta("One"),
        &malicious_report(),
        &png_of_size(2 * 1024 * 1024 + 1),
    );
    assert!(case.join("browser/screenshot.png").is_file());

    let output = run_da(&["report", path_str(&record)]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let html = read(&record.join("report.html"));
    assert!(
        !html.contains("data:image/"),
        "an oversized image was embedded"
    );
    assert!(
        html.contains("2 MiB"),
        "the single-image limit must be named"
    );
    assert!(
        html.contains("not embedded"),
        "the omission must be explicit"
    );
}

#[test]
fn the_total_image_budget_stops_after_ten_mebibytes() {
    let dir = TempDir::new("report-total-limit");
    let record = dir.path().join("record");
    let case = write_case(
        &record,
        "0-one",
        &malicious_meta("One"),
        &malicious_report(),
        &png_of_size(2 * 1024 * 1024),
    );
    for index in 1..6 {
        fs::write(
            case.join(format!("browser/screenshot-{index}.png")),
            png_of_size(2 * 1024 * 1024),
        )
        .expect("the screenshot must be written");
    }

    let output = run_da(&["report", path_str(&record)]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let html = read(&record.join("report.html"));
    assert_eq!(
        html.matches("data:image/png;base64,").count(),
        5,
        "only five 2 MiB images fit in the 10 MiB budget"
    );
    assert!(html.contains("10 MiB"), "the total budget must be named");
    assert!(
        html.contains("not embedded"),
        "the omission must be explicit"
    );
}

#[test]
fn a_missing_or_empty_record_directory_is_a_usage_error() {
    let dir = TempDir::new("report-missing");
    let missing = dir.path().join("missing");
    let output = run_da(&["report", path_str(&missing)]);
    assert_eq!(exit_code(&output), Some(2));
    assert!(
        stderr(&output).contains("does not exist"),
        "got: {}",
        stderr(&output)
    );

    let empty = dir.path().join("empty");
    fs::create_dir_all(&empty).expect("the empty directory must be created");
    let output = run_da(&["report", path_str(&empty)]);
    assert_eq!(exit_code(&output), Some(2));
    assert!(
        stderr(&output).contains("meta.json"),
        "the message must explain what makes a record complete: {}",
        stderr(&output)
    );

    let incomplete = empty.join("0-incomplete");
    fs::create_dir_all(&incomplete).expect("the incomplete directory must be created");
    fs::write(incomplete.join("report.json"), "{}").expect("the partial report must be written");
    let output = run_da(&["report", path_str(&empty)]);
    assert_eq!(exit_code(&output), Some(2));
    assert!(
        stderr(&output).contains("meta.json"),
        "got: {}",
        stderr(&output)
    );
}

#[test]
fn a_record_directory_that_already_has_files_is_refused_without_touching_them() {
    let dir = TempDir::new("record-not-empty");
    let case = dir.write("passing.yaml", "name: Passing\nsteps: []\n");
    let record = dir.path().join("record");
    fs::create_dir_all(&record).expect("the record directory must be created");
    fs::write(record.join("keep.txt"), "old evidence").expect("the old file must be written");

    let output = run_da(&["run", path_str(&case), "--record", path_str(&record)]);

    assert_eq!(exit_code(&output), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("not empty"),
        "got: {}",
        stderr(&output)
    );
    assert_eq!(
        read(&record.join("keep.txt")),
        "old evidence",
        "the existing directory must be left untouched"
    );
}

#[cfg(feature = "browser")]
fn browser_ready(server: &support::TestServer) -> bool {
    let dir = TempDir::new("report-browser-probe");
    let case = dir.write(
        "probe.yaml",
        &format!(
            "name: browser probe\nsteps:\n  - browser.open:\n      url: \"{}\"\n",
            server.url("/browser/form?case=probe")
        ),
    );
    let output = run_da(&["run", path_str(&case), "--no-bundle"]);
    if exit_code(&output) == Some(0) {
        return true;
    }
    if std::env::var("DA_TEST_BROWSER").as_deref() == Ok("1") {
        panic!(
            "DA_TEST_BROWSER=1 requires a working Chrome: stdout: {}\nstderr: {}",
            stdout(&output),
            stderr(&output)
        );
    }
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(
        stderr,
        "skipping browser report test: Chrome could not open the probe page; set DA_TEST_BROWSER=1 to fail instead"
    );
    false
}

#[cfg(feature = "browser")]
#[test]
fn a_recorded_browser_secret_and_screenshot_obey_the_security_gate() {
    let server = support::TestServer::start();
    if !browser_ready(&server) {
        return;
    }
    let dir = TempDir::new("report-browser-gate");
    let secret = "record-secret-0123456789abcdef";
    let case = dir.write(
        "gate.yaml",
        &format!(
            r##"name: Browser record gate
steps:
  - browser.open:
      url: "{base}/browser/record?secret=${{env:RECORD_SECRET}}"
    expect:
      - path: $.title
        equals: Record probe
  - browser.read:
      css: "#secret"
    expect:
      - path: $.text
        exists: true
  - browser.read:
      css: "#evil"
    expect:
      - path: $.text
        exists: true
"##,
            base = server.url("")
        ),
    );
    let record = dir.path().join("record");

    let output = Command::new(da())
        .current_dir(dir.path())
        .args(["run", path_str(&case), "--record", path_str(&record)])
        .env("RECORD_SECRET", secret)
        .output()
        .expect("failed to run the da binary");
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let case_dir = fs::read_dir(&record)
        .expect("the record directory must exist")
        .map(|entry| entry.expect("the entry must be readable").path())
        .find(|path| path.is_dir())
        .expect("one case directory must be recorded");
    assert!(case_dir.join("browser/screenshot.png").is_file());
    assert!(case_dir.join("browser/dom.html").is_file());

    let normal = dir.path().join("normal.html");
    let output = run_da(&["report", path_str(&record), "-o", path_str(&normal)]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let normal_html = read(&normal);
    assert!(normal_html.contains("data:image/png;base64,"));
    assert!(
        !normal_html.contains(secret),
        "the original secret leaked into the report"
    );
    assert!(
        !normal_html.contains(&support::percent_encode(secret.as_bytes())),
        "the percent-encoded secret leaked into the report"
    );
    assert!(
        !normal_html.contains(&support::base64_encode(secret.as_bytes())),
        "the base64 secret leaked into the report"
    );
    assert!(!normal_html.contains("https://evil.example/x"));
    assert!(!normal_html.contains("<img src=\"https"));
    assert_eq!(normal_html.matches("<script>").count(), 1);
    assert_eq!(normal_html.matches("</script>").count(), 1);

    let shareable = dir.path().join("shareable.html");
    let output = run_da(&[
        "report",
        path_str(&record),
        "-o",
        path_str(&shareable),
        "--shareable",
    ]);
    assert_eq!(exit_code(&output), Some(0), "{}", stderr(&output));
    let shareable_html = read(&shareable);
    assert!(
        !shareable_html.contains("data:image/"),
        "a shareable report embedded a screenshot"
    );
    assert!(shareable_html.contains("--shareable"));
    assert!(!shareable_html.contains(secret));
    assert!(!shareable_html.contains(&support::percent_encode(secret.as_bytes())));
    assert!(!shareable_html.contains(&support::base64_encode(secret.as_bytes())));
    assert!(!shareable_html.contains("https://evil.example/x"));
    assert!(!shareable_html.contains("<img src=\"https"));
    assert_eq!(shareable_html.matches("<script>").count(), 1);
    assert_eq!(shareable_html.matches("</script>").count(), 1);
}

#[test]
#[cfg(all(feature = "http", feature = "cli"))]
fn finally_cleanup_steps_are_rendered_and_counted_while_absent_finally_stays_compatible() {
    let server = support::TestServer::start();
    let dir = TempDir::new("report-finally");
    let case = dir.write(
        "finally.yaml",
        &format!(
            r#"name: Loopback with finally cleanup
steps:
  - http.request:
      method: POST
      url: "{base}/login"
      json: {{ user: "alice" }}
    save:
      token: $.body.token
  - http.request:
      url: "{base}/health"
    expect:
      - path: $.status
        equals: 200
  - http.request:
      url: "{base}/me"
      headers: {{ Authorization: "Bearer ${{token}}" }}
    expect:
      - path: $.name
        equals: alice
finally:
  - http.request:
      method: DELETE
      url: "{base}/status/500"
    expect:
      - path: $.status
        equals: 204
  - cli.run:
      command: "{da}"
      args: ["--version"]
    optional: true
    expect:
      - path: $.exit_code
        equals: 99
"#,
            base = server.url(""),
            da = da().replace('\\', "\\\\")
        ),
    );
    let record = dir.path().join("record");

    let run_out = run_da(&["run", path_str(&case), "--record", path_str(&record)]);
    assert_eq!(exit_code(&run_out), Some(1), "stderr: {}", stderr(&run_out));

    // Also include a legacy record without a `finally` key alongside the new one.
    let legacy_report = json!({
        "status": "passed",
        "duration_ms": 1,
        "steps": [
            {
                "name": "cli.run",
                "status": "passed",
                "duration_ms": 1,
                "result": {"exit_code": 0, "stdout": "legacy", "stderr": ""},
                "assertions": [],
                "error": null
            }
        ],
        "error": null,
        "close_errors": []
    });
    let mut legacy_meta = malicious_meta("Legacy case without finally");
    legacy_meta["status"] = json!("passed");
    write_case(&record, "1-legacy", &legacy_meta, &legacy_report, PNG);

    let report_out = run_da(&["--output", "json", "report", path_str(&record)]);
    assert_eq!(
        exit_code(&report_out),
        Some(0),
        "stderr: {}",
        stderr(&report_out)
    );
    let summary: Value =
        serde_json::from_slice(&report_out.stdout).expect("report stdout must be JSON");
    assert_eq!(summary["cases"], 2);
    // 3 main + 2 finally in case 0, plus 1 main in legacy case 1 = 6 total steps.
    assert_eq!(summary["steps"], 6, "summary: {summary}");
    assert_eq!(summary["steps_skipped"], 1, "summary: {summary}");

    let html = read(&record.join("report.html"));
    assert!(
        html.contains("Cleanup (finally)"),
        "missing cleanup section"
    );
    assert!(html.contains("boom"), "missing failed cleanup body");
    assert!(
        html.contains("204"),
        "missing failed cleanup expected status"
    );
    assert!(html.contains("SKIPPED"), "missing skipped cleanup status");
    assert!(html.contains("Legacy case without finally"));
    assert!(html.contains("legacy"));
}

#[test]
fn step_trace_artifacts_args_saved_and_shareable_local_only_budget_render_safely() {
    let dir = TempDir::new("report-step-traces");
    let record = dir.path().join("record");

    let trace_report = json!({
        "status": "passed",
        "duration_ms": 15,
        "steps": [
            {
                "name": "http.request",
                "status": "passed",
                "duration_ms": 5,
                "args": { "url": "http://127.0.0.1/trace?q=<script>arg_xss</script>" },
                "saved": { "tok_out": "<b>saved_val</b>" },
                "result": { "status": 200 },
                "assertions": [],
                "error": null
            },
            {
                "name": "cli.run",
                "status": "passed",
                "duration_ms": 3,
                "args": { "command": "echo", "args": ["ok"] },
                "saved": {},
                "result": { "exit_code": 0, "stdout": "ok", "stderr": "" },
                "assertions": [],
                "error": null
            }
        ],
        "finally": [
            {
                "name": "browser.click",
                "status": "passed",
                "duration_ms": 7,
                "args": { "css": "#cleanup" },
                "saved": {},
                "result": {},
                "assertions": [],
                "error": null
            }
        ],
        "error": null,
        "close_errors": []
    });

    let case0 = write_case(
        &record,
        "0-traced",
        &malicious_meta("Traced case"),
        &trace_report,
        PNG,
    );
    // Remove case-level screenshots on case0 so per-step trace image rendering is unambiguous.
    let _ = fs::remove_dir_all(case0.join("browser"));
    fs::create_dir_all(case0.join("trace/steps/0/browser")).unwrap();
    fs::create_dir_all(case0.join("trace/finally/0/browser")).unwrap();
    fs::write(case0.join("trace/steps/0/browser/screenshot.png"), PNG).unwrap();
    fs::write(
        case0.join("trace/steps/0/browser/dom.html"),
        "<!doctype html><html><body><p>step0-dom-text</p><script>evilDom()</script></body></html>",
    )
    .unwrap();
    fs::write(
        case0.join("trace/finally/0/browser/events.log"),
        "finally-trace-line </script> ok\n",
    )
    .unwrap();
    fs::write(case0.join("trace/finally/0/browser/screenshot.png"), PNG).unwrap();

    // Case 1 is marked local_only: step-trace images must NOT embed even in normal mode.
    let mut local_meta = malicious_meta("Local only traced");
    local_meta["local_only"] = json!(true);
    local_meta["local_only_reason"] = json!("named profile contains <script>alert(1)</script>");
    let mut local_report = trace_report.clone();
    local_report["finally"] = json!([]);
    let case1 = write_case(&record, "1-local", &local_meta, &local_report, PNG);
    let _ = fs::remove_dir_all(case1.join("browser"));
    fs::create_dir_all(case1.join("trace/steps/0/browser")).unwrap();
    fs::write(case1.join("trace/steps/0/browser/screenshot.png"), PNG).unwrap();

    let normal = dir.path().join("normal.html");
    let out = run_da(&["report", path_str(&record), "-o", path_str(&normal)]);
    assert_eq!(exit_code(&out), Some(0), "stderr: {}", stderr(&out));
    let html = read(&normal);
    assert!(
        html.contains("&lt;script&gt;arg_xss&lt;/script&gt;"),
        "missing escaped step args"
    );
    assert!(
        html.contains("tok_out") && html.contains("&lt;b&gt;saved_val&lt;/b&gt;"),
        "missing escaped saved var"
    );
    assert!(html.contains("step0-dom-text"), "missing step0 traced DOM");
    assert!(
        !html.contains("<script>evilDom()</script>"),
        "unescaped DOM script"
    );
    assert!(
        html.contains("finally-trace-line &lt;/script&gt; ok"),
        "missing escaped finally trace log"
    );
    assert!(
        html.contains("data:image/png;base64,"),
        "normal report should embed non-local trace PNG"
    );
    // Step 0 + finally 0 of case0 = 2 images; case1 must NOT contribute a 3rd because it's local_only.
    assert_eq!(
        html.matches("data:image/").count(),
        2,
        "expected exactly 2 embedded PNGs (local_only must omit)"
    );
    assert!(html.contains("named profile contains &lt;script&gt;alert(1)&lt;/script&gt;"));

    let shareable = dir.path().join("shareable.html");
    let sout = run_da(&[
        "report",
        path_str(&record),
        "-o",
        path_str(&shareable),
        "--shareable",
    ]);
    assert_eq!(exit_code(&sout), Some(0), "stderr: {}", stderr(&sout));
    let shtml = read(&shareable);
    assert!(
        !shtml.contains("data:image/"),
        "--shareable must omit all step-trace PNGs"
    );
    assert!(shtml.contains("step0-dom-text"));
    assert!(shtml.contains("finally-trace-line"));

    // Byte-budget enforcement across step-trace images (2 MiB single, 10 MiB total).
    let budget_dir = TempDir::new("report-step-budget");
    let budget_record = budget_dir.path().join("record");
    let mut med_png = PNG.to_vec();
    med_png.resize(1_800_000, 0);
    let mut big_png = PNG.to_vec();
    big_png.resize(2_200_000, 0);

    let mut big_steps = Vec::new();
    let case_b = write_case(
        &budget_record,
        "0-many",
        &malicious_meta("Large trace PNGs"),
        &trace_report,
        PNG,
    );
    let _ = fs::remove_dir_all(case_b.join("browser"));
    // 7 x 1.8 MiB steps (5 fit under 10 MiB = 9.0 MiB; 6th & 7th exceed 10 MiB) + 1 x 2.2 MiB step (> 2 MiB single limit).
    for i in 0..8usize {
        big_steps.push(json!({
            "name": "browser.click",
            "status": "passed",
            "duration_ms": 1,
            "args": { "index": i },
            "saved": {},
            "result": {},
            "assertions": [],
            "error": null
        }));
        let step_dir = case_b.join(format!("trace/steps/{i}/browser"));
        fs::create_dir_all(&step_dir).unwrap();
        let bytes = if i == 7 { &big_png } else { &med_png };
        fs::write(step_dir.join("screenshot.png"), bytes).unwrap();
    }
    let mut big_report = trace_report;
    big_report["steps"] = Value::Array(big_steps);
    big_report["finally"] = json!([]);
    fs::write(
        case_b.join("report.json"),
        serde_json::to_vec_pretty(&big_report).unwrap(),
    )
    .unwrap();

    let budget_html = budget_dir.path().join("budget.html");
    let bout = run_da(&[
        "report",
        path_str(&budget_record),
        "-o",
        path_str(&budget_html),
    ]);
    assert_eq!(exit_code(&bout), Some(0), "stderr: {}", stderr(&bout));
    let bhtml = read(&budget_html);
    assert_eq!(
        bhtml.matches("data:image/").count(),
        5,
        "total image budget must cap at 10 MiB"
    );
    assert!(
        bhtml.contains("10 MiB total image budget"),
        "expected total budget omission note"
    );
    assert!(
        bhtml.contains("2 MiB single-image limit"),
        "expected single-image limit note"
    );
}

#[test]
fn target_bounds_schema_and_validator_agree_on_valid_and_invalid_shapes() {
    let schema = do_anything::core::report::target_bounds_schema();
    let validator = jsonschema::validator_for(&schema).expect("target_bounds schema must compile");

    let valid_samples = [
        json!({
            "x": 120,
            "y": 44,
            "width": 88,
            "height": 32,
            "point": { "x": 164, "y": 60 },
            "viewport": { "width": 1280, "height": 720 }
        }),
        json!({
            "x": 50.5,
            "y": 90.25,
            "width": 0,
            "height": 0,
            "point": { "x": 50.5, "y": 90.25 },
            "viewport": { "width": 800, "height": 600 }
        }),
        json!({
            "x": 20,
            "y": 30,
            "width": 200,
            "height": 110,
            "from": { "x": 20, "y": 30 },
            "to": { "x": 220, "y": 140 },
            "from_bounds": { "x": 10, "y": 20, "width": 20, "height": 20 },
            "to_bounds": { "x": 200, "y": 120, "width": 40, "height": 40 },
            "viewport": { "width": 1280, "height": 720 }
        }),
    ];
    for sample in &valid_samples {
        assert!(
            validator.is_valid(sample),
            "schema should accept valid sample: {sample}"
        );
        assert!(
            do_anything::core::report::validate_target_bounds(sample).is_ok(),
            "validator should accept valid sample: {sample}"
        );
    }

    let invalid_samples = [
        json!("not-an-object"),
        json!({ "x": 10, "y": 20 }),
        json!({ "x": 10, "y": 20, "width": -1, "height": 10 }),
        json!({ "x": 10, "y": 20, "width": 10, "height": 10, "viewport": { "width": 0, "height": 100 } }),
        json!({ "x": 10, "y": 20, "width": 10, "height": 10, "extra": true }),
        json!({ "x": 10, "y": 20, "width": 10, "height": 10, "from": { "x": 1 } }),
    ];
    for sample in &invalid_samples {
        assert!(
            !validator.is_valid(sample),
            "schema should reject invalid sample: {sample}"
        );
        assert!(
            do_anything::core::report::validate_target_bounds(sample).is_err(),
            "validator should reject invalid sample: {sample}"
        );
    }
}

#[test]
fn interactive_report_renders_svg_overlays_timeline_controls_and_cross_layer_tabs() {
    let dir = TempDir::new("report-interactive-overlays");
    let record = dir.path().join("record");
    let report = json!({
        "name": "Interactive UI flow",
        "status": "failed",
        "duration_ms": 90,
        "steps": [
            {
                "name": "browser.click",
                "status": "passed",
                "duration_ms": 25,
                "args": { "role": "button", "name": "Submit" },
                "target_bounds": {
                    "x": 100,
                    "y": 50,
                    "width": 140,
                    "height": 40,
                    "point": { "x": 170, "y": 70 },
                    "viewport": { "width": 1280, "height": 720 }
                },
                "saved": { "label": "Submit" },
                "result": {
                    "text": "Submit",
                    "observed": {
                        "fs_events": {
                            "events": [
                                { "kind": "created", "path": "out/receipt.json", "size": 128 }
                            ],
                            "truncated": false
                        }
                    }
                },
                "assertions": [],
                "error": null
            },
            {
                "name": "browser.drag",
                "status": "failed",
                "duration_ms": 65,
                "args": { "from": { "css": "#a" }, "to": { "css": "#b" } },
                "target_bounds": {
                    "x": 40,
                    "y": 60,
                    "width": 260,
                    "height": 120,
                    "from": { "x": 40, "y": 60 },
                    "to": { "x": 300, "y": 180 },
                    "from_bounds": { "x": 20, "y": 40, "width": 40, "height": 40 },
                    "to_bounds": { "x": 280, "y": 160, "width": 40, "height": 40 },
                    "viewport": { "width": 1280, "height": 720 }
                },
                "saved": {},
                "result": {
                    "dragged": true,
                    "rows": [{ "id": 1, "state": "open" }],
                    "columns": ["id", "state"],
                    "row_count": 1,
                    "truncated": false
                },
                "assertions": [{
                    "path": "$.state",
                    "comparison": "equals",
                    "expected": "done",
                    "actual": "open",
                    "passed": false,
                    "message": "expected done"
                }],
                "error": { "kind": "assertion", "message": "1 of 1 assertions failed" }
            }
        ],
        "finally": []
    });
    let case = write_case(
        &record,
        "0-ui",
        &malicious_meta("Interactive UI flow"),
        &report,
        PNG,
    );
    let _ = fs::remove_dir_all(case.join("browser"));
    fs::create_dir_all(case.join("trace/steps/0/browser")).unwrap();
    fs::write(case.join("trace/steps/0/browser/screenshot.png"), PNG).unwrap();
    fs::create_dir_all(case.join("trace/steps/1/browser")).unwrap();
    fs::write(case.join("trace/steps/1/browser/screenshot.png"), PNG).unwrap();

    let normal = dir.path().join("interactive.html");
    let out = run_da(&["report", path_str(&record), "-o", path_str(&normal)]);
    assert_eq!(exit_code(&out), Some(0), "stderr: {}", stderr(&out));
    let html = read(&normal);

    // Offline & zero external schemes
    assert!(!html.contains("http://"), "unexpected http:// scheme");
    assert!(!html.contains("https://"), "unexpected https:// scheme");
    assert_eq!(html.matches("<script>").count(), 1);
    assert_eq!(html.matches("</script>").count(), 1);

    // Deterministic 0-token badge for da run
    assert!(
        html.contains("Deterministic Replay · 0 tokens (da run)"),
        "expected 0-token deterministic replay badge"
    );

    // Timeline playback controls + keyboard shortcuts
    assert!(html.contains("data-action=\"prev\""));
    assert!(html.contains("data-action=\"play\""));
    assert!(html.contains("data-action=\"next\""));
    assert!(html.contains("data-action=\"filter-failed\""));
    assert!(html.contains("data-action=\"toggle-overlays\""));
    assert!(html.contains("ArrowLeft") && html.contains("ArrowRight"));

    // SVG overlays: click box + reticle AND drag arrow + endpoints
    assert!(html.contains("<svg class=\"shot-overlay\" viewBox=\"0 0 1280 720\""));
    assert!(html.contains("class=\"tb-box\""));
    assert!(html.contains("class=\"tb-ring\""));
    assert!(html.contains("class=\"tb-drag-line\""));
    assert!(html.contains("class=\"tb-drag-arrow\""));

    // Cross-layer tabs & fs.watch table
    assert!(html.contains("class=\"step-tabs\""));
    assert!(html.contains("data-tab=\"assertions\""));
    assert!(html.contains("data-tab=\"observed\""));
    assert!(html.contains("data-tab=\"data\""));
    assert!(html.contains("Filesystem watch"));
    assert!(html.contains("out/receipt.json"));

    // --shareable strips screenshots while retaining target geometry summary and timeline
    let shareable = dir.path().join("shareable.html");
    let sout = run_da(&[
        "report",
        path_str(&record),
        "-o",
        path_str(&shareable),
        "--shareable",
    ]);
    assert_eq!(exit_code(&sout), Some(0), "stderr: {}", stderr(&sout));
    let shtml = read(&shareable);
    assert!(!shtml.contains("data:image/"));
    assert!(!shtml.contains("<svg class=\"shot-overlay\""));
    assert!(shtml.contains("Target geometry:"));
    assert!(shtml.contains("drag (40, 60)"));
}

#[test]
fn report_renders_session_exploration_directory_with_model_telemetry_and_shareable_stripping() {
    let dir = TempDir::new("report-session-telemetry");
    let session_dir = dir.path().join("sess-explore-01");
    fs::create_dir_all(session_dir.join("trace/steps/1/browser")).unwrap();
    fs::write(
        session_dir.join("trace/steps/1/browser/screenshot.png"),
        PNG,
    )
    .unwrap();

    fs::write(
        session_dir.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "id": "sess-explore-01",
            "created_at_ms": 1_700_000_000_000_u64,
            "last_active_ms": 1_700_000_005_000_u64,
            "idle_timeout_ms": 1_800_000_u64,
            "handles": {},
            "vars": {},
            "env_names": [],
            "observe": [],
            "plugins": [],
            "model": { "model": "specai-hy/hy4" }
        }))
        .unwrap(),
    )
    .unwrap();

    let steps = [
        json!({
            "index": 1,
            "action": "model/select_ref",
            "raw_args": { "query": "find <script>checkout</script>" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_100_u64,
            "duration_ms": 210,
            "result": { "ref": "e12", "model": "specai-hy/hy4" },
            "model": "specai-hy/hy4",
            "latency_ms": 205,
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 120,
                "total_tokens": 1120,
                "prompt_tokens_details": { "cached_tokens": 400 }
            },
            "trace_id": "tr-001",
            "confidence": 0.94
        }),
        json!({
            "index": 2,
            "action": "model/locate",
            "raw_args": { "query": "submit icon" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_400_u64,
            "duration_ms": 350,
            "result": { "x": 640, "y": 360 },
            "target_bounds": {
                "x": 640,
                "y": 360,
                "width": 0,
                "height": 0,
                "point": { "x": 640, "y": 360 },
                "viewport": { "width": 1280, "height": 720 }
            },
            "model": "specai-hy/hy4",
            "latency_ms": 345,
            "usage": {
                "input_tokens": 1500,
                "output_tokens": 80,
                "cache_read_tokens": 600,
                "reasoning_tokens": 50
            },
            "trace_id": "tr-002",
            "confidence": 0.88
        }),
        json!({
            "index": 3,
            "action": "model/judge",
            "raw_args": { "rubric": "checkout succeeded", "screenshot": true },
            "status": "passed",
            "started_at_ms": 1_700_000_000_900_u64,
            "duration_ms": 190,
            "result": { "pass": true, "reason": "confirmation banner visible" },
            "model": "specai-hy/hy4",
            "latency_ms": 180,
            "usage": {
                "prompt_tokens": 900,
                "completion_tokens": 40,
                "total_tokens": 940
            },
            "trace_id": "tr-003"
        }),
    ];
    let mut jsonl = String::new();
    for s in &steps {
        jsonl.push_str(&serde_json::to_string(s).unwrap());
        jsonl.push('\n');
    }
    fs::write(session_dir.join("steps.jsonl"), jsonl).unwrap();

    let out_html = dir.path().join("session-report.html");
    let out = run_da(&[
        "--output",
        "json",
        "report",
        path_str(&session_dir),
        "-o",
        path_str(&out_html),
    ]);
    assert_eq!(exit_code(&out), Some(0), "stderr: {}", stderr(&out));
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["ok"], true);
    assert_eq!(summary["cases"], 1);
    assert_eq!(summary["steps"], 3);
    assert_eq!(summary["images_embedded"], 1);

    let html = read(&out_html);
    assert!(html.contains("Exploration Run (da session)"));
    assert!(html.contains("Model exploration telemetry"));
    assert!(html.contains("specai-hy/hy4"));
    assert!(html.contains("tr-001") && html.contains("tr-002") && html.contains("tr-003"));
    // Total latency = 205 + 345 + 180 = 730 ms
    assert!(html.contains("730 ms"), "missing aggregated model latency");
    // Total tokens = 1120 + 1580 + 940 = 3,640; cached = 1,000; reasoning = 50
    assert!(html.contains("3,640"), "missing aggregated total tokens");
    assert!(
        html.contains("1,000 cached"),
        "missing aggregated cached tokens"
    );
    assert!(html.contains("50 reasoning"), "missing reasoning tokens");
    assert!(html.contains("<svg class=\"shot-overlay\" viewBox=\"0 0 1280 720\""));
    assert!(!html.contains("<script>checkout</script>"));
    assert!(html.contains("&lt;script&gt;checkout&lt;/script&gt;"));
    assert!(!html.contains(path_str(dir.path())));

    // --shareable omits screenshots from session reports too
    let share_html = dir.path().join("session-shareable.html");
    let sout = run_da(&[
        "report",
        path_str(&session_dir),
        "-o",
        path_str(&share_html),
        "--shareable",
    ]);
    assert_eq!(exit_code(&sout), Some(0), "stderr: {}", stderr(&sout));
    let shtml = read(&share_html);
    assert!(!shtml.contains("data:image/"));
    assert!(shtml.contains("Model exploration telemetry"));
}

#[cfg(feature = "browser")]
#[test]
fn browser_run_and_session_record_schema_valid_target_bounds_and_drag_endpoints() {
    let server = support::TestServer::start();
    if !browser_ready(&server) {
        return;
    }
    let dir = TempDir::new("report-browser-bounds");
    let case = dir.write(
        "bounds.yaml",
        &format!(
            r##"name: Browser target bounds
steps:
  - browser.open:
      url: "{base}/browser/locators"
      width: 1000
      height: 700
  - browser.click:
      testid: "solo-btn"
  - browser.hover:
      css: "#qa-btn"
  - browser.type:
      css: "#qa-input"
      value: "hello"
  - browser.drag:
      from: {{ css: "#solo-btn" }}
      to: {{ css: "#qa-btn" }}
  - browser.click:
      at: {{ x: 120, y: 80 }}
"##,
            base = server.url("")
        ),
    );
    let record = dir.path().join("record");
    let output = Command::new(da())
        .current_dir(dir.path())
        .args([
            "run",
            path_str(&case),
            "--record",
            path_str(&record),
            "--trace-steps",
        ])
        .output()
        .expect("failed to run da");
    assert_eq!(
        exit_code(&output),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );

    let cases: Vec<PathBuf> = fs::read_dir(&record)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("meta.json").is_file())
        .collect();
    assert_eq!(cases.len(), 1);
    let report_json: Value = serde_json::from_str(&read(&cases[0].join("report.json"))).unwrap();

    let schema = do_anything::core::report::target_bounds_schema();
    let validator = jsonschema::validator_for(&schema).unwrap();

    // Step 0 is browser.open (no target_bounds); steps 1..=5 are UI interactions with valid target_bounds.
    assert!(report_json["steps"][0].get("target_bounds").is_none());
    let live_vp = &report_json["steps"][5]["result"]["viewport"];
    let live_w = live_vp["width"]
        .as_u64()
        .expect("at-click must report live viewport width");
    let live_h = live_vp["height"]
        .as_u64()
        .expect("at-click must report live viewport height");
    assert!(live_w > 0 && live_h > 0);
    for idx in [1, 2, 3, 4, 5] {
        let tb = report_json["steps"][idx]
            .get("target_bounds")
            .unwrap_or_else(|| {
                panic!(
                    "step {idx} missing target_bounds: {}",
                    report_json["steps"][idx]
                )
            });
        assert!(
            validator.is_valid(tb),
            "step {idx} target_bounds failed schema validation: {tb}"
        );
        assert_eq!(tb["viewport"]["width"], live_w);
        assert_eq!(tb["viewport"]["height"], live_h);
    }
    assert!(
        report_json["steps"][1]["target_bounds"]["width"]
            .as_f64()
            .unwrap_or(0.0)
            > 0.0,
        "located click should record non-zero element width"
    );
    assert!(
        report_json["steps"][4]["target_bounds"]
            .get("from")
            .is_some()
            && report_json["steps"][4]["target_bounds"].get("to").is_some(),
        "drag step must record from/to endpoints: {}",
        report_json["steps"][4]["target_bounds"]
    );
    assert_eq!(report_json["steps"][5]["target_bounds"]["x"], 120);
    assert_eq!(report_json["steps"][5]["target_bounds"]["y"], 80);

    let report_html = dir.path().join("report.html");
    let rep_out = run_da(&["report", path_str(&record), "-o", path_str(&report_html)]);
    assert_eq!(exit_code(&rep_out), Some(0), "stderr: {}", stderr(&rep_out));
    let html = read(&report_html);
    assert!(html.contains(&format!(
        "<svg class=\"shot-overlay\" viewBox=\"0 0 {live_w} {live_h}\""
    )));
    assert!(html.contains("class=\"tb-box\""));
    assert!(html.contains("class=\"tb-ring\""));
    assert!(html.contains("class=\"tb-drag-line\""));
    assert!(html.contains("class=\"tb-drag-arrow\""));

    // Verify `da session` also persists schema-valid `target_bounds` in `steps.jsonl`
    // and `da report` renders the session directory directly.
    let sessions_dir = dir.path().join("sessions");
    let start_out = Command::new(da())
        .current_dir(dir.path())
        .args([
            "--output",
            "json",
            "session",
            "start",
            "--sessions-dir",
            path_str(&sessions_dir),
        ])
        .output()
        .expect("session start");
    assert_eq!(exit_code(&start_out), Some(0), "{}", stderr(&start_out));
    let start_json: Value = serde_json::from_slice(&start_out.stdout).unwrap();
    let sid = start_json["session_id"].as_str().unwrap().to_string();

    let open_step = json!({
        "browser.open": {
            "url": server.url("/browser/locators"),
            "width": 1000,
            "height": 700
        }
    })
    .to_string();
    let open_out = Command::new(da())
        .current_dir(dir.path())
        .args([
            "session",
            "step",
            &sid,
            "--sessions-dir",
            path_str(&sessions_dir),
            "--step",
            &open_step,
        ])
        .output()
        .expect("session open");
    assert_eq!(
        exit_code(&open_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&open_out),
        stderr(&open_out)
    );

    let click_step = json!({ "browser.click": { "testid": "solo-btn" } }).to_string();
    let click_out = Command::new(da())
        .current_dir(dir.path())
        .args([
            "session",
            "step",
            &sid,
            "--sessions-dir",
            path_str(&sessions_dir),
            "--step",
            &click_step,
        ])
        .output()
        .expect("session click");
    assert_eq!(
        exit_code(&click_out),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&click_out),
        stderr(&click_out)
    );

    let _ = Command::new(da())
        .current_dir(dir.path())
        .args([
            "session",
            "close",
            &sid,
            "--sessions-dir",
            path_str(&sessions_dir),
        ])
        .output();

    let sess_dir = sessions_dir.join(&sid);
    let steps_text = read(&sess_dir.join("steps.jsonl"));
    let lines: Vec<Value> = steps_text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    let sess_tb = lines[1]
        .get("target_bounds")
        .expect("session browser.click must persist target_bounds in steps.jsonl");
    assert!(
        validator.is_valid(sess_tb),
        "invalid session target_bounds: {sess_tb}"
    );

    let sess_html = dir.path().join("sess-report.html");
    let srep = run_da(&["report", path_str(&sess_dir), "-o", path_str(&sess_html)]);
    assert_eq!(exit_code(&srep), Some(0), "stderr: {}", stderr(&srep));
    let shtml = read(&sess_html);
    assert!(shtml.contains("Exploration Run (da session)"));
    assert!(shtml.contains("Target geometry:"));
}

#[test]
fn session_launched_temp_profile_is_not_local_only_while_named_profile_and_connect_are() {
    let dir = TempDir::new("report-session-temp-profile");

    // 1) Normal launched browser: `profile` holds da's temp profile dir, `profile_name` is null.
    let temp_sess = dir.path().join("sess-temp-profile");
    fs::create_dir_all(&temp_sess).unwrap();
    fs::write(temp_sess.join("preview.png"), PNG).unwrap();
    fs::write(
        temp_sess.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "id": "sess-temp-profile",
            "created_at_ms": 1_700_000_000_000_u64,
            "last_active_ms": 1_700_000_001_000_u64,
            "idle_timeout_ms": 1_800_000_u64,
            "handles": {
                "browser": {
                    "mode": "launched",
                    "profile": "/tmp/da-browser-temp-1234",
                    "profile_name": null,
                    "connect": null,
                    "port": 9222,
                    "websocket_url": "ws://127.0.0.1:9222/devtools/browser/abc",
                    "target_id": "t1",
                    "headless": true,
                    "width": 1280,
                    "height": 720,
                    "testid_attribute": "data-testid"
                }
            },
            "vars": {},
            "env_names": [],
            "observe": [],
            "plugins": []
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        temp_sess.join("steps.jsonl"),
        format!(
            "{}\n",
            json!({
                "index": 1,
                "action": "browser.open",
                "raw_args": { "url": "http://127.0.0.1:8080" },
                "status": "passed",
                "started_at_ms": 1_700_000_000_100_u64,
                "duration_ms": 40
            })
        ),
    )
    .unwrap();

    let out_html = dir.path().join("temp-profile.html");
    let out = run_da(&[
        "--output",
        "json",
        "report",
        path_str(&temp_sess),
        "-o",
        path_str(&out_html),
    ]);
    assert_eq!(exit_code(&out), Some(0), "stderr: {}", stderr(&out));
    let summary: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(summary["images_embedded"], 1, "temp profile must embed PNG");
    assert_eq!(
        summary["images_omitted"], 0,
        "temp profile must not omit PNG"
    );
    let html = read(&out_html);
    assert!(html.contains("data:image/png;base64,"));
    assert!(!html.contains("browser used a persistent local profile"));

    // 2) Named persistent profile (`profile_name: "staging"`) MUST be marked local_only.
    let named_sess = dir.path().join("sess-named-profile");
    fs::create_dir_all(&named_sess).unwrap();
    fs::write(named_sess.join("preview.png"), PNG).unwrap();
    fs::write(
        named_sess.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "id": "sess-named-profile",
            "created_at_ms": 1_700_000_000_000_u64,
            "last_active_ms": 1_700_000_001_000_u64,
            "idle_timeout_ms": 1_800_000_u64,
            "handles": {
                "browser": {
                    "mode": "launched",
                    "profile": "/home/user/.da/profiles/staging",
                    "profile_name": "staging",
                    "connect": null,
                    "port": 9223,
                    "websocket_url": "ws://127.0.0.1:9223/devtools/browser/def",
                    "target_id": "t2",
                    "headless": true,
                    "width": 1280,
                    "height": 720,
                    "testid_attribute": "data-testid"
                }
            },
            "vars": {},
            "env_names": [],
            "observe": [],
            "plugins": []
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(named_sess.join("steps.jsonl"), "").unwrap();

    let named_html = dir.path().join("named-profile.html");
    let named_out = run_da(&[
        "--output",
        "json",
        "report",
        path_str(&named_sess),
        "-o",
        path_str(&named_html),
    ]);
    assert_eq!(
        exit_code(&named_out),
        Some(0),
        "stderr: {}",
        stderr(&named_out)
    );
    let named_summary: Value = serde_json::from_slice(&named_out.stdout).unwrap();
    assert_eq!(named_summary["images_embedded"], 0);
    assert_eq!(named_summary["images_omitted"], 1);
    let named_text = read(&named_html);
    assert!(!named_text.contains("data:image/png;base64,"));
    assert!(named_text.contains("browser used a persistent local profile"));

    // 3) External connected browser (`connect: ...`) MUST be marked local_only.
    let conn_sess = dir.path().join("sess-connected");
    fs::create_dir_all(&conn_sess).unwrap();
    fs::write(conn_sess.join("preview.png"), PNG).unwrap();
    fs::write(
        conn_sess.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "id": "sess-connected",
            "created_at_ms": 1_700_000_000_000_u64,
            "last_active_ms": 1_700_000_001_000_u64,
            "idle_timeout_ms": 1_800_000_u64,
            "handles": {
                "browser": {
                    "mode": "connected",
                    "profile": null,
                    "profile_name": null,
                    "connect": "http://127.0.0.1:9222",
                    "port": 9222,
                    "websocket_url": "ws://127.0.0.1:9222/devtools/browser/ghi",
                    "target_id": "t3",
                    "headless": false,
                    "width": 1280,
                    "height": 720,
                    "testid_attribute": "data-testid"
                }
            },
            "vars": {},
            "env_names": [],
            "observe": [],
            "plugins": []
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(conn_sess.join("steps.jsonl"), "").unwrap();

    let conn_html = dir.path().join("connected.html");
    let conn_out = run_da(&[
        "--output",
        "json",
        "report",
        path_str(&conn_sess),
        "-o",
        path_str(&conn_html),
    ]);
    assert_eq!(
        exit_code(&conn_out),
        Some(0),
        "stderr: {}",
        stderr(&conn_out)
    );
    let conn_summary: Value = serde_json::from_slice(&conn_out.stdout).unwrap();
    assert_eq!(conn_summary["images_embedded"], 0);
    assert_eq!(conn_summary["images_omitted"], 1);
    assert!(read(&conn_html).contains("attached to an external browser endpoint"));
}

#[test]
fn session_act_and_synthesized_step_telemetry_does_not_double_count() {
    let dir = TempDir::new("report-session-act-telemetry");

    // Case A: `act` completes Done after 3 calls (25 ms, 25 ms, 25 ms = 75 ms total; 2 UI steps + 1 model.act)
    // plus a `select_ref --act` (40 ms) whose synthesized `browser.click` also carries provenance audit.
    let done_sess = dir.path().join("sess-act-done");
    fs::create_dir_all(&done_sess).unwrap();
    fs::write(
        done_sess.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "id": "sess-act-done",
            "created_at_ms": 1_700_000_000_000_u64,
            "last_active_ms": 1_700_000_005_000_u64,
            "idle_timeout_ms": 1_800_000_u64,
            "handles": {},
            "vars": {},
            "env_names": [],
            "observe": [],
            "plugins": []
        }))
        .unwrap(),
    )
    .unwrap();

    let done_steps = [
        json!({
            "index": 1,
            "action": "browser.type",
            "raw_args": { "ref": "e1", "text": "hello" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_100_u64,
            "duration_ms": 30,
            "model": "specai-hy/hy4",
            "latency_ms": 25,
            "usage": { "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120 }
        }),
        json!({
            "index": 2,
            "action": "browser.click",
            "raw_args": { "ref": "e2" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_200_u64,
            "duration_ms": 28,
            "model": "specai-hy/hy4",
            "latency_ms": 25,
            "usage": { "prompt_tokens": 110, "completion_tokens": 20, "total_tokens": 130 }
        }),
        json!({
            "index": 3,
            "action": "model.act",
            "raw_args": { "goal": "fill and submit", "max_steps": 5 },
            "status": "passed",
            "started_at_ms": 1_700_000_000_050_u64,
            "duration_ms": 200,
            "result": {
                "status": "done",
                "steps_taken": 2,
                "max_steps": 5,
                "model_calls": 3,
                "latency_ms": 75,
                "max_model_latency_ms": 25
            },
            "model": "specai-hy/hy4",
            "latency_ms": 75,
            "usage": { "prompt_tokens": 310, "completion_tokens": 50, "total_tokens": 360 }
        }),
        json!({
            "index": 4,
            "action": "model.select_ref",
            "raw_args": { "query": "confirm" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_400_u64,
            "duration_ms": 45,
            "result": { "selected_ref": "e3" },
            "model": "specai-hy/hy4",
            "latency_ms": 40,
            "usage": { "prompt_tokens": 90, "completion_tokens": 10, "total_tokens": 100 }
        }),
        json!({
            "index": 5,
            "action": "browser.click",
            "raw_args": { "ref": "e3" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_450_u64,
            "duration_ms": 12,
            "model": "specai-hy/hy4",
            "latency_ms": 40,
            "usage": { "prompt_tokens": 90, "completion_tokens": 10, "total_tokens": 100 }
        }),
    ];
    let done_jsonl: String = done_steps
        .iter()
        .map(|s| format!("{}\n", serde_json::to_string(s).unwrap()))
        .collect();
    fs::write(done_sess.join("steps.jsonl"), done_jsonl).unwrap();

    let done_html = dir.path().join("act-done.html");
    let done_out = run_da(&["report", path_str(&done_sess), "-o", path_str(&done_html)]);
    assert_eq!(
        exit_code(&done_out),
        Some(0),
        "stderr: {}",
        stderr(&done_out)
    );
    let html = read(&done_html);
    // Calls = 3 (model.act) + 1 (model.select_ref) = 4 (NOT 7)
    assert!(
        html.contains("<strong>4</strong><span>model calls</span>"),
        "unexpected call count in HTML"
    );
    // Total latency = 75 + 40 = 115 ms (NOT 205 ms); avg = 115 / 4 = 28 ms; max = 40 ms
    assert!(
        html.contains("<strong>115 ms</strong><span>total model latency</span>"),
        "expected exact 115 ms total model latency"
    );
    assert!(
        html.contains("<strong>28 / 40 ms</strong><span>avg / max latency</span>"),
        "expected exact 28 / 40 ms avg / max"
    );
    // Total tokens = 360 + 100 = 460 (NOT 810)
    assert!(
        html.contains("<strong>460</strong><span>total tokens</span>"),
        "expected exact 460 total tokens"
    );
    // Telemetry chips list only authoritative model actions, never UI steps
    assert!(html.contains("model.act: 3"));
    assert!(html.contains("model.select_ref: 1"));
    assert!(!html.contains("browser.type: 1"));
    assert!(!html.contains("browser.click: 1"));

    // Case B: `act` hits `max_steps_exceeded` after 2 calls (30 ms + 45 ms = 75 ms; 2 UI steps + 1 model.act)
    let max_sess = dir.path().join("sess-act-max-steps");
    fs::create_dir_all(&max_sess).unwrap();
    fs::write(
        max_sess.join("state.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "id": "sess-act-max-steps",
            "created_at_ms": 1_700_000_000_000_u64,
            "last_active_ms": 1_700_000_005_000_u64,
            "idle_timeout_ms": 1_800_000_u64,
            "handles": {},
            "vars": {},
            "env_names": [],
            "observe": [],
            "plugins": []
        }))
        .unwrap(),
    )
    .unwrap();
    let max_steps = [
        json!({
            "index": 1,
            "action": "browser.click",
            "raw_args": { "ref": "e1" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_100_u64,
            "duration_ms": 10,
            "model": "specai-hy/hy4",
            "latency_ms": 30,
            "usage": { "prompt_tokens": 200, "completion_tokens": 30, "total_tokens": 230 }
        }),
        json!({
            "index": 2,
            "action": "browser.click",
            "raw_args": { "ref": "e2" },
            "status": "passed",
            "started_at_ms": 1_700_000_000_200_u64,
            "duration_ms": 10,
            "model": "specai-hy/hy4",
            "latency_ms": 45,
            "usage": { "prompt_tokens": 220, "completion_tokens": 40, "total_tokens": 260 }
        }),
        json!({
            "index": 3,
            "action": "model.act",
            "raw_args": { "goal": "loop", "max_steps": 2 },
            "status": "failed",
            "error": "reached `--max-steps` limit (2) without completing goal",
            "started_at_ms": 1_700_000_000_050_u64,
            "duration_ms": 120,
            "result": {
                "status": "max_steps_exceeded",
                "steps_taken": 2,
                "max_steps": 2,
                "model_calls": 2,
                "latency_ms": 75,
                "max_model_latency_ms": 45
            },
            "model": "specai-hy/hy4",
            "latency_ms": 75,
            "usage": { "prompt_tokens": 420, "completion_tokens": 70, "total_tokens": 490 }
        }),
    ];
    let max_jsonl: String = max_steps
        .iter()
        .map(|s| format!("{}\n", serde_json::to_string(s).unwrap()))
        .collect();
    fs::write(max_sess.join("steps.jsonl"), max_jsonl).unwrap();

    let max_html = dir.path().join("act-max.html");
    let max_out = run_da(&["report", path_str(&max_sess), "-o", path_str(&max_html)]);
    assert_eq!(exit_code(&max_out), Some(0), "stderr: {}", stderr(&max_out));
    let mhtml = read(&max_html);
    assert!(
        mhtml.contains("<strong>2</strong><span>model calls</span>"),
        "max_steps_exceeded must report exactly 2 model calls"
    );
    assert!(
        mhtml.contains("<strong>75 ms</strong><span>total model latency</span>"),
        "max_steps_exceeded must report 75 ms total latency, not 150 ms"
    );
    assert!(
        mhtml.contains("<strong>37 / 45 ms</strong><span>avg / max latency</span>"),
        "max_steps_exceeded must report 37 / 45 ms avg / max"
    );
    assert!(
        mhtml.contains("<strong>490</strong><span>total tokens</span>"),
        "max_steps_exceeded must report 490 total tokens"
    );
    assert!(mhtml.contains("model.act: 2"));
    assert!(!mhtml.contains("browser.click: 2"));
}

#[test]
fn locate_target_bounds_sample_validates_against_schema() {
    let schema = do_anything::core::report::target_bounds_schema();
    let validator = jsonschema::validator_for(&schema).expect("valid target_bounds schema");
    let locate_bounds = json!({
        "x": 640.5,
        "y": 360.0,
        "width": 0,
        "height": 0,
        "point": { "x": 640.5, "y": 360.0 },
        "viewport": { "width": 1280, "height": 720 }
    });
    assert!(validator.is_valid(&locate_bounds));
    assert!(do_anything::core::report::validate_target_bounds(&locate_bounds).is_ok());
}
