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
    assert!(
        !html.contains("</script>"),
        "the report must not contain a script end tag"
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
    assert!(!normal_html.contains("</script>"));

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
    assert!(!shareable_html.contains("</script>"));
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
