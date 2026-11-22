//! Native desktop GUI automation (`desktop.*`) steps and session management.
//!
//! Tier 1 implements Windows UI Automation (`#84`) behind `#[cfg(windows)]`.
//! Platform-neutral step schemas, locator rules, and session contracts live
//! here so macOS and Linux backends (`#88`) plug into the exact same API.

#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
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
        "alt" | "menu" | "lalt" => Some("{alt}"),
        "ralt" => Some("{ralt}"),
        "shift" | "lshift" => Some("{shift}"),
        "rshift" => Some("{rshift}"),
        "win" | "windows" | "lwin" => Some("{win}"),
        "rwin" => Some("{rwin}"),
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
