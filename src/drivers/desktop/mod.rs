//! Native desktop GUI automation (`desktop.*`) steps and session management.
//!
//! Implements Windows UI Automation (`#[cfg(windows)]`), Linux AT-SPI2 + X11
//! (`#[cfg(target_os = "linux")]`), and macOS `AXUIElement` + `CGEvent`
//! (`#[cfg(target_os = "macos")]`) behind a single platform-neutral step
//! schema, locator contract, and input-masked screenshot pipeline.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as backend;
#[cfg(target_os = "macos")]
use macos as backend;
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
use unsupported as backend;
#[cfg(windows)]
use windows as backend;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::registry::{
    Registry, SessionDefinition, StepDefinition, StepError, allow_retry, disallow_retry,
    no_lint_warnings,
};
use crate::core::session::{Evidence, Session, Sessions};

/// Registered session kind for native desktop automation.
pub const SESSION_KIND: &str = "desktop";

const DEFAULT_LAUNCH_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_ATTACH_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_STEP_TIMEOUT_MS: u64 = 5_000;

fn default_launch_timeout_ms() -> u64 {
    DEFAULT_LAUNCH_TIMEOUT_MS
}

fn default_attach_timeout_ms() -> u64 {
    DEFAULT_ATTACH_TIMEOUT_MS
}

fn default_step_timeout_ms() -> u64 {
    DEFAULT_STEP_TIMEOUT_MS
}

fn default_true() -> bool {
    true
}

/// Canonical accessibility control role supported across `desktop.*` steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DesktopRole {
    /// Push button control.
    Button,
    /// Calendar date picker control.
    Calendar,
    /// Check box control.
    CheckBox,
    /// Drop-down or editable combo box.
    ComboBox,
    /// Editable text input or password field.
    Edit,
    /// Clickable hyperlink control.
    Hyperlink,
    /// Visual image element.
    Image,
    /// Item inside a list control.
    ListItem,
    /// List container control.
    List,
    /// Pop-up menu container.
    Menu,
    /// Top-level window menu bar.
    MenuBar,
    /// Selectable item inside a menu.
    MenuItem,
    /// Progress bar indicator.
    ProgressBar,
    /// Radio option button.
    RadioButton,
    /// Scroll bar track or thumb container.
    ScrollBar,
    /// Numeric range slider.
    Slider,
    /// Numeric spinner control.
    Spinner,
    /// Window status bar.
    StatusBar,
    /// Tab strip container.
    Tab,
    /// Selectable tab page header.
    TabItem,
    /// Static text label.
    Text,
    /// Toolbar strip container.
    ToolBar,
    /// Hover tooltip control.
    ToolTip,
    /// Hierarchical tree container.
    Tree,
    /// Node item inside a tree control.
    TreeItem,
    /// Custom application-defined control.
    Custom,
    /// Logical group box container.
    Group,
    /// Draggable scrollbar or slider thumb.
    Thumb,
    /// Tabular data grid container.
    DataGrid,
    /// Row or cell item inside a data grid.
    DataItem,
    /// Rich document surface.
    Document,
    /// Button paired with a secondary drop-down trigger.
    SplitButton,
    /// Top-level or child application window.
    Window,
    /// Generic container or drawing canvas pane.
    Pane,
    /// Column or row header container.
    Header,
    /// Individual header column button.
    HeaderItem,
    /// Structured table container.
    Table,
    /// Window caption bar.
    TitleBar,
    /// Visual divider line.
    Separator,
    /// Semantic zoom host container.
    SemanticZoom,
    /// Application command bar.
    AppBar,
}

const ROLE_TABLE: [(&str, DesktopRole); 41] = [
    ("button", DesktopRole::Button),
    ("calendar", DesktopRole::Calendar),
    ("checkbox", DesktopRole::CheckBox),
    ("combobox", DesktopRole::ComboBox),
    ("edit", DesktopRole::Edit),
    ("hyperlink", DesktopRole::Hyperlink),
    ("image", DesktopRole::Image),
    ("listitem", DesktopRole::ListItem),
    ("list", DesktopRole::List),
    ("menu", DesktopRole::Menu),
    ("menubar", DesktopRole::MenuBar),
    ("menuitem", DesktopRole::MenuItem),
    ("progressbar", DesktopRole::ProgressBar),
    ("radiobutton", DesktopRole::RadioButton),
    ("scrollbar", DesktopRole::ScrollBar),
    ("slider", DesktopRole::Slider),
    ("spinner", DesktopRole::Spinner),
    ("statusbar", DesktopRole::StatusBar),
    ("tab", DesktopRole::Tab),
    ("tabitem", DesktopRole::TabItem),
    ("text", DesktopRole::Text),
    ("toolbar", DesktopRole::ToolBar),
    ("tooltip", DesktopRole::ToolTip),
    ("tree", DesktopRole::Tree),
    ("treeitem", DesktopRole::TreeItem),
    ("custom", DesktopRole::Custom),
    ("group", DesktopRole::Group),
    ("thumb", DesktopRole::Thumb),
    ("datagrid", DesktopRole::DataGrid),
    ("dataitem", DesktopRole::DataItem),
    ("document", DesktopRole::Document),
    ("splitbutton", DesktopRole::SplitButton),
    ("window", DesktopRole::Window),
    ("pane", DesktopRole::Pane),
    ("header", DesktopRole::Header),
    ("headeritem", DesktopRole::HeaderItem),
    ("table", DesktopRole::Table),
    ("titlebar", DesktopRole::TitleBar),
    ("separator", DesktopRole::Separator),
    ("semanticzoom", DesktopRole::SemanticZoom),
    ("appbar", DesktopRole::AppBar),
];

impl DesktopRole {
    /// Parses a UIA control type name case-insensitively.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let key = raw.trim().to_ascii_lowercase();
        for (name, role) in ROLE_TABLE {
            if name == key {
                return Ok(role);
            }
        }
        Err(format!(
            "invalid arguments: unknown desktop `role` `{raw}`; expected a UIA control type such as `Button`, `Edit`, `CheckBox`, `RadioButton`, `ComboBox`, `ListItem`, `TabItem`, `MenuItem`, `Text`, `Pane`, or `Window`"
        ))
    }

    /// Canonical PascalCase role name matching UIA ControlType names.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Button => "Button",
            Self::Calendar => "Calendar",
            Self::CheckBox => "CheckBox",
            Self::ComboBox => "ComboBox",
            Self::Edit => "Edit",
            Self::Hyperlink => "Hyperlink",
            Self::Image => "Image",
            Self::ListItem => "ListItem",
            Self::List => "List",
            Self::Menu => "Menu",
            Self::MenuBar => "MenuBar",
            Self::MenuItem => "MenuItem",
            Self::ProgressBar => "ProgressBar",
            Self::RadioButton => "RadioButton",
            Self::ScrollBar => "ScrollBar",
            Self::Slider => "Slider",
            Self::Spinner => "Spinner",
            Self::StatusBar => "StatusBar",
            Self::Tab => "Tab",
            Self::TabItem => "TabItem",
            Self::Text => "Text",
            Self::ToolBar => "ToolBar",
            Self::ToolTip => "ToolTip",
            Self::Tree => "Tree",
            Self::TreeItem => "TreeItem",
            Self::Custom => "Custom",
            Self::Group => "Group",
            Self::Thumb => "Thumb",
            Self::DataGrid => "DataGrid",
            Self::DataItem => "DataItem",
            Self::Document => "Document",
            Self::SplitButton => "SplitButton",
            Self::Window => "Window",
            Self::Pane => "Pane",
            Self::Header => "Header",
            Self::HeaderItem => "HeaderItem",
            Self::Table => "Table",
            Self::TitleBar => "TitleBar",
            Self::Separator => "Separator",
            Self::SemanticZoom => "SemanticZoom",
            Self::AppBar => "AppBar",
        }
    }

    /// Returns true when the role represents an interactive input or action control.
    pub fn is_interactive(self) -> bool {
        matches!(
            self,
            Self::Button
                | Self::CheckBox
                | Self::RadioButton
                | Self::ComboBox
                | Self::Edit
                | Self::Hyperlink
                | Self::ListItem
                | Self::MenuItem
                | Self::TabItem
                | Self::TreeItem
                | Self::SplitButton
                | Self::Slider
                | Self::Spinner
        )
    }

    /// Returns true when a snapshot should assign a ref to this control even if non-interactive.
    pub fn is_readable_leaf_role(self) -> bool {
        matches!(
            self,
            Self::Text
                | Self::StatusBar
                | Self::Document
                | Self::DataItem
                | Self::Pane
                | Self::Custom
        )
    }
}

/// Recorded outer window dimensions in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct WindowSize {
    /// Window outer width in pixels.
    #[schemars(extend("minimum" = 1))]
    pub width: u32,
    /// Window outer height in pixels.
    #[schemars(extend("minimum" = 1))]
    pub height: u32,
}

/// Window-relative coordinates for custom-drawn surfaces lacking UIA elements.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct DesktopPoint {
    /// Horizontal pixel offset from the target window's left edge.
    #[schemars(extend("minimum" = 0))]
    pub x: i32,
    /// Vertical pixel offset from the target window's top edge.
    #[schemars(extend("minimum" = 0))]
    pub y: i32,
    /// Recorded target window size; replay refuses if the live window differs.
    #[serde(default)]
    pub window: Option<WindowSize>,
}

/// Scope container locator for narrowing descendant control matches.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(inline)]
pub struct WithinArgs {
    /// UIA AutomationId of the container control.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub automation_id: Option<String>,
    /// UIA ControlType role name of the container control.
    #[serde(default)]
    pub role: Option<String>,
    /// Exact UIA Name of the container control.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub name: Option<String>,
    /// Visible text substring inside the container control's name or value.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub text: Option<String>,
    /// Win32/framework class name of the container control.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub class_name: Option<String>,
    /// Zero-based match index if multiple containers match.
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    pub nth: Option<usize>,
}

/// Validated element filter shared across `within` scopes and leaf targets.
#[derive(Debug, Clone)]
pub struct LocatorFilter {
    /// Optional UIA AutomationId.
    pub automation_id: Option<String>,
    /// Optional canonical UIA role.
    pub role: Option<DesktopRole>,
    /// Optional exact UIA Name.
    pub name: Option<String>,
    /// Optional case-insensitive text substring.
    pub text: Option<String>,
    /// Optional UIA ClassName.
    pub class_name: Option<String>,
    /// Zero-based match index.
    pub nth: usize,
}

/// Full validated target locator with optional container scope.
#[derive(Debug, Clone)]
pub struct DesktopLocator {
    /// Target element predicate.
    pub filter: LocatorFilter,
    /// Optional container scope predicate.
    pub within: Option<LocatorFilter>,
}

/// Arguments of `desktop.launch`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    /// Executable path or command name to launch.
    #[schemars(extend("minLength" = 1))]
    pub command: String,
    /// Command-line arguments passed to the application.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory; relative paths resolve against the case directory.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables merged into the launched process environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Optional case-insensitive substring matched against the main window title.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub window_title_contains: Option<String>,
    /// How long to wait for the application window in milliseconds. Defaults to 30000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_launch_timeout_ms")]
    pub timeout_ms: u64,
}

/// Arguments of `desktop.attach`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachArgs {
    /// Exact Windows process id to attach to.
    #[schemars(extend("minimum" = 1))]
    #[serde(default)]
    pub pid: Option<u32>,
    /// Case-insensitive process executable filename (for example `notepad.exe`).
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub executable: Option<String>,
    /// Case-insensitive substring matched against the top-level window title.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    pub title_contains: Option<String>,
    /// How long to wait for a matching window in milliseconds. Defaults to 10000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_attach_timeout_ms")]
    pub timeout_ms: u64,
}

/// Mouse click action kind for `desktop.click`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum ClickButton {
    /// Single left-button click.
    #[default]
    Left,
    /// Single right-button context click.
    Right,
    /// Left-button double click.
    Double,
}

/// Arguments of `desktop.click`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClickArgs {
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    automation_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    name: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    text: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    class_name: Option<String>,
    #[serde(default)]
    within: Option<WithinArgs>,
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<usize>,
    #[serde(default)]
    at: Option<DesktopPoint>,
    #[serde(default)]
    button: ClickButton,
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

/// Arguments of `desktop.type`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeArgs {
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    automation_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    name: Option<String>,
    /// Text to type into the target control, or the visible control text when `value` carries the text to type.
    #[serde(default)]
    text: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    class_name: Option<String>,
    #[serde(default)]
    within: Option<WithinArgs>,
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<usize>,
    /// Text to type when `text` identifies the control instead.
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    at: Option<DesktopPoint>,
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

/// Arguments of `desktop.press`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PressArgs {
    /// Key name or chord such as `Enter`, `Tab`, `Ctrl+S`, or `Ctrl+Shift+P`.
    #[schemars(extend("minLength" = 1))]
    key: String,
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

/// Target state waited on by `desktop.wait`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum WaitState {
    /// Element is present in the UIA tree.
    #[default]
    Present,
    /// Element (or its closing window) is absent from the UIA tree.
    Gone,
    /// Element is present and enabled.
    Enabled,
    /// Element is present and on-screen (`!IsOffscreen`).
    Visible,
}

/// Arguments of `desktop.wait`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    automation_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    name: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    text: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    class_name: Option<String>,
    #[serde(default)]
    within: Option<WithinArgs>,
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<usize>,
    #[serde(default)]
    state: WaitState,
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

/// Arguments of `desktop.read`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    automation_id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    name: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    text: Option<String>,
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    class_name: Option<String>,
    #[serde(default)]
    within: Option<WithinArgs>,
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<usize>,
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

/// Arguments of `desktop.screenshot`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotArgs {
    /// Destination PNG path; relative paths resolve against the case directory.
    #[schemars(extend("minLength" = 1))]
    path: String,
    /// Whether to mask all `Edit` and `IsPassword` rectangles in solid black. Defaults to true.
    #[serde(default = "default_true")]
    mask_inputs: bool,
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefClickArgs {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    button: ClickButton,
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefTypeArgs {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefWaitArgs {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    state: WaitState,
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefReadArgs {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default = "default_step_timeout_ms")]
    timeout_ms: u64,
}

/// Active desktop session stored inside [`Sessions`].
pub struct DesktopSession {
    backend: Option<backend::DesktopBackend>,
}

impl DesktopSession {
    fn empty() -> Self {
        Self { backend: None }
    }

    fn from_backend(inner: backend::DesktopBackend) -> Self {
        Self {
            backend: Some(inner),
        }
    }

    fn active_mut(&mut self) -> Result<&mut backend::DesktopBackend, StepError> {
        self.backend.as_mut().ok_or_else(|| {
            StepError::Environment(
                "no active desktop window; run `desktop.launch` or `desktop.attach` first"
                    .to_string(),
            )
        })
    }
}

impl Session for DesktopSession {
    fn close(&mut self) -> Result<(), String> {
        if let Some(mut inner) = self.backend.take() {
            inner.close()?;
        }
        Ok(())
    }

    fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        match &mut self.backend {
            Some(inner) => inner.evidence(),
            None => Ok(Vec::new()),
        }
    }

    fn detach(&mut self) -> Result<Option<Value>, String> {
        match &mut self.backend {
            Some(inner) => inner.detach(),
            None => Ok(None),
        }
    }

    fn local_only_reason(&self) -> Option<String> {
        self.backend
            .as_ref()
            .and_then(|inner| inner.local_only_reason())
    }

    fn snapshot(
        &mut self,
        scope_ref: Option<&str>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        let inner = self.backend.as_mut().ok_or_else(|| {
            "no active desktop window; run `desktop.launch` or `desktop.attach` before `snapshot`"
                .to_string()
        })?;
        inner.snapshot(scope_ref, max_depth, max_nodes)
    }

    fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        match &mut self.backend {
            Some(inner) => inner.snapshot_after_step(),
            None => Ok(None),
        }
    }

    fn take_step_metadata(&mut self) -> Option<Value> {
        self.backend
            .as_mut()
            .and_then(|inner| inner.take_step_metadata())
    }
}

/// Registers every `desktop.*` step definition, session lifecycle handler, and session validator.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    registry.register_session(SessionDefinition {
        kind: SESSION_KIND,
        reattach: reattach_session,
        dispose_handle: backend::DesktopBackend::dispose_handle,
    })?;

    for definition in [
        StepDefinition {
            name: "desktop.launch",
            description: "Launch a native desktop application and wait for its main window.",
            args_schema: schema_value::<LaunchArgs>()?,
            validate: validate_launch,
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_launch,
        },
        StepDefinition {
            name: "desktop.attach",
            description: "Attach to an already-running desktop window (marks session local_only).",
            args_schema: schema_value::<AttachArgs>()?,
            validate: validate_attach,
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_attach,
        },
        StepDefinition {
            name: "desktop.click",
            description: "Click a desktop control by locator, session ref, or window coordinates.",
            args_schema: schema_value::<ClickArgs>()?,
            validate: validate_click,
            check_retry: disallow_retry,
            lint_warnings: coordinate_lint_warnings,
            run: run_click,
        },
        StepDefinition {
            name: "desktop.type",
            description: "Type text into a desktop control using ValuePattern or keyboard input.",
            args_schema: schema_value::<TypeArgs>()?,
            validate: validate_type,
            check_retry: disallow_retry,
            lint_warnings: coordinate_lint_warnings,
            run: run_type,
        },
        StepDefinition {
            name: "desktop.press",
            description: "Send a key or modifier chord (e.g. Enter, Ctrl+S) to the target window.",
            args_schema: schema_value::<PressArgs>()?,
            validate: validate_press,
            check_retry: disallow_retry,
            lint_warnings: no_lint_warnings,
            run: run_press,
        },
        StepDefinition {
            name: "desktop.wait",
            description: "Wait until a desktop control reaches the requested UI state.",
            args_schema: schema_value::<WaitArgs>()?,
            validate: validate_wait,
            check_retry: allow_retry,
            lint_warnings: no_lint_warnings,
            run: run_wait,
        },
        StepDefinition {
            name: "desktop.read",
            description: "Read metadata, value, and window-relative bounds from a desktop control.",
            args_schema: schema_value::<ReadArgs>()?,
            validate: validate_read,
            check_retry: allow_retry,
            lint_warnings: no_lint_warnings,
            run: run_read,
        },
        StepDefinition {
            name: "desktop.screenshot",
            description: "Capture the desktop window as PNG while masking Edit and IsPassword inputs.",
            args_schema: schema_value::<ScreenshotArgs>()?,
            validate: validate_screenshot,
            check_retry: allow_retry,
            lint_warnings: no_lint_warnings,
            run: run_screenshot,
        },
    ] {
        registry.register(definition)?;
    }

    registry.register_session_validator("desktop.click", validate_click_session)?;
    registry.register_session_validator("desktop.type", validate_type_session)?;
    registry.register_session_validator("desktop.wait", validate_wait_session)?;
    registry.register_session_validator("desktop.read", validate_read_session)?;

    Ok(())
}

fn schema_value<T: JsonSchema>() -> Result<Value, String> {
    let settings = SchemaSettings::draft2020_12().with(|s| s.inline_subschemas = true);
    let schema = settings.into_generator().into_root_schema_for::<T>();
    serde_json::to_value(schema)
        .map_err(|err| format!("failed to serialize desktop step schema: {err}"))
}

fn reattach_session(handle: &Value) -> Result<Box<dyn Session>, String> {
    let backend = backend::DesktopBackend::reattach(handle)?;
    Ok(Box::new(DesktopSession::from_backend(backend)))
}

fn parse_args<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, StepError> {
    serde_json::from_value(args.clone())
        .map_err(|err| StepError::InvalidArguments(format!("invalid arguments: {err}")))
}

fn parse_validate<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|err| format!("invalid arguments: {err}"))
}

fn check_timeout(timeout_ms: u64) -> Result<(), String> {
    if timeout_ms == 0 {
        return Err("invalid arguments: `timeout_ms` must be at least 1".to_string());
    }
    Ok(())
}

fn check_non_empty(field: &str, value: &Option<String>) -> Result<(), String> {
    if let Some(text) = value
        && text.trim().is_empty()
    {
        return Err(format!("invalid arguments: `{field}` must not be empty"));
    }
    Ok(())
}

fn check_ref_id(ref_id: &str) -> Result<(), String> {
    let valid = ref_id.strip_prefix('e').is_some_and(|digits| {
        !digits.is_empty() && !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit())
    });
    if !valid {
        return Err(format!(
            "invalid arguments: `ref` `{ref_id}` must name a snapshot element like `e1`"
        ));
    }
    Ok(())
}

fn has_ref_field(args: &Value) -> bool {
    args.as_object().is_some_and(|map| map.contains_key("ref"))
}

// Shared by both leaf targets and `within` scopes so validation rules stay in one place.
#[allow(clippy::too_many_arguments)]
fn build_filter(
    prefix: &str,
    automation_id: &Option<String>,
    role: &Option<String>,
    name: &Option<String>,
    text: &Option<String>,
    class_name: &Option<String>,
    nth: Option<usize>,
) -> Result<LocatorFilter, String> {
    check_non_empty(&format!("{prefix}automation_id"), automation_id)?;
    check_non_empty(&format!("{prefix}role"), role)?;
    check_non_empty(&format!("{prefix}name"), name)?;
    check_non_empty(&format!("{prefix}text"), text)?;
    check_non_empty(&format!("{prefix}class_name"), class_name)?;

    let parsed_role = role.as_deref().map(DesktopRole::parse).transpose()?;

    if automation_id.is_none()
        && parsed_role.is_none()
        && name.is_none()
        && text.is_none()
        && class_name.is_none()
    {
        let label = if prefix.is_empty() {
            "locator".to_string()
        } else {
            prefix.trim_end_matches('.').to_string()
        };
        return Err(format!(
            "invalid arguments: `{label}` must specify at least one of `automation_id`, `role`, `name`, `text`, or `class_name`"
        ));
    }

    Ok(LocatorFilter {
        automation_id: automation_id.clone(),
        role: parsed_role,
        name: name.clone(),
        text: text.clone(),
        class_name: class_name.clone(),
        nth: nth.unwrap_or(0),
    })
}

// Threads the flat step schema fields into `build_filter` without an intermediate wrapper per step struct.
#[allow(clippy::too_many_arguments)]
fn build_locator(
    automation_id: &Option<String>,
    role: &Option<String>,
    name: &Option<String>,
    text: &Option<String>,
    class_name: &Option<String>,
    within: &Option<WithinArgs>,
    nth: Option<usize>,
) -> Result<DesktopLocator, String> {
    let scope = within
        .as_ref()
        .map(|w| {
            build_filter(
                "within.",
                &w.automation_id,
                &w.role,
                &w.name,
                &w.text,
                &w.class_name,
                w.nth,
            )
        })
        .transpose()?;

    let filter = build_filter("", automation_id, role, name, text, class_name, nth)?;
    Ok(DesktopLocator {
        filter,
        within: scope,
    })
}

fn check_at_point(at: &DesktopPoint) -> Result<(), String> {
    if at.x < 0 || at.y < 0 {
        return Err("invalid arguments: `at.x` and `at.y` must not be negative".to_string());
    }
    if let Some(win) = at.window
        && (win.width == 0 || win.height == 0)
    {
        return Err(
            "invalid arguments: `at.window.width` and `at.window.height` must be at least 1"
                .to_string(),
        );
    }
    Ok(())
}

fn has_any_locator_parts(
    automation_id: &Option<String>,
    role: &Option<String>,
    name: &Option<String>,
    text: &Option<String>,
    class_name: &Option<String>,
    within: &Option<WithinArgs>,
    nth: Option<usize>,
) -> bool {
    automation_id.is_some()
        || role.is_some()
        || name.is_some()
        || text.is_some()
        || class_name.is_some()
        || within.is_some()
        || nth.is_some()
}

fn validate_launch(args: &Value) -> Result<(), String> {
    let parsed: LaunchArgs = parse_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    if parsed.command.trim().is_empty() {
        return Err("invalid arguments: `command` must not be empty".to_string());
    }
    check_non_empty("cwd", &parsed.cwd)?;
    check_non_empty("window_title_contains", &parsed.window_title_contains)?;
    Ok(())
}

fn validate_attach(args: &Value) -> Result<(), String> {
    let parsed: AttachArgs = parse_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    if parsed.pid == Some(0) {
        return Err("invalid arguments: `pid` must be at least 1".to_string());
    }
    check_non_empty("executable", &parsed.executable)?;
    check_non_empty("title_contains", &parsed.title_contains)?;
    if parsed.pid.is_none() && parsed.executable.is_none() && parsed.title_contains.is_none() {
        return Err(
            "invalid arguments: `desktop.attach` requires at least one of `pid`, `executable`, or `title_contains`"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_click(args: &Value) -> Result<(), String> {
    let parsed: ClickArgs = parse_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    if let Some(at) = &parsed.at {
        check_at_point(at)?;
        if has_any_locator_parts(
            &parsed.automation_id,
            &parsed.role,
            &parsed.name,
            &parsed.text,
            &parsed.class_name,
            &parsed.within,
            parsed.nth,
        ) {
            return Err(
                "invalid arguments: `at` cannot be combined with locator fields (`automation_id`, `role`, `name`, `text`, `class_name`, `within`, or `nth`)"
                    .to_string(),
            );
        }
        return Ok(());
    }
    build_locator(
        &parsed.automation_id,
        &parsed.role,
        &parsed.name,
        &parsed.text,
        &parsed.class_name,
        &parsed.within,
        parsed.nth,
    )?;
    Ok(())
}

fn validate_type_parts(parsed: &TypeArgs) -> Result<(Option<DesktopLocator>, &str), String> {
    check_timeout(parsed.timeout_ms)?;
    let typed = parsed.value.as_deref().or(parsed.text.as_deref()).ok_or_else(|| {
        "invalid arguments: `text` is required; give the text to type, or `value` when `text` names the control"
            .to_string()
    })?;

    if let Some(at) = &parsed.at {
        check_at_point(at)?;
        // Reject locator fields when `at` is used; remember `text` IS the typing text when `value` is None.
        let text_as_locator = if parsed.value.is_some() {
            &parsed.text
        } else {
            &None
        };
        if has_any_locator_parts(
            &parsed.automation_id,
            &parsed.role,
            &parsed.name,
            text_as_locator,
            &parsed.class_name,
            &parsed.within,
            parsed.nth,
        ) {
            return Err(
                "invalid arguments: `at` cannot be combined with locator fields in `desktop.type`"
                    .to_string(),
            );
        }
        return Ok((None, typed));
    }

    let locator_text = if parsed.value.is_some() {
        &parsed.text
    } else {
        &None
    };
    let locator = build_locator(
        &parsed.automation_id,
        &parsed.role,
        &parsed.name,
        locator_text,
        &parsed.class_name,
        &parsed.within,
        parsed.nth,
    )?;
    Ok((Some(locator), typed))
}

fn validate_type(args: &Value) -> Result<(), String> {
    let parsed: TypeArgs = parse_validate(args)?;
    validate_type_parts(&parsed).map(|_| ())
}

fn validate_press(args: &Value) -> Result<(), String> {
    let parsed: PressArgs = parse_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    parse_key_spec(&parsed.key)?;
    Ok(())
}

fn validate_wait(args: &Value) -> Result<(), String> {
    let parsed: WaitArgs = parse_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    build_locator(
        &parsed.automation_id,
        &parsed.role,
        &parsed.name,
        &parsed.text,
        &parsed.class_name,
        &parsed.within,
        parsed.nth,
    )?;
    Ok(())
}

fn validate_read(args: &Value) -> Result<(), String> {
    let parsed: ReadArgs = parse_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    build_locator(
        &parsed.automation_id,
        &parsed.role,
        &parsed.name,
        &parsed.text,
        &parsed.class_name,
        &parsed.within,
        parsed.nth,
    )?;
    Ok(())
}

fn validate_screenshot(args: &Value) -> Result<(), String> {
    let parsed: ScreenshotArgs = parse_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    if parsed.path.trim().is_empty() {
        return Err("invalid arguments: `path` must not be empty".to_string());
    }
    Ok(())
}

fn validate_click_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefClickArgs = parse_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        return check_timeout(parsed.timeout_ms);
    }
    validate_click(args)
}

fn validate_type_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefTypeArgs = parse_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        check_timeout(parsed.timeout_ms)?;
        match (&parsed.text, &parsed.value) {
            (Some(_), Some(_)) => {
                return Err(
                    "invalid arguments: give either `text` or `value` with `ref`, not both"
                        .to_string(),
                );
            }
            (None, None) => {
                return Err("invalid arguments: give `text` (or `value`) with `ref`".to_string());
            }
            _ => return Ok(()),
        }
    }
    validate_type(args)
}

fn validate_wait_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefWaitArgs = parse_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        return check_timeout(parsed.timeout_ms);
    }
    validate_wait(args)
}

fn validate_read_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefReadArgs = parse_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        return check_timeout(parsed.timeout_ms);
    }
    validate_read(args)
}

fn coordinate_lint_warnings(args: &Value) -> Vec<String> {
    let Some(at) = args.get("at") else {
        return Vec::new();
    };
    let mut warnings = vec![
        "uses explicit coordinates `at`; coordinates are a last resort for content with no UIA node (for example custom-drawn canvases) and click the wrong place when window size or DPI changes, so prefer `automation_id`, `role` + `name`, or `text`"
            .to_string(),
    ];
    if at.get("window").is_none() {
        warnings.push(
            "records no `at.window`; the step clicks at the live window size, so record the window size the coordinates were measured at (`{ width, height }`) so replay fails on a different size instead of clicking the wrong place"
                .to_string(),
        );
    }
    warnings
}

/// Parses a human-readable key or modifier chord into a validated UIAutomation `send_keys` string.
pub fn parse_key_spec(raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err("invalid arguments: `key` must not be empty".to_string());
    }

    let tokens = split_key_tokens(raw)?;
    if tokens.is_empty() {
        return Err("invalid arguments: `key` must not be empty".to_string());
    }

    let mut modifiers: Vec<&'static str> = Vec::new();
    let mut seen_mods: HashSet<&'static str> = HashSet::new();

    for mod_token in &tokens[..tokens.len() - 1] {
        let mapped = map_modifier_token(mod_token).ok_or_else(|| {
            format!(
                "invalid arguments: unknown modifier `{mod_token}` in `key` `{raw}`; expected Ctrl, Alt, Shift, or Win"
            )
        })?;
        if !seen_mods.insert(mapped) {
            return Err(format!(
                "invalid arguments: duplicate modifier `{mod_token}` in `key` `{raw}`"
            ));
        }
        modifiers.push(mapped);
    }

    let main_raw = &tokens[tokens.len() - 1];
    if map_modifier_token(main_raw).is_some() {
        return Err(format!(
            "invalid arguments: key chord `{raw}` must end with a non-modifier key"
        ));
    }

    let main_expr = map_main_key_token(main_raw, !modifiers.is_empty())
        .ok_or_else(|| format!("invalid arguments: unknown key `{main_raw}` in `key` `{raw}`"))?;

    let mut out = String::new();
    for m in &modifiers {
        out.push_str(m);
    }
    if !modifiers.is_empty() && !main_expr.starts_with('{') {
        out.push('(');
        out.push_str(&main_expr);
        out.push(')');
    } else {
        out.push_str(&main_expr);
    }
    Ok(out)
}

fn split_key_tokens(raw: &str) -> Result<Vec<String>, String> {
    if raw == "+" {
        return Ok(vec!["+".to_string()]);
    }
    if let Some(prefix) = raw.strip_suffix("++") {
        if prefix.is_empty() {
            return Err(format!("invalid arguments: malformed key chord `{raw}`"));
        }
        let mut parts: Vec<String> = prefix.split('+').map(|s| s.trim().to_string()).collect();
        if parts.iter().any(|p| p.is_empty()) {
            return Err(format!("invalid arguments: malformed key chord `{raw}`"));
        }
        parts.push("+".to_string());
        return Ok(parts);
    }

    let parts: Vec<String> = raw.split('+').map(|s| s.trim().to_string()).collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(format!("invalid arguments: malformed key chord `{raw}`"));
    }
    Ok(parts)
}

fn map_modifier_token(token: &str) -> Option<&'static str> {
    match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "lctrl" | "lcontrol" => Some("{ctrl}"),
        "rctrl" | "rcontrol" => Some("{rctrl}"),
        "alt" | "option" | "opt" | "menu" | "lalt" | "loption" => Some("{alt}"),
        "ralt" | "roption" | "ropt" | "altgr" => Some("{ralt}"),
        "shift" | "lshift" => Some("{shift}"),
        "rshift" => Some("{rshift}"),
        "win" | "windows" | "lwin" | "cmd" | "command" | "meta" | "super" => Some("{win}"),
        "rwin" | "rcmd" | "rcommand" | "rmeta" | "rsuper" => Some("{rwin}"),
        _ => None,
    }
}

fn map_main_key_token(token: &str, has_modifiers: bool) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    let named = match lower.as_str() {
        "enter" | "return" => Some("{ENTER}"),
        "tab" => Some("{TAB}"),
        "esc" | "escape" => Some("{ESC}"),
        "backspace" | "back" => Some("{BACKSPACE}"),
        "delete" | "del" => Some("{DELETE}"),
        "insert" | "ins" => Some("{INSERT}"),
        "space" => Some("{SPACE}"),
        "home" => Some("{HOME}"),
        "end" => Some("{END}"),
        "pageup" | "page_up" | "pgup" => Some("{PAGEUP}"),
        "pagedown" | "page_down" | "pgdn" => Some("{PAGEDOWN}"),
        "up" => Some("{UP}"),
        "down" => Some("{DOWN}"),
        "left" => Some("{LEFT}"),
        "right" => Some("{RIGHT}"),
        "print" | "printscreen" | "prtsc" => Some("{PRINTSCREEN}"),
        "capslock" | "capital" => Some("{CAPSLOCK}"),
        "pause" => Some("{PAUSE}"),
        "f1" => Some("{F1}"),
        "f2" => Some("{F2}"),
        "f3" => Some("{F3}"),
        "f4" => Some("{F4}"),
        "f5" => Some("{F5}"),
        "f6" => Some("{F6}"),
        "f7" => Some("{F7}"),
        "f8" => Some("{F8}"),
        "f9" => Some("{F9}"),
        "f10" => Some("{F10}"),
        "f11" => Some("{F11}"),
        "f12" => Some("{F12}"),
        "f13" => Some("{F13}"),
        "f14" => Some("{F14}"),
        "f15" => Some("{F15}"),
        "f16" => Some("{F16}"),
        "f17" => Some("{F17}"),
        "f18" => Some("{F18}"),
        "f19" => Some("{F19}"),
        "f20" => Some("{F20}"),
        "f21" => Some("{F21}"),
        "f22" => Some("{F22}"),
        "f23" => Some("{F23}"),
        "f24" => Some("{F24}"),
        "plus" => Some("+"),
        _ => None,
    };
    if let Some(code) = named {
        return Some(code.to_string());
    }

    let mut chars = token.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || ch.is_control() {
        return None;
    }

    // Escape special uiautomation parser characters; lowercase ASCII when combined
    // with modifiers so VkKeyScanW stays independent of Shift assumptions.
    let escaped = match ch {
        '{' => "{{}".to_string(),
        '}' => "{}}".to_string(),
        '(' => "{(}".to_string(),
        ')' => "{)}".to_string(),
        c if has_modifiers && c.is_ascii_uppercase() => c.to_ascii_lowercase().to_string(),
        c => c.to_string(),
    };
    Some(escaped)
}

fn base_dir(sessions: &Sessions) -> PathBuf {
    sessions
        .base_directory()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn resolve_path(sessions: &Sessions, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        base_dir(sessions).join(path)
    }
}

fn run_launch(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let parsed: LaunchArgs = parse_args(args)?;
    validate_launch(args).map_err(StepError::InvalidArguments)?;

    let interactive = sessions.is_interactive();
    let base = base_dir(sessions);
    let (backend, result) = backend::DesktopBackend::launch(&parsed, &base, interactive)?;

    let session = sessions
        .get_or_create::<DesktopSession>(SESSION_KIND, || Ok(DesktopSession::empty()))
        .map_err(StepError::Environment)?;
    if let Some(mut old) = session.backend.take() {
        let _ = old.close();
    }
    session.backend = Some(backend);
    Ok(result)
}

fn run_attach(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let parsed: AttachArgs = parse_args(args)?;
    validate_attach(args).map_err(StepError::InvalidArguments)?;

    let (backend, result) = backend::DesktopBackend::attach(&parsed)?;

    let session = sessions
        .get_or_create::<DesktopSession>(SESSION_KIND, || Ok(DesktopSession::empty()))
        .map_err(StepError::Environment)?;
    if let Some(mut old) = session.backend.take() {
        let _ = old.close();
    }
    session.backend = Some(backend);
    Ok(result)
}

fn active_backend(sessions: &mut Sessions) -> Result<(bool, &mut DesktopSession), StepError> {
    let interactive = sessions.is_interactive();
    let session = sessions
        .get::<DesktopSession>(SESSION_KIND)
        .map_err(StepError::Environment)?
        .ok_or_else(|| {
            StepError::Environment(
                "no active desktop session; run `desktop.launch` or `desktop.attach` first"
                    .to_string(),
            )
        })?;
    Ok((interactive, session))
}

fn run_click(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let (interactive, session) = active_backend(sessions)?;
    if has_ref_field(args) {
        if !interactive {
            return Err(StepError::InvalidArguments(
                "`ref` is only valid inside an interactive `da session`".to_string(),
            ));
        }
        let parsed: RefClickArgs = parse_args(args)?;
        check_ref_id(&parsed.ref_id).map_err(StepError::InvalidArguments)?;
        check_timeout(parsed.timeout_ms).map_err(StepError::InvalidArguments)?;
        return session.active_mut()?.click(
            None,
            None,
            Some(&parsed.ref_id),
            parsed.button,
            parsed.timeout_ms,
            interactive,
        );
    }

    validate_click(args).map_err(StepError::InvalidArguments)?;
    let parsed: ClickArgs = parse_args(args)?;
    if let Some(at) = &parsed.at {
        return session.active_mut()?.click(
            None,
            Some(at),
            None,
            parsed.button,
            parsed.timeout_ms,
            interactive,
        );
    }
    let locator = build_locator(
        &parsed.automation_id,
        &parsed.role,
        &parsed.name,
        &parsed.text,
        &parsed.class_name,
        &parsed.within,
        parsed.nth,
    )
    .map_err(StepError::InvalidArguments)?;
    session.active_mut()?.click(
        Some(&locator),
        None,
        None,
        parsed.button,
        parsed.timeout_ms,
        interactive,
    )
}

fn run_type(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let (interactive, session) = active_backend(sessions)?;
    if has_ref_field(args) {
        if !interactive {
            return Err(StepError::InvalidArguments(
                "`ref` is only valid inside an interactive `da session`".to_string(),
            ));
        }
        validate_type_session(args).map_err(StepError::InvalidArguments)?;
        let parsed: RefTypeArgs = parse_args(args)?;
        let text = parsed
            .value
            .as_deref()
            .or(parsed.text.as_deref())
            .ok_or_else(|| {
                StepError::InvalidArguments("give `text` (or `value`) with `ref`".to_string())
            })?;
        return session.active_mut()?.type_text(
            None,
            None,
            Some(&parsed.ref_id),
            text,
            parsed.timeout_ms,
            interactive,
        );
    }

    validate_type(args).map_err(StepError::InvalidArguments)?;
    let parsed: TypeArgs = parse_args(args)?;
    let (locator, typed) = validate_type_parts(&parsed).map_err(StepError::InvalidArguments)?;
    let owned_text = typed.to_string();
    session.active_mut()?.type_text(
        locator.as_ref(),
        parsed.at.as_ref(),
        None,
        &owned_text,
        parsed.timeout_ms,
        interactive,
    )
}

fn run_press(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    validate_press(args).map_err(StepError::InvalidArguments)?;
    let parsed: PressArgs = parse_args(args)?;
    let expr = parse_key_spec(&parsed.key).map_err(StepError::InvalidArguments)?;
    let (_, session) = active_backend(sessions)?;
    session.active_mut()?.press(&expr, parsed.timeout_ms)
}

fn run_wait(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let (interactive, session) = active_backend(sessions)?;
    if has_ref_field(args) {
        if !interactive {
            return Err(StepError::InvalidArguments(
                "`ref` is only valid inside an interactive `da session`".to_string(),
            ));
        }
        validate_wait_session(args).map_err(StepError::InvalidArguments)?;
        let parsed: RefWaitArgs = parse_args(args)?;
        return session.active_mut()?.wait(
            None,
            Some(&parsed.ref_id),
            parsed.state,
            parsed.timeout_ms,
            interactive,
        );
    }

    validate_wait(args).map_err(StepError::InvalidArguments)?;
    let parsed: WaitArgs = parse_args(args)?;
    let locator = build_locator(
        &parsed.automation_id,
        &parsed.role,
        &parsed.name,
        &parsed.text,
        &parsed.class_name,
        &parsed.within,
        parsed.nth,
    )
    .map_err(StepError::InvalidArguments)?;
    session.active_mut()?.wait(
        Some(&locator),
        None,
        parsed.state,
        parsed.timeout_ms,
        interactive,
    )
}

fn run_read(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let (interactive, session) = active_backend(sessions)?;
    if has_ref_field(args) {
        if !interactive {
            return Err(StepError::InvalidArguments(
                "`ref` is only valid inside an interactive `da session`".to_string(),
            ));
        }
        validate_read_session(args).map_err(StepError::InvalidArguments)?;
        let parsed: RefReadArgs = parse_args(args)?;
        return session.active_mut()?.read(
            None,
            Some(&parsed.ref_id),
            parsed.timeout_ms,
            interactive,
        );
    }

    validate_read(args).map_err(StepError::InvalidArguments)?;
    let parsed: ReadArgs = parse_args(args)?;
    let locator = build_locator(
        &parsed.automation_id,
        &parsed.role,
        &parsed.name,
        &parsed.text,
        &parsed.class_name,
        &parsed.within,
        parsed.nth,
    )
    .map_err(StepError::InvalidArguments)?;
    session
        .active_mut()?
        .read(Some(&locator), None, parsed.timeout_ms, interactive)
}

fn run_screenshot(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    validate_screenshot(args).map_err(StepError::InvalidArguments)?;
    let parsed: ScreenshotArgs = parse_args(args)?;
    let resolved = resolve_path(sessions, &parsed.path);
    let (_, session) = active_backend(sessions)?;
    session
        .active_mut()?
        .screenshot(&resolved, parsed.mask_inputs, parsed.timeout_ms)
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(super) mod shared {
    use std::collections::{BTreeMap, HashMap};
    use std::fs::{File, OpenOptions};
    use std::io::BufWriter;
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::{Child, Command, Stdio};
    use std::sync::{Mutex, MutexGuard};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};

    use crate::core::paths;
    use crate::core::registry::StepError;
    use crate::core::session::Evidence;

    use super::{
        ClickButton, DesktopLocator, DesktopPoint, DesktopRole, LaunchArgs, LocatorFilter,
        WaitState, WithinArgs, build_locator,
    };

    pub(super) const MAX_COLLECTED_NODES: usize = 4_000;
    pub(super) const DEFAULT_SNAPSHOT_MAX_DEPTH: usize = 8;
    pub(super) const DEFAULT_SNAPSHOT_MAX_NODES: usize = 250;
    pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(25);

    static DESKTOP_INPUT_LOCK: Mutex<()> = Mutex::new(());

    pub(super) struct DesktopInputGuard {
        _thread_guard: MutexGuard<'static, ()>,
        _file_guard: File,
    }

    pub(super) fn acquire_desktop_input_lock(
        timeout_ms: u64,
    ) -> Result<DesktopInputGuard, StepError> {
        let thread_guard = DESKTOP_INPUT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let lock_path = std::env::temp_dir().join("do-anything-desktop-input.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| {
                StepError::Environment(format!(
                    "failed to open desktop input lock `{}`: {err}",
                    paths::display(&lock_path)
                ))
            })?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(DesktopInputGuard {
                        _thread_guard: thread_guard,
                        _file_guard: file,
                    });
                }
                Err(err) => {
                    if Instant::now() >= deadline {
                        return Err(StepError::Timeout(format!(
                            "timed out after {timeout_ms}ms acquiring exclusive desktop input lock: {err}"
                        )));
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub(super) struct BoundsRect {
        pub x: i32,
        pub y: i32,
        pub width: i32,
        pub height: i32,
    }

    #[derive(Debug, Clone)]
    pub(super) struct SharedNodeData {
        pub index: usize,
        pub parent: Option<usize>,
        pub depth: usize,
        pub subtree_end: usize,
        pub runtime_key: String,
        pub role: DesktopRole,
        pub name: String,
        pub automation_id: Option<String>,
        pub class_name: String,
        pub value: Option<String>,
        pub enabled: bool,
        pub offscreen: bool,
        pub is_password: bool,
        pub screen_rect: (i32, i32, i32, i32),
        pub bounds: BoundsRect,
    }

    #[derive(Debug, Clone)]
    pub(super) struct SharedTreeData {
        pub win_left: i32,
        pub win_top: i32,
        pub win_width: u32,
        pub win_height: u32,
        pub title: String,
        pub nodes: Vec<SharedNodeData>,
        pub truncated: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(super) struct StoredRef {
        #[serde(default)]
        pub runtime_key: String,
        #[serde(default)]
        pub locator: Option<Value>,
        #[serde(default)]
        pub fragile: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub(super) struct PersistedHandle {
        pub pid: u32,
        #[serde(default)]
        pub hwnd: i64,
        pub owned_launch: bool,
        #[serde(default)]
        pub process_start_token: Option<String>,
        #[serde(default)]
        pub local_only_reason: Option<String>,
        #[serde(default)]
        pub title: String,
        #[serde(default)]
        pub refs: BTreeMap<String, StoredRef>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum KeyModifier {
        Ctrl,
        RCtrl,
        Alt,
        RAlt,
        Shift,
        RShift,
        Meta,
        RMeta,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum MainKey {
        Enter,
        Tab,
        Escape,
        Backspace,
        Delete,
        Insert,
        Space,
        Home,
        End,
        PageUp,
        PageDown,
        Up,
        Down,
        Left,
        Right,
        PrintScreen,
        CapsLock,
        Pause,
        F(u8),
        Char(char),
    }

    /// Parses the canonical UIA key expression produced by `parse_key_spec` (or legacy
    /// shorthand prefixes `^`, `%`, `+`) into structured modifiers and a main key.
    pub(super) fn parse_uia_key_expr(expr: &str) -> Result<(Vec<KeyModifier>, MainKey), StepError> {
        let mut rest = expr.trim();
        if rest.is_empty() {
            return Err(StepError::InvalidArguments(
                "invalid arguments: `key` must not be empty".to_string(),
            ));
        }

        let mut mods = Vec::new();
        loop {
            if let Some(after) = rest.strip_prefix("{ctrl}") {
                mods.push(KeyModifier::Ctrl);
                rest = after;
            } else if let Some(after) = rest.strip_prefix("{rctrl}") {
                mods.push(KeyModifier::RCtrl);
                rest = after;
            } else if let Some(after) = rest.strip_prefix("{alt}") {
                mods.push(KeyModifier::Alt);
                rest = after;
            } else if let Some(after) = rest.strip_prefix("{ralt}") {
                mods.push(KeyModifier::RAlt);
                rest = after;
            } else if let Some(after) = rest.strip_prefix("{shift}") {
                mods.push(KeyModifier::Shift);
                rest = after;
            } else if let Some(after) = rest.strip_prefix("{rshift}") {
                mods.push(KeyModifier::RShift);
                rest = after;
            } else if let Some(after) = rest.strip_prefix("{win}") {
                mods.push(KeyModifier::Meta);
                rest = after;
            } else if let Some(after) = rest.strip_prefix("{rwin}") {
                mods.push(KeyModifier::RMeta);
                rest = after;
            } else if rest.len() > 1 && rest.starts_with('^') {
                mods.push(KeyModifier::Ctrl);
                rest = &rest[1..];
            } else if rest.len() > 1 && rest.starts_with('%') {
                mods.push(KeyModifier::Alt);
                rest = &rest[1..];
            } else if rest.len() > 1 && rest.starts_with('+') {
                mods.push(KeyModifier::Shift);
                rest = &rest[1..];
            } else {
                break;
            }
        }

        let main = match rest {
            "{{}" => MainKey::Char('{'),
            "{}}" => MainKey::Char('}'),
            "{(}" => MainKey::Char('('),
            "{)}" => MainKey::Char(')'),
            "{ENTER}" => MainKey::Enter,
            "{TAB}" => MainKey::Tab,
            "{ESC}" => MainKey::Escape,
            "{BACKSPACE}" => MainKey::Backspace,
            "{DELETE}" => MainKey::Delete,
            "{INSERT}" => MainKey::Insert,
            "{SPACE}" => MainKey::Space,
            "{HOME}" => MainKey::Home,
            "{END}" => MainKey::End,
            "{PAGEUP}" => MainKey::PageUp,
            "{PAGEDOWN}" => MainKey::PageDown,
            "{UP}" => MainKey::Up,
            "{DOWN}" => MainKey::Down,
            "{LEFT}" => MainKey::Left,
            "{RIGHT}" => MainKey::Right,
            "{PRINTSCREEN}" => MainKey::PrintScreen,
            "{CAPSLOCK}" => MainKey::CapsLock,
            "{PAUSE}" => MainKey::Pause,
            other => {
                if let Some(f_num) = other
                    .strip_prefix("{F")
                    .and_then(|s| s.strip_suffix('}'))
                    .and_then(|s| s.parse::<u8>().ok())
                    .filter(|&n| (1..=24).contains(&n))
                {
                    MainKey::F(f_num)
                } else {
                    let unwrapped = other
                        .strip_prefix('(')
                        .and_then(|s| s.strip_suffix(')'))
                        .unwrap_or(other);
                    let mut chars = unwrapped.chars();
                    let Some(ch) = chars.next() else {
                        return Err(StepError::InvalidArguments(format!(
                            "invalid key expression `{expr}`"
                        )));
                    };
                    if chars.next().is_some() || ch.is_control() {
                        return Err(StepError::InvalidArguments(format!(
                            "unsupported key expression `{expr}`"
                        )));
                    }
                    MainKey::Char(ch)
                }
            }
        };

        Ok((mods, main))
    }

    pub(super) fn resolve_command_and_cwd(
        args: &LaunchArgs,
        base_dir: &Path,
    ) -> (String, Option<PathBuf>) {
        let p = Path::new(&args.command);
        let command_path = if p.is_absolute() {
            args.command.clone()
        } else if args.command.contains('/') || args.command.contains('\\') {
            base_dir.join(p).to_string_lossy().into_owned()
        } else {
            let candidate = base_dir.join(p);
            if candidate.is_file() {
                candidate.to_string_lossy().into_owned()
            } else {
                args.command.clone()
            }
        };

        let resolved_cwd = args.cwd.as_deref().map(|cwd| {
            let cwd_path = Path::new(cwd);
            if cwd_path.is_absolute() {
                cwd_path.to_path_buf()
            } else {
                base_dir.join(cwd_path)
            }
        });

        (command_path, resolved_cwd)
    }

    pub(super) fn step_error_message(err: &StepError) -> &str {
        match err {
            StepError::InvalidArguments(msg)
            | StepError::Environment(msg)
            | StepError::Timeout(msg)
            | StepError::Execution(msg) => msg,
        }
    }

    pub(super) fn missing_target_timeout(ref_id: Option<&str>, timeout_ms: u64) -> StepError {
        if let Some(r) = ref_id {
            StepError::Timeout(format!(
                "timed out after {timeout_ms}ms resolving session ref `{r}`"
            ))
        } else {
            StepError::Timeout(format!(
                "timed out after {timeout_ms}ms finding desktop control"
            ))
        }
    }

    pub(super) fn click_button_name(button: ClickButton) -> &'static str {
        match button {
            ClickButton::Left => "left",
            ClickButton::Right => "right",
            ClickButton::Double => "double",
        }
    }

    pub(super) fn wait_state_name(state: WaitState) -> &'static str {
        match state {
            WaitState::Present => "present",
            WaitState::Gone => "gone",
            WaitState::Enabled => "enabled",
            WaitState::Visible => "visible",
        }
    }

    pub(super) fn check_window_point_bounds(
        tree: &SharedTreeData,
        point: &DesktopPoint,
    ) -> Result<(), StepError> {
        if let Some(expected) = point.window
            && (expected.width != tree.win_width || expected.height != tree.win_height)
        {
            return Err(StepError::Execution(format!(
                "desktop window size {}x{} does not match expected {}x{} in `at.window`",
                tree.win_width, tree.win_height, expected.width, expected.height
            )));
        }
        if point.x < 0
            || point.y < 0
            || point.x >= tree.win_width as i32
            || point.y >= tree.win_height as i32
        {
            return Err(StepError::Execution(format!(
                "coordinates ({}, {}) fall outside target window bounds {}x{}",
                point.x, point.y, tree.win_width, tree.win_height
            )));
        }
        Ok(())
    }

    fn matches_filter(node: &SharedNodeData, filter: &LocatorFilter) -> bool {
        if let Some(expected_aid) = &filter.automation_id
            && node.automation_id.as_deref() != Some(expected_aid.as_str())
        {
            return false;
        }
        if let Some(expected_role) = filter.role
            && node.role != expected_role
        {
            return false;
        }
        if let Some(expected_name) = &filter.name
            && node.name != *expected_name
        {
            return false;
        }
        if let Some(expected_class) = &filter.class_name
            && node.class_name != *expected_class
        {
            return false;
        }
        if let Some(needle) = &filter.text {
            let lower = needle.to_ascii_lowercase();
            let name_hit = node.name.to_ascii_lowercase().contains(&lower);
            let value_hit = node
                .value
                .as_deref()
                .is_some_and(|v| v.to_ascii_lowercase().contains(&lower));
            if !name_hit && !value_hit {
                return false;
            }
        }
        true
    }

    #[derive(Deserialize)]
    struct StoredLocatorJson {
        #[serde(default)]
        automation_id: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        class_name: Option<String>,
        #[serde(default)]
        within: Option<WithinArgs>,
        #[serde(default)]
        nth: Option<usize>,
    }

    fn locator_from_json(value: &Value) -> Option<DesktopLocator> {
        let raw: StoredLocatorJson = serde_json::from_value(value.clone()).ok()?;
        build_locator(
            &raw.automation_id,
            &raw.role,
            &raw.name,
            &raw.text,
            &raw.class_name,
            &raw.within,
            raw.nth,
        )
        .ok()
    }

    fn match_locator_in_tree(tree: &SharedTreeData, locator: &DesktopLocator) -> Option<usize> {
        let (start, end) = if let Some(scope) = &locator.within {
            let scope_node = tree
                .nodes
                .iter()
                .filter(|n| matches_filter(n, scope))
                .nth(scope.nth)?;
            (scope_node.index + 1, scope_node.subtree_end)
        } else {
            (0, tree.nodes.len())
        };

        tree.nodes[start..end]
            .iter()
            .filter(|n| matches_filter(n, &locator.filter))
            .nth(locator.filter.nth)
            .map(|n| n.index)
    }

    pub(super) fn resolve_node_in_tree(
        tree: &SharedTreeData,
        target: Option<&DesktopLocator>,
        ref_id: Option<&str>,
        refs: &BTreeMap<String, StoredRef>,
    ) -> Result<Option<usize>, StepError> {
        if let Some(key) = ref_id {
            let stored = refs.get(key).ok_or_else(|| {
                StepError::InvalidArguments(format!(
                    "unknown desktop snapshot ref `{key}`; run `da session snapshot` to inspect available controls"
                ))
            })?;
            if !stored.runtime_key.is_empty()
                && let Some(pos) = tree
                    .nodes
                    .iter()
                    .position(|n| n.runtime_key == stored.runtime_key)
            {
                return Ok(Some(pos));
            }
            if let Some(loc_json) = &stored.locator
                && let Some(parsed) = locator_from_json(loc_json)
            {
                return Ok(match_locator_in_tree(tree, &parsed));
            }
            return Ok(None);
        }

        let locator = target.ok_or_else(|| {
            StepError::InvalidArguments("missing desktop locator target".to_string())
        })?;
        Ok(match_locator_in_tree(tree, locator))
    }

    pub(super) fn synthesize_locator(idx: usize, nodes: &[SharedNodeData]) -> (Value, bool) {
        let node = &nodes[idx];

        if let Some(aid) = &node.automation_id {
            let count = nodes
                .iter()
                .skip(1)
                .filter(|n| n.automation_id.as_deref() == Some(aid.as_str()))
                .count();
            if count == 1 {
                return (json!({ "automation_id": aid }), false);
            }
        }

        if !node.name.is_empty() {
            let global_count = nodes
                .iter()
                .skip(1)
                .filter(|n| n.role == node.role && n.name == node.name)
                .count();
            if global_count == 1 {
                return (
                    json!({
                        "role": node.role.as_str(),
                        "name": node.name,
                    }),
                    false,
                );
            }

            if let Some((within_json, scope_start, scope_end)) = nearest_within_scope(idx, nodes) {
                let scoped_count = nodes[scope_start..scope_end]
                    .iter()
                    .filter(|n| n.role == node.role && n.name == node.name)
                    .count();
                if scoped_count == 1 {
                    return (
                        json!({
                            "within": within_json,
                            "role": node.role.as_str(),
                            "name": node.name,
                        }),
                        false,
                    );
                }
            }
        }

        let (within_json, scope_start, scope_end) = nearest_within_scope(idx, nodes)
            .map(|(w, s, e)| (Some(w), s, e))
            .unwrap_or((None, 1.min(nodes.len()), nodes.len()));

        let use_name = !node.name.is_empty();
        let use_class = !use_name && !node.class_name.is_empty();
        let matches_before = nodes[scope_start..idx]
            .iter()
            .filter(|n| {
                n.role == node.role
                    && (!use_name || n.name == node.name)
                    && (!use_class || n.class_name == node.class_name)
            })
            .count();
        let total_matches = nodes[scope_start..scope_end]
            .iter()
            .filter(|n| {
                n.role == node.role
                    && (!use_name || n.name == node.name)
                    && (!use_class || n.class_name == node.class_name)
            })
            .count();

        let mut obj = serde_json::Map::new();
        if let Some(w) = within_json {
            obj.insert("within".to_string(), w);
        }
        obj.insert("role".to_string(), json!(node.role.as_str()));
        if use_name {
            obj.insert("name".to_string(), json!(node.name));
        } else if use_class {
            obj.insert("class_name".to_string(), json!(node.class_name));
        }
        if total_matches > 1 || matches_before > 0 {
            obj.insert("nth".to_string(), json!(matches_before));
        }
        (Value::Object(obj), true)
    }

    fn nearest_within_scope(idx: usize, nodes: &[SharedNodeData]) -> Option<(Value, usize, usize)> {
        let mut cur = nodes[idx].parent;
        while let Some(parent_idx) = cur {
            if parent_idx == 0 {
                break;
            }
            let parent = &nodes[parent_idx];
            if let Some(aid) = &parent.automation_id {
                let count = nodes
                    .iter()
                    .skip(1)
                    .filter(|n| n.automation_id.as_deref() == Some(aid.as_str()))
                    .count();
                if count == 1 {
                    return Some((
                        json!({ "automation_id": aid }),
                        parent.index + 1,
                        parent.subtree_end,
                    ));
                }
            }
            if !parent.name.is_empty() {
                let count = nodes
                    .iter()
                    .skip(1)
                    .filter(|n| n.role == parent.role && n.name == parent.name)
                    .count();
                if count == 1 {
                    return Some((
                        json!({ "role": parent.role.as_str(), "name": parent.name }),
                        parent.index + 1,
                        parent.subtree_end,
                    ));
                }
            }
            cur = parent.parent;
        }
        None
    }

    pub(super) fn reverse_lookup_in_tree(
        tree: &SharedTreeData,
        sx: i32,
        sy: i32,
    ) -> Option<(Value, bool)> {
        let hit_idx = tree
            .nodes
            .iter()
            .skip(1)
            .filter(|n| {
                !n.offscreen
                    && n.screen_rect.0 <= sx
                    && sx < n.screen_rect.2
                    && n.screen_rect.1 <= sy
                    && sy < n.screen_rect.3
            })
            .max_by_key(|n| {
                (
                    n.depth,
                    std::cmp::Reverse(
                        (n.screen_rect.2 - n.screen_rect.0)
                            .saturating_mul(n.screen_rect.3 - n.screen_rect.1),
                    ),
                )
            })?
            .index;

        let mut cur = Some(hit_idx);
        for _ in 0..=2 {
            let idx = cur?;
            if idx == 0 {
                break;
            }
            let node = &tree.nodes[idx];
            if node.role.is_interactive() {
                let (loc, fragile) = synthesize_locator(idx, &tree.nodes);
                if !fragile {
                    return Some((loc, false));
                }
                return None;
            }
            if !matches!(node.role, DesktopRole::Text | DesktopRole::Image) {
                return None;
            }
            cur = node.parent;
        }
        None
    }

    fn should_mask_node(node: &SharedNodeData) -> bool {
        if node.is_password || node.role == DesktopRole::Edit {
            return true;
        }
        let class_lower = node.class_name.to_ascii_lowercase();
        class_lower == "edit"
            || class_lower.contains("password")
            || class_lower.contains("securetextfield")
    }

    pub(super) fn mask_rgba_rect(
        pixels: &mut [u8],
        width: u32,
        height: u32,
        bounds: BoundsRect,
    ) -> bool {
        let x0 = bounds.x.max(0).min(width as i32) as u32;
        let y0 = bounds.y.max(0).min(height as i32) as u32;
        let x1 = (bounds.x.saturating_add(bounds.width))
            .max(0)
            .min(width as i32) as u32;
        let y1 = (bounds.y.saturating_add(bounds.height))
            .max(0)
            .min(height as i32) as u32;
        if x1 <= x0 || y1 <= y0 {
            return false;
        }
        for y in y0..y1 {
            let row_start = ((y * width + x0) * 4) as usize;
            let row_end = ((y * width + x1) * 4) as usize;
            for px in pixels[row_start..row_end].as_chunks_mut::<4>().0 {
                *px = [0, 0, 0, 255];
            }
        }
        true
    }

    /// Masks every `Edit` and `is_password` node bounding box in `pixels`, scaling
    /// logical window bounds by `(width / tree.win_width, height / tree.win_height)`
    /// for HiDPI / Retina window captures.
    pub(super) fn mask_tree_inputs(
        pixels: &mut [u8],
        width: u32,
        height: u32,
        tree: &SharedTreeData,
    ) -> usize {
        let scale_x = if tree.win_width > 0 {
            f64::from(width) / f64::from(tree.win_width)
        } else {
            1.0
        };
        let scale_y = if tree.win_height > 0 {
            f64::from(height) / f64::from(tree.win_height)
        } else {
            1.0
        };

        let mut masked_count = 0usize;
        for node in &tree.nodes {
            if !should_mask_node(node) {
                continue;
            }
            let x0 = (f64::from(node.bounds.x) * scale_x).floor() as i32;
            let y0 = (f64::from(node.bounds.y) * scale_y).floor() as i32;
            let x1 = (f64::from(node.bounds.x.saturating_add(node.bounds.width)) * scale_x).ceil()
                as i32;
            let y1 = (f64::from(node.bounds.y.saturating_add(node.bounds.height)) * scale_y).ceil()
                as i32;
            let scaled = BoundsRect {
                x: x0,
                y: y0,
                width: x1.saturating_sub(x0),
                height: y1.saturating_sub(y0),
            };
            if mask_rgba_rect(pixels, width, height, scaled) {
                masked_count += 1;
            }
        }
        masked_count
    }

    pub(super) fn encode_rgba_png(
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, StepError> {
        let expected_len = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        if rgba.len() < expected_len {
            return Err(StepError::Execution(
                "incomplete RGBA buffer for PNG encoding".to_string(),
            ));
        }
        let mut png_bytes = Vec::new();
        {
            let writer = BufWriter::new(&mut png_bytes);
            let mut encoder = png::Encoder::new(writer, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut png_writer = encoder
                .write_header()
                .map_err(|err| StepError::Execution(format!("PNG header error: {err}")))?;
            png_writer
                .write_image_data(&rgba[..expected_len])
                .map_err(|err| StepError::Execution(format!("PNG encode error: {err}")))?;
        }
        Ok(png_bytes)
    }

    #[cfg(any(target_os = "macos", test))]
    pub(super) fn decode_png_to_rgba8(png_bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), StepError> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        decoder.set_transformations(
            png::Transformations::EXPAND
                | png::Transformations::ALPHA
                | png::Transformations::STRIP_16,
        );
        let mut reader = decoder
            .read_info()
            .map_err(|err| StepError::Execution(format!("PNG decode header failed: {err}")))?;
        let buf_len = reader
            .output_buffer_size()
            .ok_or_else(|| StepError::Execution("PNG output buffer size overflowed".to_string()))?;
        let mut buf = vec![0u8; buf_len];
        let info = reader
            .next_frame(&mut buf)
            .map_err(|err| StepError::Execution(format!("PNG decode frame failed: {err}")))?;
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
            png::ColorType::GrayscaleAlpha => {
                let mut out = Vec::with_capacity((buf.len() / 2) * 4);
                for ga in buf.as_chunks::<2>().0 {
                    out.extend_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
                }
                out
            }
            png::ColorType::Grayscale => {
                let mut out = Vec::with_capacity(buf.len() * 4);
                for &g in &buf {
                    out.extend_from_slice(&[g, g, g, 255]);
                }
                out
            }
            other => {
                return Err(StepError::Execution(format!(
                    "unsupported PNG color type `{other:?}` after RGBA expansion"
                )));
            }
        };
        Ok((info.width, info.height, rgba))
    }

    pub(super) fn write_png_private(path: &Path, png_bytes: &[u8]) -> Result<(), StepError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| {
                StepError::Execution(format!(
                    "failed to create screenshot directory `{}`: {err}",
                    paths::display(parent)
                ))
            })?;
        }
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)
                .map_err(|err| {
                    StepError::Execution(format!(
                        "failed to write desktop screenshot `{}`: {err}",
                        paths::display(path)
                    ))
                })?;
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            file.write_all(png_bytes).map_err(|err| {
                StepError::Execution(format!(
                    "failed to write desktop screenshot `{}`: {err}",
                    paths::display(path)
                ))
            })?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(path, png_bytes).map_err(|err| {
                StepError::Execution(format!(
                    "failed to write desktop screenshot `{}`: {err}",
                    paths::display(path)
                ))
            })?;
        }
        Ok(())
    }

    pub(super) fn node_summary(node: &SharedNodeData) -> Value {
        json!({
            "role": node.role.as_str(),
            "name": node.name,
            "value": if node.is_password {
                Value::Null
            } else {
                node.value.clone().map(Value::String).unwrap_or(Value::Null)
            },
            "automation_id": node.automation_id,
            "class_name": if node.class_name.is_empty() {
                Value::Null
            } else {
                Value::String(node.class_name.clone())
            },
            "bounds": node.bounds,
            "enabled": node.enabled,
            "is_password": node.is_password,
        })
    }

    fn build_nested_tree_json(
        tree: &SharedTreeData,
        idx: usize,
        base_depth: usize,
        max_depth: usize,
        ref_by_idx: &HashMap<usize, String>,
    ) -> Value {
        let node = &tree.nodes[idx];
        let rel_depth = node.depth.saturating_sub(base_depth);
        let mut children = Vec::new();
        if rel_depth < max_depth {
            let mut child_idx = idx + 1;
            while child_idx < node.subtree_end {
                children.push(build_nested_tree_json(
                    tree, child_idx, base_depth, max_depth, ref_by_idx,
                ));
                child_idx = tree.nodes[child_idx].subtree_end;
            }
        }
        let mut obj = serde_json::Map::new();
        if let Some(r) = ref_by_idx.get(&idx) {
            obj.insert("ref".to_string(), json!(r));
        }
        obj.insert("role".to_string(), json!(node.role.as_str()));
        if !node.name.is_empty() {
            obj.insert("name".to_string(), json!(node.name));
        }
        if let Some(aid) = &node.automation_id {
            obj.insert("automation_id".to_string(), json!(aid));
        }
        if node.is_password {
            obj.insert("is_password".to_string(), json!(true));
        } else if let Some(val) = &node.value {
            obj.insert("value".to_string(), json!(val));
        }
        if !children.is_empty() {
            obj.insert("children".to_string(), Value::Array(children));
        }
        Value::Object(obj)
    }

    // Snapshot formatting takes the tree metadata and mutable session ref map together so Linux and macOS produce identical payloads.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_snapshot_payload(
        tree: &SharedTreeData,
        pid: u32,
        hwnd: i64,
        refs: &mut BTreeMap<String, StoredRef>,
        scope_ref: Option<&str>,
        max_depth: usize,
        max_nodes: usize,
        update_refs: bool,
    ) -> Result<Value, String> {
        let (range_start, range_end, base_depth) = if let Some(ref_key) = scope_ref {
            let idx = resolve_node_in_tree(tree, None, Some(ref_key), refs)
                .map_err(|e| step_error_message(&e).to_string())?
                .ok_or_else(|| format!("snapshot scope ref `{ref_key}` was not found"))?;
            let node = &tree.nodes[idx];
            (idx, node.subtree_end, node.depth)
        } else {
            (0, tree.nodes.len(), 0)
        };

        let mut new_refs: BTreeMap<String, StoredRef> = BTreeMap::new();
        let mut elements: Vec<Value> = Vec::new();
        let mut next_id = 1usize;
        let mut truncated = tree.truncated;
        let mut ref_by_node_idx: HashMap<usize, String> = HashMap::new();

        for idx in range_start..range_end {
            let node = &tree.nodes[idx];
            let rel_depth = node.depth.saturating_sub(base_depth);
            if rel_depth > max_depth {
                truncated = true;
                continue;
            }
            if elements.len() >= max_nodes {
                truncated = true;
                break;
            }

            let is_leaf = node.subtree_end == idx + 1;
            let include_as_ref = idx > 0
                && (node.role.is_interactive()
                    || (node.role.is_readable_leaf_role() && is_leaf)
                    || (node.role == DesktopRole::Text && !node.name.is_empty()));

            if !include_as_ref {
                continue;
            }

            let ref_name = format!("e{next_id}");
            next_id += 1;

            let (locator, fragile) = synthesize_locator(idx, &tree.nodes);
            new_refs.insert(
                ref_name.clone(),
                StoredRef {
                    runtime_key: node.runtime_key.clone(),
                    locator: Some(locator),
                    fragile,
                },
            );
            ref_by_node_idx.insert(idx, ref_name.clone());

            let mut item = node_summary(node);
            if let Some(obj) = item.as_object_mut() {
                obj.insert("ref".to_string(), json!(ref_name));
            }
            elements.push(item);
        }

        if update_refs {
            *refs = new_refs;
        }

        let tree_json =
            build_nested_tree_json(tree, range_start, base_depth, max_depth, &ref_by_node_idx);

        Ok(json!({
            "kind": "desktop",
            "pid": pid,
            "hwnd": hwnd,
            "title": tree.title,
            "window": { "width": tree.win_width, "height": tree.win_height },
            "node_count": elements.len(),
            "truncated": truncated,
            "elements": elements,
            "tree": tree_json,
        }))
    }

    pub(super) fn build_evidence_items(
        pid: u32,
        hwnd: i64,
        fallback_title: &str,
        tree_res: Result<SharedTreeData, StepError>,
        png_res: Option<Result<Vec<u8>, StepError>>,
        refs: &mut BTreeMap<String, StoredRef>,
    ) -> Result<Vec<Evidence>, String> {
        let mut items = Vec::new();
        match tree_res {
            Ok(tree) => {
                let mut payload = build_snapshot_payload(
                    &tree,
                    pid,
                    hwnd,
                    refs,
                    None,
                    DEFAULT_SNAPSHOT_MAX_DEPTH,
                    MAX_COLLECTED_NODES,
                    false,
                )?;
                match png_res {
                    Some(Ok(png_bytes)) => {
                        items.push(Evidence::bytes("screenshot.png", png_bytes)?);
                    }
                    Some(Err(err)) => {
                        if let Some(obj) = payload.as_object_mut() {
                            obj.insert(
                                "capture_error".to_string(),
                                json!(step_error_message(&err)),
                            );
                        }
                    }
                    None => {}
                }
                let tree_json = serde_json::to_string_pretty(&payload)
                    .map_err(|e| format!("failed to serialize desktop accessibility tree: {e}"))?;
                items.push(Evidence::text("uia-tree.json", tree_json)?);
            }
            Err(err) => {
                let msg = step_error_message(&err);
                let payload = json!({
                    "kind": "desktop",
                    "pid": pid,
                    "hwnd": hwnd,
                    "title": fallback_title,
                    "window_closed": msg.contains("closed") || msg.contains("exited"),
                    "capture_error": msg,
                });
                let tree_json = serde_json::to_string_pretty(&payload)
                    .map_err(|e| format!("failed to serialize fallback desktop tree: {e}"))?;
                items.push(Evidence::text("uia-tree.json", tree_json)?);
            }
        }
        Ok(items)
    }

    #[cfg(unix)]
    pub(super) fn spawn_unix_desktop_child(
        command_path: &str,
        args: &[String],
        cwd: Option<&Path>,
        env: &BTreeMap<String, String>,
        extra_default_env: &[(&str, &str)],
        interactive: bool,
        raw_command: &str,
    ) -> Result<Child, StepError> {
        use std::os::unix::process::CommandExt;

        let mut cmd = Command::new(command_path);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for &(k, v) in extra_default_env {
            if !env.contains_key(k) && std::env::var_os(k).is_none() {
                cmd.env(k, v);
            }
        }
        if !env.is_empty() {
            cmd.envs(env);
        }
        if interactive {
            cmd.process_group(0);
        }
        cmd.spawn().map_err(|err| {
            StepError::Environment(format!(
                "failed to launch desktop command `{raw_command}`: {err}"
            ))
        })
    }

    #[cfg(unix)]
    pub(super) fn is_unix_pid_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[cfg(unix)]
    pub(super) fn terminate_unix_child(child: &mut Child, pid: u32) -> Result<(), String> {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            return Ok(());
        }
        let _ = child.kill();
        child
            .wait()
            .map(|_| ())
            .map_err(|err| format!("failed waiting for owned desktop child PID {pid}: {err}"))
    }

    #[cfg(unix)]
    pub(super) fn verify_unix_reattach_identity(
        pid: u32,
        owned_launch: bool,
        expected_token: Option<&str>,
        current_token: fn(u32) -> Option<String>,
    ) -> Result<(), String> {
        if pid == 0 {
            return Err(
                "invalid persisted desktop session handle: `pid` must be non-zero".to_string(),
            );
        }
        if !is_unix_pid_alive(pid) {
            return Err(format!(
                "desktop window closed: target process (PID {pid}) is no longer active"
            ));
        }
        if owned_launch {
            let expected = expected_token.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                format!(
                    "cannot verify owned desktop PID {pid}: session handle is missing `process_start_token`"
                )
            })?;
            match current_token(pid) {
                Some(live) if live == expected => {}
                Some(_) => {
                    return Err(format!(
                        "desktop window closed: target process (PID {pid}) is no longer active"
                    ));
                }
                None => {
                    if !is_unix_pid_alive(pid) {
                        return Err(format!(
                            "desktop window closed: target process (PID {pid}) is no longer active"
                        ));
                    }
                    return Err(format!(
                        "could not verify process identity for owned desktop session PID {pid}"
                    ));
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn terminate_unix_detached_pid(
        pid: u32,
        expected_token: Option<&str>,
        current_token: fn(u32) -> Option<String>,
    ) -> Result<(), String> {
        if pid == 0 {
            return Err("cannot terminate owned desktop PID 0: invalid PID".to_string());
        }
        let expected = expected_token.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
            format!(
                "cannot verify or terminate owned desktop PID {pid}: session handle is missing `process_start_token`"
            )
        })?;
        if !is_unix_pid_alive(pid) {
            return Ok(());
        }
        match current_token(pid) {
            Some(live) if live == expected => {}
            Some(_) => return Ok(()),
            None => {
                if !is_unix_pid_alive(pid) {
                    return Ok(());
                }
                return Err(format!(
                    "could not verify process identity for owned desktop PID {pid}; refusing to signal"
                ));
            }
        }

        let pid_str = pid.to_string();
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let deadline = Instant::now() + Duration::from_millis(800);
        while Instant::now() < deadline {
            if !is_unix_pid_alive(pid) {
                return Ok(());
            }
            if current_token(pid).as_deref() != Some(expected) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(25));
        }

        if current_token(pid).as_deref() == Some(expected) {
            let _ = Command::new("/bin/kill")
                .args(["-KILL", &pid_str])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        Ok(())
    }
}

#[cfg(test)]
mod shared_tests {
    use std::collections::BTreeMap;

    use super::shared::{
        BoundsRect, KeyModifier, MainKey, SharedNodeData, SharedTreeData, build_snapshot_payload,
        decode_png_to_rgba8, encode_rgba_png, mask_tree_inputs, parse_uia_key_expr,
        resolve_node_in_tree, reverse_lookup_in_tree, synthesize_locator,
    };
    use super::{DesktopRole, parse_key_spec};

    fn sample_tree() -> SharedTreeData {
        SharedTreeData {
            win_left: 100,
            win_top: 50,
            win_width: 400,
            win_height: 300,
            title: "Sample".to_string(),
            truncated: false,
            nodes: vec![
                SharedNodeData {
                    index: 0,
                    parent: None,
                    depth: 0,
                    subtree_end: 4,
                    runtime_key: "win".to_string(),
                    role: DesktopRole::Window,
                    name: "Sample".to_string(),
                    automation_id: None,
                    class_name: String::new(),
                    value: None,
                    enabled: true,
                    offscreen: false,
                    is_password: false,
                    screen_rect: (100, 50, 500, 350),
                    bounds: BoundsRect {
                        x: 0,
                        y: 0,
                        width: 400,
                        height: 300,
                    },
                },
                SharedNodeData {
                    index: 1,
                    parent: Some(0),
                    depth: 1,
                    subtree_end: 2,
                    runtime_key: "pw".to_string(),
                    role: DesktopRole::Edit,
                    name: "Password".to_string(),
                    automation_id: Some("passwordBox".to_string()),
                    class_name: String::new(),
                    value: None,
                    enabled: true,
                    offscreen: false,
                    is_password: true,
                    screen_rect: (120, 90, 320, 120),
                    bounds: BoundsRect {
                        x: 20,
                        y: 40,
                        width: 200,
                        height: 30,
                    },
                },
                SharedNodeData {
                    index: 2,
                    parent: Some(0),
                    depth: 1,
                    subtree_end: 3,
                    runtime_key: String::new(),
                    role: DesktopRole::Button,
                    name: "Submit".to_string(),
                    automation_id: Some("submitBtn".to_string()),
                    class_name: String::new(),
                    value: None,
                    enabled: true,
                    offscreen: false,
                    is_password: false,
                    screen_rect: (120, 140, 220, 170),
                    bounds: BoundsRect {
                        x: 20,
                        y: 90,
                        width: 100,
                        height: 30,
                    },
                },
                SharedNodeData {
                    index: 3,
                    parent: Some(0),
                    depth: 1,
                    subtree_end: 4,
                    runtime_key: "canvas".to_string(),
                    role: DesktopRole::Pane,
                    name: String::new(),
                    automation_id: Some("drawCanvas".to_string()),
                    class_name: String::new(),
                    value: None,
                    enabled: true,
                    offscreen: false,
                    is_password: false,
                    screen_rect: (120, 190, 320, 260),
                    bounds: BoundsRect {
                        x: 20,
                        y: 140,
                        width: 200,
                        height: 70,
                    },
                },
            ],
        }
    }

    #[test]
    fn key_spec_roundtrips_through_parse_uia_key_expr() {
        let ctrl_s = parse_key_spec("Ctrl+S").unwrap();
        assert_eq!(
            parse_uia_key_expr(&ctrl_s).unwrap(),
            (vec![KeyModifier::Ctrl], MainKey::Char('s'))
        );
        let cmd_shift_p = parse_key_spec("Cmd+Shift+P").unwrap();
        assert_eq!(
            parse_uia_key_expr(&cmd_shift_p).unwrap(),
            (
                vec![KeyModifier::Meta, KeyModifier::Shift],
                MainKey::Char('p')
            )
        );
        let opt_f4 = parse_key_spec("Option+F4").unwrap();
        assert_eq!(
            parse_uia_key_expr(&opt_f4).unwrap(),
            (vec![KeyModifier::Alt], MainKey::F(4))
        );
        let enter = parse_key_spec("Enter").unwrap();
        assert_eq!(
            parse_uia_key_expr(&enter).unwrap(),
            (vec![], MainKey::Enter)
        );
        assert_eq!(
            parse_uia_key_expr("^a").unwrap(),
            (vec![KeyModifier::Ctrl], MainKey::Char('a'))
        );
    }

    #[test]
    fn snapshot_refs_locator_fallback_and_reverse_lookup_work() {
        let tree = sample_tree();
        let mut refs = BTreeMap::new();
        let snap = build_snapshot_payload(&tree, 1234, 99, &mut refs, None, 8, 250, true).unwrap();
        assert_eq!(snap["node_count"], 3);

        // `submitBtn` had an empty runtime_key (simulating macOS cross-process reattach)
        // and must resolve cleanly via its synthesized locator!
        let submit_idx = resolve_node_in_tree(&tree, None, Some("e2"), &refs)
            .unwrap()
            .expect("resolve e2 via locator");
        assert_eq!(submit_idx, 2);
        assert_eq!(
            synthesize_locator(2, &tree.nodes),
            (serde_json::json!({ "automation_id": "submitBtn" }), false)
        );

        // Reverse lookup at button center yields non-fragile locator, while canvas stays None.
        let btn_hit = reverse_lookup_in_tree(&tree, 150, 155);
        assert_eq!(
            btn_hit,
            Some((serde_json::json!({ "automation_id": "submitBtn" }), false))
        );
        assert_eq!(reverse_lookup_in_tree(&tree, 180, 210), None);
    }

    #[test]
    fn retina_scaled_masking_and_png_roundtrip_work() {
        let tree = sample_tree();
        // Simulate 2x Retina capture (800x600 physical pixels for 400x300 logical window).
        let (w, h) = (800u32, 600u32);
        let mut rgba = vec![200u8; (w * h * 4) as usize];
        for px in rgba.as_chunks_mut::<4>().0 {
            *px = [40, 210, 220, 255];
        }
        let masked_count = mask_tree_inputs(&mut rgba, w, h, &tree);
        assert_eq!(masked_count, 1);

        let png_bytes = encode_rgba_png(&rgba, w, h).unwrap();
        let (dw, dh, decoded) = decode_png_to_rgba8(&png_bytes).unwrap();
        assert_eq!((dw, dh), (w, h));

        // Center of password box in 2x coordinates: logical (120, 55) -> physical (240, 110).
        let masked_idx = ((110 * w + 240) * 4) as usize;
        assert_eq!(&decoded[masked_idx..masked_idx + 4], &[0, 0, 0, 255]);

        // Outside input (10, 10) remains unmasked cyan.
        let outside_idx = ((10 * w + 10) * 4) as usize;
        assert_eq!(&decoded[outside_idx..outside_idx + 4], &[40, 210, 220, 255]);
    }

    #[test]
    fn shared_bounds_evidence_lock_and_metadata_helpers_work() {
        use super::shared::{
            DEFAULT_SNAPSHOT_MAX_DEPTH, DEFAULT_SNAPSHOT_MAX_NODES, MAX_COLLECTED_NODES,
            POLL_INTERVAL, PersistedHandle, acquire_desktop_input_lock, build_evidence_items,
            check_window_point_bounds, click_button_name, missing_target_timeout,
            resolve_command_and_cwd, step_error_message, wait_state_name, write_png_private,
        };
        use super::{ClickButton, DesktopPoint, LaunchArgs, WaitState, WindowSize};

        const { assert!(MAX_COLLECTED_NODES >= DEFAULT_SNAPSHOT_MAX_NODES) };
        const { assert!(DEFAULT_SNAPSHOT_MAX_DEPTH > 0) };
        assert!(!POLL_INTERVAL.is_zero());

        let tree = sample_tree();
        assert_eq!((tree.win_left, tree.win_top), (100, 50));

        let _lock = acquire_desktop_input_lock(1_000).expect("acquire desktop input lock");

        let ok_pt = DesktopPoint {
            x: 10,
            y: 20,
            window: Some(WindowSize {
                width: 400,
                height: 300,
            }),
        };
        assert!(check_window_point_bounds(&tree, &ok_pt).is_ok());

        let bad_win = DesktopPoint {
            x: 10,
            y: 20,
            window: Some(WindowSize {
                width: 999,
                height: 300,
            }),
        };
        assert!(check_window_point_bounds(&tree, &bad_win).is_err());

        assert_eq!(click_button_name(ClickButton::Left), "left");
        assert_eq!(click_button_name(ClickButton::Right), "right");
        assert_eq!(click_button_name(ClickButton::Double), "double");
        assert_eq!(wait_state_name(WaitState::Present), "present");
        assert_eq!(wait_state_name(WaitState::Gone), "gone");
        assert_eq!(wait_state_name(WaitState::Enabled), "enabled");
        assert_eq!(wait_state_name(WaitState::Visible), "visible");

        let err = missing_target_timeout(Some("e1"), 250);
        assert!(step_error_message(&err).contains("e1"));

        let launch_args = LaunchArgs {
            command: "demo-app".to_string(),
            args: Vec::new(),
            cwd: Some("sub".to_string()),
            env: BTreeMap::new(),
            window_title_contains: None,
            timeout_ms: 5_000,
        };
        let (cmd, cwd) = resolve_command_and_cwd(&launch_args, std::path::Path::new("."));
        assert_eq!(cmd, "demo-app");
        assert!(cwd.is_some());

        let handle = PersistedHandle {
            pid: 42,
            hwnd: 7,
            owned_launch: true,
            process_start_token: Some("tok".to_string()),
            local_only_reason: None,
            title: "Sample".to_string(),
            refs: BTreeMap::new(),
        };
        let serialized = serde_json::to_value(&handle).unwrap();
        let round: PersistedHandle = serde_json::from_value(serialized).unwrap();
        assert_eq!(round.pid, 42);
        assert_eq!(round.hwnd, 7);
        assert!(round.owned_launch);
        assert_eq!(round.process_start_token.as_deref(), Some("tok"));
        assert!(round.local_only_reason.is_none());
        assert_eq!(round.title, "Sample");
        assert!(round.refs.is_empty());

        let png_bytes = encode_rgba_png(&[10, 20, 30, 255], 1, 1).unwrap();
        let tmp_png =
            std::env::temp_dir().join(format!("da-shared-png-test-{}.png", std::process::id()));
        write_png_private(&tmp_png, &png_bytes).unwrap();
        assert!(tmp_png.is_file());
        let _ = std::fs::remove_file(&tmp_png);

        let mut refs = BTreeMap::new();
        let evidence =
            build_evidence_items(42, 7, "Sample", Ok(tree), Some(Ok(png_bytes)), &mut refs)
                .unwrap();
        assert_eq!(evidence.len(), 2);
    }
}
