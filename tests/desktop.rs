#![cfg(feature = "desktop")]

//! Integration, schema, lint, session, export, and screenshot-masking tests
//! for the cross-platform `desktop.*` driver (`#84`, `#88`).

mod support;

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use support::TempDir;

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use std::path::PathBuf;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use std::process::Stdio;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use std::sync::{Mutex, OnceLock};
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use std::time::Instant;

/// Serializes interactive desktop tests inside this binary so two tests never
/// compete for focus or input simultaneously.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
static TEST_DESKTOP_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
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

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
struct ProcessGuard(Option<std::process::Child>);

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
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
    assert!(click.validate(&json!({"automation_id": "btn"})).is_ok());
    assert!(
        click
            .validate(&json!({"role": "Button", "name": "Submit"}))
            .is_ok()
    );
    assert!(click.validate(&json!({"at": {"x": 10, "y": 20}})).is_ok());
    assert!(
        click
            .validate(&json!({"at": {"x": 10, "y": 20, "window": {"width": 440, "height": 320}}}))
            .is_ok()
    );
    // Ref is rejected by static validator (session validator handles it).
    assert!(click.validate(&json!({"ref": "e1"})).is_err());
    // Cannot combine `at` with locator.
    assert!(
        click
            .validate(&json!({"at": {"x": 10, "y": 20}, "automation_id": "btn"}))
            .is_err()
    );
    // Negative coordinates and zero window bounds rejected.
    assert!(click.validate(&json!({"at": {"x": -1, "y": 5}})).is_err());
    assert!(
        click
            .validate(&json!({"at": {"x": 5, "y": 5, "window": {"width": 0, "height": 100}}}))
            .is_err()
    );
    // Unknown role rejected.
    assert!(click.validate(&json!({"role": "NotARealRole"})).is_err());
    // Empty locator rejected.
    assert!(click.validate(&json!({})).is_err());

    let type_step = registry.get("desktop.type").expect("desktop.type");
    assert!(
        type_step
            .validate(&json!({"automation_id": "user", "text": "alice"}))
            .is_ok()
    );
    assert!(
        type_step
            .validate(&json!({"text": "Username", "value": "alice"}))
            .is_ok(),
        "text locator + value text-to-type must validate"
    );
    assert!(
        type_step
            .validate(&json!({"at": {"x": 20, "y": 20}, "text": "alice"}))
            .is_ok()
    );
    assert!(
        type_step
            .validate(&json!({"automation_id": "user"}))
            .is_err(),
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
            press.validate(&json!({"key": good})).is_ok(),
            "`{good}` should validate cleanly"
        );
    }
    for bad in ["", "   ", "Ctrl+", "UnknownKey999", "Ctrl+Alt+"] {
        assert!(
            press.validate(&json!({"key": bad})).is_err(),
            "`{bad}` should be rejected"
        );
    }

    let attach = registry.get("desktop.attach").expect("desktop.attach");
    assert!(attach.validate(&json!({"pid": 1234})).is_ok());
    assert!(
        attach
            .validate(&json!({"executable": "notepad.exe"}))
            .is_ok()
    );
    assert!(
        attach
            .validate(&json!({"title_contains": "Notepad"}))
            .is_ok()
    );
    assert!(attach.validate(&json!({})).is_err());
    assert!(attach.validate(&json!({"pid": 0})).is_err());

    // Verify retryability classification matches read-only vs mutating steps.
    assert!(
        registry
            .get("desktop.wait")
            .unwrap()
            .check_retry(&json!({"automation_id": "x"}))
            .is_ok()
    );
    assert!(
        registry
            .get("desktop.read")
            .unwrap()
            .check_retry(&json!({"automation_id": "x"}))
            .is_ok()
    );
    assert!(
        registry
            .get("desktop.screenshot")
            .unwrap()
            .check_retry(&json!({"path": "a.png"}))
            .is_ok()
    );
    assert!(click.check_retry(&json!({"automation_id": "x"})).is_err());
    assert!(
        type_step
            .check_retry(&json!({"automation_id": "x", "text": "a"}))
            .is_err()
    );
    assert!(press.check_retry(&json!({"key": "Enter"})).is_err());
    assert!(
        registry
            .get("desktop.launch")
            .unwrap()
            .check_retry(&json!({"command": "x"}))
            .is_err()
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

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
#[test]
fn desktop_unsupported_os_stub_returns_environment_error() {
    let dir = TempDir::new("desktop-stub");
    let case_path = dir.path().join("stub.yaml");
    std::fs::write(
        &case_path,
        "\
name: Unsupported OS stub
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
        err.contains("Windows, Linux, and macOS"),
        "expected unsupported-OS Environment error, got: {report}"
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

#[cfg(windows)]
fn is_windows_pid_alive(pid: u32) -> bool {
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let tasklist = system_root.join("System32").join("tasklist.exe");
    let output = Command::new(if tasklist.is_file() {
        tasklist
    } else {
        PathBuf::from("tasklist")
    })
    .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
    .stdin(Stdio::null())
    .output()
    .expect("run tasklist");
    let pid_text = pid.to_string();
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('"') {
            return false;
        }
        let mut fields = trimmed.split("\",\"");
        let _image = fields.next();
        fields
            .next()
            .is_some_and(|f| f.trim_matches('"') == pid_text)
    })
}

#[cfg(windows)]
#[test]
fn desktop_session_stop_kills_owned_pid_and_refuses_recycled_or_tampered_pid() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-pid-reuse");
    let exe = compile_fixture(dir.path());

    // 1. Normal `da session stop` terminates an owned launched GUI process cleanly.
    let title1 = format!("DA Owned Stop {}", std::process::id());
    let s1 = json_stdout(&run_da(
        dir.path(),
        &["session", "start", "--output", "json"],
        &[],
    ));
    let id1 = s1["session_id"].as_str().expect("session_id").to_string();
    let launch1 = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &id1,
            "desktop.launch",
            "--output",
            "json",
            "--with",
            &json!({
                "command": exe,
                "args": ["--title", title1],
                "window_title_contains": title1,
                "timeout_ms": 15000
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(launch1["ok"].as_bool().unwrap_or(false), "{launch1}");
    let owned_pid1 = launch1["step"]["result"]["pid"].as_u64().expect("pid") as u32;
    assert!(is_windows_pid_alive(owned_pid1));

    let state1_path = dir
        .path()
        .join(".da")
        .join("sessions")
        .join(&id1)
        .join("state.json");
    let state1: Value =
        serde_json::from_str(&std::fs::read_to_string(&state1_path).expect("read state1"))
            .expect("parse state1");
    assert!(
        state1["handles"]["desktop"]["process_start_filetime"]
            .as_i64()
            .is_some_and(|ticks| ticks > 0),
        "persisted owned desktop handle must record positive process_start_filetime: {state1}"
    );

    let stop1 = json_stdout(&run_da(
        dir.path(),
        &["session", "stop", &id1, "--output", "json"],
        &[],
    ));
    assert!(stop1["ok"].as_bool().unwrap_or(false), "{stop1}");
    assert!(
        !is_windows_pid_alive(owned_pid1),
        "normal `da session stop` must terminate owned launched desktop process PID {owned_pid1}"
    );

    // Spawn an unrelated background process to stand in for a recycled PID holder.
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let ping_bin = system_root.join("System32").join("PING.EXE");
    let victim_child = Command::new(if ping_bin.is_file() {
        ping_bin
    } else {
        PathBuf::from("ping")
    })
    .args(["-n", "60", "127.0.0.1"])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("spawn unrelated dummy process");
    let victim_pid = victim_child.id();
    let mut victim_guard = ProcessGuard(Some(victim_child));

    // 2. Simulate PID reuse: launch owned GUI app, let it exit, then point `state.json`
    // `pid` at `victim_pid` while keeping the original `process_start_filetime`.
    let title2 = format!("DA Recycled PID {}", std::process::id());
    let s2 = json_stdout(&run_da(
        dir.path(),
        &["session", "start", "--output", "json"],
        &[],
    ));
    let id2 = s2["session_id"].as_str().expect("session_id").to_string();
    let launch2 = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &id2,
            "desktop.launch",
            "--output",
            "json",
            "--with",
            &json!({
                "command": exe,
                "args": ["--title", title2],
                "window_title_contains": title2,
                "timeout_ms": 15000
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(launch2["ok"].as_bool().unwrap_or(false), "{launch2}");

    // Close the owned fixture cleanly via its Close button so only the simulated recycled PID remains.
    let close_click = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &id2,
            "desktop.click",
            "--output",
            "json",
            "--with",
            &json!({"automation_id": "closeBtn"}).to_string(),
        ],
        &[],
    ));
    assert!(
        close_click["ok"].as_bool().unwrap_or(false),
        "{close_click}"
    );

    let state2_path = dir
        .path()
        .join(".da")
        .join("sessions")
        .join(&id2)
        .join("state.json");
    let mut state2: Value =
        serde_json::from_str(&std::fs::read_to_string(&state2_path).expect("read state2"))
            .expect("parse state2");
    state2["handles"]["desktop"]["pid"] = json!(victim_pid);
    std::fs::write(
        &state2_path,
        serde_json::to_string_pretty(&state2).expect("serialize state2"),
    )
    .expect("write simulated recycled-pid state2");

    // Verify reattach (`session snapshot`) also refuses the recycled PID cleanly without touching `victim_pid`.
    let reattach_attempt = run_da(
        dir.path(),
        &["session", "snapshot", &id2, "--output", "json"],
        &[],
    );
    assert!(
        !reattach_attempt.status.success(),
        "reattach must reject a recycled PID whose StartTime differs"
    );
    assert!(
        victim_guard
            .0
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none(),
        "reattach check must not disturb unrelated process"
    );

    // `da session stop` treats the mismatched StartTime as original process already exited -> succeeds without killing `victim_pid`!
    let stop2 = json_stdout(&run_da(
        dir.path(),
        &["session", "stop", &id2, "--output", "json"],
        &[],
    ));
    assert!(stop2["ok"].as_bool().unwrap_or(false), "{stop2}");
    assert!(
        victim_guard
            .0
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none(),
        "`da session stop` must NEVER kill an unrelated process holding a recycled PID"
    );

    // 3. Fail-closed tamper check: `owned_launch: true` with missing `process_start_filetime`
    // refuses termination with an error and leaves `victim_pid` untouched.
    let s3 = json_stdout(&run_da(
        dir.path(),
        &["session", "start", "--output", "json"],
        &[],
    ));
    let id3 = s3["session_id"].as_str().expect("session_id").to_string();
    let state3_path = dir
        .path()
        .join(".da")
        .join("sessions")
        .join(&id3)
        .join("state.json");
    let mut state3: Value =
        serde_json::from_str(&std::fs::read_to_string(&state3_path).expect("read state3"))
            .expect("parse state3");
    state3["handles"]["desktop"] = json!({
        "pid": victim_pid,
        "hwnd": 123456,
        "owned_launch": true
    });
    std::fs::write(
        &state3_path,
        serde_json::to_string_pretty(&state3).expect("serialize state3"),
    )
    .expect("write tampered state3");

    let stop3 = run_da(
        dir.path(),
        &["session", "stop", &id3, "--output", "json"],
        &[],
    );
    assert!(
        !stop3.status.success(),
        "tampered owned handle missing process_start_filetime must fail closed: {}",
        stdout_str(&stop3)
    );
    let stop3_json = json_stdout(&stop3);
    assert!(
        stop3_json.to_string().contains("process_start_filetime"),
        "expected missing process_start_filetime diagnostic: {stop3_json}"
    );
    assert!(
        victim_guard
            .0
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none(),
        "fail-closed stop must NEVER kill an unrelated process when identity metadata is missing"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn is_unix_pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn decode_png_rgba(path: &Path) -> (u32, u32, Vec<u8>) {
    let file = std::fs::File::open(path).expect("open png");
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::ALPHA | png::Transformations::STRIP_16,
    );
    let mut reader = decoder.read_info().expect("read png info");
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).expect("decode png frame");
    buf.truncate(info.buffer_size());
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((buf.len() / 3) * 4);
            for rgb in buf.as_chunks::<3>().0 {
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
            out
        }
        other => panic!("unexpected PNG color type in test screenshot: {other:?}"),
    };
    (info.width, info.height, rgba)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn assert_unix_mode_0600(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .expect("screenshot metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        mode,
        0o600,
        "expected screenshot `{}` permissions 0600, got {mode:04o}",
        path.display()
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn extract_case_error_message(report: &Value) -> String {
    report["cases"][0]["error"]["message"]
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
        .unwrap_or_default()
        .to_string()
}

// ============================================================================
// Linux AT-SPI2 + X11/XTEST Integration Tests
// ============================================================================

#[cfg(target_os = "linux")]
const LINUX_GTK_FIXTURE_PY: &str = r#"#!/usr/bin/env python3
import sys
import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk, Gdk, GLib

def set_a11y(widget, aid=None, name=None):
    acc = widget.get_accessible()
    if acc is not None:
        if aid is not None and hasattr(acc, "set_accessible_id"):
            acc.set_accessible_id(aid)
        if name is not None:
            acc.set_name(name)

title = "DA Linux Fixture"
secret = ""
idx = 1
while idx < len(sys.argv):
    if sys.argv[idx] == "--title" and idx + 1 < len(sys.argv):
        title = sys.argv[idx + 1]
        idx += 2
    elif sys.argv[idx] == "--secret" and idx + 1 < len(sys.argv):
        secret = sys.argv[idx + 1]
        idx += 2
    else:
        idx += 1

css = Gtk.CssProvider()
css.load_from_data(b"""
window { background-image: none; background-color: rgb(240, 240, 245); }
entry#userEntry { background-image: none; background-color: rgb(255, 180, 60); color: rgb(20, 20, 20); }
entry#pwEntry { background-image: none; background-color: rgb(0, 220, 220); color: rgb(0, 60, 60); }
""")
screen = Gdk.Screen.get_default()
if screen is not None:
    Gtk.StyleContext.add_provider_for_screen(
        screen, css, Gtk.STYLE_PROVIDER_PRIORITY_USER
    )

win = Gtk.Window(title=title)
win.set_default_size(440, 320)
win.set_size_request(440, 320)
win.set_resizable(False)
win.move(60, 60)
win.connect("destroy", Gtk.main_quit)

vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
vbox.set_margin_top(12)
vbox.set_margin_bottom(12)
vbox.set_margin_start(16)
vbox.set_margin_end(16)
win.add(vbox)

user_entry = Gtk.Entry()
user_entry.set_name("userEntry")
set_a11y(user_entry, aid="usernameBox", name="Username")
vbox.pack_start(user_entry, False, False, 0)

pw_entry = Gtk.Entry()
pw_entry.set_name("pwEntry")
pw_entry.set_visibility(False)
set_a11y(pw_entry, aid="passwordBox", name="Password")
vbox.pack_start(pw_entry, False, False, 0)

btn_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
vbox.pack_start(btn_row, False, False, 0)

status_label = Gtk.Label.new("Status:Ready")
set_a11y(status_label, aid="statusLabel")

def on_submit(_btn):
    u = user_entry.get_text()
    p = pw_entry.get_text()
    status_label.set_text(f"Submitted:{u}|pwlen={len(p)}")

submit_btn = Gtk.Button.new_with_label("Submit")
set_a11y(submit_btn, aid="submitBtn", name="Submit")
submit_btn.connect("clicked", on_submit)
btn_row.pack_start(submit_btn, False, False, 0)

def on_close(_btn):
    GLib.timeout_add(30, lambda: (win.destroy(), False)[1])

close_btn = Gtk.Button.new_with_label("CloseWindow")
set_a11y(close_btn, aid="closeBtn", name="CloseWindow")
close_btn.connect("clicked", on_close)
btn_row.pack_start(close_btn, False, False, 0)

vbox.pack_start(status_label, False, False, 0)

if secret:
    secret_label = Gtk.Label.new(f"SecretEcho:{secret}")
    set_a11y(secret_label, aid="secretEcho")
    vbox.pack_start(secret_label, False, False, 0)

canvas = Gtk.DrawingArea()
canvas.set_size_request(200, 64)
canvas.add_events(Gdk.EventMask.BUTTON_PRESS_MASK)
set_a11y(canvas, aid="drawCanvas")

def on_draw(_widget, cr):
    cr.set_source_rgb(0.85, 0.90, 0.98)
    cr.paint()
    return False

def on_canvas_press(_widget, event):
    status_label.set_text(f"Canvas:{int(event.x)},{int(event.y)}")
    return True

canvas.connect("draw", on_draw)
canvas.connect("button-press-event", on_canvas_press)
vbox.pack_start(canvas, False, False, 0)

def on_key_press(_widget, event):
    if (event.state & Gdk.ModifierType.CONTROL_MASK) and Gdk.keyval_to_lower(event.keyval) == Gdk.KEY_s:
        status_label.set_text("Shortcut:Ctrl+S")
        return True
    return False

win.connect("key-press-event", on_key_press)
win.show_all()
Gtk.main()
"#;

#[cfg(target_os = "linux")]
fn write_linux_fixture(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("gtk_fixture.py");
    std::fs::write(&script, LINUX_GTK_FIXTURE_PY).expect("write gtk_fixture.py");
    let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
    script
}

#[cfg(target_os = "linux")]
#[test]
fn desktop_linux_session_snapshot_drive_export_lint_and_replay() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-linux-session-replay");
    let script = write_linux_fixture(dir.path());
    let title = format!("DA Linux Session {}", std::process::id());

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
                "command": "python3",
                "args": [script, "--title", title],
                "window_title_contains": title,
                "timeout_ms": 20000
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(
        launch_step["ok"].as_bool().unwrap_or(false),
        "{launch_step}"
    );
    let launched_pid = launch_step["step"]["result"]["pid"]
        .as_u64()
        .expect("launched pid") as u32;

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

    let find_ref = |aid: &str, fallback_name: &str| -> String {
        elements
            .iter()
            .find(|el| {
                el["automation_id"].as_str() == Some(aid)
                    || (!fallback_name.is_empty() && el["name"].as_str() == Some(fallback_name))
            })
            .unwrap_or_else(|| panic!("missing element `{aid}` in snapshot: {snap}"))["ref"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let user_ref = find_ref("usernameBox", "Username");
    let pass_ref = find_ref("passwordBox", "Password");
    let submit_ref = find_ref("submitBtn", "Submit");
    let status_ref = find_ref("statusLabel", "Status:Ready");

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
            &json!({"ref": pass_ref, "text": "${env:DA_TEST_DESKTOP_SECRET}"}).to_string(),
        ],
        &[("DA_TEST_DESKTOP_SECRET", "s3cret-pass")],
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

    let case_path = dir.path().join("exported-linux-desktop.yaml");
    let export_default = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "export",
            &session_id,
            "--output",
            "json",
            "--name",
            "Linux desktop login replay",
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
    assert!(!yaml_text.contains("s3cret-pass"));
    assert!(yaml_text.contains("${env:DA_TEST_DESKTOP_SECRET}"));

    let stopped = json_stdout(&run_da(
        dir.path(),
        &["session", "stop", &session_id, "--output", "json"],
        &[],
    ));
    assert!(stopped["ok"].as_bool().unwrap_or(false), "{stopped}");
    assert!(
        !is_unix_pid_alive(launched_pid),
        "`da session stop` must terminate owned launched Linux process PID {launched_pid}"
    );

    let mut final_yaml = std::fs::read_to_string(&case_path).expect("read exported yaml");
    final_yaml
        .push_str("  - desktop.wait:\n      role: Text\n      text: Submitted:alice|pwlen=11\n");
    std::fs::write(&case_path, final_yaml).expect("update exported yaml");

    let lint = run_da(
        dir.path(),
        &["lint", "--output", "json", case_path.to_str().unwrap()],
        &[("DA_TEST_DESKTOP_SECRET", "s3cret-pass")],
    );
    assert!(
        lint.status.success(),
        "da lint failed: {}",
        stderr_str(&lint)
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
        &[("DA_TEST_DESKTOP_SECRET", "s3cret-pass")],
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

#[cfg(target_os = "linux")]
#[test]
fn desktop_linux_failure_bundle_masks_inputs_and_redacts_secrets() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-linux-mask");
    let script = write_linux_fixture(dir.path());
    let title = format!("DA Linux Mask {}", std::process::id());
    let secret_value = "LINUX-TOP-SECRET-8841";
    let case_path = dir.path().join("mask-failure.yaml");
    let masked_path = dir.path().join("shot-masked.png");
    let unmasked_path = dir.path().join("shot-unmasked.png");

    std::fs::write(
        &case_path,
        format!(
            "\
name: Linux masked screenshot and redacted tree
steps:
  - desktop.launch:
      command: python3
      args:
        - \"{}\"
        - --title
        - \"{title}\"
        - --secret
        - \"${{env:DA_TEST_DESKTOP_SECRET}}\"
      window_title_contains: \"{title}\"
      timeout_ms: 20000
  - desktop.type:
      role: Edit
      name: Password
      text: \"${{env:DA_TEST_DESKTOP_SECRET}}\"
  - desktop.read:
      role: Edit
      name: Password
    save:
      pw_bounds: $.bounds
  - desktop.screenshot:
      path: shot-masked.png
  - desktop.screenshot:
      path: shot-unmasked.png
      mask_inputs: false
  - desktop.wait:
      role: Text
      text: NEVER-APPEARS
      timeout_ms: 250
",
            script.display()
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
        &[("DA_TEST_DESKTOP_SECRET", secret_value)],
    );
    assert!(!output.status.success(), "expected failing desktop.wait");
    let report = json_stdout(&output);
    assert!(
        !report.to_string().contains(secret_value),
        "stdout JSON report leaked secret: {report}"
    );
    for idx in 0..5 {
        assert_eq!(
            report["cases"][0]["steps"][idx]["status"], "passed",
            "expected step {idx} to pass before desktop.wait fails: {report}"
        );
    }
    assert_eq!(
        report["cases"][0]["steps"][5]["status"], "error",
        "expected desktop.wait (step 5) to timeout/error: {report}"
    );

    assert_unix_mode_0600(&masked_path);
    assert_unix_mode_0600(&unmasked_path);

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

    let (uw, uh, unmasked) = decode_png_rgba(&unmasked_path);
    let (mw, mh, masked) = decode_png_rgba(&masked_path);
    let (fw, fh, fail_png) = decode_png_rgba(&bundle_png);
    assert_eq!((uw, uh), (mw, mh));
    assert_eq!((uw, uh), (fw, fh));

    let sample_center = |w: u32, h: u32, buf: &[u8]| -> [u8; 4] {
        let cx = (bx + bw / 2).min(w - 1);
        let cy = (by + bh / 2).min(h - 1);
        let idx = ((cy * w + cx) * 4) as usize;
        [buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3]]
    };

    assert_ne!(
        &sample_center(uw, uh, &unmasked)[..3],
        &[0, 0, 0],
        "unmasked password input center should not be solid black"
    );
    assert_eq!(
        sample_center(mw, mh, &masked),
        [0, 0, 0, 255],
        "masked screenshot must paint Edit/Password center solid black"
    );
    assert_eq!(
        sample_center(fw, fh, &fail_png),
        [0, 0, 0, 255],
        "failure bundle screenshot must paint Edit/Password center solid black"
    );

    let tree_text = std::fs::read_to_string(&bundle_tree).expect("read uia-tree.json");
    assert!(
        !tree_text.contains(secret_value),
        "uia-tree.json leaked secret value: {tree_text}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn desktop_linux_at_press_attach_closed_window_and_pure_wayland_boundary() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-linux-wayland-and-at");
    let script = write_linux_fixture(dir.path());
    let title = format!("DA Linux Onscreen {}", std::process::id());

    // 1. Test `at:` canvas + `desktop.press` (`Ctrl+S`) + closed-window fast error on X11/Xvfb.
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
                "command": "python3",
                "args": [script, "--title", title],
                "window_title_contains": title,
                "timeout_ms": 20000
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
                "at": { "x": 30, "y": 30, "window": { "width": win_w + 50, "height": win_h } }
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(!bad_size["ok"].as_bool().unwrap_or(true), "{bad_size}");

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

    let wait_shortcut = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.wait",
            "--output",
            "json",
            "--with",
            &json!({
                "role": "Text",
                "text": "Shortcut:Ctrl+S",
                "timeout_ms": 5000
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(
        wait_shortcut["ok"].as_bool().unwrap_or(false),
        "{wait_shortcut}"
    );

    // Click CloseWindow, then verify subsequent session step fails fast (< 5s, not 15s)
    // with an actionable closed-window `environment` error, AND verify `da run` handles
    // `desktop.wait` (`state: gone`, `window_closed: true`) followed by fast closed-window exit.
    let close_click = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.click",
            "--output",
            "json",
            "--with",
            &json!({"role": "Button", "name": "CloseWindow"}).to_string(),
        ],
        &[],
    ));
    assert!(
        close_click["ok"].as_bool().unwrap_or(false),
        "{close_click}"
    );

    let fast_start = Instant::now();
    let after_close = run_da(
        dir.path(),
        &[
            "session",
            "step",
            &session_id,
            "desktop.click",
            "--output",
            "json",
            "--with",
            &json!({"role": "Button", "name": "Submit", "timeout_ms": 15000}).to_string(),
        ],
        &[],
    );
    assert_eq!(
        after_close.status.code(),
        Some(4),
        "closed-window desktop.click must exit 4 (Environment): stdout={}\nstderr={}",
        stdout_str(&after_close),
        stderr_str(&after_close)
    );
    let after_close_json = json_stdout(&after_close);
    let after_close_err = if after_close_json["error"].is_object() {
        &after_close_json["error"]
    } else {
        &after_close_json["step"]["error"]
    };
    assert_eq!(
        after_close_err["kind"], "environment",
        "expected environment error in error or step.error, got: {after_close_json}"
    );
    assert!(
        after_close_err["message"]
            .as_str()
            .unwrap_or("")
            .contains("desktop window closed"),
        "expected closed-window message, got: {after_close_json}"
    );
    assert!(
        fast_start.elapsed().as_secs() < 12,
        "closed-window desktop.click on Linux session must fail fast, took {:?}",
        fast_start.elapsed()
    );

    let _ = run_da(
        dir.path(),
        &["session", "stop", &session_id, "--output", "json"],
        &[],
    );

    let run_close_title = format!("DA Linux RunClose {}", std::process::id());
    let run_close_case = dir.path().join("closed-window-linux.yaml");
    std::fs::write(
        &run_close_case,
        format!(
            "\
name: Linux closed window wait gone and fast failure
steps:
  - desktop.launch:
      command: python3
      args:
        - \"{}\"
        - --title
        - \"{run_close_title}\"
      window_title_contains: \"{run_close_title}\"
      timeout_ms: 20000
  - desktop.click:
      role: Button
      name: CloseWindow
  - desktop.wait:
      role: Button
      name: Submit
      state: gone
      timeout_ms: 5000
  - desktop.click:
      role: Button
      name: Submit
      timeout_ms: 15000
",
            script.display()
        ),
    )
    .expect("write linux closed-window case");

    let run_close_start = Instant::now();
    let run_close_out = run_da(
        dir.path(),
        &[
            "run",
            "--output",
            "json",
            "--no-bundle",
            run_close_case.to_str().unwrap(),
        ],
        &[],
    );
    let run_close_elapsed = run_close_start.elapsed();
    assert!(!run_close_out.status.success());
    let run_close_report = json_stdout(&run_close_out);
    assert_eq!(
        run_close_report["cases"][0]["steps"][2]["status"], "passed",
        "desktop.wait state: gone should pass when window closes: {run_close_report}"
    );
    let run_close_err = extract_case_error_message(&run_close_report);
    assert!(
        run_close_err.to_lowercase().contains("closed")
            || run_close_err.to_lowercase().contains("exited"),
        "expected closed-window error, got: {run_close_report}"
    );
    assert!(
        run_close_elapsed.as_secs() < 12,
        "closed-window click in da run should fail fast without waiting 15s, took {run_close_elapsed:?}"
    );

    // 2. Pure-Wayland (`DISPLAY` unset) boundary + `desktop.attach` `local_only`:
    // Semantic AT-SPI2 attach/type/click/wait SUCCEED with `DISPLAY` unset, while X11
    // `desktop.screenshot` returns an actionable `Environment` error (exit code 4).
    let ext_title = format!("DA Linux WaylandSim {}", std::process::id());
    let ext_child = Command::new("python3")
        .args([script.to_str().unwrap(), "--title", &ext_title])
        .env("GTK_MODULES", "gail:atk-bridge")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn external GTK3 fixture");
    let ext_pid = ext_child.id();
    let mut ext_guard = ProcessGuard(Some(ext_child));

    let semantic_wayland_case = dir.path().join("wayland-semantic.yaml");
    std::fs::write(
        &semantic_wayland_case,
        format!(
            "\
name: Semantic AT-SPI2 without X11 DISPLAY
steps:
  - desktop.attach:
      pid: {ext_pid}
      title_contains: \"{ext_title}\"
      timeout_ms: 20000
  - desktop.type:
      role: Edit
      name: Username
      text: wayland_user
  - desktop.click:
      role: Button
      name: Submit
  - desktop.wait:
      role: Text
      text: Submitted:wayland_user|pwlen=0
      timeout_ms: 5000
"
        ),
    )
    .expect("write wayland semantic case");

    let mut semantic_cmd = Command::new(da());
    semantic_cmd
        .current_dir(dir.path())
        .env_remove("DISPLAY")
        .env("WAYLAND_DISPLAY", "wayland-0")
        .args([
            "run",
            "--output",
            "json",
            "--no-bundle",
            semantic_wayland_case.to_str().unwrap(),
        ]);
    let semantic_out = semantic_cmd.output().expect("run semantic wayland case");
    assert!(
        semantic_out.status.success(),
        "semantic AT-SPI2 steps must succeed without X11 DISPLAY:\n{}\n{}",
        stdout_str(&semantic_out),
        stderr_str(&semantic_out)
    );
    let semantic_report = json_stdout(&semantic_out);
    assert_eq!(semantic_report["cases"][0]["local_only"], true);

    let wayland_shot_case = dir.path().join("wayland-screenshot.yaml");
    std::fs::write(
        &wayland_shot_case,
        format!(
            "\
name: Pure Wayland screenshot environment error
steps:
  - desktop.attach:
      pid: {ext_pid}
      title_contains: \"{ext_title}\"
      timeout_ms: 10000
  - desktop.screenshot:
      path: wayland-fail.png
"
        ),
    )
    .expect("write wayland screenshot case");

    let mut shot_cmd = Command::new(da());
    shot_cmd
        .current_dir(dir.path())
        .env_remove("DISPLAY")
        .env("WAYLAND_DISPLAY", "wayland-0")
        .args([
            "run",
            "--output",
            "json",
            "--no-bundle",
            wayland_shot_case.to_str().unwrap(),
        ]);
    let shot_out = shot_cmd.output().expect("run wayland screenshot case");
    assert_eq!(
        shot_out.status.code(),
        Some(4),
        "expected exit code 4 (Environment) on pure Wayland screenshot:\n{}",
        stdout_str(&shot_out)
    );
    let shot_report = json_stdout(&shot_out);
    let shot_err = extract_case_error_message(&shot_report);
    assert!(
        shot_err.contains("DISPLAY") && shot_err.contains("Wayland"),
        "expected actionable Wayland/DISPLAY diagnostic, got: {shot_err}"
    );

    assert!(
        ext_guard.0.as_mut().unwrap().try_wait().unwrap().is_none(),
        "desktop.attach must never kill the external GTK3 process"
    );
}

// ============================================================================
// macOS AXUIElement + Enigo + screencapture Integration Tests
// ============================================================================

#[cfg(target_os = "macos")]
const MACOS_APPKIT_FIXTURE_SWIFT: &str = r#"
import Cocoa

final class CanvasView: NSView {
    var onClick: ((Int, Int) -> Void)?
    override var isFlipped: Bool { true }
    override func draw(_ dirtyRect: NSRect) {
        NSColor(calibratedRed: 0.85, green: 0.90, blue: 0.98, alpha: 1.0).setFill()
        dirtyRect.fill()
    }
    override func mouseDown(with event: NSEvent) {
        let pt = convert(event.locationInWindow, from: nil)
        onClick?(Int(pt.x.rounded()), Int(pt.y.rounded()))
    }
}

final class KeyWindow: NSWindow {
    var onCtrlS: (() -> Void)?
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
    override func sendEvent(_ event: NSEvent) {
        if event.type == .keyDown {
            let chars = event.charactersIgnoringModifiers?.lowercased() ?? ""
            if (event.modifierFlags.contains(.control) || event.modifierFlags.contains(.command)) && chars == "s" {
                onCtrlS?()
                return
            }
        }
        super.sendEvent(event)
    }
}

final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    let titleText: String
    let secretText: String
    var window: KeyWindow!
    var usernameField: NSTextField!
    var passwordField: NSSecureTextField!
    var statusLabel: NSTextField!

    init(titleText: String, secretText: String) {
        self.titleText = titleText
        self.secretText = secretText
    }

    @objc func onSubmit(_ sender: Any?) {
        let u = usernameField.stringValue
        let p = passwordField.stringValue
        statusLabel.stringValue = "Submitted:\(u)|pwlen=\(p.count)"
    }

    @objc func onClose(_ sender: Any?) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.04) {
            NSApp.terminate(nil)
        }
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.appearance = NSAppearance(named: .aqua)
        let rect = NSRect(x: 80, y: 120, width: 440, height: 320)
        window = KeyWindow(
            contentRect: rect,
            styleMask: [.titled, .closable],
            backing: .buffered,
            defer: false
        )
        window.title = titleText
        window.backgroundColor = NSColor(calibratedRed: 0.94, green: 0.94, blue: 0.96, alpha: 1.0)
        let content = FlippedView(frame: NSRect(x: 0, y: 0, width: 440, height: 320))
        window.contentView = content

        usernameField = NSTextField(frame: NSRect(x: 20, y: 20, width: 220, height: 26))
        usernameField.setAccessibilityIdentifier("usernameBox")
        usernameField.setAccessibilityLabel("Username")
        usernameField.drawsBackground = true
        usernameField.backgroundColor = NSColor(calibratedRed: 1.0, green: 0.72, blue: 0.25, alpha: 1.0)
        content.addSubview(usernameField)

        passwordField = NSSecureTextField(frame: NSRect(x: 20, y: 56, width: 220, height: 26))
        passwordField.setAccessibilityIdentifier("passwordBox")
        passwordField.setAccessibilityLabel("Password")
        passwordField.drawsBackground = true
        passwordField.backgroundColor = NSColor(calibratedRed: 0.0, green: 0.86, blue: 0.86, alpha: 1.0)
        content.addSubview(passwordField)

        let submitBtn = NSButton(title: "Submit", target: self, action: #selector(onSubmit(_:)))
        submitBtn.frame = NSRect(x: 20, y: 94, width: 110, height: 30)
        submitBtn.setAccessibilityIdentifier("submitBtn")
        content.addSubview(submitBtn)

        let closeBtn = NSButton(title: "CloseWindow", target: self, action: #selector(onClose(_:)))
        closeBtn.frame = NSRect(x: 140, y: 94, width: 120, height: 30)
        closeBtn.setAccessibilityIdentifier("closeBtn")
        content.addSubview(closeBtn)

        statusLabel = NSTextField(labelWithString: "Status:Ready")
        statusLabel.frame = NSRect(x: 20, y: 136, width: 380, height: 24)
        statusLabel.setAccessibilityIdentifier("statusLabel")
        content.addSubview(statusLabel)

        if !secretText.isEmpty {
            let secretEcho = NSTextField(labelWithString: "SecretEcho:\(secretText)")
            secretEcho.frame = NSRect(x: 20, y: 166, width: 380, height: 22)
            secretEcho.setAccessibilityIdentifier("secretEcho")
            content.addSubview(secretEcho)
        }

        let canvas = CanvasView(frame: NSRect(x: 20, y: 200, width: 200, height: 70))
        canvas.setAccessibilityElement(true)
        canvas.setAccessibilityRole(.group)
        canvas.setAccessibilityIdentifier("drawCanvas")
        canvas.onClick = { [weak self] x, y in
            self?.statusLabel.stringValue = "Canvas:\(x),\(y)"
        }
        content.addSubview(canvas)

        window.onCtrlS = { [weak self] in
            self?.statusLabel.stringValue = "Shortcut:Ctrl+S"
        }

        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}

var titleArg = "DA macOS Fixture"
var secretArg = ""
var argIdx = 1
let cliArgs = CommandLine.arguments
while argIdx < cliArgs.count {
    if cliArgs[argIdx] == "--title", argIdx + 1 < cliArgs.count {
        titleArg = cliArgs[argIdx + 1]
        argIdx += 2
    } else if cliArgs[argIdx] == "--secret", argIdx + 1 < cliArgs.count {
        secretArg = cliArgs[argIdx + 1]
        argIdx += 2
    } else {
        argIdx += 1
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let delegate = AppDelegate(titleText: titleArg, secretText: secretArg)
app.delegate = delegate
app.run()
"#;

#[cfg(target_os = "macos")]
fn compile_macos_fixture(dir: &Path) -> PathBuf {
    let swift_path = dir.join("fixture.swift");
    let bin_path = dir.join("DesktopFixtureMac");
    std::fs::write(&swift_path, MACOS_APPKIT_FIXTURE_SWIFT).expect("write fixture.swift");

    let output = Command::new("swiftc")
        .args([
            "-swift-version",
            "5",
            swift_path.to_str().unwrap(),
            "-o",
            bin_path.to_str().unwrap(),
        ])
        .output()
        .expect("failed to invoke swiftc");
    assert!(
        output.status.success(),
        "swiftc failed:\n{}\n{}",
        stdout_str(&output),
        stderr_str(&output)
    );
    bin_path
}

#[cfg(target_os = "macos")]
#[test]
fn desktop_macos_session_snapshot_drive_export_lint_and_replay() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-macos-session-replay");
    let exe = compile_macos_fixture(dir.path());
    let title = format!("DA macOS Session {}", std::process::id());

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
                "timeout_ms": 20000
            })
            .to_string(),
        ],
        &[],
    ));
    assert!(
        launch_step["ok"].as_bool().unwrap_or(false),
        "{launch_step}"
    );
    let launched_pid = launch_step["step"]["result"]["pid"]
        .as_u64()
        .expect("launched pid") as u32;

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
            &json!({"ref": pass_ref, "text": "${env:DA_TEST_DESKTOP_SECRET}"}).to_string(),
        ],
        &[("DA_TEST_DESKTOP_SECRET", "s3cret-pass")],
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

    let case_path = dir.path().join("exported-macos-desktop.yaml");
    let export_default = json_stdout(&run_da(
        dir.path(),
        &[
            "session",
            "export",
            &session_id,
            "--output",
            "json",
            "--name",
            "macOS desktop login replay",
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
    assert!(!yaml_text.contains("s3cret-pass"));
    assert!(yaml_text.contains("automation_id: usernameBox"));
    assert!(yaml_text.contains("automation_id: submitBtn"));
    assert!(yaml_text.contains("${env:DA_TEST_DESKTOP_SECRET}"));

    let stopped = json_stdout(&run_da(
        dir.path(),
        &["session", "stop", &session_id, "--output", "json"],
        &[],
    ));
    assert!(stopped["ok"].as_bool().unwrap_or(false), "{stopped}");
    assert!(
        !is_unix_pid_alive(launched_pid),
        "`da session stop` must terminate owned launched macOS process PID {launched_pid}"
    );

    let mut final_yaml = std::fs::read_to_string(&case_path).expect("read exported yaml");
    final_yaml.push_str(
        "  - desktop.wait:\n      automation_id: statusLabel\n      text: Submitted:alice|pwlen=11\n",
    );
    std::fs::write(&case_path, final_yaml).expect("update exported yaml");

    let lint = run_da(
        dir.path(),
        &["lint", "--output", "json", case_path.to_str().unwrap()],
        &[("DA_TEST_DESKTOP_SECRET", "s3cret-pass")],
    );
    assert!(
        lint.status.success(),
        "da lint failed: {}",
        stderr_str(&lint)
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
        &[("DA_TEST_DESKTOP_SECRET", "s3cret-pass")],
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

#[cfg(target_os = "macos")]
#[test]
fn desktop_macos_failure_bundle_masks_inputs_and_redacts_secrets() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-macos-mask");
    let exe = compile_macos_fixture(dir.path());
    let title = format!("DA macOS Mask {}", std::process::id());
    let secret_value = "MACOS-TOP-SECRET-7731";
    let case_path = dir.path().join("mask-failure.yaml");
    let masked_path = dir.path().join("shot-masked.png");
    let unmasked_path = dir.path().join("shot-unmasked.png");

    std::fs::write(
        &case_path,
        format!(
            "\
name: macOS masked screenshot and redacted tree
steps:
  - desktop.launch:
      command: \"{}\"
      args:
        - --title
        - \"{title}\"
        - --secret
        - \"${{env:DA_TEST_DESKTOP_SECRET}}\"
      window_title_contains: \"{title}\"
      timeout_ms: 20000
  - desktop.type:
      automation_id: passwordBox
      text: \"${{env:DA_TEST_DESKTOP_SECRET}}\"
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
",
            exe.display()
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
        &[("DA_TEST_DESKTOP_SECRET", secret_value)],
    );
    assert!(!output.status.success(), "expected failing desktop.wait");
    let report = json_stdout(&output);
    assert!(
        !report.to_string().contains(secret_value),
        "stdout JSON report leaked secret: {report}"
    );
    for idx in 0..5 {
        assert_eq!(
            report["cases"][0]["steps"][idx]["status"], "passed",
            "expected step {idx} to pass before desktop.wait fails: {report}"
        );
    }
    assert_eq!(
        report["cases"][0]["steps"][5]["status"], "error",
        "expected desktop.wait (step 5) to timeout/error: {report}"
    );

    assert_unix_mode_0600(&masked_path);
    assert_unix_mode_0600(&unmasked_path);

    let win_w = report["cases"][0]["steps"][0]["result"]["window"]["width"]
        .as_u64()
        .unwrap()
        .max(1) as f64;
    let win_h = report["cases"][0]["steps"][0]["result"]["window"]["height"]
        .as_u64()
        .unwrap()
        .max(1) as f64;
    let pw_bounds = report["cases"][0]["steps"][2]["result"]["bounds"].clone();
    let bx = pw_bounds["x"].as_i64().unwrap().max(0) as f64;
    let by = pw_bounds["y"].as_i64().unwrap().max(0) as f64;
    let bw = pw_bounds["width"].as_i64().unwrap().max(1) as f64;
    let bh = pw_bounds["height"].as_i64().unwrap().max(1) as f64;

    let bundle_dir = PathBuf::from(report["cases"][0]["bundle"].as_str().expect("bundle path"));
    let bundle_png = bundle_dir.join("desktop").join("screenshot.png");
    let bundle_tree = bundle_dir.join("desktop").join("uia-tree.json");
    assert!(bundle_png.is_file(), "missing {}", bundle_png.display());
    assert!(bundle_tree.is_file(), "missing {}", bundle_tree.display());

    let (uw, uh, unmasked) = decode_png_rgba(&unmasked_path);
    let (mw, mh, masked) = decode_png_rgba(&masked_path);
    let (fw, fh, fail_png) = decode_png_rgba(&bundle_png);
    assert_eq!((uw, uh), (mw, mh));
    assert_eq!((uw, uh), (fw, fh));

    let sample_center = |w: u32, h: u32, buf: &[u8]| -> [u8; 4] {
        let sx = f64::from(w) / win_w;
        let sy = f64::from(h) / win_h;
        let cx = ((bx + bw * 0.5) * sx).round().clamp(0.0, f64::from(w - 1)) as u32;
        let cy = ((by + bh * 0.5) * sy).round().clamp(0.0, f64::from(h - 1)) as u32;
        let idx = ((cy * w + cx) * 4) as usize;
        [buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3]]
    };

    assert_ne!(
        &sample_center(uw, uh, &unmasked)[..3],
        &[0, 0, 0],
        "unmasked password input center should not be solid black"
    );
    assert_eq!(
        sample_center(mw, mh, &masked),
        [0, 0, 0, 255],
        "masked screenshot must paint Edit/AXSecureTextField center solid black"
    );
    assert_eq!(
        sample_center(fw, fh, &fail_png),
        [0, 0, 0, 255],
        "failure bundle screenshot must paint Edit/AXSecureTextField center solid black"
    );

    let tree_text = std::fs::read_to_string(&bundle_tree).expect("read uia-tree.json");
    assert!(
        !tree_text.contains(secret_value),
        "uia-tree.json leaked secret value: {tree_text}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn desktop_macos_at_press_attach_and_closed_window() {
    let _guard = desktop_test_guard();
    let dir = TempDir::new("desktop-macos-at-attach");
    let exe = compile_macos_fixture(dir.path());
    let title = format!("DA macOS Onscreen {}", std::process::id());

    // 1. `at:` size mismatch, `desktop.press` (`Ctrl+S`), and closed-window fast error.
    let case_path = dir.path().join("closed-and-press.yaml");
    std::fs::write(
        &case_path,
        format!(
            "\
name: macOS press and closed window
steps:
  - desktop.launch:
      command: \"{}\"
      args:
        - --title
        - \"{title}\"
      window_title_contains: \"{title}\"
      timeout_ms: 20000
  - desktop.press:
      key: Ctrl+S
  - desktop.wait:
      automation_id: statusLabel
      text: Shortcut:Ctrl+S
      timeout_ms: 5000
  - desktop.click:
      automation_id: closeBtn
  - desktop.wait:
      automation_id: submitBtn
      state: gone
      timeout_ms: 5000
  - desktop.click:
      automation_id: submitBtn
      timeout_ms: 15000
",
            exe.display()
        ),
    )
    .expect("write closed-and-press case");

    let start = Instant::now();
    let out = run_da(
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
    assert!(!out.status.success());
    let report = json_stdout(&out);
    let err_msg = extract_case_error_message(&report);
    assert!(
        err_msg.to_lowercase().contains("closed") || err_msg.to_lowercase().contains("exited"),
        "expected closed-window error, got: {report}"
    );
    assert!(
        elapsed.as_secs() < 15,
        "closed-window click should fail fast without waiting 15s, took {elapsed:?}"
    );

    // 2. `desktop.attach` marks `local_only: true` and preserves the external process.
    let ext_title = format!("DA macOS Attach {}", std::process::id());
    let ext_child = Command::new(&exe)
        .args(["--title", &ext_title])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn external macOS fixture");
    let ext_pid = ext_child.id();
    let mut ext_guard = ProcessGuard(Some(ext_child));

    let attach_case = dir.path().join("attach-macos.yaml");
    std::fs::write(
        &attach_case,
        format!(
            "\
name: macOS attach external window
steps:
  - desktop.attach:
      pid: {ext_pid}
      title_contains: \"{ext_title}\"
      timeout_ms: 20000
  - desktop.read:
      automation_id: statusLabel
    expect:
      - path: \"$.name\"
        equals: Status:Ready
"
        ),
    )
    .expect("write macOS attach case");

    let attach_run = run_da(
        dir.path(),
        &[
            "run",
            "--output",
            "json",
            "--no-bundle",
            attach_case.to_str().unwrap(),
        ],
        &[],
    );
    assert!(
        attach_run.status.success(),
        "desktop.attach on macOS failed:\n{}\n{}",
        stdout_str(&attach_run),
        stderr_str(&attach_run)
    );
    let attach_report = json_stdout(&attach_run);
    assert_eq!(attach_report["cases"][0]["local_only"], true);
    assert!(
        ext_guard.0.as_mut().unwrap().try_wait().unwrap().is_none(),
        "desktop.attach must never terminate the attached external macOS process"
    );
}
