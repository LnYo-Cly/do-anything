#![cfg(feature = "desktop")]

//! Integration, schema, lint, session, export, and screenshot-masking tests
//! for the `desktop.*` driver (`#84`).

mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use support::TempDir;

#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::process::Stdio;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};
#[cfg(windows)]
use std::time::Instant;

/// Serializes interactive desktop tests inside this binary so two tests never
/// compete for focus or input simultaneously.
#[cfg(windows)]
static TEST_DESKTOP_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(windows)]
fn desktop_test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_DESKTOP_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(da());
    cmd.current_dir(dir).args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("failed to run the da binary")
}

fn stdout_str(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_str(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json_stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "expected valid JSON on stdout: {err}\nstdout:\n{}\nstderr:\n{}",
            stdout_str(output),
            stderr_str(output)
        )
    })
}

#[cfg(windows)]
fn find_csc() -> PathBuf {
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let candidates = [
        system_root.join(r"Microsoft.NET\Framework64\v4.0.30319\csc.exe"),
        system_root.join(r"Microsoft.NET\Framework\v4.0.30319\csc.exe"),
        PathBuf::from("csc.exe"),
    ];
    for candidate in candidates {
        if candidate.is_file() || candidate == Path::new("csc.exe") {
            return candidate;
        }
    }
    panic!("could not locate Windows csc.exe to build WinForms test fixture");
}

#[cfg(windows)]
const FIXTURE_CS: &str = r#"
using System;
using System.Drawing;
using System.Windows.Forms;

public class FixtureForm : Form {
    private readonly bool _onscreen;
    private readonly TextBox _usernameBox;
    private readonly TextBox _passwordBox;
    private readonly Label _statusLabel;
    private readonly Panel _drawCanvas;

    public FixtureForm(string title, bool onscreen, string secretLabel) {
        _onscreen = onscreen;
        this.Text = title;
        this.ClientSize = new Size(440, 320);
        this.FormBorderStyle = FormBorderStyle.FixedSingle;
        this.MaximizeBox = false;
        this.MinimizeBox = false;
        this.StartPosition = FormStartPosition.Manual;
        this.Location = onscreen ? new Point(60, 60) : new Point(-3200, -3200);
        this.BackColor = Color.FromArgb(240, 240, 245);
        this.KeyPreview = true;

        _usernameBox = new TextBox();
        _usernameBox.Name = "usernameBox";
        _usernameBox.AccessibleName = "Username";
        _usernameBox.Location = new Point(20, 20);
        _usernameBox.Size = new Size(200, 26);
        _usernameBox.BackColor = Color.FromArgb(255, 180, 60);
        this.Controls.Add(_usernameBox);

        _passwordBox = new TextBox();
        _passwordBox.Name = "passwordBox";
        _passwordBox.AccessibleName = "Password";
        _passwordBox.UseSystemPasswordChar = true;
        _passwordBox.Location = new Point(20, 56);
        _passwordBox.Size = new Size(200, 26);
        _passwordBox.BackColor = Color.FromArgb(0, 220, 220);
        this.Controls.Add(_passwordBox);

        var actionPanel = new Panel();
        actionPanel.Name = "actionPanel";
        actionPanel.AccessibleName = "Actions";
        actionPanel.Location = new Point(20, 92);
        actionPanel.Size = new Size(400, 56);

        var submitBtn = new Button();
        submitBtn.Name = "submitBtn";
        submitBtn.Text = "Submit";
        submitBtn.Location = new Point(10, 12);
        submitBtn.Size = new Size(100, 32);
        submitBtn.Click += (s, e) => {
            _statusLabel.Text = "Submitted:" + _usernameBox.Text + "|pwlen=" + _passwordBox.Text.Length;
        };
        actionPanel.Controls.Add(submitBtn);

        // Intentionally leave .Name unset so WinForms exposes HWND as AutomationId,
        // verifying that our driver filters volatile HWND AutomationIds.
        var secondaryBtn = new Button();
        secondaryBtn.Text = "Secondary";
        secondaryBtn.Location = new Point(120, 12);
        secondaryBtn.Size = new Size(90, 32);
        secondaryBtn.Click += (s, e) => {
            _statusLabel.Text = "SecondaryClicked";
        };
        actionPanel.Controls.Add(secondaryBtn);

        var dup1 = new Button();
        dup1.Text = "Dup";
        dup1.Location = new Point(220, 12);
        dup1.Size = new Size(70, 32);
        actionPanel.Controls.Add(dup1);

        var dup2 = new Button();
        dup2.Text = "Dup";
        dup2.Location = new Point(300, 12);
        dup2.Size = new Size(70, 32);
        dup2.Click += (s, e) => {
            _statusLabel.Text = "Dup2Clicked";
        };
        actionPanel.Controls.Add(dup2);

        this.Controls.Add(actionPanel);

        _statusLabel = new Label();
        _statusLabel.Name = "statusLabel";
        _statusLabel.Text = "Status:Ready";
        _statusLabel.Location = new Point(20, 158);
        _statusLabel.Size = new Size(380, 26);
        this.Controls.Add(_statusLabel);

        _drawCanvas = new Panel();
        _drawCanvas.AccessibleRole = AccessibleRole.Client;
        _drawCanvas.Location = new Point(20, 192);
        _drawCanvas.Size = new Size(180, 80);
        _drawCanvas.BackColor = Color.FromArgb(40, 140, 240);
        _drawCanvas.MouseClick += (s, e) => {
            _statusLabel.Text = "CanvasClick:" + e.X + "," + e.Y;
        };
        this.Controls.Add(_drawCanvas);

        var closeBtn = new Button();
        closeBtn.Name = "closeBtn";
        closeBtn.Text = "Close";
        closeBtn.Location = new Point(220, 200);
        closeBtn.Size = new Size(90, 32);
        closeBtn.Click += (s, e) => {
            this.Close();
        };
        this.Controls.Add(closeBtn);

        if (!string.IsNullOrEmpty(secretLabel)) {
            var secretLbl = new Label();
            secretLbl.Name = "secretDisplay";
            secretLbl.Text = "Secret:" + secretLabel;
            secretLbl.Location = new Point(20, 282);
            secretLbl.Size = new Size(380, 24);
            this.Controls.Add(secretLbl);
        }

        this.KeyDown += (s, e) => {
            if (e.Control && e.KeyCode == Keys.S) {
                _statusLabel.Text = "Shortcut:Ctrl+S";
                e.Handled = true;
                e.SuppressKeyPress = true;
            }
        };
    }

    protected override bool ShowWithoutActivation {
        get { return !_onscreen; }
    }

    protected override CreateParams CreateParams {
        get {
            CreateParams cp = base.CreateParams;
            if (!_onscreen) {
                // WS_EX_NOACTIVATE prevents UIA ValuePattern::SetValue from stealing foreground focus.
                cp.ExStyle |= 0x08000000;
            }
            return cp;
        }
    }

    protected override void OnLoad(EventArgs e) {
        base.OnLoad(e);
        this.Location = _onscreen ? new Point(60, 60) : new Point(-3200, -3200);
    }

    [STAThread]
    public static void Main(string[] args) {
        Application.EnableVisualStyles();
        Application.SetCompatibleTextRenderingDefault(false);
        string title = "DA Desktop Fixture";
        bool onscreen = false;
        string secret = null;
        for (int i = 0; i < args.Length; i++) {
            if (args[i] == "--title" && i + 1 < args.Length) {
                title = args[++i];
            } else if (args[i] == "--onscreen") {
                onscreen = true;
            } else if (args[i] == "--secret" && i + 1 < args.Length) {
                secret = args[++i];
            }
        }
        Application.Run(new FixtureForm(title, onscreen, secret));
    }
}
"#;

#[cfg(windows)]
fn compile_fixture(dir: &Path) -> PathBuf {
    let cs_path = dir.join("fixture.cs");
    let exe_path = dir.join("DesktopFixture.exe");
    std::fs::write(&cs_path, FIXTURE_CS).expect("failed to write fixture.cs");

    let csc = find_csc();
    let output = Command::new(&csc)
        .args([
            "/nologo",
            "/target:winexe",
            "/r:System.Windows.Forms.dll",
            "/r:System.Drawing.dll",
            &format!("/out:{}", exe_path.display()),
            &cs_path.display().to_string(),
        ])
        .output()
        .unwrap_or_else(|err| panic!("failed to run {}: {err}", csc.display()));
    assert!(
        output.status.success(),
        "csc failed: {}\n{}",
        stdout_str(&output),
        stderr_str(&output)
    );
    exe_path
}

#[cfg(windows)]
struct ProcessGuard(Option<std::process::Child>);

#[cfg(windows)]
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn desktop_registry_and_validation_enforces_locators_at_and_key_specs() {
    let mut registry = do_anything::core::registry::Registry::new();
    do_anything::drivers::register(&mut registry).expect("desktop driver should register cleanly");

    for step in [
        "desktop.launch",
        "desktop.attach",
        "desktop.click",
        "desktop.type",
        "desktop.press",
        "desktop.wait",
        "desktop.read",
        "desktop.screenshot",
    ] {
        assert!(
            registry.get(step).is_some(),
            "missing registered step `{step}`"
        );
    }

    let click = registry.get("desktop.click").expect("desktop.click");
    assert!((click.validate)(&json!({"automation_id": "btn"})).is_ok());
    assert!((click.validate)(&json!({"role": "Button", "name": "Submit"})).is_ok());
    assert!((click.validate)(&json!({"at": {"x": 10, "y": 20}})).is_ok());
    assert!(
        (click.validate)(
            &json!({"at": {"x": 10, "y": 20, "window": {"width": 440, "height": 320}}})
        )
        .is_ok()
    );
    // Ref is rejected by static validator (session validator handles it).
    assert!((click.validate)(&json!({"ref": "e1"})).is_err());
    // Cannot combine `at` with locator.
    assert!((click.validate)(&json!({"at": {"x": 10, "y": 20}, "automation_id": "btn"})).is_err());
    // Negative coordinates and zero window bounds rejected.
    assert!((click.validate)(&json!({"at": {"x": -1, "y": 5}})).is_err());
    assert!(
        (click.validate)(&json!({"at": {"x": 5, "y": 5, "window": {"width": 0, "height": 100}}}))
            .is_err()
    );
    // Unknown role rejected.
    assert!((click.validate)(&json!({"role": "NotARealRole"})).is_err());
    // Empty locator rejected.
    assert!((click.validate)(&json!({})).is_err());

    let type_step = registry.get("desktop.type").expect("desktop.type");
    assert!((type_step.validate)(&json!({"automation_id": "user", "text": "alice"})).is_ok());
    assert!(
        (type_step.validate)(&json!({"text": "Username", "value": "alice"})).is_ok(),
        "text locator + value text-to-type must validate"
    );
    assert!((type_step.validate)(&json!({"at": {"x": 20, "y": 20}, "text": "alice"})).is_ok());
    assert!(
        (type_step.validate)(&json!({"automation_id": "user"})).is_err(),
        "missing typing text must be rejected"
    );

    let press = registry.get("desktop.press").expect("desktop.press");
    for good in [
        "Enter",
        "Ctrl+S",
        "Ctrl+Shift+P",
        "Alt+F4",
        "Tab",
        "Escape",
        "a",
    ] {
        assert!(
            (press.validate)(&json!({"key": good})).is_ok(),
            "`{good}` should validate cleanly"
        );
    }
    for bad in ["", "   ", "Ctrl+", "UnknownKey999", "Ctrl+Alt+"] {
        assert!(
            (press.validate)(&json!({"key": bad})).is_err(),
            "`{bad}` should be rejected"
        );
    }

    let attach = registry.get("desktop.attach").expect("desktop.attach");
    assert!((attach.validate)(&json!({"pid": 1234})).is_ok());
    assert!((attach.validate)(&json!({"executable": "notepad.exe"})).is_ok());
    assert!((attach.validate)(&json!({"title_contains": "Notepad"})).is_ok());
    assert!((attach.validate)(&json!({})).is_err());
    assert!((attach.validate)(&json!({"pid": 0})).is_err());

    // Verify retryability classification matches read-only vs mutating steps.
    assert!(
        (registry.get("desktop.wait").unwrap().check_retry)(&json!({"automation_id": "x"})).is_ok()
    );
    assert!(
        (registry.get("desktop.read").unwrap().check_retry)(&json!({"automation_id": "x"})).is_ok()
    );
    assert!(
        (registry.get("desktop.screenshot").unwrap().check_retry)(&json!({"path": "a.png"}))
            .is_ok()
    );
    assert!((click.check_retry)(&json!({"automation_id": "x"})).is_err());
    assert!((type_step.check_retry)(&json!({"automation_id": "x", "text": "a"})).is_err());
    assert!((press.check_retry)(&json!({"key": "Enter"})).is_err());
    assert!(
        (registry.get("desktop.launch").unwrap().check_retry)(&json!({"command": "x"})).is_err()
    );
}

#[test]
fn desktop_lint_warns_on_at_coordinates_and_missing_window_size() {
    let dir = TempDir::new("desktop-lint");
    let case_path = dir.path().join("at-case.yaml");
    std::fs::write(
        &case_path,
        "\
name: Coordinate warning check
steps:
  - desktop.launch:
      command: notepad.exe
  - desktop.click:
      at:
        x: 10
        y: 20
  - desktop.click:
      at:
        x: 10
        y: 20
        window:
          width: 440
          height: 320
",
    )
    .expect("write case");

    let output = run_da(
        dir.path(),
        &["lint", "--output", "json", case_path.to_str().unwrap()],
        &[],
    );
    assert!(
        output.status.success(),
        "da lint failed: {}",
        stderr_str(&output)
    );
    let report = json_stdout(&output);
    let warnings = report["warnings"]
        .as_array()
        .expect("lint report should contain warnings array");
    assert!(
        warnings.len() >= 3,
        "expected warnings for fragile `at` and missing `at.window`: {report}"
    );
    let joined: Vec<&str> = warnings
        .iter()
        .filter_map(|w| w["message"].as_str())
        .collect();
    assert!(joined.iter().any(|m| m.contains("`at`")));
    assert!(joined.iter().any(|m| m.contains("`at.window`")));
}

#[cfg(not(windows))]
#[test]
fn desktop_non_windows_stub_returns_tracking_issue_environment_error() {
    let dir = TempDir::new("desktop-stub");
    let case_path = dir.path().join("stub.yaml");
    std::fs::write(
        &case_path,
        "\
name: Non-Windows stub
steps:
  - desktop.launch:
      command: app
",
    )
    .expect("write stub case");

    let output = run_da(
        dir.path(),
        &[
            "run",
            "--output",
            "json",
            "--no-bundle",
            case_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(!output.status.success());
    let report = json_stdout(&output);
    let err = report["cases"][0]["error"]["message"]
        .as_str()
        .or_else(|| report["cases"][0]["error"].as_str())
        .or_else(|| {
            report["cases"][0]["steps"].as_array().and_then(|steps| {
                steps
                    .iter()
                    .rev()
                    .find_map(|s| s["error"]["message"].as_str())
            })
        })
        .unwrap_or_default();
    assert!(
        err.contains("#88") && err.contains("Windows"),
        "expected #88 Environment error on non-Windows, got: {report}"
    );
}

#[cfg(windows)]
#[test]
fn desktop_session_snapshot_drive_export_lint_and_replay() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-session-replay");
    let exe = compile_fixture(dir.path());
    let title = format!("DA Session Fixture {}", std::process::id());

    // 1. Start session and launch off-screen fixture.
    let started = json_stdout(&run_da(
        dir.path(),
        &["session", "start", "--output", "json"],
        &[],
    ));
    let session_id = started["session_id"]
        .as_str()
        .expect("session_id")
        .to_string();

    let launch_step = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.launch",
            "--output",
            "json",
            "--with",
            &json!({
                "command": exe,
                "args": ["--title", title],
                "window_title_contains": title,
                "timeout_ms": 15000
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(
        launch_step["ok"].as_bool().unwrap_or(false),
        "{launch_step}"
    );

    // 2. Snapshot: verify refs assigned, volatile HWND AutomationId filtered,
    // and IsPassword element never exposes plaintext value.
    let snap = json_stdout(&run_da(
        dir.path(),
        &["session", "snapshot", &session_id, "--output", "json"],
        &[],
    ));
    assert!(snap["ok"].as_bool().unwrap_or(false), "{snap}");
    let elements = snap["snapshot"]["elements"]
        .as_array()
        .or_else(|| snap["snapshots"]["desktop"]["elements"].as_array())
        .unwrap_or_else(|| panic!("snapshot elements missing: {snap}"));

    let find_ref = |aid: &str| -> String {
        elements
            .iter()
            .find(|el| el["automation_id"].as_str() == Some(aid))
            .unwrap_or_else(|| panic!("missing element `{aid}` in snapshot: {snap}"))["ref"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let user_ref = find_ref("usernameBox");
    let pass_ref = find_ref("passwordBox");
    let submit_ref = find_ref("submitBtn");
    let status_ref = find_ref("statusLabel");
    let secondary_el = elements
        .iter()
        .find(|el| el["name"].as_str() == Some("Secondary"))
        .expect("Secondary button missing in snapshot");
    assert!(
        secondary_el["automation_id"].is_null(),
        "WinForms HWND AutomationId must be filtered to null: {secondary_el}"
    );
    let dup2_el = elements
        .iter()
        .rfind(|el| el["name"].as_str() == Some("Dup"))
        .expect("second Dup button missing");
    assert!(dup2_el["automation_id"].is_null());

    // 3. Drive by refs: type into username + password, click Submit, and read status/password.
    let s1 = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.type",
            "--output",
            "json",
            "--with",
            &json!({"ref": user_ref, "text": "alice"}).to_string(),
        ],
        &[],
    ));
    assert!(s1["ok"].as_bool().unwrap_or(false), "{s1}");

    let s2 = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.type",
            "--output",
            "json",
            "--with",
            &json!({"ref": pass_ref, "text": "s3cret-pass"}).to_string(),
        ],
        &[],
    ));
    assert!(s2["ok"].as_bool().unwrap_or(false), "{s2}");

    let s3 = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.click",
            "--output",
            "json",
            "--with",
            &json!({"ref": submit_ref}).to_string(),
        ],
        &[],
    ));
    assert!(s3["ok"].as_bool().unwrap_or(false), "{s3}");

    // Pure exploratory read (no expect/save) -> skipped on export unless --include-reads.
    let s4 = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.read",
            "--output",
            "json",
            "--with",
            &json!({"ref": status_ref}).to_string(),
        ],
        &[],
    ));
    assert!(s4["ok"].as_bool().unwrap_or(false), "{s4}");
    assert_eq!(s4["step"]["result"]["name"], "Submitted:alice|pwlen=11");

    // Read password field: value must stay redacted/null.
    let pw_read = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.read",
            "--output",
            "json",
            "--with",
            &json!({"ref": pass_ref}).to_string(),
        ],
        &[],
    ));
    assert_eq!(pw_read["step"]["result"]["is_password"], true);
    assert!(
        pw_read["step"]["result"]["value"].is_null(),
        "password field value must never be returned: {pw_read}"
    );

    // 4. Export YAML (default omits bare desktop.read; --include-reads includes them).
    let case_path = dir.path().join("exported-desktop.yaml");
    let export_default = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "export",
            &session_id,
            "--output",
            "json",
            "--name",
            "Desktop login replay",
            "-o",
            case_path.to_str().unwrap(),
        ],
        &[],
    ));
    assert!(
        export_default["ok"].as_bool().unwrap_or(false),
        "{export_default}"
    );
    let yaml_text = export_default["yaml"].as_str().unwrap();
    assert!(!yaml_text.contains("ref:"));
    assert!(!yaml_text.contains("desktop.read"));
    assert!(yaml_text.contains("automation_id: usernameBox"));
    assert!(yaml_text.contains("automation_id: submitBtn"));

    let export_with_reads = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "export",
            &session_id,
            "--output",
            "json",
            "--include-reads",
        ],
        &[],
    ));
    assert!(
        export_with_reads["yaml"]
            .as_str()
            .unwrap()
            .contains("desktop.read")
    );

    // Stop session cleanly (must terminate owned launch process).
    let stopped = json_stdout(&run_da(
        dir.path(),
        &["session", "stop", &session_id, "--output", "json"],
        &[],
    ));
    assert!(stopped["ok"].as_bool().unwrap_or(false), "{stopped}");

    // 5. Append wait assertion onto exported YAML, lint it, and replay cleanly with `da run`!
    let mut final_yaml = std::fs::read_to_string(&case_path).expect("read exported yaml");
    final_yaml.push_str(
        "  - desktop.wait:\n      automation_id: statusLabel\n      text: Submitted:alice|pwlen=11\n",
    );
    std::fs::write(&case_path, final_yaml).expect("update exported yaml");

    let lint = run_da(
        dir.path(),
        &["lint", "--output", "json", case_path.to_str().unwrap()],
        &[],
    );
    assert!(
        lint.status.success(),
        "da lint failed: {}",
        stderr_str(&lint)
    );
    let lint_json = json_stdout(&lint);
    assert!(
        lint_json["warnings"]
            .as_array()
            .is_none_or(|w| w.is_empty()),
        "expected no lint warnings: {lint_json}"
    );

    let replay = run_da(
        dir.path(),
        &[
            "run",
            "--output",
            "json",
            "--no-bundle",
            case_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        replay.status.success(),
        "da run replay failed:\n{}\n{}",
        stdout_str(&replay),
        stderr_str(&replay)
    );
    let replay_json = json_stdout(&replay);
    assert_eq!(replay_json["summary"]["cases_passed"], 1);
}

#[cfg(windows)]
#[test]
fn desktop_failure_bundle_masks_inputs_and_redacts_secrets() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-mask-bundle");
    let exe = compile_fixture(dir.path());
    let title = format!("DA Mask Fixture {}", std::process::id());
    let secret_value = "TOP-SECRET-TOKEN-9942";
    let case_path = dir.path().join("mask-failure.yaml");
    let masked_path = dir.path().join("shot-masked.png");
    let unmasked_path = dir.path().join("shot-unmasked.png");

    let exe_yaml = exe.to_string_lossy().replace('\\', "\\\\");
    std::fs::write(
        &case_path,
        format!(
            "\
name: Masked screenshot and redacted tree
steps:
  - desktop.launch:
      command: \"{exe_yaml}\"
      args:
        - --title
        - \"{title}\"
        - --secret
        - \"${{env:DESKTOP_TEST_SECRET}}\"
      window_title_contains: \"{title}\"
  - desktop.type:
      automation_id: passwordBox
      text: \"${{env:DESKTOP_TEST_SECRET}}\"
  - desktop.read:
      automation_id: passwordBox
    save:
      pw_bounds: $.bounds
  - desktop.screenshot:
      path: shot-masked.png
  - desktop.screenshot:
      path: shot-unmasked.png
      mask_inputs: false
  - desktop.wait:
      automation_id: statusLabel
      text: NEVER-APPEARS
      timeout_ms: 250
"
        ),
    )
    .expect("write mask case");

    let bundle_root = dir.path().join("bundles");
    let output = run_da(
        dir.path(),
        &[
            "run",
            "--output",
            "json",
            "--bundle-dir",
            bundle_root.to_str().unwrap(),
            case_path.to_str().unwrap(),
        ],
        &[("DESKTOP_TEST_SECRET", secret_value)],
    );
    assert!(
        !output.status.success(),
        "expected failing desktop.wait case"
    );
    let report = json_stdout(&output);
    let pw_bounds = report["cases"][0]["steps"][2]["result"]["bounds"].clone();
    let bx = pw_bounds["x"].as_i64().unwrap().max(0) as u32;
    let by = pw_bounds["y"].as_i64().unwrap().max(0) as u32;
    let bw = pw_bounds["width"].as_i64().unwrap().max(1) as u32;
    let bh = pw_bounds["height"].as_i64().unwrap().max(1) as u32;

    let bundle_dir = PathBuf::from(report["cases"][0]["bundle"].as_str().expect("bundle path"));
    let bundle_png = bundle_dir.join("desktop").join("screenshot.png");
    let bundle_tree = bundle_dir.join("desktop").join("uia-tree.json");
    assert!(bundle_png.is_file(), "missing {}", bundle_png.display());
    assert!(bundle_tree.is_file(), "missing {}", bundle_tree.display());

    // Decode masked, unmasked, and failure-bundle PNGs and inspect pixels inside password box.
    let decode_rgba = |path: &Path| -> (u32, u32, Vec<u8>) {
        let file = std::fs::File::open(path).expect("open png");
        let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
        decoder.set_transformations(png::Transformations::ALPHA);
        let mut reader = decoder.read_info().expect("read png info");
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).expect("decode png frame");
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    };

    let (uw, uh, unmasked) = decode_rgba(&unmasked_path);
    let (mw, mh, masked) = decode_rgba(&masked_path);
    let (fw, fh, fail_png) = decode_rgba(&bundle_png);
    assert_eq!((uw, uh), (mw, mh));
    assert_eq!((uw, uh), (fw, fh));

    let sample_center = |w: u32, buf: &[u8]| -> [u8; 4] {
        let cx = (bx + bw / 2).min(w - 1);
        let cy = (by + bh / 2).min(uh - 1);
        let idx = ((cy * w + cx) * 4) as usize;
        [buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3]]
    };

    let unmasked_center = sample_center(uw, &unmasked);
    let masked_center = sample_center(mw, &masked);
    let fail_center = sample_center(fw, &fail_png);

    assert_ne!(
        &unmasked_center[..3],
        &[0, 0, 0],
        "unmasked password input center should not be solid black"
    );
    assert_eq!(
        masked_center,
        [0, 0, 0, 255],
        "masked screenshot must paint Edit/IsPassword center solid black"
    );
    assert_eq!(
        fail_center,
        [0, 0, 0, 255],
        "failure bundle screenshot must paint Edit/IsPassword center solid black"
    );

    // Ensure pixels outside inputs remain non-black in the failure screenshot.
    let has_non_black = fail_png
        .as_chunks::<4>()
        .0
        .iter()
        .any(|p| p[0] > 0 || p[1] > 0 || p[2] > 0);
    assert!(
        has_non_black,
        "failure screenshot should preserve non-input pixels"
    );

    // Verify uia-tree.json redacted the secret env var.
    let tree_text = std::fs::read_to_string(&bundle_tree).expect("read uia-tree.json");
    assert!(
        !tree_text.contains(secret_value),
        "uia-tree.json leaked secret value: {tree_text}"
    );
}

#[cfg(windows)]
#[test]
fn desktop_closed_window_returns_fast_environment_error_not_uipi() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-closed-fast");
    let exe = compile_fixture(dir.path());
    let title = format!("DA Close Fixture {}", std::process::id());
    let exe_yaml = exe.to_string_lossy().replace('\\', "\\\\");
    let case_path = dir.path().join("closed-window.yaml");

    std::fs::write(
        &case_path,
        format!(
            "\
name: Closed window classification
steps:
  - desktop.launch:
      command: \"{exe_yaml}\"
      args:
        - --title
        - \"{title}\"
      window_title_contains: \"{title}\"
  - desktop.click:
      automation_id: closeBtn
  - desktop.wait:
      automation_id: submitBtn
      state: gone
      timeout_ms: 5000
  - desktop.click:
      automation_id: submitBtn
      timeout_ms: 15000
"
        ),
    )
    .expect("write closed-window case");

    let start = Instant::now();
    let output = run_da(
        dir.path(),
        &[
            "run",
            "--output",
            "json",
            "--no-bundle",
            case_path.to_str().unwrap(),
        ],
        &[],
    );
    let elapsed = start.elapsed();
    assert!(!output.status.success());
    let report = json_stdout(&output);
    let case_err = report["cases"][0]["error"]["message"]
        .as_str()
        .or_else(|| report["cases"][0]["error"].as_str())
        .or_else(|| {
            report["cases"][0]["steps"].as_array().and_then(|steps| {
                steps
                    .iter()
                    .rev()
                    .find_map(|s| s["error"]["message"].as_str())
            })
        })
        .unwrap_or_default();
    assert!(
        case_err.to_lowercase().contains("closed") && !case_err.to_lowercase().contains("uipi"),
        "expected closed-window error, not UIPI: {report}"
    );
    let click_step = &report["cases"][0]["steps"][3];
    assert_eq!(click_step["status"], "error", "{report}");
    assert!(
        click_step["duration_ms"].as_u64().unwrap_or(99_999) < 2_000,
        "closed-window desktop.click should fail fast without waiting for timeout_ms=15000: {click_step}"
    );
    assert!(
        elapsed.as_secs() < 12,
        "closed-window case took {elapsed:?}; expected fast termination under 12s"
    );
}

#[cfg(windows)]
#[test]
fn desktop_at_size_mismatch_element_from_point_and_press() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-at-onscreen");
    let exe = compile_fixture(dir.path());
    let title = format!("DA Onscreen Fixture {}", std::process::id());

    let started = json_stdout(&run_da(
        dir.path(),
        &["session", "start", "--output", "json"],
        &[],
    ));
    let session_id = started["session_id"].as_str().unwrap().to_string();

    let launch = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.launch",
            "--output",
            "json",
            "--with",
            &json!({
                "command": exe,
                "args": ["--title", title, "--onscreen"],
                "window_title_contains": title,
                "timeout_ms": 15000
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(launch["ok"].as_bool().unwrap_or(false), "{launch}");
    let win_w = launch["step"]["result"]["window"]["width"]
        .as_u64()
        .unwrap();
    let win_h = launch["step"]["result"]["window"]["height"]
        .as_u64()
        .unwrap();

    // 1. Mismatched `at.window` fails cleanly without clicking.
    let bad_size = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.click",
            "--output",
            "json",
            "--with",
            &json!({
                "at": { "x": 30, "y": 30, "window": { "width": 100, "height": 100 } }
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(!bad_size["ok"].as_bool().unwrap_or(true), "{bad_size}");
    assert!(
        bad_size["step"]["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("100x100"),
        "{bad_size}"
    );

    // 2. Read bounds of submitBtn and drawCanvas to get accurate window-relative hit points.
    let submit_read = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.read",
            "--output",
            "json",
            "--with",
            &json!({"automation_id": "submitBtn"}).to_string(),
        ],
        &[],
    ));
    let sb = &submit_read["step"]["result"]["bounds"];
    let sx = sb["x"].as_i64().unwrap() + sb["width"].as_i64().unwrap() / 2;
    let sy = sb["y"].as_i64().unwrap() + sb["height"].as_i64().unwrap() / 2;

    let snap = json_stdout(&run_da(
        dir.path(),
        &["session", "snapshot", &session_id, "--output", "json"],
        &[],
    ));
    let snap_elements = snap["snapshot"]["elements"]
        .as_array()
        .or_else(|| snap["snapshots"]["desktop"]["elements"].as_array())
        .unwrap_or_else(|| panic!("expected elements in snapshot: {snap}"));
    let panes: Vec<&Value> = snap_elements
        .iter()
        .filter(|e| e["role"].as_str() == Some("Pane") && e["name"].as_str() != Some("Actions"))
        .collect();
    let canvas_bounds = &panes
        .last()
        .unwrap_or_else(|| panic!("expected canvas Pane in snapshot: {snap}"))["bounds"];
    let cx = canvas_bounds["x"].as_i64().unwrap() + canvas_bounds["width"].as_i64().unwrap() / 2;
    let cy = canvas_bounds["y"].as_i64().unwrap() + canvas_bounds["height"].as_i64().unwrap() / 2;

    // 3. `at` click on Submit button -> ElementFromPoint synthesizes `automation_id: "submitBtn"`.
    let click_submit_at = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.click",
            "--output",
            "json",
            "--with",
            &json!({"at": {"x": sx, "y": sy}}).to_string(),
        ],
        &[],
    ));
    assert!(
        click_submit_at["ok"].as_bool().unwrap_or(false),
        "{click_submit_at}"
    );

    // 4. `at` click on unlabeled canvas Pane -> stays fragile coordinate with `at.window` pinned on export.
    let click_canvas_at = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.click",
            "--output",
            "json",
            "--with",
            &json!({"at": {"x": cx, "y": cy}}).to_string(),
        ],
        &[],
    ));
    assert!(
        click_canvas_at["ok"].as_bool().unwrap_or(false),
        "{click_canvas_at}"
    );

    // 5. `desktop.press` Ctrl+S shortcut under desktop lock.
    let press_res = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.press",
            "--output",
            "json",
            "--with",
            &json!({"key": "Ctrl+S"}).to_string(),
        ],
        &[],
    ));
    assert!(press_res["ok"].as_bool().unwrap_or(false), "{press_res}");
    let after_press = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.read",
            "--output",
            "json",
            "--with",
            &json!({"automation_id": "statusLabel"}).to_string(),
        ],
        &[],
    ));
    assert_eq!(after_press["step"]["result"]["name"], "Shortcut:Ctrl+S");

    // Export and verify Submit step became `automation_id: submitBtn` (no `at:`),
    // while canvas click kept `at:` with pinned `window: { width, height }`.
    let exported = json_stdout(&run_da(
        dir.path(),
        &["session", "export", &session_id, "--output", "json"],
        &[],
    ));
    assert!(exported["ok"].as_bool().unwrap_or(false), "{exported}");
    let yaml = exported["yaml"].as_str().unwrap();
    assert!(
        yaml.contains("automation_id: submitBtn"),
        "expected synthesized locator for submit button click:\n{yaml}"
    );
    assert!(
        yaml.contains(&format!("width: {win_w}")) && yaml.contains(&format!("height: {win_h}")),
        "expected pinned `at.window` size {win_w}x{win_h} on canvas click:\n{yaml}"
    );

    let _ = run_da(
        dir.path(),
        &["session", "stop", &session_id, "--output", "json"],
        &[],
    );
}

#[cfg(windows)]
#[test]
fn desktop_attach_marks_local_only_and_preserves_external_process() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-attach");
    let exe = compile_fixture(dir.path());
    let title = format!("DA Attach Fixture {}", std::process::id());

    let child = Command::new(&exe)
        .args(["--title", &title])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn external fixture");
    let pid = child.id();
    let mut guard = ProcessGuard(Some(child));

    let case_path = dir.path().join("attach.yaml");
    std::fs::write(
        &case_path,
        format!(
            "\
name: Attach external window
steps:
  - desktop.attach:
      pid: {pid}
      title_contains: \"{title}\"
      timeout_ms: 15000
  - desktop.read:
      automation_id: statusLabel
    expect:
      - path: \"$.name\"
        equals: Status:Ready
"
        ),
    )
    .expect("write attach case");

    let run = run_da(
        dir.path(),
        &[
            "run",
            "--output",
            "json",
            "--no-bundle",
            case_path.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        run.status.success(),
        "desktop.attach case failed:\n{}\n{}",
        stdout_str(&run),
        stderr_str(&run)
    );
    let report = json_stdout(&run);
    let case_res = &report["cases"][0];
    assert_eq!(case_res["local_only"], true, "{report}");
    assert!(
        case_res["local_only_reason"]
            .as_str()
            .unwrap_or_default()
            .contains("desktop.attach"),
        "{report}"
    );

    // Attached external process MUST NOT be killed by DesktopSession::close().
    let external = guard.0.as_mut().expect("child guard");
    assert!(
        external.try_wait().expect("try_wait").is_none(),
        "desktop.attach must never terminate the attached external process"
    );
}
