//! The `browser.*` steps: drive one Chrome page across a case.
//!
//! The first `browser.open` of a case creates a [`BrowserSession`] through the
//! session contract. Every later step of the same case asks [`Sessions`] for the
//! same session, so it sees the page the first step opened; the runner closes
//! the browser when the case ends, whatever the case's result was.
//!
//! A case either starts its own browser or, with `connect`, drives a browser the
//! user already runs with a debug port. The connected browser is never closed:
//! only the tab this case opened is.
//!
//! Locators are deterministic on purpose: a CSS selector, an XPath expression,
//! visible text or an accessibility role and name, never a coordinate or a
//! screenshot. Element lookup waits for the element to appear and to be visible,
//! so a step does not race the page's own scripts.
//!
//! The protocol lives in [`cdp`], the process in [`launch`], the loopback
//! DevTools lookup in [`connect`], and the `browser.network` observer in
//! [`network`]; this module turns their messages into step results and step
//! errors.

mod cdp;
mod connect;
mod launch;
mod network;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use self::cdp::{
    CdpError, Connection, Deadline, DialogPolicy, DialogWatch, OpenedDialog, RemoteResult,
};
use self::connect::Endpoint;
use self::launch::Launched;
use crate::core::paths;
use crate::core::registry::{Registry, StepDefinition, StepError, allow_retry, disallow_retry};
use crate::core::session::{Evidence, Session, Sessions};

/// Session kind the browser steps share.
pub const SESSION_KIND: &str = "browser";

/// Step names, in the order they are registered.
pub const STEP_NAMES: [&str; 7] = [
    "browser.open",
    "browser.click",
    "browser.type",
    "browser.press",
    "browser.wait",
    "browser.read",
    "browser.screenshot",
];

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_NAVIGATION_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;
/// How long starting the browser and opening its page may take.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// How often an element or URL is looked at again.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Registers every browser step and the network observer.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    for definition in [
        open_step()?,
        click_step()?,
        type_step()?,
        press_step()?,
        wait_step(
            STEP_NAMES[4],
            "Waits for the element the locator names, or for the URL to contain `url_contains`, and returns the `url`.",
            validate_wait,
            run_wait,
        )?,
        read_step()?,
        screenshot_step()?,
    ] {
        registry.register(definition)?;
    }
    network::register(registry)?;
    Ok(())
}

/// Default HTML attribute matched by `testid` locators.
const DEFAULT_TESTID_ATTRIBUTE: &str = "data-testid";

/// Builds `browser.open`, whose `connect` is a plain string in the schema.
fn open_step() -> Result<StepDefinition, String> {
    let mut definition = step::<OpenArgs>(
        STEP_NAMES[0],
        "Opens `url` in the case's browser, or in a browser the user already runs when `connect` is given, and returns the page's `url` and `title`.",
        validate_open,
        run_open,
    )?;
    string_property(&mut definition.args_schema, "connect");
    string_property(&mut definition.args_schema, "testid_attribute");
    apply_dialog_schema(&mut definition.args_schema);
    Ok(definition)
}

/// Builds `browser.click`, with optional `dialog` handling.
fn click_step() -> Result<StepDefinition, String> {
    let mut definition = locator_step::<ClickArgs>(
        STEP_NAMES[1],
        "Clicks the element the locator names and returns its `text` when it has one.",
        validate_click,
        run_click,
    )?;
    apply_dialog_schema(&mut definition.args_schema);
    Ok(definition)
}

/// Builds `browser.type`, whose `value` is a plain string in the schema.
///
/// `text` has two meanings here: it is the typing text unless `value` is also
/// given, and only then does it name the element. The schema must accept the
/// common `css` + `text` form, so its `text` branch also requires `value`.
fn type_step() -> Result<StepDefinition, String> {
    let mut definition = locator_step::<TypeArgs>(
        STEP_NAMES[2],
        "Types `text`, or `value` when `text` names the element, into the element the locator names.",
        validate_type,
        run_type,
    )?;
    string_property(&mut definition.args_schema, "value");
    apply_dialog_schema(&mut definition.args_schema);
    definition.args_schema["oneOf"] = json!([
        { "required": ["css"] },
        { "required": ["xpath"] },
        { "required": ["text", "value"] },
        { "required": ["role"] },
        { "required": ["testid"] }
    ]);
    Ok(definition)
}

/// Builds `browser.press`, with optional `dialog` handling.
fn press_step() -> Result<StepDefinition, String> {
    let mut definition = step::<PressArgs>(
        STEP_NAMES[3],
        "Presses `key`, such as `Enter` or `Tab`, on the focused element.",
        validate_press,
        run_press,
    )?;
    apply_dialog_schema(&mut definition.args_schema);
    Ok(definition)
}

/// Builds `browser.read`, which is read-only and allows `retry`.
fn read_step() -> Result<StepDefinition, String> {
    let mut definition = locator_step::<ReadArgs>(
        STEP_NAMES[5],
        "Returns the `text` of the element the locator names, plus its `value` when `attribute` is given.",
        validate_read,
        run_read,
    )?;
    definition.check_retry = allow_retry;
    Ok(definition)
}

/// Builds `browser.screenshot`, which takes only the output path.
fn screenshot_step() -> Result<StepDefinition, String> {
    step::<ScreenshotArgs>(
        STEP_NAMES[6],
        "Writes a PNG of the page to `path` after covering every visible editable field, and returns the `path` and the number of `bytes` written.",
        validate_screenshot,
        run_screenshot,
    )
}

/// Builds one step from its argument type.
fn step<T: JsonSchema>(
    name: &'static str,
    description: &'static str,
    validate: fn(&Value) -> Result<(), String>,
    run: fn(&Value, &mut Sessions) -> Result<Value, StepError>,
) -> Result<StepDefinition, String> {
    let args_schema = serde_json::to_value(schema_for!(T))
        .map_err(|error| format!("could not build the schema of `{name}`: {error}"))?;
    Ok(StepDefinition {
        name,
        description,
        args_schema,
        validate,
        check_retry: disallow_retry,
        run,
    })
}

/// Builds a step whose locator arguments are exactly one of the five forms.
fn locator_step<T: JsonSchema>(
    name: &'static str,
    description: &'static str,
    validate: fn(&Value) -> Result<(), String>,
    run: fn(&Value, &mut Sessions) -> Result<Value, StepError>,
) -> Result<StepDefinition, String> {
    let mut definition = step::<T>(name, description, validate, run)?;
    apply_locator_schema(&mut definition.args_schema, name);
    exactly_one_of(
        &mut definition.args_schema,
        &["css", "xpath", "text", "role", "testid"],
    );
    Ok(definition)
}

/// Builds `browser.wait`, which takes a locator or `url_contains`.
fn wait_step(
    name: &'static str,
    description: &'static str,
    validate: fn(&Value) -> Result<(), String>,
    run: fn(&Value, &mut Sessions) -> Result<Value, StepError>,
) -> Result<StepDefinition, String> {
    let mut definition = step::<WaitArgs>(name, description, validate, run)?;
    definition.check_retry = allow_retry;
    apply_locator_schema(&mut definition.args_schema, name);
    string_property(&mut definition.args_schema, "url_contains");
    apply_dialog_schema(&mut definition.args_schema);
    definition.args_schema["oneOf"] = json!([
        { "required": ["css"] },
        { "required": ["xpath"] },
        { "required": ["text"] },
        { "required": ["role"] },
        { "required": ["testid"] },
        {
            "required": ["url_contains"],
            "not": {
                "anyOf": [
                    { "required": ["within"] },
                    { "required": ["nth"] },
                    { "required": ["name"] }
                ]
            }
        }
    ]);
    Ok(definition)
}

/// Normalizes optional locator properties, `within` embedding, and `name -> role`
/// dependency on a step schema.
fn apply_locator_schema(schema: &mut Value, step_name: &str) {
    for property in ["css", "xpath", "text", "role", "name", "testid"] {
        string_property(schema, property);
    }
    integer_property(schema, "nth");
    if let Value::Object(entries) = schema {
        // Root `da schema` embeds step schemas under `$defs[step_name]`, so keep
        // recursive `within` self-contained with its own `$id` instead of `#/$defs/...`.
        entries.remove("$defs");
        entries.insert("dependentRequired".to_string(), json!({ "name": ["role"] }));
    }
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert("within".to_string(), within_schema(step_name));
    }
}

/// Self-contained JSON Schema resource for recursive `within` containers.
fn within_schema(step_name: &str) -> Value {
    json!({
        "$id": format!("urn:da:schema:{step_name}:within"),
        "description": "Container locator to narrow the search to a uniquely matched element subtree.",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "css": { "type": "string" },
            "xpath": { "type": "string" },
            "text": { "type": "string" },
            "role": { "type": "string" },
            "name": { "type": "string" },
            "testid": { "type": "string", "minLength": 1 },
            "within": { "$ref": "#" },
            "nth": {
                "type": "integer",
                "format": "uint64",
                "minimum": 0
            }
        },
        "oneOf": [
            { "required": ["css"] },
            { "required": ["xpath"] },
            { "required": ["text"] },
            { "required": ["role"] },
            { "required": ["testid"] }
        ],
        "dependentRequired": {
            "name": ["role"]
        }
    })
}

/// Replaces `dialog` with a self-contained schema that rejects `null`.
fn apply_dialog_schema(schema: &mut Value) {
    if let Value::Object(entries) = schema {
        entries.remove("$defs");
    }
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert("dialog".to_string(), dialog_schema_value());
    }
}

/// Self-contained JSON Schema for the optional `dialog` step option.
fn dialog_schema_value() -> Value {
    json!({
        "description": "How to respond if this step opens a JavaScript dialog (`alert`, `confirm`, `prompt`, or `beforeunload`).",
        "oneOf": [
            {
                "type": "string",
                "enum": ["accept", "dismiss"]
            },
            {
                "type": "object",
                "additionalProperties": false,
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["accept", "dismiss"]
                    },
                    "prompt_text": {
                        "type": "string"
                    }
                }
            }
        ]
    })
}

/// Action taken when a step encounters a JavaScript dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DialogAction {
    Accept,
    Dismiss,
}

impl DialogAction {
    fn is_accept(self) -> bool {
        matches!(self, Self::Accept)
    }
}

/// Detailed dialog handling configuration for a browser step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct DialogOptions {
    action: DialogAction,
    #[serde(default)]
    prompt_text: Option<String>,
}

/// Optional `dialog` argument accepted by interactive browser steps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum DialogArg {
    Action(DialogAction),
    Options(DialogOptions),
}

impl DialogArg {
    fn to_policy(&self) -> DialogPolicy {
        match self {
            Self::Action(action) => DialogPolicy::Handle {
                accept: action.is_accept(),
                prompt_text: None,
            },
            Self::Options(options) => DialogPolicy::Handle {
                accept: options.action.is_accept(),
                prompt_text: options.prompt_text.clone(),
            },
        }
    }
}

impl JsonSchema for DialogArg {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("DialogArg")
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Replaced by `apply_dialog_schema` so `Option<DialogArg>` never emits `null`.
        schemars::Schema::from(serde_json::Map::new())
    }
}

/// Per-step dialog watch state passed through page-session CDP waits.
struct DialogContext {
    policy: DialogPolicy,
    opened: Option<OpenedDialog>,
}

impl DialogContext {
    fn fail_fast() -> Self {
        Self {
            policy: DialogPolicy::FailFast,
            opened: None,
        }
    }

    fn from_arg(arg: Option<&DialogArg>) -> Self {
        match arg {
            None => Self::fail_fast(),
            Some(arg) => Self {
                policy: arg.to_policy(),
                opened: None,
            },
        }
    }

    fn watch<'a>(&'a mut self, session_id: &'a str) -> DialogWatch<'a> {
        DialogWatch {
            session_id,
            policy: &self.policy,
            opened: &mut self.opened,
        }
    }

    fn is_fail_fast_triggered(&self) -> bool {
        matches!(self.policy, DialogPolicy::FailFast) && self.opened.is_some()
    }

    fn take_result(&mut self) -> Option<Value> {
        self.opened.take().map(|dialog| {
            json!({
                "type": dialog.dialog_type,
                "message": dialog.message,
                "accepted": dialog.accepted,
            })
        })
    }
}

/// Attaches the handled `dialog` summary to `result` when a dialog opened.
fn attach_dialog(mut result: Value, dialog: &mut DialogContext) -> Value {
    if let Some(info) = dialog.take_result()
        && let Some(entries) = result.as_object_mut()
    {
        entries.insert("dialog".to_string(), info);
    }
    result
}

/// Makes one optional property a string in the emitted schema.
///
/// `Option<String>` would otherwise also accept `null`; the step's `validate`
/// refuses it, so the schema must refuse it too.
fn string_property(schema: &mut Value, field: &str) {
    let Some(property) = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(field))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    property.insert("type".to_string(), json!("string"));
}

/// Makes one optional property an integer in the emitted schema.
fn integer_property(schema: &mut Value, field: &str) {
    let Some(property) = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .and_then(|properties| properties.get_mut(field))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    property.insert("type".to_string(), json!("integer"));
}

/// Requires exactly one of `fields` to be present.
///
/// `oneOf` is the JSON Schema form of "exactly one": a value with two of the
/// fields matches two branches and is refused, and a value with none matches
/// none.
fn exactly_one_of(schema: &mut Value, fields: &[&str]) {
    let branches: Vec<Value> = fields
        .iter()
        .map(|field| json!({ "required": [field] }))
        .collect();
    if let Value::Object(entries) = schema {
        entries.insert("oneOf".to_string(), Value::Array(branches));
    }
}

/// Arguments of `browser.open`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenArgs {
    /// Absolute URL of the page to open, including the scheme.
    #[schemars(extend("minLength" = 1))]
    url: String,
    /// Loopback DevTools endpoint of a browser the user already runs, for
    /// example `http://127.0.0.1:9222`. The case opens a new tab there and
    /// closes only that tab. Cannot be combined with `headless`, `width` or
    /// `height`.
    #[serde(default)]
    connect: Option<String>,
    /// Run Chrome without a visible window. Defaults to `true`.
    #[serde(default = "default_headless")]
    headless: bool,
    /// Window width in pixels. Defaults to 1280.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_width")]
    width: u32,
    /// Window height in pixels. Defaults to 800.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_height")]
    height: u32,
    /// How long the page may take to finish loading, in milliseconds. Defaults
    /// to 30000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_navigation_timeout_ms")]
    timeout_ms: u64,
    /// Attribute name matched by `testid` locators. Defaults to `data-testid`.
    #[schemars(extend("minLength" = 1, "pattern" = "^\\S+$"))]
    #[serde(default)]
    testid_attribute: Option<String>,
    /// How to respond if navigating opens a JavaScript dialog.
    #[serde(default)]
    dialog: Option<DialogArg>,
}

/// Container locator used by `within` to narrow a search to a subtree.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WithinArgs {
    /// CSS selector of the container element.
    #[serde(default)]
    css: Option<String>,
    /// XPath expression of the container element.
    #[serde(default)]
    xpath: Option<String>,
    /// Visible text of the container element, with whitespace normalized.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role of the container element, for example `region`.
    #[serde(default)]
    role: Option<String>,
    /// Accessibility name of the container element; only meaningful with `role`.
    #[serde(default)]
    name: Option<String>,
    /// Test id attribute value of the container element.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    testid: Option<String>,
    /// Outer container locator to narrow this container further.
    #[serde(default)]
    within: Option<Box<WithinArgs>>,
    /// Zero-based index when this container locator matches more than one element.
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<u64>,
}

/// Arguments of `browser.click`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ClickArgs {
    /// CSS selector of the element.
    #[serde(default)]
    css: Option<String>,
    /// XPath expression of the element.
    #[serde(default)]
    xpath: Option<String>,
    /// Visible text of the element, with whitespace normalized.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role of the element, for example `button`.
    #[serde(default)]
    role: Option<String>,
    /// Accessibility name of the element; only meaningful with `role`.
    #[serde(default)]
    name: Option<String>,
    /// Test id attribute value of the element.
    #[serde(default)]
    testid: Option<String>,
    /// Container locator to narrow the search to a uniquely matched element.
    #[serde(default)]
    within: Option<Box<WithinArgs>>,
    /// Zero-based index when the locator matches more than one element.
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<u64>,
    /// How long to wait for the element, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// How to respond if clicking opens a JavaScript dialog.
    #[serde(default)]
    dialog: Option<DialogArg>,
}

/// Arguments of `browser.type`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TypeArgs {
    /// CSS selector of the element.
    #[serde(default)]
    css: Option<String>,
    /// XPath expression of the element.
    #[serde(default)]
    xpath: Option<String>,
    /// Text to type into the element, or the visible text of the element when
    /// `value` gives the text to type.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role of the element, for example `textbox`.
    #[serde(default)]
    role: Option<String>,
    /// Accessibility name of the element; only meaningful with `role`.
    #[serde(default)]
    name: Option<String>,
    /// Test id attribute value of the element.
    #[serde(default)]
    testid: Option<String>,
    /// Text to type when `text` names the element instead.
    #[serde(default)]
    value: Option<String>,
    /// Container locator to narrow the search to a uniquely matched element.
    #[serde(default)]
    within: Option<Box<WithinArgs>>,
    /// Zero-based index when the locator matches more than one element.
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<u64>,
    /// How long to wait for the element, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// How to respond if typing opens a JavaScript dialog.
    #[serde(default)]
    dialog: Option<DialogArg>,
}

/// Arguments of `browser.press`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PressArgs {
    /// Key name, for example `Enter` or `Tab`.
    #[schemars(extend("minLength" = 1))]
    key: String,
    /// How to respond if pressing the key opens a JavaScript dialog.
    #[serde(default)]
    dialog: Option<DialogArg>,
}

/// Arguments of `browser.wait`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    /// CSS selector of the element.
    #[serde(default)]
    css: Option<String>,
    /// XPath expression of the element.
    #[serde(default)]
    xpath: Option<String>,
    /// Visible text of the element, with whitespace normalized.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role of the element, for example `button`.
    #[serde(default)]
    role: Option<String>,
    /// Accessibility name of the element; only meaningful with `role`.
    #[serde(default)]
    name: Option<String>,
    /// Test id attribute value of the element.
    #[serde(default)]
    testid: Option<String>,
    /// Text the current URL must contain.
    #[serde(default)]
    url_contains: Option<String>,
    /// Container locator to narrow the search to a uniquely matched element.
    #[serde(default)]
    within: Option<Box<WithinArgs>>,
    /// Zero-based index when the locator matches more than one element.
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<u64>,
    /// How long to wait, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// How to respond if waiting encounters a JavaScript dialog.
    #[serde(default)]
    dialog: Option<DialogArg>,
}

/// Arguments of `browser.screenshot`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotArgs {
    /// File to write. A relative path is resolved against the directory that
    /// holds the case file.
    #[schemars(extend("minLength" = 1))]
    path: String,
}

/// Arguments of `browser.read`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    /// CSS selector of the element.
    #[serde(default)]
    css: Option<String>,
    /// XPath expression of the element.
    #[serde(default)]
    xpath: Option<String>,
    /// Visible text of the element, with whitespace normalized.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role of the element, for example `button`.
    #[serde(default)]
    role: Option<String>,
    /// Accessibility name of the element; only meaningful with `role`.
    #[serde(default)]
    name: Option<String>,
    /// Test id attribute value of the element.
    #[serde(default)]
    testid: Option<String>,
    /// Container locator to narrow the search to a uniquely matched element.
    #[serde(default)]
    within: Option<Box<WithinArgs>>,
    /// Zero-based index when the locator matches more than one element.
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    nth: Option<u64>,
    /// How long to wait for the element, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// Attribute whose value is returned as `value`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    attribute: Option<String>,
}

fn default_headless() -> bool {
    true
}

fn default_width() -> u32 {
    DEFAULT_WIDTH
}

fn default_height() -> u32 {
    DEFAULT_HEIGHT
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

fn default_navigation_timeout_ms() -> u64 {
    DEFAULT_NAVIGATION_TIMEOUT_MS
}

/// How long all failure evidence of one browser session may take.
const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one explicit `browser.screenshot` may take.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long removing the masks may take after the screenshot itself.
const MASK_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

/// JavaScript that covers every visible editable field with an opaque box.
///
/// `position: fixed` uses the viewport rectangle `getBoundingClientRect`
/// returns, and `z-index` is the largest value a browser accepts. The element
/// stays in the page only until the screenshot is captured; cleanup is a
/// separate call so it can run after a failed capture too. The page is not
/// notified when the masks appear or disappear.
const MASK_INPUTS_SCRIPT: &str = r#"(function () {
  var skipped = ['button', 'submit', 'reset', 'checkbox', 'radio', 'image', 'hidden', 'range', 'color'];
  var fields = document.querySelectorAll('input, textarea, select, [contenteditable]');
  var masks = [];
  for (var i = 0; i < fields.length; i++) {
    var el = fields[i];
    if (el.nodeType !== 1) continue;
    var tag = el.tagName;
    var field = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT';
    if (!field && !el.isContentEditable) continue;
    if (tag === 'INPUT' && skipped.indexOf((el.type || 'text').toLowerCase()) !== -1) continue;
    var style = window.getComputedStyle(el);
    var rect = el.getBoundingClientRect();
    if (style.display === 'none' || style.visibility === 'hidden' || style.visibility === 'collapse' || rect.width <= 0 || rect.height <= 0) continue;
    var mask = document.createElement('div');
    mask.setAttribute('data-da-screenshot-mask', '');
    mask.style.cssText = 'position:fixed;box-sizing:border-box;left:' + rect.left + 'px;top:' + rect.top + 'px;width:' + rect.width + 'px;height:' + rect.height + 'px;background:#000;opacity:1;border:0;padding:0;margin:0;z-index:2147483647;pointer-events:none;';
    document.documentElement.appendChild(mask);
    masks.push(mask);
  }
  return masks.length;
})()"#;

/// JavaScript that removes every mask this module added.
const REMOVE_MASKS_SCRIPT: &str = r#"(function () {
  var masks = document.querySelectorAll('[data-da-screenshot-mask]');
  for (var i = 0; i < masks.length; i++) masks[i].remove();
  return masks.length;
})()"#;

/// A deterministic way to name one element.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Locator {
    /// A CSS selector.
    Css(String),
    /// An XPath expression.
    Xpath(String),
    /// The visible text of an element, with whitespace normalized.
    Text(String),
    /// One accessibility role, optionally narrowed by an exact name.
    Role { role: String, name: Option<String> },
    /// An exact match on the session's configured test-id attribute.
    TestId(String),
}

/// One element target: a locator optionally narrowed by `within` and indexed by `nth`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ElementTarget {
    locator: Locator,
    within: Option<Box<ElementTarget>>,
    nth: Option<usize>,
}

impl ElementTarget {
    /// Builds a target from a parsed locator and its optional `within` / `nth` modifiers.
    fn from_parts(
        locator: Locator,
        within: Option<&WithinArgs>,
        nth: Option<u64>,
    ) -> Result<Self, String> {
        let within = within.map(Self::from_within).transpose()?.map(Box::new);
        let nth = nth
            .map(|index| {
                usize::try_from(index).map_err(|_| {
                    "invalid arguments: `nth` is too large for this platform".to_string()
                })
            })
            .transpose()?;
        Ok(Self {
            locator,
            within,
            nth,
        })
    }

    /// Parses a recursive `within` container into an [`ElementTarget`].
    fn from_within(args: &WithinArgs) -> Result<Self, String> {
        let locator = Locator::from_args(
            &args.css,
            &args.xpath,
            &args.text,
            &args.role,
            &args.name,
            &args.testid,
        )?;
        Self::from_parts(locator, args.within.as_deref(), args.nth)
    }

    /// Human-readable description of the target, including modifiers.
    fn describe_with(&self, testid_attribute: &str) -> String {
        let mut out = self.locator.describe_with(testid_attribute);
        if let Some(index) = self.nth {
            out.push_str(&format!(" (nth: {index})"));
        }
        if let Some(container) = &self.within {
            out.push_str(&format!(
                " within ({})",
                container.describe_with(testid_attribute)
            ));
        }
        out
    }
}

impl Locator {
    /// Reads exactly one locator from the optional arguments.
    ///
    /// `name` is not a locator of its own: it narrows `role`, so a case that
    /// gives only `name` is refused instead of being read as a broad match.
    fn from_args(
        css: &Option<String>,
        xpath: &Option<String>,
        text: &Option<String>,
        role: &Option<String>,
        name: &Option<String>,
        testid: &Option<String>,
    ) -> Result<Self, String> {
        if name.is_some() && role.is_none() {
            return Err(
                "invalid arguments: `name` can only be used with `role`; give `role` and `name` together"
                    .to_string(),
            );
        }
        let given = [
            css.is_some(),
            xpath.is_some(),
            text.is_some(),
            role.is_some(),
            testid.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if given == 0 {
            return Err(
                "invalid arguments: give one locator: `css`, `xpath`, `text`, `role` or `testid`"
                    .to_string(),
            );
        }
        if given > 1 {
            return Err(
                "invalid arguments: locator fields cannot both be set; give exactly one of `css`, `xpath`, `text`, `role` and `testid`"
                    .to_string(),
            );
        }
        if let Some(value) = css {
            if value.trim().is_empty() {
                return Err("invalid arguments: `css` must not be empty".to_string());
            }
            return Ok(Self::Css(value.to_string()));
        }
        if let Some(value) = xpath {
            if value.trim().is_empty() {
                return Err("invalid arguments: `xpath` must not be empty".to_string());
            }
            return Ok(Self::Xpath(value.to_string()));
        }
        if let Some(value) = text {
            if value.trim().is_empty() {
                return Err("invalid arguments: `text` must not be empty".to_string());
            }
            return Ok(Self::Text(value.to_string()));
        }
        if let Some(value) = testid {
            if value.trim().is_empty() {
                return Err("invalid arguments: `testid` must not be empty".to_string());
            }
            return Ok(Self::TestId(value.to_string()));
        }
        let Some(role) = role else {
            return Err(
                "invalid arguments: give one locator: `css`, `xpath`, `text`, `role` or `testid`"
                    .to_string(),
            );
        };
        if role.trim().is_empty() {
            return Err("invalid arguments: `role` must not be empty".to_string());
        }
        if let Some(name) = name
            && name.trim().is_empty()
        {
            return Err("invalid arguments: `name` must not be empty".to_string());
        }
        Ok(Self::Role {
            role: role.clone(),
            name: name.clone(),
        })
    }

    /// The locator as an error message names it, using `testid_attribute` for `TestId`.
    fn describe_with(&self, testid_attribute: &str) -> String {
        match self {
            Self::Css(selector) => format!("CSS selector `{selector}`"),
            Self::Xpath(expression) => format!("XPath `{expression}`"),
            Self::Text(text) => format!("visible text `{text}`"),
            Self::Role {
                role,
                name: Some(name),
            } => format!("role `{role}` named `{name}`"),
            Self::Role { role, name: None } => format!("role `{role}`"),
            Self::TestId(id) => format!("test id `{id}` (`{testid_attribute}`)"),
        }
    }

    /// A JavaScript function body that resolves all candidates inside `root`
    /// (`document` or a `within` element) and returns either the selected
    /// element or a JSON status string (`missing`, `ambiguous`, `out_of_range`).
    ///
    /// Every user string is passed through `serde_json` so a quote or a
    /// backslash in it can never end the string literal or add code.
    fn resolve_body_with(&self, testid_attribute: &str, nth: Option<usize>) -> Option<String> {
        let collect = match self {
            Self::Css(selector) => format!(
                "var matches = Array.prototype.slice.call(root.querySelectorAll({}));\nvar hiddenFound = false;",
                json_string(selector)
            ),
            Self::Xpath(expression) => format!(
                "var snap = document.evaluate({}, root, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);\nvar matches = [];\nfor (var i = 0; i < snap.snapshotLength; i++) {{\n  var node = snap.snapshotItem(i);\n  if (node && node.nodeType === 1 && (root === document || (node !== root && root.contains(node)))) matches.push(node);\n}}\nvar hiddenFound = false;",
                json_string(expression)
            ),
            Self::TestId(id) => format!(
                "var attr = {};\nvar wanted = {};\nvar all = root.getElementsByTagName('*');\nvar matches = [];\nfor (var i = 0; i < all.length; i++) {{\n  if (all[i].getAttribute(attr) === wanted) matches.push(all[i]);\n}}\nvar hiddenFound = false;",
                json_string(testid_attribute),
                json_string(id)
            ),
            Self::Text(text) => text_collect_script(text),
            // The accessibility tree names this element; there is no selector
            // to evaluate in the page.
            Self::Role { .. } => return None,
        };
        let wanted_nth = match nth {
            Some(index) => index.to_string(),
            None => "null".to_string(),
        };
        Some(format!(
            "{collect}\nvar wantedNth = {wanted_nth};\n{RESOLVE_CANDIDATES_HELPER}"
        ))
    }
}

/// The body of a focus lookup, run with the located element as `el`.
const FOCUS_BODY: &str = "if (!el) return false; el.focus(); return document.activeElement === el;";

/// The body of a text read, run with the located element as `el`.
const TEXT_BODY: &str =
    "if (!el) return ''; return String(el.textContent === null ? '' : el.textContent).trim();";

/// The body of a visibility read, run with the located element as `el`.
const VISIBLE_BODY: &str = "if (!el || el.nodeType !== 1) return false; var style = window.getComputedStyle(el); var rect = el.getBoundingClientRect(); return style.display !== 'none' && style.visibility !== 'hidden' && style.visibility !== 'collapse' && rect.width > 0 && rect.height > 0;";

/// Summarizes one DOM element (`el`) as `{ tag, id, text }` for multi-match diagnostics.
const SUMMARIZE_ELEMENT_BODY: &str = r#"
if (!el || el.nodeType !== 1) return { tag: 'element', id: '', text: '' };
var tag = el.tagName ? el.tagName.toLowerCase() : 'element';
var id = el.id ? String(el.id) : '';
var raw = (el.innerText && el.innerText.trim()) ? el.innerText : el.textContent;
var text = String(raw === null || raw === undefined ? '' : raw).replace(/\s+/g, ' ').trim();
if (text.length > 240) text = text.slice(0, 240);
return { tag: tag, id: id, text: text };
"#;

/// Shared tail for `resolve_body_with`: checks candidate count / `nth` and
/// returns either the matched `Element` or a JSON status string.
const RESOLVE_CANDIDATES_HELPER: &str = r#"
var summarize = function (list) {
  var out = [];
  for (var i = 0; i < list.length && i < 5; i++) {
    var el = list[i];
    var tag = el.tagName ? el.tagName.toLowerCase() : 'element';
    var id = el.id ? String(el.id) : '';
    var raw = (el.innerText && el.innerText.trim()) ? el.innerText : el.textContent;
    var text = String(raw === null || raw === undefined ? '' : raw).replace(/\s+/g, ' ').trim();
    if (text.length > 240) text = text.slice(0, 240);
    out.push({ tag: tag, id: id, text: text });
  }
  return out;
};
if (matches.length === 0) {
  return JSON.stringify({ status: 'missing', hidden_found: hiddenFound });
}
if (wantedNth === null) {
  if (matches.length > 1) {
    return JSON.stringify({ status: 'ambiguous', count: matches.length, candidates: summarize(matches) });
  }
  return matches[0];
}
if (wantedNth >= matches.length) {
  return JSON.stringify({ status: 'out_of_range', count: matches.length, hidden_found: hiddenFound });
}
return matches[wantedNth];
"#;

/// JavaScript that collects every innermost visible element inside `root`
/// whose normalized `innerText` equals `__DA_WANTED__`.
const TEXT_COLLECT_TEMPLATE: &str = r#"var wanted = __DA_WANTED__;
var normalize = function (value) {
  return String(value === null || value === undefined ? '' : value).replace(/\s+/g, ' ').trim();
};
var visible = function (el) {
  var style = window.getComputedStyle(el);
  var rect = el.getBoundingClientRect();
  return style.display !== 'none' && style.visibility !== 'hidden' && style.visibility !== 'collapse' && rect.width > 0 && rect.height > 0;
};
var all = root.querySelectorAll('*');
var hiddenFound = false;
var visibleMatches = [];
for (var i = 0; i < all.length; i++) {
  var el = all[i];
  if (normalize(el.innerText) === wanted) {
    if (visible(el)) visibleMatches.push(el);
    else hiddenFound = true;
  }
}
var matches = [];
for (var j = 0; j < visibleMatches.length; j++) {
  var candidate = visibleMatches[j];
  var innermost = true;
  for (var k = 0; k < visibleMatches.length; k++) {
    if (k !== j && candidate.contains(visibleMatches[k])) { innermost = false; break; }
  }
  if (innermost) matches.push(candidate);
}"#;

/// Builds the text candidate collection snippet for one value.
fn text_collect_script(text: &str) -> String {
    TEXT_COLLECT_TEMPLATE.replace("__DA_WANTED__", &json_string(text))
}

/// One matched element summary returned for ambiguous-locator and occlusion errors.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
struct CandidateInfo {
    tag: String,
    id: String,
    text: String,
}

impl CandidateInfo {
    /// Formats `<tag#id> "text"` with text truncated to at most 60 characters.
    fn format_label(&self) -> String {
        let mut label = format!("<{}", self.tag);
        if !self.id.is_empty() {
            label.push('#');
            label.push_str(&self.id);
        }
        label.push('>');
        let text = truncate_chars(self.text.trim(), 60);
        if !text.is_empty() {
            label.push_str(&format!(" \"{text}\""));
        }
        label
    }
}

/// Inspects an element (`el`) for visibility, disabled state, geometry, and hit-test.
const ACTIONABLE_INSPECT_BODY: &str = r#"
var summarizeNode = function (node) {
  var tag = node.tagName ? node.tagName.toLowerCase() : 'element';
  var id = node.id ? String(node.id) : '';
  var raw = (node.innerText && node.innerText.trim()) ? node.innerText : node.textContent;
  var text = String(raw === null || raw === undefined ? '' : raw).replace(/\s+/g, ' ').trim();
  if (text.length > 240) text = text.slice(0, 240);
  return { tag: tag, id: id, text: text };
};
if (!el || el.nodeType !== 1) return { status: 'not_visible' };
var style = window.getComputedStyle(el);
var rect = el.getBoundingClientRect();
if (
  style.display === 'none' ||
  style.visibility === 'hidden' ||
  style.visibility === 'collapse' ||
  rect.width <= 0 ||
  rect.height <= 0
) {
  return { status: 'not_visible' };
}
var fs = typeof el.closest === 'function' ? el.closest('fieldset[disabled]') : null;
var inDisabledFieldset = false;
if (fs) {
  var legend = fs.querySelector('legend');
  if (!legend || !legend.contains(el)) inDisabledFieldset = true;
}
var ariaNode = typeof el.closest === 'function' ? el.closest('[aria-disabled]') : null;
var ariaDisabled = ariaNode !== null && String(ariaNode.getAttribute('aria-disabled') || '').toLowerCase() === 'true';
if (el.disabled === true || el.hasAttribute('disabled') || inDisabledFieldset || ariaDisabled) {
  return { status: 'disabled' };
}
var vw = window.innerWidth || document.documentElement.clientWidth || 0;
var vh = window.innerHeight || document.documentElement.clientHeight || 0;
var cx0 = rect.left + rect.width / 2;
var cy0 = rect.top + rect.height / 2;
if (cx0 < 0 || cx0 >= vw || cy0 < 0 || cy0 >= vh) {
  el.scrollIntoView({ block: 'center', inline: 'center' });
  rect = el.getBoundingClientRect();
}
var cx = rect.left + rect.width / 2;
var cy = rect.top + rect.height / 2;
var hit = document.elementFromPoint(cx, cy);
var hitOk = hit !== null && (hit === el || el.contains(hit));
var blocker = !hitOk && hit ? summarizeNode(hit) : null;
return {
  status: 'ready_candidate',
  rect: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
  x: cx,
  y: cy,
  hit_ok: hitOk,
  blocker: blocker
};
"#;

/// Viewport rectangle used to check stability across consecutive polls.
#[derive(Clone, Copy, Debug, Deserialize)]
struct ElementRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl ElementRect {
    fn same_geometry(self, other: Self) -> bool {
        (self.x - other.x).abs() <= 0.5
            && (self.y - other.y).abs() <= 0.5
            && (self.width - other.width).abs() <= 0.5
            && (self.height - other.height).abs() <= 0.5
    }
}

/// JSON result returned by `ACTIONABLE_INSPECT_BODY`.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ActionableInspect {
    NotVisible,
    Disabled,
    ReadyCandidate {
        rect: ElementRect,
        x: f64,
        y: f64,
        hit_ok: bool,
        blocker: Option<CandidateInfo>,
    },
}

/// Status returned by the page candidate-resolution script when no single element was picked.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PageResolveStatus {
    Missing {
        hidden_found: bool,
    },
    Ambiguous {
        count: usize,
        candidates: Vec<CandidateInfo>,
    },
    OutOfRange {
        count: usize,
        hidden_found: bool,
    },
}

/// Formats up to 5 candidates with tag name, id, and `<= 60`-char text.
fn format_candidates(candidates: &[CandidateInfo], total: usize) -> String {
    let mut parts = Vec::with_capacity(candidates.len().min(5));
    for (index, candidate) in candidates.iter().take(5).enumerate() {
        parts.push(format!("{}: {}", index + 1, candidate.format_label()));
    }
    if total > 5 {
        format!("{} (showing 5 of {total})", parts.join(", "))
    } else {
        parts.join(", ")
    }
}

/// Truncates `text` to at most `max_chars` Unicode scalar values.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let mut out: String = text.chars().take(keep).collect();
    out.push_str("...");
    out
}

/// Builds the immediate failure error when a locator without `nth` matches multiple elements.
fn ambiguous_error(target_desc: &str, total: usize, candidates: &[CandidateInfo]) -> StepError {
    StepError::Execution(format!(
        "{target_desc} matched {total} elements; narrow the locator or pass `nth` (candidates: {})",
        format_candidates(candidates, total)
    ))
}

/// Encodes text as a JavaScript string literal.
fn json_string(value: &str) -> String {
    // `to_string` of a JSON string cannot fail: the value is already UTF-8.
    Value::String(value.to_string()).to_string()
}

/// What `browser.wait` waits for.
enum WaitTarget {
    /// An element.
    Element(ElementTarget),
    /// The current URL containing some text.
    Url(String),
}

impl WaitTarget {
    /// Reads exactly one of the accepted forms.
    fn from_args(args: &WaitArgs) -> Result<Self, String> {
        let has_locator = args.css.is_some()
            || args.xpath.is_some()
            || args.text.is_some()
            || args.role.is_some()
            || args.testid.is_some();
        if let Some(text) = args.url_contains.as_deref() {
            if has_locator || args.name.is_some() || args.within.is_some() || args.nth.is_some() {
                return Err(
                    "invalid arguments: `url_contains` cannot be combined with a locator or modifier (`within`, `nth`, `name`); give exactly one of `css`, `xpath`, `text`, `role`, `testid` and `url_contains`"
                        .to_string(),
                );
            }
            if text.trim().is_empty() {
                return Err("invalid arguments: `url_contains` must not be empty".to_string());
            }
            return Ok(Self::Url(text.to_string()));
        }
        if !has_locator && args.name.is_none() {
            return Err(
                "invalid arguments: give exactly one of `css`, `xpath`, `text`, `role`, `testid` and `url_contains`"
                    .to_string(),
            );
        }
        let locator = Locator::from_args(
            &args.css,
            &args.xpath,
            &args.text,
            &args.role,
            &args.name,
            &args.testid,
        )?;
        ElementTarget::from_parts(locator, args.within.as_deref(), args.nth).map(Self::Element)
    }
}

/// One key `browser.press` can send.
struct Key {
    key: String,
    code: String,
    virtual_key_code: i64,
    text: Option<String>,
}

impl Key {
    /// Reads a named key or one printable character.
    fn parse(value: &str) -> Result<Self, String> {
        if let Some((key, code, virtual_key_code, text)) = named_key(value) {
            return Ok(Self {
                key: key.to_string(),
                code: code.to_string(),
                virtual_key_code,
                text: text.map(str::to_string),
            });
        }
        let mut characters = value.chars();
        if let (Some(character), None) = (characters.next(), characters.next())
            && !character.is_control()
        {
            return Ok(Self {
                key: character.to_string(),
                code: code_for_char(character),
                virtual_key_code: if character.is_ascii() {
                    i64::from(character.to_ascii_uppercase() as u8)
                } else {
                    0
                },
                text: Some(character.to_string()),
            });
        }
        Err(format!(
            "invalid arguments: `{value}` is not a key name; use `Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, an arrow key, `Home`, `End`, `PageUp`, `PageDown`, `Space`, or a single printable character"
        ))
    }

    /// The parameters of the key-down event this key sends.
    fn down_params(&self) -> Value {
        self.event_params(
            if self.text.is_some() {
                "keyDown"
            } else {
                "rawKeyDown"
            },
            true,
        )
    }

    /// The parameters of the key-up event this key sends.
    fn up_params(&self) -> Value {
        self.event_params("keyUp", false)
    }

    /// One `Input.dispatchKeyEvent` parameter object.
    fn event_params(&self, event_type: &str, with_text: bool) -> Value {
        let mut params = json!({
            "type": event_type,
            "key": self.key,
            "windowsVirtualKeyCode": self.virtual_key_code,
            "nativeVirtualKeyCode": self.virtual_key_code,
        });
        if !self.code.is_empty() {
            params["code"] = json!(self.code);
        }
        if with_text && let Some(text) = &self.text {
            params["text"] = json!(text);
            params["unmodifiedText"] = json!(text);
        }
        params
    }
}

/// The named keys `browser.press` accepts, with their DOM key, code, virtual
/// key code and the text a key-down event carries.
fn named_key(name: &str) -> Option<(&'static str, &'static str, i64, Option<&'static str>)> {
    Some(match name {
        "Enter" => ("Enter", "Enter", 13, Some("\r")),
        "Tab" => ("Tab", "Tab", 9, None),
        "Escape" => ("Escape", "Escape", 27, None),
        "Backspace" => ("Backspace", "Backspace", 8, None),
        "Delete" => ("Delete", "Delete", 46, None),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38, None),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40, None),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37, None),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39, None),
        "Home" => ("Home", "Home", 36, None),
        "End" => ("End", "End", 35, None),
        "PageUp" => ("PageUp", "PageUp", 33, None),
        "PageDown" => ("PageDown", "PageDown", 34, None),
        "Space" => (" ", "Space", 32, Some(" ")),
        _ => return None,
    })
}

/// The DOM `code` of a printable character, when it has a conventional one.
fn code_for_char(character: char) -> String {
    if character.is_ascii_alphabetic() {
        format!("Key{}", character.to_ascii_uppercase())
    } else if character.is_ascii_digit() {
        format!("Digit{character}")
    } else if character == ' ' {
        "Space".to_string()
    } else {
        String::new()
    }
}

/// How the case's browser was obtained.
enum Origin {
    /// The case started this browser process and ends it when the case ends.
    Launched(Launched),
    /// The case connected to the user's browser and closes only its own tab.
    Connected,
}

/// One case's browser, its page, and the options that created it.
struct BrowserSession {
    /// Taken by [`Session::close`], which releases it before the case report is
    /// written. `Drop` only matters on the fallback path.
    connection: Option<Connection>,
    origin: Option<Origin>,
    /// The CDP target id attached for this session's page.
    target_id: String,
    /// The target session every page command is sent with.
    page: String,
    /// The requests this page reported, bounded and merged by request id.
    network: network::NetworkState,
    port: u16,
    headless: bool,
    width: u32,
    height: u32,
    /// The connected endpoint, as `connect` named it; `None` when the case
    /// started its own browser.
    connect: Option<String>,
    /// Attribute name matched by `testid` locators in this session.
    testid_attribute: String,
}

/// One element resolved from a target, held as a CDP DOM `objectId`.
struct Located(String);

impl BrowserSession {
    /// Creates the session described by `browser.open`.
    fn open(args: &OpenArgs, connect: Option<&Endpoint>) -> Result<Self, String> {
        match connect {
            Some(endpoint) => Self::connect_to(endpoint, args.testid_attribute.as_deref()),
            None => Self::launch(args),
        }
    }

    /// Starts the browser described by `browser.open` and attaches its page.
    fn launch(args: &OpenArgs) -> Result<Self, String> {
        let browser = Launched::launch(
            args.headless,
            args.width,
            args.height,
            Deadline::after(LAUNCH_TIMEOUT),
        )?;
        let port = browser.port();
        let mut connection = Connection::connect(
            browser.websocket_url(),
            Deadline::after(LAUNCH_TIMEOUT),
        )
        .map_err(|error| {
            format!("could not connect to the browser it started: {error}; set `DA_CHROME` to the Chrome or Chromium executable")
        })?;
        let (page, target_id) = attach(&mut connection, Deadline::after(LAUNCH_TIMEOUT))?;
        let network = network::NetworkState::new(&page);

        Ok(Self {
            connection: Some(connection),
            origin: Some(Origin::Launched(browser)),
            target_id,
            page,
            network,
            port,
            headless: args.headless,
            width: args.width,
            height: args.height,
            connect: None,
            testid_attribute: args
                .testid_attribute
                .clone()
                .unwrap_or_else(|| DEFAULT_TESTID_ATTRIBUTE.to_string()),
        })
    }

    /// Attaches a page in a browser the user already runs.
    fn connect_to(endpoint: &Endpoint, testid_attribute: Option<&str>) -> Result<Self, String> {
        let websocket_url = endpoint.websocket_url(connect::CONNECT_TIMEOUT)?;
        let mut connection = Connection::connect(&websocket_url, Deadline::after(LAUNCH_TIMEOUT))
            .map_err(|error| {
            endpoint.explain(format!("could not open the DevTools connection: {error}"))
        })?;
        let (page, target_id) = attach(&mut connection, Deadline::after(LAUNCH_TIMEOUT))?;
        let network = network::NetworkState::new(&page);

        Ok(Self {
            connection: Some(connection),
            origin: Some(Origin::Connected),
            target_id,
            page,
            network,
            port: endpoint.port(),
            headless: true,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            connect: Some(endpoint.name()),
            testid_attribute: testid_attribute
                .unwrap_or(DEFAULT_TESTID_ATTRIBUTE)
                .to_string(),
        })
    }

    /// Refuses an option that only applies when the session is created.
    fn ensure_same_options(
        &self,
        args: &OpenArgs,
        connect: Option<&Endpoint>,
    ) -> Result<(), StepError> {
        let mut changes = Vec::new();
        let wanted_connect = connect.map(Endpoint::name);
        if wanted_connect.as_deref() != self.connect.as_deref() {
            changes.push(format!(
                "`connect` from `{}` to `{}`",
                self.connect.as_deref().unwrap_or("none"),
                wanted_connect.as_deref().unwrap_or("none")
            ));
        }
        if args.headless != self.headless {
            changes.push(format!(
                "`headless` from `{}` to `{}`",
                self.headless, args.headless
            ));
        }
        if args.width != self.width {
            changes.push(format!("`width` from {} to {}", self.width, args.width));
        }
        if args.height != self.height {
            changes.push(format!("`height` from {} to {}", self.height, args.height));
        }
        if changes.is_empty() {
            return Ok(());
        }
        Err(StepError::Execution(format!(
            "`browser.open` changed {} after the browser session was created; the first `browser.open` of a case sets `connect`, `headless`, `width` and `height`, so repeat those values or open the page in a new case",
            changes.join(", ")
        )))
    }

    /// Merges the network events already received into the request table.
    ///
    /// Messages that are not network events go back to the queue, so the page
    /// events a later step waits for are still there.
    fn absorb_network(&mut self) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        self.network.absorb(connection);
    }

    /// Reads the network events that already arrived, without waiting.
    fn poll_network(&mut self) -> Result<(), String> {
        let Some(connection) = self.connection.as_mut() else {
            return Ok(());
        };
        self.network.poll(connection)
    }

    /// How many network messages and requests this page discarded so far.
    fn network_dropped(&self) -> u64 {
        self.network.dropped()
    }

    /// Takes the requests finished since the last collection.
    fn take_network_step(&mut self, dropped_baseline: u64) -> Value {
        self.network.take_step(dropped_baseline)
    }

    /// Navigates the session's page and returns its current URL and title.
    fn navigate(
        &mut self,
        url: &str,
        timeout: Duration,
        dialog: &mut DialogContext,
    ) -> Result<(String, String), StepError> {
        let scope = self.scope();
        let deadline = Deadline::after(timeout);
        // Network events that arrive before a navigation still belong to a
        // request of this page, so they are merged before the rest is cleared.
        self.absorb_network();
        let connection = self.connection_mut()?;
        // Events from the about:blank page must not be mistaken for this
        // navigation's load event, while any terminal target/session event or
        // pending dialog opening stays queued so navigation handles it at once.
        let terminal_events: Vec<Value> = connection
            .take_messages()
            .into_iter()
            .filter(|event| {
                scope.is_terminal(event) || cdp::is_dialog_opening_for(event, &scope.session_id)
            })
            .collect();
        connection.restore_messages(terminal_events);
        let reply = page_call(
            connection,
            &scope,
            "Page.navigate",
            json!({ "url": url }),
            deadline,
            dialog,
        )
        .map_err(|failure| {
            let action = if failure.is_timeout() {
                format!("could not open `{url}` within {} ms", timeout.as_millis())
            } else {
                format!("could not open `{url}`")
            };
            failure.step(&action, timeout_or_environment)
        })?;
        let beforeunload_dismissed = dialog
            .opened
            .as_ref()
            .is_some_and(|opened| opened.dialog_type == "beforeunload" && !opened.accepted);
        if let Some(error_text) = reply.get("errorText").and_then(Value::as_str)
            && !(beforeunload_dismissed && error_text == "net::ERR_ABORTED")
        {
            return Err(StepError::Environment(format!(
                "could not open `{url}`: {error_text}"
            )));
        }
        // Chrome omits `loaderId` for a same-document navigation (a fragment
        // or history change): no new document loads, so no load event fires.
        if reply.get("loaderId").is_some() && !beforeunload_dismissed {
            wait_for_load(connection, &scope, url, deadline, dialog)?;
        }

        let current = page_evaluate(connection, &scope, "location.href", deadline, dialog)
            .map(|value| value.as_str().unwrap_or(url).to_string())
            .map_err(|failure| load_error(failure, url, "read the URL of", deadline))?;
        let title = page_evaluate(connection, &scope, "document.title", deadline, dialog)
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .map_err(|failure| load_error(failure, url, "read the title of", deadline))?;
        Ok((current, title))
    }

    /// Waits for the target element to exist, match uniquely (or `nth`), and be visible.
    fn locate_target(
        &mut self,
        target: &ElementTarget,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Located, StepError> {
        let is_locator_visible_only =
            matches!(target.locator, Locator::Text(_) | Locator::Role { .. });
        self.poll_target(
            target,
            deadline,
            dialog,
            |session, target_desc, object_id, dialog| {
                let visible = if is_locator_visible_only {
                    true
                } else {
                    session.call_function_poll(
                        target_desc,
                        &object_id,
                        VISIBLE_BODY,
                        "check the visibility of",
                        deadline,
                        dialog,
                    )? == Value::Bool(true)
                };
                if visible {
                    Ok(Ok(Located(object_id)))
                } else {
                    Ok(Err(PendingReason::Hidden))
                }
            },
        )
    }

    /// Waits for the target element to exist, be visible, enabled, stable across
    /// two consecutive polls, and unoccluded at its click point.
    fn locate_actionable_target(
        &mut self,
        target: &ElementTarget,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<ActionableTarget, StepError> {
        let mut previous_rect: Option<ElementRect> = None;
        self.poll_target(
            target,
            deadline,
            dialog,
            |session, target_desc, object_id, dialog| {
                let raw = session.call_function_poll(
                    target_desc,
                    &object_id,
                    ACTIONABLE_INSPECT_BODY,
                    "inspect the actionability of",
                    deadline,
                    dialog,
                )?;
                let inspect: ActionableInspect = serde_json::from_value(raw).map_err(|error| {
                    LocateError::Step(StepError::Environment(format!(
                        "the browser returned an invalid actionability state for {target_desc}: {error}"
                    )))
                })?;
                match evaluate_actionable_inspect(&mut previous_rect, inspect) {
                    Ok((x, y)) => Ok(Ok(ActionableTarget {
                        located: Located(object_id),
                        x,
                        y,
                    })),
                    Err(reason) => Ok(Err(reason)),
                }
            },
        )
    }

    /// Shared polling loop for `locate_target` and `locate_actionable_target`.
    fn poll_target<T>(
        &mut self,
        target: &ElementTarget,
        deadline: Deadline,
        dialog: &mut DialogContext,
        mut check: impl FnMut(
            &mut Self,
            &str,
            String,
            &mut DialogContext,
        ) -> Result<Result<T, PendingReason>, LocateError>,
    ) -> Result<T, StepError> {
        let target_desc = target.describe_with(&self.testid_attribute);
        let mut last_pending = (target_desc.clone(), PendingReason::Missing);
        loop {
            let poll = match self.resolve_target_once(target, deadline, dialog) {
                Ok(poll) => poll,
                Err(LocateError::Timeout) => {
                    return Err(pending_timeout_error(
                        &last_pending.0,
                        last_pending.1,
                        deadline,
                    ));
                }
                Err(LocateError::Step(error)) => return Err(error),
            };
            match poll {
                ResolvePoll::Found(object_id) => match check(self, &target_desc, object_id, dialog)
                {
                    Ok(Ok(ready)) => return Ok(ready),
                    Ok(Err(reason)) => {
                        last_pending = (target_desc.clone(), reason);
                    }
                    Err(LocateError::Timeout) => {
                        return Err(pending_timeout_error(
                            &last_pending.0,
                            last_pending.1,
                            deadline,
                        ));
                    }
                    Err(LocateError::Step(error)) => return Err(error),
                },
                ResolvePoll::Pending {
                    target_desc: desc,
                    reason,
                } => {
                    last_pending = (desc, reason);
                }
            }
            if deadline.passed() {
                return Err(pending_timeout_error(
                    &last_pending.0,
                    last_pending.1,
                    deadline,
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Runs one action body inside a poll loop, preserving `LocateError::Timeout`.
    fn call_function_poll(
        &mut self,
        target_desc: &str,
        object_id: &str,
        body: &str,
        action: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Value, LocateError> {
        let scope = self.scope();
        let function = format!("function () {{ var el = this; {body} }}");
        let connection = self.connection_mut().map_err(LocateError::Step)?;
        page_call_function(connection, &scope, object_id, &function, deadline, dialog)
            .map_err(|failure| action_poll_error(target_desc, action, failure))
    }

    /// Runs one non-blocking resolution pass over `target` and its `within` chain.
    fn resolve_target_once(
        &mut self,
        target: &ElementTarget,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<ResolvePoll, LocateError> {
        let root = match &target.within {
            Some(container) => match self.resolve_target_once(container, deadline, dialog)? {
                ResolvePoll::Found(container_id) => Some(container_id),
                pending @ ResolvePoll::Pending { .. } => return Ok(pending),
            },
            None => None,
        };
        let testid_attribute = self.testid_attribute.clone();
        let target_desc = target.describe_with(&testid_attribute);
        if let Locator::Role { role, name } = &target.locator {
            return self.resolve_role_once(
                &target_desc,
                role,
                name.as_deref(),
                target.nth,
                root.as_deref(),
                deadline,
                dialog,
            );
        }
        let body = target
            .locator
            .resolve_body_with(&testid_attribute, target.nth)
            .ok_or_else(|| {
                LocateError::Step(StepError::Execution(format!(
                    "could not use {target_desc}: the locator has no page expression"
                )))
            })?;
        let scope = self.scope();
        let remote = match root.as_deref() {
            Some(root_id) => {
                let fn_decl = format!("function () {{ var root = this; {body} }}");
                let connection = self.connection_mut().map_err(LocateError::Step)?;
                page_call_function_remote(connection, &scope, root_id, &fn_decl, deadline, dialog)
            }
            None => {
                let expr = format!("(function () {{ var root = document; {body} }})()");
                let connection = self.connection_mut().map_err(LocateError::Step)?;
                page_evaluate_remote(connection, &scope, &expr, deadline, dialog)
            }
        }
        .map_err(|failure| locate_error(&target_desc, failure))?;

        match remote {
            RemoteResult::Object(object_id) => Ok(ResolvePoll::Found(object_id)),
            RemoteResult::Value(Value::String(raw_json)) => {
                let status: PageResolveStatus = serde_json::from_str(&raw_json).map_err(|error| {
                    LocateError::Step(StepError::Environment(format!(
                        "the browser returned an invalid resolution status for {target_desc}: {error}"
                    )))
                })?;
                match status {
                    PageResolveStatus::Missing { hidden_found } => Ok(ResolvePoll::Pending {
                        target_desc,
                        reason: if hidden_found {
                            PendingReason::Hidden
                        } else {
                            PendingReason::Missing
                        },
                    }),
                    PageResolveStatus::OutOfRange {
                        count,
                        hidden_found,
                    } => {
                        let index = target.nth.unwrap_or(0);
                        let reason = if count == 0 && hidden_found {
                            PendingReason::Hidden
                        } else if count == 0 {
                            PendingReason::Missing
                        } else {
                            PendingReason::OutOfRange { index, count }
                        };
                        Ok(ResolvePoll::Pending {
                            target_desc,
                            reason,
                        })
                    }
                    PageResolveStatus::Ambiguous { count, candidates } => Err(LocateError::Step(
                        ambiguous_error(&target_desc, count, &candidates),
                    )),
                }
            }
            RemoteResult::Value(other) => Err(LocateError::Step(StepError::Environment(format!(
                "the browser returned an unexpected value while looking up {target_desc}: {other}"
            )))),
        }
    }

    /// Runs one non-blocking resolution pass for a `role` locator inside `root`.
    #[allow(clippy::too_many_arguments)]
    // Splitting role, name, nth, root, deadline, and dialog across a single-use struct would obscure the direct poll arguments.
    fn resolve_role_once(
        &mut self,
        target_desc: &str,
        role: &str,
        name: Option<&str>,
        nth: Option<usize>,
        root: Option<&str>,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<ResolvePoll, LocateError> {
        let (matched, objects) =
            self.role_objects(target_desc, role, name, root, deadline, dialog)?;
        let mut visible_objects = Vec::new();
        for object_id in objects {
            let visible = self.call_function_poll(
                target_desc,
                &object_id,
                VISIBLE_BODY,
                "check the visibility of",
                deadline,
                dialog,
            )?;
            if visible == Value::Bool(true) {
                visible_objects.push(object_id);
            }
        }
        if visible_objects.is_empty() {
            return Ok(ResolvePoll::Pending {
                target_desc: target_desc.to_string(),
                reason: if matched {
                    PendingReason::Hidden
                } else {
                    PendingReason::Missing
                },
            });
        }
        match nth {
            None => {
                if visible_objects.len() > 1 {
                    let mut candidates = Vec::with_capacity(visible_objects.len().min(5));
                    for object_id in visible_objects.iter().take(5) {
                        let summary_val = self.call_function_poll(
                            target_desc,
                            object_id,
                            SUMMARIZE_ELEMENT_BODY,
                            "summarize",
                            deadline,
                            dialog,
                        )?;
                        let candidate: CandidateInfo = serde_json::from_value(summary_val)
                            .unwrap_or(CandidateInfo {
                                tag: "element".to_string(),
                                id: String::new(),
                                text: String::new(),
                            });
                        candidates.push(candidate);
                    }
                    return Err(LocateError::Step(ambiguous_error(
                        target_desc,
                        visible_objects.len(),
                        &candidates,
                    )));
                }
                Ok(ResolvePoll::Found(
                    visible_objects.into_iter().next().unwrap_or_default(),
                ))
            }
            Some(index) => {
                if index >= visible_objects.len() {
                    Ok(ResolvePoll::Pending {
                        target_desc: target_desc.to_string(),
                        reason: PendingReason::OutOfRange {
                            index,
                            count: visible_objects.len(),
                        },
                    })
                } else {
                    Ok(ResolvePoll::Found(visible_objects.swap_remove(index)))
                }
            }
        }
    }

    /// Queries the accessibility tree and resolves its matching DOM nodes.
    ///
    /// `queryAXTree` already filters by role and accessible name; the checks
    /// repeat both so a browser that filters loosely cannot match the wrong
    /// name. Only non-ignored nodes count.
    fn role_objects(
        &mut self,
        target_desc: &str,
        role: &str,
        name: Option<&str>,
        root: Option<&str>,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<(bool, Vec<String>), LocateError> {
        let scope = self.scope();
        let mut params = if let Some(root_id) = root {
            json!({ "objectId": root_id, "role": role })
        } else {
            let connection = self.connection_mut().map_err(LocateError::Step)?;
            let document = page_call(
                connection,
                &scope,
                "DOM.getDocument",
                json!({ "depth": 0 }),
                deadline,
                dialog,
            )
            .map_err(|failure| locate_error(target_desc, failure))?;
            let node_id = document
                .pointer("/root/nodeId")
                .and_then(Value::as_i64)
                .ok_or_else(|| {
                    LocateError::Step(StepError::Environment(
                        "the browser did not return the document root while looking up a role"
                            .to_string(),
                    ))
                })?;
            json!({ "nodeId": node_id, "role": role })
        };
        if let Some(name) = name {
            params["accessibleName"] = json!(name);
        }
        let connection = self.connection_mut().map_err(LocateError::Step)?;
        let reply = page_call(
            connection,
            &scope,
            "Accessibility.queryAXTree",
            params,
            deadline,
            dialog,
        )
        .map_err(|failure| locate_error(target_desc, failure))?;
        let nodes = reply
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut matched = false;
        let mut objects = Vec::new();
        for node in nodes {
            if node.get("ignored").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if node.pointer("/role/value").and_then(Value::as_str) != Some(role) {
                continue;
            }
            if let Some(name) = name
                && node.pointer("/name/value").and_then(Value::as_str) != Some(name)
            {
                continue;
            }
            let Some(backend_node_id) = node.get("backendDOMNodeId").and_then(Value::as_i64) else {
                continue;
            };
            let connection = self.connection_mut().map_err(LocateError::Step)?;
            let resolved = page_call(
                connection,
                &scope,
                "DOM.resolveNode",
                json!({ "backendNodeId": backend_node_id }),
                deadline,
                dialog,
            )
            .map_err(|failure| locate_error(target_desc, failure))?;
            if let Some(object_id) = resolved.pointer("/object/objectId").and_then(Value::as_str) {
                if let Some(root_id) = root {
                    let connection = self.connection_mut().map_err(LocateError::Step)?;
                    let inside = page_call_function_with_object(
                        connection,
                        &scope,
                        root_id,
                        object_id,
                        "function (el) { return Boolean(el) && this !== el && this.contains(el); }",
                        deadline,
                        dialog,
                    )
                    .map_err(|failure| locate_error(target_desc, failure))?;
                    if inside != Value::Bool(true) {
                        continue;
                    }
                }
                matched = true;
                objects.push(object_id.to_string());
            }
        }
        Ok((matched, objects))
    }

    /// Runs one action body on a located element and maps its errors.
    fn run_on(
        &mut self,
        target_desc: &str,
        located: &Located,
        body: &str,
        action: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Value, StepError> {
        self.call_function_on(target_desc, &located.0, body, action, deadline, dialog)
    }

    /// Runs one action body with a resolved DOM node as `this`.
    fn call_function_on(
        &mut self,
        target_desc: &str,
        object_id: &str,
        body: &str,
        action: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Value, StepError> {
        let scope = self.scope();
        let function = format!("function () {{ var el = this; {body} }}");
        let connection = self.connection_mut()?;
        page_call_function(connection, &scope, object_id, &function, deadline, dialog)
            .map_err(|failure| evaluate_error(target_desc, failure, action, deadline))
    }

    /// Sends one page command for a step and maps its errors.
    fn dispatch(
        &mut self,
        target_desc: &str,
        method: &str,
        params: Value,
        action: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<(), StepError> {
        let scope = self.scope();
        let connection = self.connection_mut()?;
        page_call(connection, &scope, method, params, deadline, dialog)
            .map(|_| ())
            .map_err(|failure| {
                failure.step(
                    &format!("could not {action} {target_desc}"),
                    execution_error,
                )
            })
    }

    /// The open connection, or the error a step reports after a close.
    fn connection_mut(&mut self) -> Result<&mut Connection, StepError> {
        self.connection.as_mut().ok_or_else(|| {
            StepError::Execution(
                "the browser session is already closed; run `browser.open` again".to_string(),
            )
        })
    }

    /// The target and session every page-session wait watches.
    fn scope(&self) -> PageScope {
        PageScope {
            target_id: self.target_id.clone(),
            session_id: self.page.clone(),
        }
    }

    /// Covers every editable field, captures a PNG and removes the covers.
    ///
    /// Cleanup runs after a failed capture as well; a browser that has already
    /// gone cannot be cleaned, and then the capture error is the one reported.
    fn capture_masked_png(&mut self, deadline: Deadline) -> Result<Vec<u8>, StepError> {
        let scope = self.scope();
        let mut dialog = DialogContext::fail_fast();
        page_evaluate(
            self.connection_mut()?,
            &scope,
            MASK_INPUTS_SCRIPT,
            deadline,
            &mut dialog,
        )
        .map_err(|failure| {
            failure.environment("could not cover the editable fields before the screenshot")
        })?;

        let captured = page_call(
            self.connection_mut()?,
            &scope,
            "Page.captureScreenshot",
            json!({ "format": "png" }),
            deadline,
            &mut dialog,
        );
        let cleaned = page_evaluate(
            self.connection_mut()?,
            &scope,
            REMOVE_MASKS_SCRIPT,
            Deadline::after(MASK_CLEANUP_TIMEOUT),
            &mut dialog,
        );
        if let Err(failure) = cleaned {
            let mut message =
                failure.describe("could not remove the screenshot covers from the page");
            if let Err(capture_error) = captured {
                message.push_str(&format!(
                    "; the screenshot also failed: {}",
                    capture_error.describe("the screenshot")
                ));
            }
            return Err(StepError::Environment(message));
        }

        let reply = captured.map_err(|failure| {
            let prefix = if failure.is_timeout() {
                format!(
                    "the screenshot did not finish within {} ms",
                    deadline.timeout_ms()
                )
            } else {
                "could not capture the screenshot".to_string()
            };
            failure.step(&prefix, timeout_or_environment)
        })?;
        let data = reply.get("data").and_then(Value::as_str).ok_or_else(|| {
            StepError::Environment(
                "the browser did not return PNG data for the screenshot".to_string(),
            )
        })?;
        decode_base64(data).map_err(|error| {
            StepError::Environment(format!("the screenshot data is invalid: {error}"))
        })
    }
}

/// Maximum number of target/session CDP events summarized on a target/session failure.
const MAX_TARGET_DIAGNOSTIC_EVENTS: usize = 5;
/// Maximum number of fresh `about:blank` tabs tried during one `attach` call.
///
/// The tab is fresh (`about:blank`) and no navigation or user script has run in
/// it yet, so closing a stalled/crashed tab and opening a replacement is safe
/// and idempotent inside the existing `LAUNCH_TIMEOUT` budget.
const MAX_ATTACH_ATTEMPTS: usize = 3;
/// Per-attempt budget for `Page.enable` / `Runtime.enable` / `Network.enable`.
///
/// Healthy targets answer in tens of milliseconds; capping each attempt at 5 s
/// lets a silent renderer hang abort and retry within the 30 s `LAUNCH_TIMEOUT`.
const ATTACH_ENABLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `attach` waits for `Target.closeTarget` when cleaning up a failed fresh tab.
const ATTACH_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// Which phase of `attach` failed, used to classify whether target recreation can help.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachStep {
    CreateTarget,
    AttachToTarget,
    EnableDomain,
}

/// Whether a failed `attach` attempt failed at the target/session layer or the browser layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachFailureKind {
    /// Target/session failed before domains were enabled while the browser WebSocket stayed open.
    TargetFailed,
    /// Browser-level WebSocket closed/errored or `Target.createTarget` itself failed.
    BrowserFailed,
}

/// Decision after an `attach` attempt fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachRetryDecision {
    Retry,
    GiveUp,
}

/// Outcome of one `attach_once` attempt.
struct AttachAttemptFailure {
    target_id: Option<String>,
    kind: AttachFailureKind,
    message: String,
    target_destroyed: bool,
}

/// Opens one new tab and attaches it as the session's page.
///
/// Retries up to [`MAX_ATTACH_ATTEMPTS`] times inside `deadline` if a fresh
/// `about:blank` target crashes, detaches, or silently stalls before domain
/// setup completes; never retries once `attach` returns or navigation starts.
fn attach(connection: &mut Connection, deadline: Deadline) -> Result<(String, String), String> {
    attach_with_enable_timeout(connection, deadline, ATTACH_ENABLE_TIMEOUT)
}

/// Runs bounded target creation and domain setup with a configurable per-attempt
/// domain-enable timeout (used directly by unit tests without sleeping 5 s).
fn attach_with_enable_timeout(
    connection: &mut Connection,
    deadline: Deadline,
    enable_timeout: Duration,
) -> Result<(String, String), String> {
    let mut attempt_errors = Vec::with_capacity(MAX_ATTACH_ATTEMPTS);
    for attempt in 1..=MAX_ATTACH_ATTEMPTS {
        match attach_once(connection, deadline, enable_timeout) {
            Ok(attached) => return Ok(attached),
            Err(failure) => {
                let mut effective_kind = failure.kind;
                attempt_errors.push(failure.message);
                if let Some(target_id) = failure.target_id.as_deref()
                    && !failure.target_destroyed
                    && effective_kind == AttachFailureKind::TargetFailed
                    && !deadline.passed()
                    && attempt < MAX_ATTACH_ATTEMPTS
                {
                    if let Err(CdpError::Transport(_)) = connection.call(
                        "Target.closeTarget",
                        json!({ "targetId": target_id }),
                        None,
                        deadline.capped(ATTACH_CLOSE_TIMEOUT),
                    ) {
                        effective_kind = AttachFailureKind::BrowserFailed;
                    }
                    let _ = connection.take_messages();
                }
                if decide_attach_retry(
                    attempt,
                    MAX_ATTACH_ATTEMPTS,
                    effective_kind,
                    deadline.passed(),
                ) == AttachRetryDecision::GiveUp
                {
                    return Err(format_attempts_error(&attempt_errors));
                }
            }
        }
    }
    Err(format_attempts_error(&attempt_errors))
}

/// Target and session identifiers known at a given point in `attach`.
#[derive(Clone, Copy)]
struct AttachScope<'a> {
    target_id: Option<&'a str>,
    session_id: Option<&'a str>,
}

/// Attempts once to create a fresh `about:blank` target, attach to it, and
/// enable `Page`, `Runtime`, and `Network`.
fn attach_once(
    connection: &mut Connection,
    deadline: Deadline,
    enable_timeout: Duration,
) -> Result<(String, String), AttachAttemptFailure> {
    let target = attach_call(
        connection,
        AttachStep::CreateTarget,
        "could not open a page in the browser",
        "Target.createTarget",
        json!({ "url": "about:blank" }),
        AttachScope {
            target_id: None,
            session_id: None,
        },
        deadline,
    )?;
    let target_id = target
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| AttachAttemptFailure {
            target_id: None,
            kind: AttachFailureKind::BrowserFailed,
            message: "the browser did not return a target id".to_string(),
            target_destroyed: false,
        })?
        .to_string();
    let attached = attach_call(
        connection,
        AttachStep::AttachToTarget,
        "could not attach to the page the browser opened",
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
        AttachScope {
            target_id: Some(&target_id),
            session_id: None,
        },
        deadline,
    )?;
    let page = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| AttachAttemptFailure {
            target_id: Some(target_id.clone()),
            kind: AttachFailureKind::TargetFailed,
            message: "the browser did not return a page session id".to_string(),
            target_destroyed: false,
        })?
        .to_string();
    let domain_deadline = deadline.capped(enable_timeout);
    let page_scope = AttachScope {
        target_id: Some(&target_id),
        session_id: Some(&page),
    };
    attach_call(
        connection,
        AttachStep::EnableDomain,
        "could not enable page events",
        "Page.enable",
        json!({}),
        page_scope,
        domain_deadline,
    )?;
    attach_call(
        connection,
        AttachStep::EnableDomain,
        "could not enable page scripts",
        "Runtime.enable",
        json!({}),
        page_scope,
        domain_deadline,
    )?;
    // The first navigation may start right after this call, so the network
    // domain is enabled before the page can issue a request; an observer
    // enabled later would miss the document request.
    attach_call(
        connection,
        AttachStep::EnableDomain,
        "could not enable network events",
        "Network.enable",
        json!({}),
        page_scope,
        domain_deadline,
    )?;
    Ok((page, target_id))
}

/// Runs one CDP command while watching for terminal target/session events.
///
/// On failure, drains any burst sibling events after an abort and returns the
/// raw [`CdpError`] together with all queued messages for diagnostic formatting.
fn call_watching_target(
    connection: &mut Connection,
    method: &str,
    params: Value,
    target_id: Option<&str>,
    session_id: Option<&str>,
    deadline: Deadline,
) -> Result<Value, (CdpError, Vec<Value>)> {
    connection
        .call_until(method, params, session_id, deadline, |event| {
            is_terminal_target_event(event, target_id, session_id)
        })
        .map_err(|error| {
            let events = watched_failure(connection, &error);
            (error, events)
        })
}

/// The events that explain a failed wait, drained from the queue.
///
/// A crash is usually followed by sibling events, so an abort first reads the
/// burst already on the socket before the queue is taken.
fn watched_failure(connection: &mut Connection, error: &CdpError) -> Vec<Value> {
    if matches!(error, CdpError::Aborted { .. }) {
        let _ = connection.poll_messages();
    }
    connection.take_messages()
}

/// The page's target and session: the pair whose death ends a page-session wait.
#[derive(Clone)]
struct PageScope {
    /// The tab this session drives.
    target_id: String,
    /// The CDP session attached to that tab.
    session_id: String,
}

impl PageScope {
    /// Whether `event` proves this page's target or session ended.
    fn is_terminal(&self, event: &Value) -> bool {
        is_terminal_target_event(event, Some(&self.target_id), Some(&self.session_id))
    }

    /// Pairs a CDP failure of this page with the events that explain it.
    ///
    /// Only an abort or a timeout can name events; any other failure keeps its
    /// own message, so the queue is left alone for the network observer.
    fn failure(&self, connection: &mut Connection, error: CdpError) -> PageFailure {
        let events = if matches!(error, CdpError::Aborted { .. } | CdpError::Timeout { .. }) {
            watched_failure(connection, &error)
        } else {
            Vec::new()
        };
        PageFailure(Box::new(PageFailureDetail {
            error,
            events,
            scope: self.clone(),
        }))
    }
}

/// Boxed payload for [`PageFailure`] so `Result<_, PageFailure>` stays pointer-sized.
struct PageFailureDetail {
    /// The CDP failure that ended the wait.
    error: CdpError,
    /// The events received while waiting, when the failure can name them.
    events: Vec<Value>,
    /// The target and session the wait watched.
    scope: PageScope,
}

/// One failed CDP wait on the page session.
struct PageFailure(Box<PageFailureDetail>);

impl PageFailure {
    /// The underlying CDP error that ended the wait.
    fn error(&self) -> &CdpError {
        &self.0.error
    }

    /// Whether a terminal target/session event ended the wait.
    fn is_death(&self) -> bool {
        matches!(self.0.error, CdpError::Aborted { .. })
    }

    /// Whether the deadline passed before an answer arrived.
    fn is_timeout(&self) -> bool {
        matches!(self.0.error, CdpError::Timeout { .. })
    }

    /// The failure text: `prefix`, the CDP error, and what the watch saw.
    fn describe(&self, prefix: &str) -> String {
        format_target_error(
            prefix,
            &self.0.error,
            Some(&self.0.scope.target_id),
            Some(&self.0.scope.session_id),
            &self.0.events,
        )
    }

    /// The step error for this failure, keeping `kind` unless the page died
    /// or an unhandled JavaScript dialog opened.
    ///
    /// A page death is always an environment error, so the step exits with 4.
    /// An unhandled dialog is always an execution error, so the step exits with 1.
    fn step(self, prefix: &str, kind: fn(&CdpError) -> StepErrorKind) -> StepError {
        if let CdpError::Dialog(opened) = &self.0.error {
            return StepError::Execution(format_dialog_error(prefix, opened));
        }
        let message = self.describe(prefix);
        if self.is_death() {
            return StepError::Environment(message);
        }
        kind(&self.0.error).into_step(message)
    }

    /// The step error for a failure the page caused: always an environment error,
    /// except an unhandled JavaScript dialog which is an execution error.
    fn environment(self, prefix: &str) -> StepError {
        if let CdpError::Dialog(opened) = &self.0.error {
            return StepError::Execution(format_dialog_error(prefix, opened));
        }
        StepError::Environment(self.describe(prefix))
    }
}

/// Formats an unhandled JavaScript dialog failure with type, message, and guidance.
fn format_dialog_error(prefix: &str, dialog: &OpenedDialog) -> String {
    let detail = if dialog.message.is_empty() {
        format!(
            "unexpected JavaScript `{}` dialog opened (empty message)",
            dialog.dialog_type
        )
    } else {
        format!(
            "unexpected JavaScript `{}` dialog opened: {}",
            dialog.dialog_type, dialog.message
        )
    };
    format!("{prefix}: {detail}; add `dialog: \"accept\"` or `dialog: \"dismiss\"` to handle it")
}

/// The step error a page-wait failure reports unless the page died.
#[derive(Clone, Copy)]
enum StepErrorKind {
    /// The environment failed (exit 4).
    Environment,
    /// The step ran out of time (exit 5).
    Timeout,
    /// The step ran but could not finish its work (exit 1).
    Execution,
}

impl StepErrorKind {
    /// Wraps `message` in the step error of this kind.
    fn into_step(self, message: String) -> StepError {
        match self {
            Self::Environment => StepError::Environment(message),
            Self::Timeout => StepError::Timeout(message),
            Self::Execution => StepError::Execution(message),
        }
    }
}

/// Reports a timeout as a timeout and anything else as an environment error.
fn timeout_or_environment(error: &CdpError) -> StepErrorKind {
    if matches!(error, CdpError::Timeout { .. }) {
        StepErrorKind::Timeout
    } else {
        StepErrorKind::Environment
    }
}

/// Reports a timeout as a timeout and anything else as an execution error.
fn timeout_or_execution(error: &CdpError) -> StepErrorKind {
    if matches!(error, CdpError::Timeout { .. }) {
        StepErrorKind::Timeout
    } else {
        StepErrorKind::Execution
    }
}

/// Reports every failure as an execution error.
fn execution_error(_: &CdpError) -> StepErrorKind {
    StepErrorKind::Execution
}

/// Runs one CDP command on the page session, watching for terminal events and dialogs.
///
/// This is the only path a step uses to reach the page. `Target.*` and
/// `Browser.*` run on the browser itself, and `close` deliberately bypasses the
/// watch so a tab that already died still closes.
fn page_call(
    connection: &mut Connection,
    scope: &PageScope,
    method: &str,
    params: Value,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, PageFailure> {
    let watch = dialog.watch(&scope.session_id);
    connection
        .call_until_with_dialog(
            method,
            params,
            Some(&scope.session_id),
            deadline,
            |event| scope.is_terminal(event),
            Some(watch),
        )
        .map_err(|error| scope.failure(connection, error))
}

/// Evaluates one expression on the page session.
fn page_evaluate(
    connection: &mut Connection,
    scope: &PageScope,
    expression: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, PageFailure> {
    let watch = dialog.watch(&scope.session_id);
    connection
        .evaluate(
            &scope.session_id,
            expression,
            deadline,
            |event| scope.is_terminal(event),
            Some(watch),
        )
        .map_err(|error| scope.failure(connection, error))
}

/// Evaluates one expression on the page session, returning an object handle or a value.
fn page_evaluate_remote(
    connection: &mut Connection,
    scope: &PageScope,
    expression: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<RemoteResult, PageFailure> {
    let watch = dialog.watch(&scope.session_id);
    connection
        .evaluate_remote(
            &scope.session_id,
            expression,
            deadline,
            |event| scope.is_terminal(event),
            Some(watch),
        )
        .map_err(|error| scope.failure(connection, error))
}

/// Calls one function with a DOM node as `this` on the page session.
fn page_call_function(
    connection: &mut Connection,
    scope: &PageScope,
    object_id: &str,
    function: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, PageFailure> {
    let watch = dialog.watch(&scope.session_id);
    connection
        .call_function(
            &scope.session_id,
            object_id,
            function,
            deadline,
            |event| scope.is_terminal(event),
            Some(watch),
        )
        .map_err(|error| scope.failure(connection, error))
}

/// Calls one function with a DOM node as `this`, returning an object handle or a value.
fn page_call_function_remote(
    connection: &mut Connection,
    scope: &PageScope,
    object_id: &str,
    function: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<RemoteResult, PageFailure> {
    let watch = dialog.watch(&scope.session_id);
    connection
        .call_function_remote(
            &scope.session_id,
            object_id,
            function,
            deadline,
            |event| scope.is_terminal(event),
            Some(watch),
        )
        .map_err(|error| scope.failure(connection, error))
}

/// Calls one function with a DOM node as `this` and one object argument.
fn page_call_function_with_object(
    connection: &mut Connection,
    scope: &PageScope,
    object_id: &str,
    arg_object_id: &str,
    function: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, PageFailure> {
    let watch = dialog.watch(&scope.session_id);
    connection
        .call_function_with_object(
            &scope.session_id,
            (object_id, arg_object_id),
            function,
            deadline,
            |event| scope.is_terminal(event),
            Some(watch),
        )
        .map_err(|error| scope.failure(connection, error))
}

/// Waits for one page event, watching for terminal target/session events and dialogs.
fn page_wait_for_event(
    connection: &mut Connection,
    scope: &PageScope,
    method: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, PageFailure> {
    let watch = dialog.watch(&scope.session_id);
    connection
        .wait_for_event_with_dialog(
            method,
            deadline,
            |event| scope.is_terminal(event),
            Some(watch),
        )
        .map_err(|error| scope.failure(connection, error))
}

/// Runs one CDP command during session creation, aborting early on terminal
/// target/session events and enriching any failure with diagnostics.
fn attach_call(
    connection: &mut Connection,
    step: AttachStep,
    action: &str,
    method: &str,
    params: Value,
    scope: AttachScope<'_>,
    deadline: Deadline,
) -> Result<Value, AttachAttemptFailure> {
    let outcome = match step {
        AttachStep::CreateTarget => connection
            .call(method, params, scope.session_id, deadline)
            .map_err(|error| (error, connection.take_messages())),
        AttachStep::AttachToTarget | AttachStep::EnableDomain => call_watching_target(
            connection,
            method,
            params,
            scope.target_id,
            scope.session_id,
            deadline,
        ),
    };
    outcome.map_err(|(error, events)| {
        let target_destroyed = events.iter().any(|event| {
            event_matches_target(event, scope.target_id, scope.session_id)
                && event.get("method").and_then(Value::as_str) == Some("Target.targetDestroyed")
        });
        AttachAttemptFailure {
            target_id: scope.target_id.map(str::to_string),
            kind: classify_attach_failure(step, &error),
            message: format_target_error(
                action,
                &error,
                scope.target_id,
                scope.session_id,
                &events,
            ),
            target_destroyed,
        }
    })
}

/// Classifies whether an `attach` error warrants recreating the target or giving up.
fn classify_attach_failure(step: AttachStep, error: &CdpError) -> AttachFailureKind {
    match step {
        AttachStep::CreateTarget => AttachFailureKind::BrowserFailed,
        AttachStep::AttachToTarget => match error {
            CdpError::Timeout { .. } | CdpError::Aborted { .. } | CdpError::Dialog(_) => {
                AttachFailureKind::TargetFailed
            }
            CdpError::Method { message, .. } if message.contains("No target") => {
                AttachFailureKind::TargetFailed
            }
            CdpError::Method { .. } | CdpError::Script(_) | CdpError::Transport(_) => {
                AttachFailureKind::BrowserFailed
            }
        },
        AttachStep::EnableDomain => match error {
            CdpError::Timeout { .. }
            | CdpError::Aborted { .. }
            | CdpError::Dialog(_)
            | CdpError::Method { .. } => AttachFailureKind::TargetFailed,
            CdpError::Script(_) | CdpError::Transport(_) => AttachFailureKind::BrowserFailed,
        },
    }
}

/// Decides whether `attach` should close the failed target and create a fresh one.
fn decide_attach_retry(
    attempt: usize,
    max_attempts: usize,
    failure_kind: AttachFailureKind,
    deadline_passed: bool,
) -> AttachRetryDecision {
    if failure_kind == AttachFailureKind::TargetFailed && attempt < max_attempts && !deadline_passed
    {
        AttachRetryDecision::Retry
    } else {
        AttachRetryDecision::GiveUp
    }
}

/// Formats one or more failed `attach` attempts into a single error message.
fn format_attempts_error(attempt_errors: &[String]) -> String {
    match attempt_errors {
        [] => "could not attach to a page in the browser".to_string(),
        [single] => single.clone(),
        many => many
            .iter()
            .enumerate()
            .map(|(index, message)| format!("attempt {}: {message}", index + 1))
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// Reports whether an event proves that the target or session has crashed or detached.
fn is_terminal_target_event(
    event: &Value,
    target_id: Option<&str>,
    session_id: Option<&str>,
) -> bool {
    if !event_matches_target(event, target_id, session_id) {
        return false;
    }
    matches!(
        event.get("method").and_then(Value::as_str),
        Some(
            "Target.targetCrashed"
                | "Inspector.targetCrashed"
                | "Target.detachedFromTarget"
                | "Inspector.detached"
                | "Target.targetDestroyed"
        )
    )
}

/// Formats a target/session CDP error with any diagnostic events received
/// for the target or session while waiting.
fn format_target_error(
    action: &str,
    error: &CdpError,
    target_id: Option<&str>,
    session_id: Option<&str>,
    events: &[Value],
) -> String {
    let mut matched = Vec::new();
    for event in events {
        if !event_matches_target(event, target_id, session_id) {
            continue;
        }
        if let Some(summary) = summarize_target_event(event)
            && !matched.contains(&summary)
        {
            matched.push(summary);
        }
    }

    if matched.is_empty() {
        return match error {
            CdpError::Timeout { .. } => format!(
                "{action}: {error}; browser never answered (no CDP events received for this target or session)"
            ),
            _ => format!("{action}: {error}"),
        };
    }

    let shown: Vec<&str> = matched
        .iter()
        .take(MAX_TARGET_DIAGNOSTIC_EVENTS)
        .map(String::as_str)
        .collect();
    let extra = matched.len().saturating_sub(MAX_TARGET_DIAGNOSTIC_EVENTS);
    let summary = if extra > 0 {
        format!("{}, +{extra} more", shown.join(", "))
    } else {
        shown.join(", ")
    };
    format!("{action}: {error}; browser events: {summary}")
}

/// Reports whether a queued CDP message is an event belonging to `target_id` or `session_id`.
fn event_matches_target(event: &Value, target_id: Option<&str>, session_id: Option<&str>) -> bool {
    if event.get("method").and_then(Value::as_str).is_none() {
        return false;
    }
    if let Some(sid) = session_id
        && (event.get("sessionId").and_then(Value::as_str) == Some(sid)
            || event.pointer("/params/sessionId").and_then(Value::as_str) == Some(sid))
    {
        return true;
    }
    if let Some(tid) = target_id
        && (event.pointer("/params/targetId").and_then(Value::as_str) == Some(tid)
            || event
                .pointer("/params/targetInfo/targetId")
                .and_then(Value::as_str)
                == Some(tid))
    {
        return true;
    }
    false
}

/// Formats one CDP event into a short human-readable diagnostic label.
fn summarize_target_event(event: &Value) -> Option<String> {
    let method = event.get("method").and_then(Value::as_str)?;
    let params = event.get("params");
    match method {
        "Target.targetCrashed" => {
            let status = params
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str);
            let error_code = params
                .and_then(|value| value.get("errorCode"))
                .and_then(Value::as_i64);
            match (status, error_code) {
                (Some(status), Some(code)) => Some(format!(
                    "target crashed ({method}: status={status}, errorCode={code})"
                )),
                (Some(status), None) => Some(format!("target crashed ({method}: status={status})")),
                (None, Some(code)) => Some(format!("target crashed ({method}: errorCode={code})")),
                (None, None) => Some(format!("target crashed ({method})")),
            }
        }
        "Inspector.targetCrashed" => Some(format!("target crashed ({method})")),
        "Target.detachedFromTarget" => Some(format!("target detached ({method})")),
        "Inspector.detached" => {
            let reason = params
                .and_then(|value| value.get("reason"))
                .and_then(Value::as_str);
            match reason {
                Some(reason) if !reason.is_empty() => {
                    Some(format!("target detached ({method}: {reason})"))
                }
                _ => Some(format!("target detached ({method})")),
            }
        }
        "Target.targetDestroyed" => Some(format!("target destroyed ({method})")),
        _ => Some(method.to_string()),
    }
}

/// An actionable target with its verified viewport click coordinates.
struct ActionableTarget {
    located: Located,
    x: f64,
    y: f64,
}

/// Outcome of one resolution poll for an [`ElementTarget`].
enum ResolvePoll {
    /// A single element (or the `nth`-indexed element) resolved to a DOM `objectId`.
    Found(String),
    /// The target (or one of its `within` containers) is not ready yet.
    Pending {
        target_desc: String,
        reason: PendingReason,
    },
}

/// Why one poll did not yield an actionable element yet.
#[derive(Debug, PartialEq, Eq)]
enum PendingReason {
    Missing,
    Hidden,
    OutOfRange { index: usize, count: usize },
    Disabled,
    Unstable,
    Occluded(Option<CandidateInfo>),
}

/// Evaluates one actionability snapshot against the prior poll's geometry.
fn evaluate_actionable_inspect(
    previous_rect: &mut Option<ElementRect>,
    inspect: ActionableInspect,
) -> Result<(f64, f64), PendingReason> {
    match inspect {
        ActionableInspect::NotVisible => {
            *previous_rect = None;
            Err(PendingReason::Hidden)
        }
        ActionableInspect::Disabled => {
            *previous_rect = None;
            Err(PendingReason::Disabled)
        }
        ActionableInspect::ReadyCandidate {
            rect,
            x,
            y,
            hit_ok,
            blocker,
        } => {
            let stable = previous_rect.is_some_and(|prev| prev.same_geometry(rect));
            *previous_rect = Some(rect);
            if !hit_ok {
                return Err(PendingReason::Occluded(blocker));
            }
            if !stable {
                return Err(PendingReason::Unstable);
            }
            Ok((x, y))
        }
    }
}

/// Failure during one non-blocking resolution pass.
enum LocateError {
    Timeout,
    Step(StepError),
}

/// Maps a pending resolution reason at deadline expiry into a step error.
fn pending_timeout_error(
    target_desc: &str,
    reason: PendingReason,
    deadline: Deadline,
) -> StepError {
    let ms = deadline.timeout_ms();
    match reason {
        PendingReason::Missing => StepError::Execution(format!(
            "could not find {target_desc} within {ms} ms; check the page and the locator"
        )),
        PendingReason::Hidden => StepError::Execution(format!(
            "{target_desc} was found but not visible within {ms} ms; check the page and the locator"
        )),
        PendingReason::OutOfRange { index, count } => {
            let noun = if count == 1 { "element" } else { "elements" };
            StepError::Execution(format!(
                "could not find {target_desc} within {ms} ms: `nth` index {index} is out of range for {count} matched {noun}"
            ))
        }
        PendingReason::Disabled => StepError::Execution(format!(
            "{target_desc} remained disabled (`disabled` or `aria-disabled=\"true\"`) within {ms} ms"
        )),
        PendingReason::Unstable => StepError::Execution(format!(
            "{target_desc} position or size was not stable across consecutive polls within {ms} ms"
        )),
        PendingReason::Occluded(Some(blocker)) => StepError::Execution(format!(
            "{target_desc} was occluded by {} at its click point within {ms} ms",
            blocker.format_label()
        )),
        PendingReason::Occluded(None) => StepError::Execution(format!(
            "{target_desc} was not hit-testable (`elementFromPoint` returned nothing) within {ms} ms"
        )),
    }
}

/// Builds a step failure that names the target and what was attempted.
fn action_error(target_desc: &str, action: &str, error: impl std::fmt::Display) -> StepError {
    StepError::Execution(format!("could not {action} {target_desc}: {error}"))
}

/// Maps a failure while looking for an element into a poll-time error.
fn locate_error(target_desc: &str, failure: PageFailure) -> LocateError {
    if failure.is_timeout() {
        return LocateError::Timeout;
    }
    if let CdpError::Script(message) = failure.error() {
        return LocateError::Step(StepError::Execution(format!(
            "could not use {target_desc}: {message}"
        )));
    }
    LocateError::Step(failure.environment(&format!(
        "the browser failed while waiting for {target_desc}"
    )))
}

/// Maps a failed page action inside a poll loop into a poll-time error.
fn action_poll_error(target_desc: &str, action: &str, failure: PageFailure) -> LocateError {
    if failure.is_timeout() {
        return LocateError::Timeout;
    }
    LocateError::Step(failure.step(
        &format!("could not {action} {target_desc}"),
        execution_error,
    ))
}

/// Maps a failure of one element action to the error class a step uses.
///
/// A timeout keeps the message this step reported before the watch existed and
/// gains the watch note from [`PageFailure::describe`].
fn evaluate_error(
    target_desc: &str,
    failure: PageFailure,
    action: &str,
    deadline: Deadline,
) -> StepError {
    let prefix = if failure.is_timeout() {
        format!(
            "could not {action} {target_desc} within {} ms",
            deadline.timeout_ms()
        )
    } else {
        format!("could not {action} {target_desc}")
    };
    failure.step(&prefix, timeout_or_execution)
}

/// Waits for the loaded document, skipping an event that belongs to the page
/// the session started on.
fn wait_for_load(
    connection: &mut Connection,
    scope: &PageScope,
    url: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<(), StepError> {
    loop {
        if let Err(failure) =
            page_wait_for_event(connection, scope, "Page.loadEventFired", deadline, dialog)
        {
            let prefix = if failure.is_timeout() {
                format!(
                    "the page `{url}` did not finish loading within {} ms",
                    deadline.timeout_ms()
                )
            } else {
                format!("the browser failed while loading `{url}`")
            };
            return Err(failure.step(&prefix, timeout_or_environment));
        }
        let current = page_evaluate(connection, scope, "location.href", deadline, dialog)
            .map_err(|failure| load_error(failure, url, "read the URL of", deadline))?;
        let ready = page_evaluate(connection, scope, "document.readyState", deadline, dialog)
            .map_err(|failure| load_error(failure, url, "read the state of", deadline))?;
        if current.as_str() != Some("about:blank") && ready.as_str() == Some("complete") {
            return Ok(());
        }
    }
}

/// Maps a failed read after navigation to the error class the step reports.
///
/// A timeout keeps the message this step reported before the watch existed and
/// gains the watch note from [`PageFailure::describe`].
fn load_error(failure: PageFailure, url: &str, action: &str, deadline: Deadline) -> StepError {
    let prefix = if failure.is_timeout() {
        format!(
            "the page `{url}` did not finish loading within {} ms",
            deadline.timeout_ms()
        )
    } else {
        format!("could not {action} `{url}`")
    };
    failure.step(&prefix, timeout_or_environment)
}

impl Session for BrowserSession {
    fn evidence(&mut self) -> Result<Vec<Evidence>, String> {
        if self.connection.is_none() {
            return Err(
                "the browser session is already closed; no evidence was collected".to_string(),
            );
        }
        let deadline = Deadline::after(EVIDENCE_TIMEOUT);
        let mut files = Vec::new();
        let mut errors = Vec::new();

        match self.capture_masked_png(deadline) {
            Ok(png) => files.push(
                Evidence::bytes("screenshot.png", png)
                    .map_err(|error| format!("could not name the screenshot evidence: {error}"))?,
            ),
            Err(error) => errors.push(format!(
                "the screenshot could not be collected: {}",
                step_error_text(error)
            )),
        }

        if deadline.passed() {
            errors.push(format!(
                "the DOM snapshot was skipped because the {} ms evidence budget was spent",
                EVIDENCE_TIMEOUT.as_millis()
            ));
        } else {
            let scope = self.scope();
            let mut dialog = DialogContext::fail_fast();
            match self.connection_mut().and_then(|connection| {
                page_evaluate(
                    connection,
                    &scope,
                    "document.documentElement.outerHTML",
                    deadline,
                    &mut dialog,
                )
                .map_err(|failure| failure.environment("could not read the DOM snapshot"))
            }) {
                Ok(Value::String(html)) => files.push(
                    Evidence::text("dom.html", html)
                        .map_err(|error| format!("could not name the DOM evidence: {error}"))?,
                ),
                Ok(_) => errors.push(
                    "the DOM snapshot was not a string; the page may have navigated".to_string(),
                ),
                Err(error) => errors.push(step_error_text(error)),
            }
        }

        if files.is_empty() && !errors.is_empty() {
            return Err(format!(
                "could not collect browser evidence: {}",
                errors.join("; ")
            ));
        }
        if !errors.is_empty() {
            files.push(
                Evidence::text(
                    "evidence-error.txt",
                    format!(
                        "some browser evidence could not be collected:\n{}\n",
                        errors.join("\n")
                    ),
                )
                .map_err(|error| format!("could not name the evidence error file: {error}"))?,
            );
        }
        Ok(files)
    }

    fn close(&mut self) -> Result<(), String> {
        let Some(mut connection) = self.connection.take() else {
            return Ok(());
        };
        record_close(self.port);
        let mut errors = Vec::new();
        match self.origin.take() {
            Some(Origin::Launched(browser)) => {
                if let Err(error) = connection.call(
                    "Browser.close",
                    json!({}),
                    None,
                    Deadline::after(Duration::from_secs(2)),
                ) {
                    match error {
                        // Chrome may close the socket as it answers, or never
                        // answer. The process check in `shutdown` decides
                        // whether the close worked.
                        CdpError::Transport(_) | CdpError::Timeout { .. } => {}
                        error => {
                            errors.push(format!("could not ask the browser to close: {error}"));
                        }
                    }
                }
                drop(connection);
                if let Err(error) = browser.shutdown() {
                    errors.push(error);
                }
            }
            Some(Origin::Connected) => {
                // Only this case's tab is ours. `Browser.close` is never sent:
                // the browser belongs to the user and must keep running.
                if let Err(error) = connection.call(
                    "Target.closeTarget",
                    json!({ "targetId": self.target_id }),
                    None,
                    Deadline::after(Duration::from_secs(2)),
                ) {
                    match error {
                        // The tab may have closed itself; disconnecting is
                        // enough in that case.
                        CdpError::Transport(_) | CdpError::Timeout { .. } => {}
                        CdpError::Method { message, .. } if message.contains("No target") => {}
                        error => errors.push(format!("could not close the page: {error}")),
                    }
                }
                drop(connection);
            }
            None => drop(connection),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// Records that [`Session::close`] ran, with the debug port it closed.
///
/// This is a test hook for the case-end test: the process also ends when the
/// fallback `Drop` runs, so the record is what tells an explicit close from the
/// fallback. It writes nothing unless `DA_TEST_BROWSER_CLOSE_LOG` names a file.
fn record_close(port: u16) {
    let Some(path) = std::env::var_os("DA_TEST_BROWSER_CLOSE_LOG") else {
        return;
    };
    if path.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "browser session closed port={port}");
    }
}

/// The message of a step error, for a session method that reports strings.
fn step_error_text(error: StepError) -> String {
    match error {
        StepError::InvalidArguments(message)
        | StepError::Environment(message)
        | StepError::Timeout(message)
        | StepError::Execution(message) => message,
    }
}

/// Decodes the base64 body Chrome returns for a screenshot.
///
/// The repository has no direct base64 dependency, and a screenshot only needs
/// this one direction. Padding and alphabet are checked, so corrupt data is an
/// error rather than a truncated PNG.
fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err("the base64 value is empty".to_string());
    }
    if !bytes.len().is_multiple_of(4) {
        return Err(format!(
            "the base64 value has {} characters, which is not a multiple of 4",
            bytes.len()
        ));
    }

    let chunks = bytes.len() / 4;
    let mut output = Vec::with_capacity(chunks * 3);
    for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let last = index + 1 == chunks;
        let mut values = [0_u8; 4];
        let mut padding = 0;
        for (position, byte) in chunk.iter().copied().enumerate() {
            if byte == b'=' {
                if !last || position < 2 {
                    return Err("base64 padding is only allowed at the end".to_string());
                }
                if padding > 0 && position != 3 {
                    return Err("base64 padding is not contiguous".to_string());
                }
                padding += 1;
                values[position] = 0;
            } else {
                if padding > 0 {
                    return Err("base64 data follows its padding".to_string());
                }
                values[position] = base64_value(byte).ok_or_else(|| {
                    format!(
                        "base64 contains `{}`, which is not in its alphabet",
                        byte as char
                    )
                })?;
            }
        }
        if padding > 2 {
            return Err("base64 ends with more than two padding characters".to_string());
        }

        output.push((values[0] << 2) | (values[1] >> 4));
        if padding < 2 {
            output.push((values[1] << 4) | (values[2] >> 2));
        }
        if padding < 1 {
            output.push((values[2] << 6) | values[3]);
        }
    }
    Ok(output)
}

/// One base64 alphabet value.
fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Rejects explicit `null` values in step arguments (including nested `within`),
/// because `serde` otherwise maps `null` into `Option::None` while JSON Schema refuses it.
fn reject_null_fields(value: &Value) -> Result<(), String> {
    let Some(entries) = value.as_object() else {
        return Ok(());
    };
    for (key, field) in entries {
        if field.is_null() {
            return Err(format!("invalid arguments: `{key}` must not be null"));
        }
        if key == "within" || key == "dialog" {
            reject_null_fields(field)?;
        }
    }
    Ok(())
}

/// Parses a step's arguments.
fn parse<T: DeserializeOwned>(args: &Value) -> Result<T, StepError> {
    reject_null_fields(args).map_err(StepError::InvalidArguments)?;
    serde_json::from_value(args.clone())
        .map_err(|error| StepError::InvalidArguments(format!("invalid arguments: {error}")))
}

/// Parses a step's arguments for `validate`.
fn parse_for_validate<T: DeserializeOwned>(args: &Value) -> Result<T, String> {
    reject_null_fields(args)?;
    serde_json::from_value(args.clone()).map_err(|error| format!("invalid arguments: {error}"))
}

/// Checks a `timeout_ms` argument.
fn check_timeout(timeout_ms: u64) -> Result<(), String> {
    if timeout_ms == 0 {
        return Err("invalid arguments: `timeout_ms` must be at least 1".to_string());
    }
    Ok(())
}

fn check_open(raw: &Value, args: &OpenArgs) -> Result<(), String> {
    if args.url.trim().is_empty() {
        return Err(
            "invalid arguments: `url` must not be empty; give the page to open".to_string(),
        );
    }
    if raw.get("connect").is_some_and(Value::is_null) {
        return Err(
            "invalid arguments: `connect` must be a string like `http://127.0.0.1:9222`"
                .to_string(),
        );
    }
    if let Some(connect) = &args.connect {
        for field in ["headless", "width", "height"] {
            if raw.get(field).is_some() {
                return Err(format!(
                    "invalid arguments: `connect` cannot be combined with `{field}`; the connected browser keeps its own window"
                ));
            }
        }
        Endpoint::parse(connect)?;
    }
    if let Some(attribute) = &args.testid_attribute
        && (attribute.trim().is_empty() || attribute.chars().any(char::is_whitespace))
    {
        return Err(
            "invalid arguments: `testid_attribute` must not be empty or contain whitespace"
                .to_string(),
        );
    }
    if args.width == 0 {
        return Err("invalid arguments: `width` must be at least 1".to_string());
    }
    if args.height == 0 {
        return Err("invalid arguments: `height` must be at least 1".to_string());
    }
    check_timeout(args.timeout_ms)
}

fn validate_open(raw: &Value) -> Result<(), String> {
    let args: OpenArgs = parse_for_validate(raw)?;
    check_open(raw, &args)
}

fn validate_click(args: &Value) -> Result<(), String> {
    let args: ClickArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    let locator = Locator::from_args(
        &args.css,
        &args.xpath,
        &args.text,
        &args.role,
        &args.name,
        &args.testid,
    )?;
    ElementTarget::from_parts(locator, args.within.as_deref(), args.nth).map(|_| ())
}

/// The text `browser.type` sends.
///
/// `text` is the typing text unless `value` is also given; then `text` names
/// the element and `value` is what is typed.
fn type_text(args: &TypeArgs) -> Result<&str, String> {
    args.value.as_deref().or(args.text.as_deref()).ok_or_else(|| {
        "invalid arguments: `text` is required; give the text to type, or `value` when `text` names the element"
            .to_string()
    })
}

/// The target locator `browser.type` uses.
fn type_target(args: &TypeArgs) -> Result<ElementTarget, String> {
    let locator = if args.value.is_some() {
        Locator::from_args(
            &args.css,
            &args.xpath,
            &args.text,
            &args.role,
            &args.name,
            &args.testid,
        )?
    } else {
        Locator::from_args(
            &args.css,
            &args.xpath,
            &None,
            &args.role,
            &args.name,
            &args.testid,
        )?
    };
    ElementTarget::from_parts(locator, args.within.as_deref(), args.nth)
}

fn validate_type(args: &Value) -> Result<(), String> {
    let args: TypeArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    type_text(&args)?;
    type_target(&args).map(|_| ())
}

fn validate_press(args: &Value) -> Result<(), String> {
    let args: PressArgs = parse_for_validate(args)?;
    Key::parse(&args.key).map(|_| ())
}

fn validate_wait(args: &Value) -> Result<(), String> {
    let args: WaitArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    WaitTarget::from_args(&args).map(|_| ())
}

fn validate_screenshot(args: &Value) -> Result<(), String> {
    let args: ScreenshotArgs = parse_for_validate(args)?;
    if args.path.trim().is_empty() {
        return Err("invalid arguments: `path` must name the PNG file to write".to_string());
    }
    Ok(())
}

fn validate_read(args: &Value) -> Result<(), String> {
    let args: ReadArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    let locator = Locator::from_args(
        &args.css,
        &args.xpath,
        &args.text,
        &args.role,
        &args.name,
        &args.testid,
    )?;
    ElementTarget::from_parts(locator, args.within.as_deref(), args.nth)?;
    if let Some(attribute) = &args.attribute
        && attribute.trim().is_empty()
    {
        return Err("invalid arguments: `attribute` must not be empty".to_string());
    }
    Ok(())
}

/// Returns the case's browser session or explains how to open one.
fn session(sessions: &mut Sessions) -> Result<&mut BrowserSession, StepError> {
    sessions
        .get::<BrowserSession>(SESSION_KIND)
        .map_err(StepError::Execution)?
        .ok_or_else(|| {
            StepError::Execution(
                "no browser session is open; run `browser.open` before this step".to_string(),
            )
        })
}

fn run_open(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let raw = args;
    let args: OpenArgs = parse(raw)?;
    check_open(raw, &args).map_err(StepError::InvalidArguments)?;
    let connect = args
        .connect
        .as_deref()
        .map(Endpoint::parse)
        .transpose()
        .map_err(StepError::InvalidArguments)?;
    let existing = sessions
        .get::<BrowserSession>(SESSION_KIND)
        .map_err(StepError::Execution)?
        .is_some();
    let session = sessions
        .get_or_create(SESSION_KIND, || {
            BrowserSession::open(&args, connect.as_ref())
        })
        .map_err(StepError::Environment)?;
    if existing {
        session.ensure_same_options(&args, connect.as_ref())?;
        if let Some(attribute) = &args.testid_attribute {
            session.testid_attribute = attribute.clone();
        }
    }
    let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
    let (url, title) = session.navigate(
        &args.url,
        Duration::from_millis(args.timeout_ms),
        &mut dialog,
    )?;
    Ok(attach_dialog(
        json!({ "url": url, "title": title }),
        &mut dialog,
    ))
}

fn run_click(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ClickArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator = Locator::from_args(
        &args.css,
        &args.xpath,
        &args.text,
        &args.role,
        &args.name,
        &args.testid,
    )
    .map_err(StepError::InvalidArguments)?;
    let target = ElementTarget::from_parts(locator, args.within.as_deref(), args.nth)
        .map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let actionable = session.locate_actionable_target(&target, deadline, &mut dialog)?;
    let located = actionable.located;
    let (x, y) = (actionable.x, actionable.y);
    for (event_type, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
        session.dispatch(
            &target_desc,
            "Input.dispatchMouseEvent",
            json!({
                "type": event_type,
                "x": x,
                "y": y,
                "button": "left",
                "buttons": buttons,
                "clickCount": 1,
            }),
            "click",
            deadline,
            &mut dialog,
        )?;
    }
    // A click may navigate, which detaches the element. The click itself
    // succeeded, so a text that can no longer be read is not a failure unless
    // an unhandled JavaScript dialog opened.
    let text = match session.run_on(
        &target_desc,
        &located,
        TEXT_BODY,
        "read the text of",
        deadline,
        &mut dialog,
    ) {
        Ok(value) => value.as_str().map(str::to_string),
        Err(error) if dialog.is_fail_fast_triggered() => return Err(error),
        Err(_) => None,
    };
    let result = match text {
        Some(text) if !text.trim().is_empty() => json!({ "text": text.trim() }),
        _ => json!({}),
    };
    Ok(attach_dialog(result, &mut dialog))
}

fn run_type(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: TypeArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let text = type_text(&args)
        .map_err(StepError::InvalidArguments)?
        .to_string();
    let target = type_target(&args).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let actionable = session.locate_actionable_target(&target, deadline, &mut dialog)?;
    let focused = session.run_on(
        &target_desc,
        &actionable.located,
        FOCUS_BODY,
        "focus",
        deadline,
        &mut dialog,
    )?;
    if focused != Value::Bool(true) {
        return Err(action_error(
            &target_desc,
            "focus",
            "the element did not take focus",
        ));
    }
    session.dispatch(
        &target_desc,
        "Input.insertText",
        json!({ "text": text }),
        "type into",
        deadline,
        &mut dialog,
    )?;
    Ok(attach_dialog(json!({}), &mut dialog))
}

fn run_press(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: PressArgs = parse(args)?;
    let key = Key::parse(&args.key).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_secs(5));
    let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
    let session = session(sessions)?;
    let scope = session.scope();
    for params in [key.down_params(), key.up_params()] {
        let connection = session.connection_mut()?;
        page_call(
            connection,
            &scope,
            "Input.dispatchKeyEvent",
            params,
            deadline,
            &mut dialog,
        )
        .map_err(|failure| {
            let prefix = if failure.is_timeout() {
                format!(
                    "could not press `{}` within {} ms",
                    args.key,
                    deadline.timeout_ms()
                )
            } else {
                format!("could not press `{}`", args.key)
            };
            failure.step(&prefix, timeout_or_execution)
        })?;
    }
    Ok(attach_dialog(json!({}), &mut dialog))
}

fn run_wait(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: WaitArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let target = WaitTarget::from_args(&args).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
    let session = session(sessions)?;
    let url = match target {
        WaitTarget::Element(target) => {
            session.locate_target(&target, deadline, &mut dialog)?;
            let scope = session.scope();
            let connection = session.connection_mut()?;
            page_evaluate(connection, &scope, "location.href", deadline, &mut dialog)
                .map_err(|failure| failure.environment("could not read the current URL"))?
        }
        WaitTarget::Url(wanted) => wait_for_url(session, &wanted, deadline, &mut dialog)?,
    };
    Ok(attach_dialog(
        json!({ "url": url.as_str().unwrap_or_default() }),
        &mut dialog,
    ))
}

/// Waits until the page's URL contains `wanted`.
fn wait_for_url(
    session: &mut BrowserSession,
    wanted: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, StepError> {
    let scope = session.scope();
    loop {
        let connection = session.connection_mut()?;
        match page_evaluate(connection, &scope, "location.href", deadline, dialog) {
            Ok(value) => {
                if value.as_str().is_some_and(|url| url.contains(wanted)) {
                    return Ok(value);
                }
                if deadline.passed() {
                    return Err(StepError::Execution(format!(
                        "the URL did not contain `{wanted}` within {} ms",
                        deadline.timeout_ms()
                    )));
                }
            }
            Err(failure) if failure.is_timeout() => {
                return Err(StepError::Execution(format!(
                    "the URL did not contain `{wanted}` within {} ms",
                    deadline.timeout_ms()
                )));
            }
            Err(failure) => {
                return Err(failure.environment("the browser failed while waiting for the URL"));
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn run_screenshot(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ScreenshotArgs = parse(args)?;
    if args.path.trim().is_empty() {
        return Err(StepError::InvalidArguments(
            "invalid arguments: `path` must name the PNG file to write".to_string(),
        ));
    }
    let path = resolve_path(&args.path, sessions);
    let session = session(sessions)?;
    let png = session.capture_masked_png(Deadline::after(SCREENSHOT_TIMEOUT))?;
    write_png(&path, &png)?;
    Ok(json!({
        "path": paths::display(&path),
        "bytes": png.len(),
    }))
}

/// Resolves a relative output path against the directory that holds the case
/// file, so a case can keep its screenshots beside itself.
fn resolve_path(path: &str, sessions: &Sessions) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return path;
    }
    match sessions.base_directory() {
        Some(directory) => directory.join(path),
        None => path,
    }
}

/// Writes one PNG, creating its parent directory when it is missing.
fn write_png(path: &Path, png: &[u8]) -> Result<(), StepError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            StepError::Environment(format!(
                "could not create the screenshot directory `{}`: {error}",
                paths::display(parent)
            ))
        })?;
    }
    std::fs::write(path, png).map_err(|error| {
        StepError::Environment(format!(
            "could not write the screenshot `{}`: {error}",
            paths::display(path)
        ))
    })
}

fn run_read(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ReadArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator = Locator::from_args(
        &args.css,
        &args.xpath,
        &args.text,
        &args.role,
        &args.name,
        &args.testid,
    )
    .map_err(StepError::InvalidArguments)?;
    let target = ElementTarget::from_parts(locator, args.within.as_deref(), args.nth)
        .map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let mut dialog = DialogContext::fail_fast();
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let located = session.locate_target(&target, deadline, &mut dialog)?;
    let text = session.run_on(
        &target_desc,
        &located,
        TEXT_BODY,
        "read the text of",
        deadline,
        &mut dialog,
    )?;
    let mut result = json!({ "text": text.as_str().unwrap_or_default() });
    if let Some(attribute) = &args.attribute {
        let body = format!(
            "if (!el) return null; return el.getAttribute({});",
            json_string(attribute)
        );
        let value = session.run_on(
            &target_desc,
            &located,
            &body,
            "read the attribute of",
            deadline,
            &mut dialog,
        )?;
        result["value"] = value;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use crate::core::registry::Registry;

    fn registry() -> Registry {
        let mut registry = Registry::new();
        register(&mut registry).expect("the browser steps must register");
        registry
    }

    fn definition(name: &str) -> StepDefinition {
        registry()
            .get(name)
            .unwrap_or_else(|| panic!("`{name}` must be registered"))
            .clone()
    }

    #[test]
    fn every_browser_step_is_registered_with_a_schema_and_a_description() {
        let registry = registry();
        for name in STEP_NAMES {
            let definition = registry
                .get(name)
                .unwrap_or_else(|| panic!("`{name}` must be registered"));
            assert_eq!(definition.name, name);
            assert!(
                !definition.description.is_empty(),
                "`{name}` must describe itself"
            );
            assert_eq!(
                definition.args_schema["type"], "object",
                "`{name}` must take an object: {}",
                definition.args_schema
            );
            assert_eq!(
                definition.args_schema["additionalProperties"], false,
                "`{name}` must refuse unknown arguments: {}",
                definition.args_schema
            );
        }
        assert_eq!(
            registry.names().len(),
            STEP_NAMES.len(),
            "only the browser steps this module declares may be registered by it"
        );
        assert!(
            registry.observer(network::OBSERVER_NAME).is_some(),
            "the browser module must register its network observer"
        );
    }

    #[test]
    fn only_read_and_wait_allow_retry() {
        for name in ["browser.read", "browser.wait"] {
            let def = definition(name);
            assert!(
                (def.check_retry)(&json!({})).is_ok(),
                "`{name}` must allow retry"
            );
        }
        for name in [
            "browser.open",
            "browser.click",
            "browser.type",
            "browser.press",
            "browser.screenshot",
        ] {
            let def = definition(name);
            let error = (def.check_retry)(&json!({}))
                .err()
                .unwrap_or_else(|| panic!("`{name}` must reject retry"));
            assert!(error.contains("cannot use `retry`"), "{name}: {error}");
        }
    }

    #[test]
    fn open_defaults_and_bounds_are_in_the_schema_and_in_validate() {
        let definition = definition("browser.open");
        assert_eq!(
            definition.args_schema["required"],
            json!(["url"]),
            "only `url` is required: {}",
            definition.args_schema
        );
        assert_eq!(definition.args_schema["properties"]["width"]["minimum"], 1);
        assert_eq!(definition.args_schema["properties"]["height"]["minimum"], 1);
        assert_eq!(definition.args_schema["properties"]["url"]["minLength"], 1);
        assert_eq!(
            definition.args_schema["properties"]["timeout_ms"]["minimum"],
            1
        );
        assert_eq!(
            definition.args_schema["properties"]["connect"]["type"],
            "string"
        );

        assert!((definition.validate)(&json!({"url": "http://127.0.0.1:1/"})).is_ok());
        assert!(
            (definition.validate)(&json!({"url": "http://127.0.0.1:1/", "timeout_ms": 30000}))
                .is_ok(),
            "a navigation timeout is a valid argument"
        );
        assert!(
            (definition.validate)(&json!({
                "url": "http://127.0.0.1:1/",
                "connect": "http://127.0.0.1:9222"
            }))
            .is_ok(),
            "a loopback connect endpoint is a valid argument"
        );
        for (label, args) in [
            ("missing url", json!({})),
            ("empty url", json!({"url": ""})),
            ("zero width", json!({"url": "http://x/", "width": 0})),
            ("zero height", json!({"url": "http://x/", "height": 0})),
            ("zero timeout", json!({"url": "http://x/", "timeout_ms": 0})),
            ("unknown", json!({"url": "http://x/", "bogus": true})),
            (
                "remote connect",
                json!({"url": "http://x/", "connect": "http://192.168.1.1:9222"}),
            ),
            (
                "connect with headless",
                json!({"url": "http://x/", "connect": "http://127.0.0.1:9222", "headless": true}),
            ),
            (
                "connect with width",
                json!({"url": "http://x/", "connect": "http://127.0.0.1:9222", "width": 800}),
            ),
            (
                "connect with height",
                json!({"url": "http://x/", "connect": "http://127.0.0.1:9222", "height": 600}),
            ),
        ] {
            assert!(
                (definition.validate)(&args).is_err(),
                "{label} must be refused: {args}"
            );
        }
    }

    #[test]
    fn a_locator_is_exactly_one_of_five_forms() {
        for step in ["browser.click", "browser.type", "browser.read"] {
            let definition = definition(step);
            let branches = definition.args_schema["oneOf"]
                .as_array()
                .unwrap_or_else(|| panic!("`{step}` must declare oneOf"));
            assert_eq!(branches.len(), 5, "`{step}`: {branches:?}");
            for property in ["css", "xpath", "text", "role", "name", "testid"] {
                assert_eq!(
                    definition.args_schema["properties"][property]["type"], "string",
                    "`{step}`: `{property}` must be a string"
                );
            }

            let base = if step == "browser.type" {
                json!({"css": "#name", "text": "hello"})
            } else {
                json!({"css": "#name"})
            };
            assert!(
                (definition.validate)(&base).is_ok(),
                "`{step}` must accept one locator: {base}"
            );
            let text = if step == "browser.type" {
                json!({"text": "Later", "value": "hello"})
            } else {
                json!({"text": "Later"})
            };
            assert!(
                (definition.validate)(&text).is_ok(),
                "`{step}` must accept a text locator: {text}"
            );
            let role = if step == "browser.type" {
                json!({"role": "textbox", "name": "Name", "text": "hello"})
            } else {
                json!({"role": "textbox", "name": "Name"})
            };
            assert!(
                (definition.validate)(&role).is_ok(),
                "`{step}` must accept a role locator: {role}"
            );
            let both = if step == "browser.type" {
                json!({"css": "#a", "xpath": "//*[@id='a']", "text": "hello"})
            } else {
                json!({"css": "#a", "xpath": "//*[@id='a']"})
            };
            let error = (definition.validate)(&both).expect_err("both locators must be refused");
            assert!(error.contains("cannot both be set"), "{step}: {error}");
            let none = if step == "browser.type" {
                json!({"text": "hello"})
            } else {
                json!({})
            };
            let error =
                (definition.validate)(&none).expect_err("a missing locator must be refused");
            assert!(error.contains("one locator"), "{step}: {error}");
            let name = if step == "browser.type" {
                json!({"name": "Submit", "text": "hello"})
            } else {
                json!({"name": "Submit"})
            };
            let error =
                (definition.validate)(&name).expect_err("a name without a role must be refused");
            assert!(error.contains("role"), "{step}: {error}");
        }
    }

    #[test]
    fn wait_accepts_a_locator_or_url_contains_but_not_both() {
        let definition = definition("browser.wait");
        let branches = definition.args_schema["oneOf"]
            .as_array()
            .expect("`browser.wait` must declare oneOf");
        assert_eq!(branches.len(), 6, "{branches:?}");

        for args in [
            json!({"css": "#late"}),
            json!({"xpath": "//*[@id='late']"}),
            json!({"text": "late result"}),
            json!({"role": "button", "name": "Later"}),
            json!({"testid": "late"}),
            json!({"url_contains": "/done"}),
        ] {
            assert!(
                (definition.validate)(&args).is_ok(),
                "one target must be accepted: {args}"
            );
        }
        for (label, args, fragment) in [
            (
                "both locators",
                json!({"css": "#a", "xpath": "//*[@id='a']"}),
                "cannot both be set",
            ),
            (
                "locator and url",
                json!({"css": "#a", "url_contains": "/done"}),
                "cannot be combined",
            ),
            ("nothing", json!({}), "exactly one of"),
            (
                "empty url",
                json!({"url_contains": ""}),
                "must not be empty",
            ),
            ("name without role", json!({"name": "Later"}), "role"),
        ] {
            let error = (definition.validate)(&args).expect_err("the target must be refused");
            assert!(error.contains(fragment), "{label}: {error}");
        }
    }

    #[test]
    fn timeouts_and_keys_are_checked_by_validate() {
        for step in [
            "browser.click",
            "browser.type",
            "browser.wait",
            "browser.read",
        ] {
            let definition = definition(step);
            let mut args = match step {
                "browser.type" => json!({"css": "#a", "text": "x"}),
                "browser.wait" => json!({"css": "#a"}),
                _ => json!({"css": "#a"}),
            };
            args["timeout_ms"] = json!(0);
            let error = (definition.validate)(&args).expect_err("a zero timeout must be refused");
            assert!(error.contains("timeout_ms"), "{step}: {error}");
        }

        let press = definition("browser.press");
        assert!((press.validate)(&json!({"key": "Enter"})).is_ok());
        assert!((press.validate)(&json!({"key": "x"})).is_ok());
        let error =
            (press.validate)(&json!({"key": ""})).expect_err("an empty key must be refused");
        assert!(error.contains("key"), "{error}");
        let error = (press.validate)(&json!({"key": "NotAKey"}))
            .expect_err("an unknown key name must be refused");
        assert!(error.contains("not a key"), "{error}");
        assert_eq!(press.args_schema["properties"]["key"]["minLength"], 1);
    }

    #[test]
    fn read_returns_the_attribute_only_when_one_is_named() {
        let definition = definition("browser.read");
        assert!(
            (definition.validate)(&json!({"css": "#a"})).is_ok(),
            "an attribute is optional"
        );
        assert!((definition.validate)(&json!({"css": "#a", "attribute": "data-state"})).is_ok());
        let error = (definition.validate)(&json!({"css": "#a", "attribute": ""}))
            .expect_err("an empty attribute must be refused");
        assert!(error.contains("attribute"), "{error}");
        assert_eq!(
            definition.args_schema["properties"]["attribute"]["minLength"],
            1
        );
    }

    #[test]
    fn a_locator_names_itself_in_an_error_and_in_its_script() {
        assert_eq!(
            Locator::Css("#name".to_string()).describe_with(DEFAULT_TESTID_ATTRIBUTE),
            "CSS selector `#name`"
        );
        assert_eq!(
            Locator::Xpath("//*[@id='name']".to_string()).describe_with(DEFAULT_TESTID_ATTRIBUTE),
            "XPath `//*[@id='name']`"
        );
        assert_eq!(
            Locator::Text("Later".to_string()).describe_with(DEFAULT_TESTID_ATTRIBUTE),
            "visible text `Later`"
        );
        assert_eq!(
            Locator::TestId("save".to_string()).describe_with("data-qa"),
            "test id `save` (`data-qa`)"
        );
        assert_eq!(
            Locator::Role {
                role: "button".to_string(),
                name: Some("Later".to_string()),
            }
            .describe_with(DEFAULT_TESTID_ATTRIBUTE),
            "role `button` named `Later`"
        );

        let quoted = Locator::Css("a\"b\\c".to_string())
            .resolve_body_with(DEFAULT_TESTID_ATTRIBUTE, None)
            .expect("a CSS locator has a page expression");
        assert!(
            quoted.contains(r#"root.querySelectorAll("a\"b\\c")"#),
            "a selector must be passed as a JSON string literal: {quoted}"
        );

        let text = Locator::Text("a\"b\\c".to_string())
            .resolve_body_with(DEFAULT_TESTID_ATTRIBUTE, Some(2))
            .expect("a text locator has a page expression");
        assert!(
            text.contains(r#""a\"b\\c""#) && text.contains("var wantedNth = 2;"),
            "the wanted text and nth must be encoded safely: {text}"
        );

        let testid = Locator::TestId("a\"b\\c".to_string())
            .resolve_body_with("data-qa", None)
            .expect("a testid locator has a page expression");
        assert!(
            testid.contains(r#""data-qa""#) && testid.contains(r#""a\"b\\c""#),
            "the testid attribute and value must be encoded safely: {testid}"
        );

        assert!(
            Locator::Role {
                role: "button".to_string(),
                name: None,
            }
            .resolve_body_with(DEFAULT_TESTID_ATTRIBUTE, None)
            .is_none(),
            "a role locator has no page expression"
        );
    }

    #[test]
    fn candidate_formatting_caps_at_five_and_sixty_characters() {
        let long_text = "x".repeat(75);
        let candidates: Vec<CandidateInfo> = (1..=7)
            .map(|index| CandidateInfo {
                tag: "button".to_string(),
                id: if index == 2 {
                    "two".to_string()
                } else {
                    String::new()
                },
                text: if index == 1 {
                    long_text.clone()
                } else {
                    format!("Btn {index}")
                },
            })
            .collect();

        let truncated = truncate_chars(&long_text, 60);
        assert_eq!(truncated.chars().count(), 60);
        assert!(truncated.ends_with("..."));

        let summary = format_candidates(&candidates, 7);
        assert!(summary.contains("1: <button> \""));
        assert!(summary.contains("2: <button#two> \"Btn 2\""));
        assert!(summary.contains("5: <button> \"Btn 5\""));
        assert!(!summary.contains("Btn 6"));
        assert!(summary.contains("showing 5 of 7"));
    }

    #[test]
    fn type_reads_the_value_only_when_text_names_the_element() {
        let typed = TypeArgs {
            css: Some("#name".to_string()),
            xpath: None,
            text: Some("Ada".to_string()),
            role: None,
            name: None,
            testid: None,
            value: None,
            within: None,
            nth: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            dialog: None,
        };
        assert_eq!(type_text(&typed).ok(), Some("Ada"));
        assert_eq!(
            type_target(&typed).ok().map(|target| target.locator),
            Some(Locator::Css("#name".to_string()))
        );

        let located = TypeArgs {
            css: None,
            xpath: None,
            text: Some("Editor".to_string()),
            role: None,
            name: None,
            testid: None,
            value: Some(" typed".to_string()),
            within: None,
            nth: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            dialog: None,
        };
        assert_eq!(type_text(&located).ok(), Some(" typed"));
        assert_eq!(
            type_target(&located).ok().map(|target| target.locator),
            Some(Locator::Text("Editor".to_string()))
        );

        let missing = TypeArgs {
            css: Some("#name".to_string()),
            xpath: None,
            text: None,
            role: None,
            name: None,
            testid: None,
            value: None,
            within: None,
            nth: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            dialog: None,
        };
        assert!(type_text(&missing).is_err());
    }

    #[test]
    fn named_and_printable_keys_produce_their_events() {
        let enter = Key::parse("Enter").expect("Enter is a key name");
        let down = enter.down_params();
        assert_eq!(down["type"], "keyDown");
        assert_eq!(down["key"], "Enter");
        assert_eq!(down["code"], "Enter");
        assert_eq!(down["windowsVirtualKeyCode"], 13);
        assert_eq!(down["text"], "\r");
        assert_eq!(enter.up_params()["type"], "keyUp");
        assert!(enter.up_params().get("text").is_none());

        let tab = Key::parse("Tab").expect("Tab is a key name");
        assert_eq!(tab.down_params()["type"], "rawKeyDown");
        assert_eq!(tab.down_params()["windowsVirtualKeyCode"], 9);
        assert!(tab.down_params().get("text").is_none());

        let letter = Key::parse("a").expect("one printable character is a key");
        assert_eq!(letter.down_params()["text"], "a");
        assert_eq!(letter.down_params()["code"], "KeyA");
        assert_eq!(letter.down_params()["type"], "keyDown");

        for value in ["", "NotAKey", "ab", "\u{1}"] {
            assert!(
                Key::parse(value).is_err(),
                "`{value}` must not be accepted as a key"
            );
        }
    }

    #[test]
    fn base64_screenshot_data_is_decoded_and_bad_padding_is_refused() {
        assert_eq!(decode_base64("TQ==").ok(), Some(vec![b'M']));
        assert_eq!(decode_base64("TWE=").ok(), Some(b"Ma".to_vec()));
        assert_eq!(decode_base64("TWFu").ok(), Some(b"Man".to_vec()));
        assert_eq!(
            decode_base64("iVBORw0KGgo=").ok(),
            Some(b"\x89PNG\r\n\x1a\n".to_vec())
        );
        for value in [
            "", "TQ=", "TQ===", "T===", "TQ=A", "TQ==TQ==", "T Q==", "TQ-_",
        ] {
            assert!(
                decode_base64(value).is_err(),
                "`{value}` must be refused as base64"
            );
        }
    }

    #[test]
    fn locator_modifiers_testid_within_nth_and_testid_attribute_are_validated() {
        let open = definition("browser.open");
        assert_eq!(
            open.args_schema["properties"]["testid_attribute"]["type"],
            "string"
        );
        assert!(
            (open.validate)(&json!({"url": "http://x/", "testid_attribute": "data-qa"})).is_ok()
        );
        for bad_open in [
            json!({"url": "http://x/", "testid_attribute": ""}),
            json!({"url": "http://x/", "testid_attribute": "   "}),
            json!({"url": "http://x/", "testid_attribute": "data qa"}),
            json!({"url": "http://x/", "testid_attribute": null}),
        ] {
            assert!(
                (open.validate)(&bad_open).is_err(),
                "invalid open args must be refused: {bad_open}"
            );
        }

        for step in [
            "browser.click",
            "browser.type",
            "browser.read",
            "browser.wait",
        ] {
            let def = definition(step);
            let branches = def.args_schema["oneOf"]
                .as_array()
                .unwrap_or_else(|| panic!("`{step}` must declare oneOf"));
            let expected_branches = if step == "browser.wait" { 6 } else { 5 };
            assert_eq!(branches.len(), expected_branches, "`{step}`: {branches:?}");
            assert_eq!(def.args_schema["properties"]["testid"]["type"], "string");
            assert_eq!(def.args_schema["properties"]["nth"]["type"], "integer");
            assert_eq!(def.args_schema["properties"]["nth"]["minimum"], 0);
            assert_eq!(def.args_schema["properties"]["within"]["type"], "object");
            assert!(
                def.args_schema["properties"]["within"]["$id"].is_string(),
                "`{step}` within schema must declare an embedded $id: {}",
                def.args_schema["properties"]["within"]
            );

            let valid_testid = if step == "browser.type" {
                json!({"testid": "submit-btn", "text": "hello"})
            } else {
                json!({"testid": "submit-btn"})
            };
            assert!(
                (def.validate)(&valid_testid).is_ok(),
                "`{step}` must accept testid: {valid_testid}"
            );

            let valid_nested = if step == "browser.type" {
                json!({
                    "role": "textbox",
                    "name": "Email",
                    "text": "ada@example.com",
                    "nth": 0,
                    "within": {
                        "css": "section.card",
                        "nth": 1,
                        "within": {
                            "testid": "auth-panel"
                        }
                    }
                })
            } else {
                json!({
                    "testid": "save",
                    "nth": 0,
                    "within": {
                        "css": "section.card",
                        "nth": 1,
                        "within": {
                            "role": "dialog",
                            "name": "Settings"
                        }
                    }
                })
            };
            assert!(
                (def.validate)(&valid_nested).is_ok(),
                "`{step}` must accept nested within + nth: {valid_nested}"
            );

            let mut invalid_cases = vec![
                ("empty testid", json!({"testid": ""})),
                ("blank testid", json!({"testid": "  "})),
                ("null testid", json!({"testid": null})),
                ("css + testid", json!({"css": "#a", "testid": "b"})),
                ("name + testid", json!({"testid": "b", "name": "Ok"})),
                ("negative nth", json!({"testid": "b", "nth": -1})),
                ("float nth", json!({"testid": "b", "nth": 1.5})),
                ("string nth", json!({"testid": "b", "nth": "0"})),
                ("null nth", json!({"testid": "b", "nth": null})),
                ("null within", json!({"testid": "b", "within": null})),
                ("empty within", json!({"testid": "b", "within": {}})),
                (
                    "within two locators",
                    json!({"testid": "b", "within": {"css": "#a", "testid": "c"}}),
                ),
                (
                    "within name without role",
                    json!({"testid": "b", "within": {"css": "#a", "name": "Ok"}}),
                ),
                (
                    "within negative nth",
                    json!({"testid": "b", "within": {"css": "#a", "nth": -1}}),
                ),
            ];
            for (label, mut args) in invalid_cases.drain(..) {
                if step == "browser.type"
                    && !args.as_object().is_some_and(|o| o.contains_key("text"))
                {
                    args["text"] = json!("hello");
                }
                assert!(
                    (def.validate)(&args).is_err(),
                    "`{step}` must refuse {label}: {args}"
                );
            }
        }

        let wait = definition("browser.wait");
        for (label, args) in [
            (
                "url_contains + within",
                json!({"url_contains": "/done", "within": {"css": "#a"}}),
            ),
            (
                "url_contains + nth",
                json!({"url_contains": "/done", "nth": 0}),
            ),
            (
                "url_contains + testid",
                json!({"url_contains": "/done", "testid": "ok"}),
            ),
            (
                "url_contains + name",
                json!({"url_contains": "/done", "name": "Ok"}),
            ),
        ] {
            assert!(
                (wait.validate)(&args).is_err(),
                "`browser.wait` must refuse {label}: {args}"
            );
        }

        let target = ElementTarget {
            locator: Locator::TestId("save".to_string()),
            within: Some(Box::new(ElementTarget {
                locator: Locator::Css("form".to_string()),
                within: None,
                nth: Some(1),
            })),
            nth: Some(0),
        };
        assert_eq!(
            target.describe_with("data-qa"),
            "test id `save` (`data-qa`) (nth: 0) within (CSS selector `form` (nth: 1))"
        );
    }

    #[test]
    fn element_rect_same_geometry_handles_subpixel_jitter() {
        let base = ElementRect {
            x: 10.0,
            y: 20.0,
            width: 160.0,
            height: 36.0,
        };
        let subpixel = ElementRect {
            x: 10.5,
            y: 19.5,
            width: 160.25,
            height: 35.75,
        };
        let shifted = ElementRect {
            x: 11.0,
            y: 20.0,
            width: 160.0,
            height: 36.0,
        };
        assert!(base.same_geometry(base));
        assert!(base.same_geometry(subpixel));
        assert!(!base.same_geometry(shifted));
    }

    #[test]
    fn actionable_inspect_reports_occlusion_on_first_poll_and_tracks_stability() {
        let rect = ElementRect {
            x: 8.0,
            y: 8.0,
            width: 160.0,
            height: 36.0,
        };
        let moved = ElementRect {
            x: 24.0,
            y: 8.0,
            width: 160.0,
            height: 36.0,
        };
        let host = CandidateInfo {
            tag: "div".to_string(),
            id: "shadow-host".to_string(),
            text: String::new(),
        };

        // First poll observing an occluded element must report Occluded, not Unstable,
        // so a deadline hit before poll 2 finishes does not mask the blocker.
        let mut previous = None;
        let first = evaluate_actionable_inspect(
            &mut previous,
            ActionableInspect::ReadyCandidate {
                rect,
                x: 88.0,
                y: 26.0,
                hit_ok: false,
                blocker: Some(host.clone()),
            },
        );
        assert_eq!(first, Err(PendingReason::Occluded(Some(host.clone()))));
        assert!(previous.is_some());

        // Second poll with unchanged geometry still reports Occluded.
        let second = evaluate_actionable_inspect(
            &mut previous,
            ActionableInspect::ReadyCandidate {
                rect,
                x: 88.0,
                y: 26.0,
                hit_ok: false,
                blocker: Some(host.clone()),
            },
        );
        assert_eq!(second, Err(PendingReason::Occluded(Some(host))));

        // Second poll with changed geometry reports Unstable even if poll 1 was occluded.
        let moved_second = evaluate_actionable_inspect(
            &mut previous,
            ActionableInspect::ReadyCandidate {
                rect: moved,
                x: 104.0,
                y: 26.0,
                hit_ok: true,
                blocker: None,
            },
        );
        assert_eq!(moved_second, Err(PendingReason::Unstable));

        // Unoccluded element still requires two matching polls, and Hidden resets history.
        let mut fresh = None;
        assert_eq!(
            evaluate_actionable_inspect(
                &mut fresh,
                ActionableInspect::ReadyCandidate {
                    rect,
                    x: 88.0,
                    y: 26.0,
                    hit_ok: true,
                    blocker: None,
                },
            ),
            Err(PendingReason::Unstable)
        );
        assert_eq!(
            evaluate_actionable_inspect(&mut fresh, ActionableInspect::NotVisible),
            Err(PendingReason::Hidden)
        );
        assert!(fresh.is_none());
        assert_eq!(
            evaluate_actionable_inspect(
                &mut fresh,
                ActionableInspect::ReadyCandidate {
                    rect,
                    x: 88.0,
                    y: 26.0,
                    hit_ok: true,
                    blocker: None,
                },
            ),
            Err(PendingReason::Unstable)
        );
        assert_eq!(
            evaluate_actionable_inspect(
                &mut fresh,
                ActionableInspect::ReadyCandidate {
                    rect,
                    x: 88.0,
                    y: 26.0,
                    hit_ok: true,
                    blocker: None,
                },
            ),
            Ok((88.0, 26.0))
        );
    }

    fn spawn_mock_cdp<F>(handler: F) -> (String, thread::JoinHandle<()>)
    where
        F: FnOnce(tungstenite::WebSocket<std::net::TcpStream>) + Send + 'static,
    {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("must bind mock CDP listener");
        let port = listener
            .local_addr()
            .expect("must read mock CDP port")
            .port();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("mock CDP must accept connection");
            let _ = stream.set_nodelay(true);
            let socket = tungstenite::accept(stream).expect("mock CDP handshake must succeed");
            handler(socket);
        });
        (
            format!("ws://127.0.0.1:{port}/devtools/browser/mock"),
            handle,
        )
    }

    fn read_cdp_request(socket: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        loop {
            match socket.read().expect("mock CDP must read command") {
                tungstenite::Message::Text(text) => {
                    return serde_json::from_str(text.as_str())
                        .expect("mock CDP command must be JSON");
                }
                tungstenite::Message::Ping(_)
                | tungstenite::Message::Pong(_)
                | tungstenite::Message::Binary(_)
                | tungstenite::Message::Frame(_) => {}
                tungstenite::Message::Close(_) => panic!("client closed mock CDP unexpectedly"),
            }
        }
    }

    #[test]
    fn attach_aborts_fast_and_reports_target_crashed_event() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let create = read_cdp_request(&mut socket);
            assert_eq!(create["method"], "Target.createTarget");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": create["id"],
                        "result": { "targetId": "target-1" }
                    })
                    .to_string(),
                ))
                .expect("send createTarget reply");

            let attach_cmd = read_cdp_request(&mut socket);
            assert_eq!(attach_cmd["method"], "Target.attachToTarget");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": attach_cmd["id"],
                        "result": { "sessionId": "session-1" }
                    })
                    .to_string(),
                ))
                .expect("send attachToTarget reply");

            let page_enable = read_cdp_request(&mut socket);
            assert_eq!(page_enable["method"], "Page.enable");
            for event in [
                json!({
                    "method": "Target.targetInfoChanged",
                    "params": { "targetInfo": { "targetId": "other-target" } }
                }),
                json!({
                    "method": "Target.targetCrashed",
                    "params": {
                        "targetId": "target-1",
                        "status": "crashed",
                        "errorCode": 11
                    }
                }),
            ] {
                socket
                    .send(tungstenite::Message::text(event.to_string()))
                    .expect("send diagnostic CDP event");
            }
            while socket.read().is_ok() {}
        });

        let mut conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let started = std::time::Instant::now();
        let err = attach(&mut conn, Deadline::after(Duration::from_millis(250)))
            .expect_err("attach must fail when Page.enable never answers");
        drop(conn);
        server.join().expect("mock CDP thread must finish");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "crash event must abort attach fast instead of waiting 30s"
        );
        assert!(err.contains("Page.enable"), "missing method name: {err}");
        assert!(
            err.contains("Target.targetCrashed") && err.contains("target crashed"),
            "missing crashed event detail: {err}"
        );
        assert!(
            !err.contains("other-target"),
            "unrelated target event must not leak into error: {err}"
        );
    }

    #[test]
    fn attach_aborts_fast_and_reports_detached_event() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let create = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": create["id"],
                        "result": { "targetId": "target-1" }
                    })
                    .to_string(),
                ))
                .expect("send createTarget reply");

            let attach_cmd = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": attach_cmd["id"],
                        "result": { "sessionId": "session-1" }
                    })
                    .to_string(),
                ))
                .expect("send attachToTarget reply");

            let page_enable = read_cdp_request(&mut socket);
            assert_eq!(page_enable["method"], "Page.enable");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "method": "Target.detachedFromTarget",
                        "params": {
                            "sessionId": "session-1",
                            "targetId": "target-1"
                        }
                    })
                    .to_string(),
                ))
                .expect("send detached event");
            while socket.read().is_ok() {}
        });

        let mut conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let started = std::time::Instant::now();
        // Large outer deadline proves we fail fast on the event instead of waiting 30 s.
        let err = attach(&mut conn, Deadline::after(Duration::from_millis(250)))
            .expect_err("attach must fail when target detaches");
        drop(conn);
        server.join().expect("mock CDP thread must finish");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "detached event must abort attach fast"
        );
        assert!(err.contains("Page.enable"), "missing method name: {err}");
        assert!(
            err.contains("Target.detachedFromTarget") && err.contains("detached"),
            "missing detached event detail: {err}"
        );
    }

    #[test]
    fn attach_recovers_by_recreating_target_after_crash_and_silent_timeout() {
        // Case 1: attempt 1 emits Inspector.targetCrashed during Page.enable;
        // attach closes target-1, creates target-2, and succeeds fast inside 30s.
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let create1 = read_cdp_request(&mut socket);
            assert_eq!(create1["method"], "Target.createTarget");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": create1["id"], "result": { "targetId": "target-1" } })
                        .to_string(),
                ))
                .expect("create target-1");

            let attach1 = read_cdp_request(&mut socket);
            assert_eq!(attach1["method"], "Target.attachToTarget");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": attach1["id"], "result": { "sessionId": "session-1" } })
                        .to_string(),
                ))
                .expect("attach session-1");

            let page1 = read_cdp_request(&mut socket);
            assert_eq!(page1["method"], "Page.enable");
            socket
                .send(tungstenite::Message::text(
                    json!({ "method": "Inspector.targetCrashed", "sessionId": "session-1" })
                        .to_string(),
                ))
                .expect("emit crash on session-1");

            let close1 = read_cdp_request(&mut socket);
            assert_eq!(close1["method"], "Target.closeTarget");
            assert_eq!(close1["params"]["targetId"], "target-1");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": close1["id"], "result": { "success": true } }).to_string(),
                ))
                .expect("close target-1");

            let create2 = read_cdp_request(&mut socket);
            assert_eq!(create2["method"], "Target.createTarget");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": create2["id"], "result": { "targetId": "target-2" } })
                        .to_string(),
                ))
                .expect("create target-2");

            let attach2 = read_cdp_request(&mut socket);
            assert_eq!(attach2["method"], "Target.attachToTarget");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": attach2["id"], "result": { "sessionId": "session-2" } })
                        .to_string(),
                ))
                .expect("attach session-2");

            for expected in ["Page.enable", "Runtime.enable", "Network.enable"] {
                let cmd = read_cdp_request(&mut socket);
                assert_eq!(cmd["method"], expected);
                assert_eq!(cmd["sessionId"], "session-2");
                socket
                    .send(tungstenite::Message::text(
                        json!({ "id": cmd["id"], "result": {} }).to_string(),
                    ))
                    .expect("reply domain enable on session-2");
            }
            while socket.read().is_ok() {}
        });

        let mut conn =
            Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2))).expect("connect");
        let started = std::time::Instant::now();
        let (page, target) = attach(&mut conn, Deadline::after(Duration::from_secs(10)))
            .expect("attach must recover on target-2 after target-1 crashed");
        drop(conn);
        server.join().expect("mock server finish");
        assert_eq!((page.as_str(), target.as_str()), ("session-2", "target-2"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "crash recovery must not wait out 30s timeout"
        );

        // Case 2: attempt 1 hangs silently on Page.enable beyond per-attempt cap;
        // attach closes target-1, creates target-2, and succeeds.
        let (ws_url2, server2) = spawn_mock_cdp(|mut socket| {
            let create1 = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": create1["id"], "result": { "targetId": "target-1" } })
                        .to_string(),
                ))
                .expect("create target-1");
            let attach1 = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": attach1["id"], "result": { "sessionId": "session-1" } })
                        .to_string(),
                ))
                .expect("attach session-1");
            let page1 = read_cdp_request(&mut socket);
            assert_eq!(page1["method"], "Page.enable");
            // Never answer page1; wait for Target.closeTarget after enable_timeout expires.
            let close1 = read_cdp_request(&mut socket);
            assert_eq!(close1["method"], "Target.closeTarget");
            assert_eq!(close1["params"]["targetId"], "target-1");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": close1["id"], "result": { "success": true } }).to_string(),
                ))
                .expect("close target-1");

            let create2 = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": create2["id"], "result": { "targetId": "target-2" } })
                        .to_string(),
                ))
                .expect("create target-2");
            let attach2 = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": attach2["id"], "result": { "sessionId": "session-2" } })
                        .to_string(),
                ))
                .expect("attach session-2");
            for expected in ["Page.enable", "Runtime.enable", "Network.enable"] {
                let cmd = read_cdp_request(&mut socket);
                assert_eq!(cmd["method"], expected);
                socket
                    .send(tungstenite::Message::text(
                        json!({ "id": cmd["id"], "result": {} }).to_string(),
                    ))
                    .expect("reply on session-2");
            }
            while socket.read().is_ok() {}
        });

        let mut conn2 = Connection::connect(&ws_url2, Deadline::after(Duration::from_secs(2)))
            .expect("connect");
        let (page2, target2) = attach_with_enable_timeout(
            &mut conn2,
            Deadline::after(Duration::from_secs(5)),
            Duration::from_millis(120),
        )
        .expect("attach must recover on target-2 after silent hang on target-1");
        drop(conn2);
        server2.join().expect("mock server 2 finish");
        assert_eq!(
            (page2.as_str(), target2.as_str()),
            ("session-2", "target-2")
        );
    }

    #[test]
    fn attach_gives_up_after_max_attempts_and_reports_all_diagnostics() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            for attempt in 1..=MAX_ATTACH_ATTEMPTS {
                let tid = format!("target-{attempt}");
                let sid = format!("session-{attempt}");
                let create = read_cdp_request(&mut socket);
                assert_eq!(create["method"], "Target.createTarget");
                socket
                    .send(tungstenite::Message::text(
                        json!({ "id": create["id"], "result": { "targetId": tid } }).to_string(),
                    ))
                    .expect("create target");

                let attach_cmd = read_cdp_request(&mut socket);
                assert_eq!(attach_cmd["method"], "Target.attachToTarget");
                socket
                    .send(tungstenite::Message::text(
                        json!({ "id": attach_cmd["id"], "result": { "sessionId": sid } })
                            .to_string(),
                    ))
                    .expect("attach target");

                let page_enable = read_cdp_request(&mut socket);
                assert_eq!(page_enable["method"], "Page.enable");
                socket
                    .send(tungstenite::Message::text(
                        json!({
                            "method": "Inspector.detached",
                            "sessionId": sid,
                            "params": { "reason": "Render process gone." }
                        })
                        .to_string(),
                    ))
                    .expect("emit detached");

                if attempt < MAX_ATTACH_ATTEMPTS {
                    let close_cmd = read_cdp_request(&mut socket);
                    assert_eq!(close_cmd["method"], "Target.closeTarget");
                    socket
                        .send(tungstenite::Message::text(
                            json!({ "id": close_cmd["id"], "result": { "success": true } })
                                .to_string(),
                        ))
                        .expect("close target");
                }
            }
            while socket.read().is_ok() {}
        });

        let mut conn =
            Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2))).expect("connect");
        let started = std::time::Instant::now();
        let err = attach(&mut conn, Deadline::after(Duration::from_secs(10)))
            .expect_err("attach must give up after MAX_ATTACH_ATTEMPTS");
        drop(conn);
        server.join().expect("mock server finish");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "all 3 crash/detach attempts must fail fast"
        );
        assert!(err.contains("attempt 1:"), "{err}");
        assert!(err.contains("attempt 3:"), "{err}");
        assert!(err.contains("Render process gone."), "{err}");
    }

    #[test]
    fn attach_retry_decision_handles_target_and_browser_failures() {
        assert_eq!(
            decide_attach_retry(1, 3, AttachFailureKind::TargetFailed, false),
            AttachRetryDecision::Retry
        );
        assert_eq!(
            decide_attach_retry(2, 3, AttachFailureKind::TargetFailed, false),
            AttachRetryDecision::Retry
        );
        assert_eq!(
            decide_attach_retry(3, 3, AttachFailureKind::TargetFailed, false),
            AttachRetryDecision::GiveUp
        );
        assert_eq!(
            decide_attach_retry(1, 3, AttachFailureKind::TargetFailed, true),
            AttachRetryDecision::GiveUp
        );
        assert_eq!(
            decide_attach_retry(1, 3, AttachFailureKind::BrowserFailed, false),
            AttachRetryDecision::GiveUp
        );

        let timeout = CdpError::Timeout {
            waiting_for: "Page.enable".to_string(),
            timeout: Duration::from_secs(5),
        };
        let aborted = CdpError::Aborted {
            waiting_for: "Page.enable".to_string(),
        };
        let transport = CdpError::Transport("socket closed".to_string());
        let no_target = CdpError::Method {
            method: "Target.attachToTarget".to_string(),
            message: "No target with given id found".to_string(),
        };

        assert_eq!(
            classify_attach_failure(AttachStep::CreateTarget, &timeout),
            AttachFailureKind::BrowserFailed
        );
        assert_eq!(
            classify_attach_failure(AttachStep::AttachToTarget, &no_target),
            AttachFailureKind::TargetFailed
        );
        assert_eq!(
            classify_attach_failure(AttachStep::AttachToTarget, &transport),
            AttachFailureKind::BrowserFailed
        );
        assert_eq!(
            classify_attach_failure(AttachStep::EnableDomain, &timeout),
            AttachFailureKind::TargetFailed
        );
        assert_eq!(
            classify_attach_failure(AttachStep::EnableDomain, &aborted),
            AttachFailureKind::TargetFailed
        );
        assert_eq!(
            classify_attach_failure(AttachStep::EnableDomain, &transport),
            AttachFailureKind::BrowserFailed
        );
    }

    #[test]
    fn attach_timeout_without_events_reports_browser_never_answered() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let create = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": create["id"],
                        "result": { "targetId": "target-1" }
                    })
                    .to_string(),
                ))
                .expect("send createTarget reply");

            let attach_cmd = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": attach_cmd["id"],
                        "result": { "sessionId": "session-1" }
                    })
                    .to_string(),
                ))
                .expect("send attachToTarget reply");

            let page_enable = read_cdp_request(&mut socket);
            assert_eq!(page_enable["method"], "Page.enable");
            while socket.read().is_ok() {}
        });

        let mut conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let err = attach(&mut conn, Deadline::after(Duration::from_millis(250)))
            .expect_err("attach must fail when Page.enable never answers");
        drop(conn);
        server.join().expect("mock CDP thread must finish");

        assert!(err.contains("Page.enable"), "missing method name: {err}");
        assert!(
            err.contains("never answered") && err.contains("no CDP events"),
            "bare timeout must explain that the browser never answered and sent no events: {err}"
        );
    }

    #[test]
    fn target_error_formatter_summarizes_inspector_crash_detach_and_bounds_events() {
        let timeout = CdpError::Timeout {
            waiting_for: "Page.enable".to_string(),
            timeout: Duration::from_secs(30),
        };
        let inspector = format_target_error(
            "could not enable page events",
            &timeout,
            Some("t-1"),
            Some("s-1"),
            &[
                json!({ "id": 99, "result": {} }),
                json!({ "method": "Inspector.targetCrashed", "sessionId": "s-1" }),
                json!({
                    "method": "Inspector.detached",
                    "sessionId": "s-1",
                    "params": { "reason": "Render process gone" }
                }),
                json!({
                    "method": "Target.targetDestroyed",
                    "params": { "targetId": "t-1" }
                }),
            ],
        );
        assert!(
            inspector.contains("target crashed (Inspector.targetCrashed)"),
            "{inspector}"
        );
        assert!(
            inspector.contains("target detached (Inspector.detached: Render process gone)"),
            "{inspector}"
        );
        assert!(
            inspector.contains("target destroyed (Target.targetDestroyed)"),
            "{inspector}"
        );

        let many_events: Vec<Value> = (0..7)
            .map(|i| {
                json!({
                    "method": format!("Page.event{i}"),
                    "sessionId": "s-1"
                })
            })
            .collect();
        let bounded = format_target_error(
            "could not enable page events",
            &timeout,
            Some("t-1"),
            Some("s-1"),
            &many_events,
        );
        assert!(bounded.contains("Page.event0"), "{bounded}");
        assert!(bounded.contains("Page.event4"), "{bounded}");
        assert!(!bounded.contains("Page.event5"), "{bounded}");
        assert!(bounded.contains("+2 more"), "{bounded}");

        let transport = CdpError::Transport("socket closed".to_string());
        let closed = format_target_error(
            "could not enable page events",
            &transport,
            Some("t-1"),
            Some("s-1"),
            &[json!({
                "method": "Target.targetCrashed",
                "params": { "targetId": "t-1", "status": "killed" }
            })],
        );
        assert!(
            closed.contains("socket closed")
                && closed.contains("target crashed (Target.targetCrashed: status=killed)"),
            "{closed}"
        );
    }

    fn mock_session(connection: Connection, target_id: &str, session_id: &str) -> BrowserSession {
        BrowserSession {
            connection: Some(connection),
            origin: Some(Origin::Connected),
            target_id: target_id.to_string(),
            page: session_id.to_string(),
            network: network::NetworkState::new(session_id),
            port: 9222,
            headless: true,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            connect: Some("http://127.0.0.1:9222".to_string()),
            testid_attribute: DEFAULT_TESTID_ATTRIBUTE.to_string(),
        }
    }

    #[test]
    fn navigate_aborts_fast_on_arriving_detach_and_reports_events() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let nav = read_cdp_request(&mut socket);
            assert_eq!(nav["method"], "Page.navigate");
            for event in [
                json!({
                    "method": "Target.targetInfoChanged",
                    "params": { "targetInfo": { "targetId": "other-target" } }
                }),
                json!({
                    "method": "Inspector.detached",
                    "sessionId": "session-1",
                    "params": { "reason": "target closed" }
                }),
                json!({
                    "method": "Target.targetDestroyed",
                    "params": { "targetId": "target-1" }
                }),
            ] {
                socket
                    .send(tungstenite::Message::text(event.to_string()))
                    .expect("send diagnostic CDP event");
            }
            while socket.read().is_ok() {}
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut session = mock_session(conn, "target-1", "session-1");
        let started = std::time::Instant::now();
        let err = session
            .navigate(
                "http://127.0.0.1/page",
                Duration::from_secs(5),
                &mut DialogContext::fail_fast(),
            )
            .expect_err("navigate must abort when session detaches");
        drop(session);
        server.join().expect("mock CDP thread must finish");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "detach event must abort navigate fast, took {:?}",
            started.elapsed()
        );
        match err {
            StepError::Environment(message) => {
                assert!(message.contains("Page.navigate"), "{message}");
                assert!(
                    message.contains("target detached (Inspector.detached: target closed)"),
                    "{message}"
                );
                assert!(
                    !message.contains("other-target"),
                    "unrelated target event must not appear: {message}"
                );
            }
            other => panic!("expected environment error on target death, got {other:?}"),
        }
    }

    #[test]
    fn navigate_preserves_already_queued_terminal_event_across_stale_clear() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let enable = read_cdp_request(&mut socket);
            assert_eq!(enable["method"], "Page.enable");
            for payload in [
                json!({ "method": "Page.loadEventFired", "sessionId": "session-1" }),
                json!({
                    "method": "Target.targetCrashed",
                    "params": { "targetId": "target-1", "status": "killed", "errorCode": 9 }
                }),
                json!({ "id": enable["id"], "result": {} }),
            ] {
                socket
                    .send(tungstenite::Message::text(payload.to_string()))
                    .expect("send queued events and reply");
            }
            while socket.read().is_ok() {}
        });

        let mut conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        conn.call(
            "Page.enable",
            json!({}),
            Some("session-1"),
            Deadline::after(Duration::from_secs(2)),
        )
        .expect("Page.enable completes after emitting queued events");
        let mut session = mock_session(conn, "target-1", "session-1");
        let started = std::time::Instant::now();
        let err = session
            .navigate(
                "http://127.0.0.1/page",
                Duration::from_secs(5),
                &mut DialogContext::fail_fast(),
            )
            .expect_err("navigate must abort immediately on queued crash event");
        drop(session);
        server.join().expect("mock CDP thread must finish");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "already-queued crash event must abort navigate fast, took {:?}",
            started.elapsed()
        );
        match err {
            StepError::Environment(message) => {
                assert!(
                    message.contains(
                        "target crashed (Target.targetCrashed: status=killed, errorCode=9)"
                    ),
                    "{message}"
                );
                assert!(
                    !message.contains("Page.loadEventFired"),
                    "stale load event must remain cleared: {message}"
                );
            }
            other => panic!("expected environment error on queued crash event, got {other:?}"),
        }
    }

    #[test]
    fn load_wait_aborts_fast_and_names_the_terminal_event() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let nav = read_cdp_request(&mut socket);
            assert_eq!(nav["method"], "Page.navigate");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": nav["id"], "result": { "loaderId": "loader-1" } }).to_string(),
                ))
                .expect("send navigate reply");
            // `Page.loadEventFired` never arrives: the page dies instead.
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "method": "Target.targetCrashed",
                        "params": {
                            "targetId": "target-1",
                            "status": "crashed",
                            "errorCode": 11
                        }
                    })
                    .to_string(),
                ))
                .expect("send crash event");
            while socket.read().is_ok() {}
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut session = mock_session(conn, "target-1", "session-1");
        let started = std::time::Instant::now();
        let err = session
            .navigate(
                "http://127.0.0.1/page",
                Duration::from_secs(5),
                &mut DialogContext::fail_fast(),
            )
            .expect_err("the load wait must abort when the page crashes");
        drop(session);
        server.join().expect("mock CDP thread must finish");

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the load wait must abort at once instead of waiting 5 s, took {:?}",
            started.elapsed()
        );
        match err {
            StepError::Environment(message) => {
                assert!(message.contains("Target.targetCrashed"), "{message}");
                assert!(message.contains("target crashed"), "{message}");
            }
            other => panic!("expected an environment error on page death, got {other:?}"),
        }
    }

    #[test]
    fn load_wait_timeout_reports_that_no_target_event_arrived() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let nav = read_cdp_request(&mut socket);
            assert_eq!(nav["method"], "Page.navigate");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": nav["id"], "result": { "loaderId": "loader-1" } }).to_string(),
                ))
                .expect("send navigate reply");
            while socket.read().is_ok() {}
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut session = mock_session(conn, "target-1", "session-1");
        let err = session
            .navigate(
                "http://127.0.0.1/page",
                Duration::from_millis(250),
                &mut DialogContext::fail_fast(),
            )
            .expect_err("a silent load wait must time out");
        drop(session);
        server.join().expect("mock CDP thread must finish");

        match err {
            StepError::Timeout(message) => {
                assert!(
                    message.contains(
                        "the page `http://127.0.0.1/page` did not finish loading within 250 ms"
                    ),
                    "{message}"
                );
                assert!(message.contains("no CDP events"), "{message}");
            }
            other => panic!("expected a timeout error, got {other:?}"),
        }
    }

    #[test]
    fn locator_action_timeout_keeps_its_wording_and_notes_no_events() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let evaluate = read_cdp_request(&mut socket);
            assert_eq!(evaluate["method"], "Runtime.evaluate");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": evaluate["id"],
                        "result": { "result": { "objectId": "element-1" } }
                    })
                    .to_string(),
                ))
                .expect("send locator reply");

            let visible = read_cdp_request(&mut socket);
            assert_eq!(visible["method"], "Runtime.callFunctionOn");
            assert!(
                visible["params"]["functionDeclaration"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("getComputedStyle"),
                "the first action must be the visibility check: {visible}"
            );
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": visible["id"], "result": { "result": { "value": true } } })
                        .to_string(),
                ))
                .expect("send visibility reply");

            // The text read never answers; only the session close follows.
            let text = read_cdp_request(&mut socket);
            assert_eq!(text["method"], "Runtime.callFunctionOn");
            loop {
                match socket.read() {
                    Ok(tungstenite::Message::Text(frame)) => {
                        let command: Value = serde_json::from_str(frame.as_str())
                            .expect("mock CDP command must be JSON");
                        if command["method"] == "Target.closeTarget" {
                            socket
                                .send(tungstenite::Message::text(
                                    json!({ "id": command["id"], "result": { "success": true } })
                                        .to_string(),
                                ))
                                .expect("send close reply");
                        }
                    }
                    Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut sessions = Sessions::new();
        sessions
            .get_or_create(SESSION_KIND, || {
                Ok(mock_session(conn, "target-1", "session-1"))
            })
            .expect("must store the mock browser session");
        let err = run_read(&json!({ "css": "#go", "timeout_ms": 250 }), &mut sessions)
            .expect_err("a silent text read must time out");
        drop(sessions);
        server.join().expect("mock CDP thread must finish");

        match err {
            StepError::Timeout(message) => {
                assert!(
                    message.contains("could not read the text of CSS selector `#go` within 250 ms"),
                    "{message}"
                );
                assert!(message.contains("no CDP events"), "{message}");
            }
            other => panic!("expected a timeout error, got {other:?}"),
        }
    }

    #[test]
    fn press_timeout_keeps_its_wording_and_notes_no_events() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let press = read_cdp_request(&mut socket);
            assert_eq!(press["method"], "Input.dispatchKeyEvent");
            // The key event never answers; the press waits out its 5 s budget.
            loop {
                match socket.read() {
                    Ok(tungstenite::Message::Text(frame)) => {
                        let command: Value = serde_json::from_str(frame.as_str())
                            .expect("mock CDP command must be JSON");
                        if command["method"] == "Target.closeTarget" {
                            socket
                                .send(tungstenite::Message::text(
                                    json!({ "id": command["id"], "result": { "success": true } })
                                        .to_string(),
                                ))
                                .expect("send close reply");
                        }
                    }
                    Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut sessions = Sessions::new();
        sessions
            .get_or_create(SESSION_KIND, || {
                Ok(mock_session(conn, "target-1", "session-1"))
            })
            .expect("must store the mock browser session");
        let err = run_press(&json!({ "key": "Enter" }), &mut sessions)
            .expect_err("a silent press must time out");
        drop(sessions);
        server.join().expect("mock CDP thread must finish");

        match err {
            StepError::Timeout(message) => {
                assert!(
                    message.contains("could not press `Enter` within 5000 ms"),
                    "{message}"
                );
                assert!(message.contains("no CDP events"), "{message}");
            }
            other => panic!("expected a timeout error, got {other:?}"),
        }
    }

    #[test]
    fn element_step_aborts_fast_when_the_page_dies_mid_evaluate() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let evaluate = read_cdp_request(&mut socket);
            assert_eq!(evaluate["method"], "Runtime.evaluate");
            // The locator expression never answers; the page dies instead.
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "method": "Target.targetCrashed",
                        "params": {
                            "targetId": "target-1",
                            "status": "crashed",
                            "errorCode": 11
                        }
                    })
                    .to_string(),
                ))
                .expect("send crash event");
            while socket.read().is_ok() {}
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut sessions = Sessions::new();
        sessions
            .get_or_create(SESSION_KIND, || {
                Ok(mock_session(conn, "target-1", "session-1"))
            })
            .expect("must store the mock browser session");
        let started = std::time::Instant::now();
        let err = run_click(&json!({ "css": "#go", "timeout_ms": 3000 }), &mut sessions)
            .expect_err("a click must fail at once when the page crashes");
        // The step's own duration is what matters; closing the dead session has
        // its own budget.
        let elapsed = started.elapsed();
        drop(sessions);
        server.join().expect("mock CDP thread must finish");

        assert!(
            elapsed < Duration::from_secs(1),
            "the element step must abort at once instead of waiting 3 s, took {elapsed:?}"
        );
        match err {
            StepError::Environment(message) => {
                assert!(message.contains("Target.targetCrashed"), "{message}");
                assert!(message.contains("target crashed"), "{message}");
            }
            other => panic!("expected an environment error on page death, got {other:?}"),
        }
    }

    #[test]
    fn close_after_a_page_death_returns_without_an_error() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let evaluate = read_cdp_request(&mut socket);
            assert_eq!(evaluate["method"], "Runtime.evaluate");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "method": "Inspector.detached",
                        "sessionId": "session-1",
                        "params": { "reason": "Render process gone." }
                    })
                    .to_string(),
                ))
                .expect("send detached event");
            // Nothing answers `Target.closeTarget`: the tab is already gone.
            while socket.read().is_ok() {}
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut session = mock_session(conn, "target-1", "session-1");
        let target = ElementTarget::from_parts(Locator::Css("#go".to_string()), None, None)
            .expect("a css locator needs no modifiers");
        let err = match session.locate_target(
            &target,
            Deadline::after(Duration::from_secs(5)),
            &mut DialogContext::fail_fast(),
        ) {
            Ok(_) => panic!("locating must abort when the page detaches"),
            Err(error) => error,
        };
        assert!(
            matches!(err, StepError::Environment(_)),
            "expected an environment error on page death, got {err:?}"
        );
        let started = std::time::Instant::now();
        let closed = session.close();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "close must not hang on a dead page, took {:?}",
            started.elapsed()
        );
        assert!(closed.is_ok(), "close must accept a dead page: {closed:?}");
        drop(session);
        server.join().expect("mock CDP thread must finish");
    }

    #[test]
    fn navigate_timeout_reports_non_terminal_events_or_none_received() {
        // 1. Silent timeout: no events received for this target or session.
        let (silent_url, silent_server) = spawn_mock_cdp(|mut socket| {
            let nav = read_cdp_request(&mut socket);
            assert_eq!(nav["method"], "Page.navigate");
            while socket.read().is_ok() {}
        });
        let silent_conn = Connection::connect(&silent_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut silent_session = mock_session(silent_conn, "target-1", "session-1");
        let silent_err = silent_session
            .navigate(
                "http://127.0.0.1/silent",
                Duration::from_millis(250),
                &mut DialogContext::fail_fast(),
            )
            .expect_err("silent navigate must time out");
        drop(silent_session);
        silent_server.join().expect("silent server must finish");
        match silent_err {
            StepError::Timeout(message) => {
                assert!(message.contains("Page.navigate"), "{message}");
                assert!(
                    message.contains("never answered") && message.contains("no CDP events"),
                    "{message}"
                );
            }
            other => panic!("expected timeout error, got {other:?}"),
        }

        // 2. Timeout with non-terminal events: names the events received while waiting.
        let (noisy_url, noisy_server) = spawn_mock_cdp(|mut socket| {
            let nav = read_cdp_request(&mut socket);
            assert_eq!(nav["method"], "Page.navigate");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "method": "Page.frameStartedLoading",
                        "sessionId": "session-1",
                        "params": { "frameId": "frame-1" }
                    })
                    .to_string(),
                ))
                .expect("send frameStartedLoading");
            while socket.read().is_ok() {}
        });
        let noisy_conn = Connection::connect(&noisy_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut noisy_session = mock_session(noisy_conn, "target-1", "session-1");
        let noisy_err = noisy_session
            .navigate(
                "http://127.0.0.1/noisy",
                Duration::from_millis(250),
                &mut DialogContext::fail_fast(),
            )
            .expect_err("non-terminal event must not abort before timeout");
        drop(noisy_session);
        noisy_server.join().expect("noisy server must finish");
        match noisy_err {
            StepError::Timeout(message) => {
                assert!(message.contains("Page.navigate"), "{message}");
                assert!(
                    message.contains("browser events: Page.frameStartedLoading"),
                    "{message}"
                );
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
    }

    #[test]
    fn dialog_option_schema_and_validate_agree() {
        let cases: [(&str, Value); 5] = [
            ("browser.open", json!({"url": "http://127.0.0.1:1/"})),
            ("browser.click", json!({"css": "#go"})),
            ("browser.type", json!({"css": "#go", "text": "hello"})),
            ("browser.press", json!({"key": "Enter"})),
            ("browser.wait", json!({"css": "#go"})),
        ];
        for (step_name, base) in cases {
            let def = definition(step_name);
            for valid_dialog in [
                json!("accept"),
                json!("dismiss"),
                json!({"action": "accept"}),
                json!({"action": "dismiss"}),
                json!({"action": "accept", "prompt_text": ""}),
                json!({"action": "accept", "prompt_text": "hello"}),
            ] {
                let mut args = base.clone();
                args["dialog"] = valid_dialog.clone();
                assert!(
                    (def.validate)(&args).is_ok(),
                    "`{step_name}` validate must accept {valid_dialog}: {args}"
                );
            }
            for invalid_dialog in [
                json!(null),
                json!("cancel"),
                json!(123),
                json!({}),
                json!({"action": null}),
                json!({"action": "cancel"}),
                json!({"action": "accept", "prompt_text": null}),
                json!({"action": "accept", "prompt_text": 42}),
                json!({"action": "accept", "extra": true}),
            ] {
                let mut args = base.clone();
                args["dialog"] = invalid_dialog.clone();
                assert!(
                    (def.validate)(&args).is_err(),
                    "`{step_name}` validate must refuse {invalid_dialog}: {args}"
                );
            }
        }
    }

    #[test]
    fn beforeunload_dismiss_stays_on_page_without_environment_error() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let nav = read_cdp_request(&mut socket);
            assert_eq!(nav["method"], "Page.navigate");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "method": "Page.javascriptDialogOpening",
                        "sessionId": "session-1",
                        "params": { "type": "beforeunload", "message": "" }
                    })
                    .to_string(),
                ))
                .expect("send beforeunload");

            let handle = read_cdp_request(&mut socket);
            assert_eq!(handle["method"], "Page.handleJavaScriptDialog");
            assert_eq!(handle["params"]["accept"], false);
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": handle["id"], "result": {} }).to_string(),
                ))
                .expect("reply handleJavaScriptDialog");

            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": nav["id"],
                        "result": { "frameId": "frame-1", "errorText": "net::ERR_ABORTED" }
                    })
                    .to_string(),
                ))
                .expect("reply aborted Page.navigate");

            let href = read_cdp_request(&mut socket);
            assert_eq!(href["method"], "Runtime.evaluate");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": href["id"], "result": { "result": { "value": "http://127.0.0.1/stay" } } })
                        .to_string(),
                ))
                .expect("reply location.href");

            let title = read_cdp_request(&mut socket);
            assert_eq!(title["method"], "Runtime.evaluate");
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": title["id"], "result": { "result": { "value": "Stay" } } })
                        .to_string(),
                ))
                .expect("reply document.title");
            while socket.read().is_ok() {}
        });

        let conn = Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2)))
            .expect("must connect to mock CDP");
        let mut session = mock_session(conn, "target-1", "session-1");
        let mut dialog = DialogContext::from_arg(Some(&DialogArg::Action(DialogAction::Dismiss)));
        let (url, title) = session
            .navigate(
                "http://127.0.0.1/leave",
                Duration::from_secs(5),
                &mut dialog,
            )
            .expect("dismissing beforeunload must keep the current page");
        drop(session);
        server.join().expect("mock server finish");

        assert_eq!(url, "http://127.0.0.1/stay");
        assert_eq!(title, "Stay");
        assert_eq!(
            dialog.opened,
            Some(OpenedDialog {
                dialog_type: "beforeunload".to_string(),
                message: String::new(),
                accepted: false,
            })
        );
    }

    type MockSocket = tungstenite::WebSocket<std::net::TcpStream>;

    fn send_mock_reply(socket: &mut MockSocket, id: &Value, result: Value) {
        socket
            .send(tungstenite::Message::text(
                json!({ "id": id, "result": result }).to_string(),
            ))
            .expect("send mock CDP reply");
    }

    fn send_mock_dialog(socket: &mut MockSocket, dialog_type: &str, message: &str) {
        socket
            .send(tungstenite::Message::text(
                json!({
                    "sessionId": "session-1",
                    "method": "Page.javascriptDialogOpening",
                    "params": { "type": dialog_type, "message": message }
                })
                .to_string(),
            ))
            .expect("send mock dialog opening");
    }

    fn mock_open_handshake(socket: &mut MockSocket, on_navigate: impl FnOnce(&mut MockSocket)) {
        let create = read_cdp_request(socket);
        assert_eq!(create["method"], "Target.createTarget");
        send_mock_reply(socket, &create["id"], json!({ "targetId": "target-1" }));

        let attach_cmd = read_cdp_request(socket);
        assert_eq!(attach_cmd["method"], "Target.attachToTarget");
        send_mock_reply(
            socket,
            &attach_cmd["id"],
            json!({ "sessionId": "session-1" }),
        );

        for expected in ["Page.enable", "Runtime.enable", "Network.enable"] {
            let cmd = read_cdp_request(socket);
            assert_eq!(cmd["method"], expected);
            send_mock_reply(socket, &cmd["id"], json!({}));
        }

        let nav = read_cdp_request(socket);
        assert_eq!(nav["method"], "Page.navigate");
        on_navigate(socket);
        socket
            .send(tungstenite::Message::text(
                json!({ "sessionId": "session-1", "method": "Page.loadEventFired" }).to_string(),
            ))
            .expect("send loadEventFired");
        send_mock_reply(socket, &nav["id"], json!({ "loaderId": "loader-1" }));

        for expected_val in [
            "http://127.0.0.1/app",
            "complete",
            "http://127.0.0.1/app",
            "App",
        ] {
            let req = read_cdp_request(socket);
            assert_eq!(req["method"], "Runtime.evaluate");
            send_mock_reply(
                socket,
                &req["id"],
                json!({ "result": { "value": expected_val } }),
            );
        }
    }

    fn mock_close_loop(socket: &mut MockSocket) {
        loop {
            match socket.read() {
                Ok(tungstenite::Message::Text(frame)) => {
                    let cmd: Value = serde_json::from_str(frame.as_str())
                        .expect("mock CDP command must be JSON");
                    if cmd["method"] == "Target.closeTarget" {
                        send_mock_reply(socket, &cmd["id"], json!({ "success": true }));
                        break;
                    }
                }
                Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }

    fn run_mock_browser_retry_case(
        name: &str,
        observe: Vec<crate::core::case::Observation>,
        second_step: crate::core::case::Step,
        cdp_handler: impl FnOnce(MockSocket) + Send + 'static,
    ) -> crate::core::report::RunReport {
        use std::collections::BTreeMap;
        use std::io::Read as _;
        use std::net::TcpListener;

        use crate::core::case::{Case, LoadedCase, Step};
        use crate::core::events::EventBus;
        use crate::core::filter::Filters;
        use crate::core::report::RunOutcome;
        use crate::core::runner;
        use crate::core::secrets::Secrets;
        use serde_saphyr::Location;

        let (ws_url, cdp_server) = spawn_mock_cdp(cdp_handler);
        let http_listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock /json/version");
        let http_port = http_listener.local_addr().expect("http port").port();
        let http_server = thread::spawn(move || {
            let (mut stream, _) = http_listener.accept().expect("accept /json/version");
            let mut buf = [0_u8; 512];
            let _ = stream.read(&mut buf);
            let body = json!({ "webSocketDebuggerUrl": ws_url }).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write /json/version response");
        });

        let mut reg = Registry::new();
        register(&mut reg).expect("browser module must register");

        let case = Case {
            name: name.to_string(),
            vars: BTreeMap::new(),
            project: None,
            suite: None,
            layer: None,
            tags: Vec::new(),
            observe,
            steps: vec![
                Step {
                    action: "browser.open".to_string(),
                    args: json!({
                        "connect": format!("http://127.0.0.1:{http_port}"),
                        "url": "http://127.0.0.1/app",
                    }),
                    optional: false,
                    retry: None,
                    expect: Vec::new(),
                    save: Vec::new(),
                    location: Location::UNKNOWN,
                    args_location: Location::UNKNOWN,
                    retry_location: Location::UNKNOWN,
                    arg_locations: Vec::new(),
                },
                second_step,
            ],
            finally: Vec::new(),
        };

        let outcome = runner::run(
            &[LoadedCase {
                path: "mock-browser-retry.yaml".into(),
                case,
            }],
            &reg,
            &EventBus::new(),
            None,
            &Filters::default(),
            &mut Secrets::new(),
            1,
        );

        http_server
            .join()
            .expect("mock /json/version server must finish");
        cdp_server.join().expect("mock CDP server must finish");

        let RunOutcome::Completed(report) = outcome else {
            panic!("expected completed run, got {outcome:?}");
        };
        report
    }

    fn mock_read_step(
        timeout_ms: u64,
        retry: crate::core::case::RetryPolicy,
    ) -> crate::core::case::Step {
        use serde_saphyr::Location;
        crate::core::case::Step {
            action: "browser.read".to_string(),
            args: json!({ "css": "#status", "timeout_ms": timeout_ms }),
            optional: false,
            retry: Some(retry),
            expect: Vec::new(),
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        }
    }

    fn mock_wait_step(
        dialog: Option<Value>,
        timeout_ms: u64,
        retry: crate::core::case::RetryPolicy,
    ) -> crate::core::case::Step {
        use serde_saphyr::Location;
        let mut args = json!({ "css": "#status", "timeout_ms": timeout_ms });
        if let Some(dialog) = dialog {
            args["dialog"] = dialog;
        }
        crate::core::case::Step {
            action: "browser.wait".to_string(),
            args,
            optional: false,
            retry: Some(retry),
            expect: Vec::new(),
            save: Vec::new(),
            location: Location::UNKNOWN,
            args_location: Location::UNKNOWN,
            retry_location: Location::UNKNOWN,
            arg_locations: Vec::new(),
        }
    }

    #[test]
    fn retry_with_browser_network_discards_failed_attempt_requests() {
        use crate::core::case::{Expectation, Matcher, Observation, RetryPolicy};
        use crate::core::report::CaseStatus;
        use serde_saphyr::Location;

        let send_request = |socket: &mut MockSocket, request_id: &str, url: &str, status: i64| {
            for payload in [
                json!({
                    "sessionId": "session-1",
                    "method": "Network.requestWillBeSent",
                    "params": {
                        "requestId": request_id,
                        "type": "Fetch",
                        "request": { "method": "GET", "url": url }
                    }
                }),
                json!({
                    "sessionId": "session-1",
                    "method": "Network.responseReceived",
                    "params": {
                        "requestId": request_id,
                        "type": "Fetch",
                        "response": { "url": url, "status": status }
                    }
                }),
                json!({
                    "sessionId": "session-1",
                    "method": "Network.loadingFinished",
                    "params": { "requestId": request_id }
                }),
            ] {
                socket
                    .send(tungstenite::Message::text(payload.to_string()))
                    .expect("send mock network event");
            }
        };

        let mut step = mock_read_step(
            20,
            RetryPolicy {
                until: vec![Expectation {
                    path: "$.text".to_string(),
                    matcher: Matcher::Equals(json!("ready")),
                }],
                timeout_ms: 3000,
                interval_ms: 10,
            },
        );
        step.expect = vec![Expectation {
            path: "$.observed.net.requests".to_string(),
            matcher: Matcher::Equals(json!([{
                "method": "GET",
                "url": "http://127.0.0.1/attempt3-ok",
                "status": 200,
                "type": "Fetch",
                "failed": null
            }])),
        }];

        let report = run_mock_browser_retry_case(
            "browser network retry isolation",
            vec![Observation {
                name: "net".to_string(),
                kind: network::OBSERVER_NAME.to_string(),
                args: json!({}),
                location: Location::UNKNOWN,
                args_location: Location::UNKNOWN,
            }],
            step,
            move |mut socket| {
                mock_open_handshake(&mut socket, |sock| {
                    send_request(sock, "req-doc", "http://127.0.0.1/app", 200);
                });

                // attempt 1: Execution (ambiguous)
                let attempt1 = read_cdp_request(&mut socket);
                assert_eq!(attempt1["method"], "Runtime.evaluate");
                send_request(
                    &mut socket,
                    "req-exec",
                    "http://127.0.0.1/attempt1-exec",
                    500,
                );
                send_mock_reply(
                    &mut socket,
                    &attempt1["id"],
                    json!({
                        "result": {
                            "type": "string",
                            "value": serde_json::to_string(&json!({
                                "status": "ambiguous",
                                "count": 2,
                                "candidates": []
                            })).expect("serialize status")
                        }
                    }),
                );

                // attempt 2: Timeout
                let attempt2_loc = read_cdp_request(&mut socket);
                assert_eq!(attempt2_loc["method"], "Runtime.evaluate");
                send_request(
                    &mut socket,
                    "req-timeout",
                    "http://127.0.0.1/attempt2-timeout",
                    504,
                );
                thread::sleep(Duration::from_millis(30));
                send_mock_reply(
                    &mut socket,
                    &attempt2_loc["id"],
                    json!({
                        "result": {
                            "type": "string",
                            "value": serde_json::to_string(&json!({
                                "status": "missing",
                                "hidden_found": false
                            })).expect("serialize missing status")
                        }
                    }),
                );

                // attempt 3: succeeds
                let attempt3_loc = read_cdp_request(&mut socket);
                assert_eq!(attempt3_loc["method"], "Runtime.evaluate");
                send_mock_reply(
                    &mut socket,
                    &attempt3_loc["id"],
                    json!({ "result": { "objectId": "elem-3" } }),
                );
                let attempt3_vis = read_cdp_request(&mut socket);
                assert_eq!(attempt3_vis["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &attempt3_vis["id"],
                    json!({ "result": { "value": true } }),
                );
                let attempt3_text = read_cdp_request(&mut socket);
                assert_eq!(attempt3_text["method"], "Runtime.callFunctionOn");
                send_request(&mut socket, "req-ok", "http://127.0.0.1/attempt3-ok", 200);
                send_mock_reply(
                    &mut socket,
                    &attempt3_text["id"],
                    json!({ "result": { "value": "ready" } }),
                );

                mock_close_loop(&mut socket);
            },
        );
        assert_eq!(
            report.cases[0].status,
            CaseStatus::Passed,
            "stale failed-attempt network requests leaked into final observation: {report:?}"
        );
        let net = &report.cases[0].steps[1]
            .result
            .as_ref()
            .expect("browser.read result must exist")["observed"]["net"];
        assert_eq!(
            net["requests"],
            json!([{
                "method": "GET",
                "url": "http://127.0.0.1/attempt3-ok",
                "status": 200,
                "type": "Fetch",
                "failed": null
            }]),
            "only attempt 3's request may appear in observed.net: {net}"
        );
    }

    #[test]
    fn retry_browser_read_fails_fast_on_unexpected_dialog_and_recovers() {
        use crate::core::case::{Expectation, Matcher, RetryPolicy};
        use crate::core::report::CaseStatus;

        let started = std::time::Instant::now();
        let report = run_mock_browser_retry_case(
            "read retry dialog recovery",
            Vec::new(),
            mock_read_step(
                5000,
                RetryPolicy {
                    until: vec![Expectation {
                        path: "$.text".to_string(),
                        matcher: Matcher::Equals(json!("ready")),
                    }],
                    timeout_ms: 2000,
                    interval_ms: 10,
                },
            ),
            |mut socket| {
                mock_open_handshake(&mut socket, |_| {});

                // Attempt 1: unexpected alert opens -> driver must dismiss (accept: false) and fail fast
                let attempt1 = read_cdp_request(&mut socket);
                assert_eq!(attempt1["method"], "Runtime.evaluate");
                send_mock_dialog(&mut socket, "alert", "transient");
                let handle = read_cdp_request(&mut socket);
                assert_eq!(handle["method"], "Page.handleJavaScriptDialog");
                assert_eq!(handle["params"]["accept"], false);
                send_mock_reply(&mut socket, &handle["id"], json!({}));
                // Send late reply for the abandoned Runtime.evaluate so `ignored_reply_ids` is exercised.
                send_mock_reply(
                    &mut socket,
                    &attempt1["id"],
                    json!({ "result": { "value": null } }),
                );

                // Attempt 2: element resolves and reads "ready"
                let attempt2_loc = read_cdp_request(&mut socket);
                assert_eq!(attempt2_loc["method"], "Runtime.evaluate");
                send_mock_reply(
                    &mut socket,
                    &attempt2_loc["id"],
                    json!({ "result": { "objectId": "elem-2" } }),
                );
                let attempt2_vis = read_cdp_request(&mut socket);
                assert_eq!(attempt2_vis["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &attempt2_vis["id"],
                    json!({ "result": { "value": true } }),
                );
                let attempt2_text = read_cdp_request(&mut socket);
                assert_eq!(attempt2_text["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &attempt2_text["id"],
                    json!({ "result": { "value": "ready" } }),
                );

                mock_close_loop(&mut socket);
            },
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "unexpected dialog must fail fast instead of waiting step timeout_ms"
        );
        assert_eq!(report.cases[0].status, CaseStatus::Passed, "{report:?}");
        assert_eq!(
            report.cases[0].steps[1].result,
            Some(json!({ "text": "ready" }))
        );
    }

    #[test]
    fn retry_browser_read_expires_fast_reporting_unexpected_dialog_error() {
        use crate::core::case::{Expectation, Matcher, RetryPolicy};
        use crate::core::report::{CaseStatus, ProblemKind, StepStatus};

        let started = std::time::Instant::now();
        let report = run_mock_browser_retry_case(
            "read retry dialog expiry",
            Vec::new(),
            mock_read_step(
                5000,
                RetryPolicy {
                    until: vec![Expectation {
                        path: "$.text".to_string(),
                        matcher: Matcher::Equals(json!("ready")),
                    }],
                    timeout_ms: 80,
                    interval_ms: 10,
                },
            ),
            |mut socket| {
                mock_open_handshake(&mut socket, |_| {});
                loop {
                    let cmd = match socket.read() {
                        Ok(tungstenite::Message::Text(frame)) => {
                            serde_json::from_str::<Value>(frame.as_str()).expect("valid JSON")
                        }
                        Ok(tungstenite::Message::Close(_)) | Err(_) => break,
                        Ok(_) => continue,
                    };
                    match cmd["method"].as_str() {
                        Some("Runtime.evaluate") => {
                            send_mock_dialog(&mut socket, "confirm", "still blocked");
                            let handle = read_cdp_request(&mut socket);
                            assert_eq!(handle["method"], "Page.handleJavaScriptDialog");
                            assert_eq!(handle["params"]["accept"], false);
                            send_mock_reply(&mut socket, &handle["id"], json!({}));
                            send_mock_reply(
                                &mut socket,
                                &cmd["id"],
                                json!({ "result": { "value": null } }),
                            );
                        }
                        Some("Target.closeTarget") => {
                            send_mock_reply(&mut socket, &cmd["id"], json!({ "success": true }));
                            break;
                        }
                        other => panic!("unexpected CDP command: {other:?}"),
                    }
                }
            },
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "retry expiry with unexpected dialogs must not wait 5000ms step timeout"
        );
        assert_eq!(report.cases[0].status, CaseStatus::Error, "{report:?}");
        let step = &report.cases[0].steps[1];
        assert_eq!(step.status, StepStatus::Error);
        let err = step.error.as_ref().expect("step error must exist");
        assert_eq!(err.kind, ProblemKind::Execution);
        assert!(
            err.message.contains("confirm") && err.message.contains("still blocked"),
            "unexpected dialog detail missing from error: {}",
            err.message
        );
    }

    #[test]
    fn retry_browser_wait_without_dialog_fails_fast_on_unexpected_dialog_and_recovers() {
        use crate::core::case::{Expectation, Matcher, RetryPolicy};
        use crate::core::report::CaseStatus;

        let report = run_mock_browser_retry_case(
            "wait retry without dialog",
            Vec::new(),
            mock_wait_step(
                None,
                5000,
                RetryPolicy {
                    until: vec![Expectation {
                        path: "$.url".to_string(),
                        matcher: Matcher::Equals(json!("http://127.0.0.1/ready")),
                    }],
                    timeout_ms: 2000,
                    interval_ms: 10,
                },
            ),
            |mut socket| {
                mock_open_handshake(&mut socket, |_| {});

                // Attempt 1: unexpected alert during wait
                let attempt1 = read_cdp_request(&mut socket);
                assert_eq!(attempt1["method"], "Runtime.evaluate");
                send_mock_dialog(&mut socket, "alert", "wait-alert");
                let handle = read_cdp_request(&mut socket);
                assert_eq!(handle["method"], "Page.handleJavaScriptDialog");
                assert_eq!(handle["params"]["accept"], false);
                send_mock_reply(&mut socket, &handle["id"], json!({}));

                // Attempt 2: locates element and reads location.href
                let attempt2_loc = read_cdp_request(&mut socket);
                assert_eq!(attempt2_loc["method"], "Runtime.evaluate");
                send_mock_reply(
                    &mut socket,
                    &attempt2_loc["id"],
                    json!({ "result": { "objectId": "elem-2" } }),
                );
                let attempt2_vis = read_cdp_request(&mut socket);
                assert_eq!(attempt2_vis["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &attempt2_vis["id"],
                    json!({ "result": { "value": true } }),
                );
                let attempt2_href = read_cdp_request(&mut socket);
                assert_eq!(attempt2_href["method"], "Runtime.evaluate");
                send_mock_reply(
                    &mut socket,
                    &attempt2_href["id"],
                    json!({ "result": { "value": "http://127.0.0.1/ready" } }),
                );

                mock_close_loop(&mut socket);
            },
        );
        assert_eq!(report.cases[0].status, CaseStatus::Passed, "{report:?}");
        assert_eq!(
            report.cases[0].steps[1].result,
            Some(json!({ "url": "http://127.0.0.1/ready" }))
        );
    }

    #[test]
    fn retry_browser_wait_with_dialog_handles_delayed_dialog_for_until() {
        use crate::core::case::{Expectation, Matcher, RetryPolicy};
        use crate::core::report::CaseStatus;

        let report = run_mock_browser_retry_case(
            "wait retry with dialog",
            Vec::new(),
            mock_wait_step(
                Some(json!({ "action": "accept", "prompt_text": "ok-token" })),
                200,
                RetryPolicy {
                    until: vec![
                        Expectation {
                            path: "$.dialog.type".to_string(),
                            matcher: Matcher::Equals(json!("prompt")),
                        },
                        Expectation {
                            path: "$.dialog.accepted".to_string(),
                            matcher: Matcher::Equals(json!(true)),
                        },
                    ],
                    timeout_ms: 2000,
                    interval_ms: 10,
                },
            ),
            |mut socket| {
                mock_open_handshake(&mut socket, |_| {});

                // Attempt 1: element exists before dialog arrives -> returns without `dialog`
                let a1_loc = read_cdp_request(&mut socket);
                send_mock_reply(
                    &mut socket,
                    &a1_loc["id"],
                    json!({ "result": { "objectId": "elem-1" } }),
                );
                let a1_vis = read_cdp_request(&mut socket);
                send_mock_reply(
                    &mut socket,
                    &a1_vis["id"],
                    json!({ "result": { "value": true } }),
                );
                let a1_href = read_cdp_request(&mut socket);
                send_mock_reply(
                    &mut socket,
                    &a1_href["id"],
                    json!({ "result": { "value": "http://127.0.0.1/app" } }),
                );

                // Attempt 2: prompt dialog opens while waiting -> accept with prompt_text
                let a2_loc = read_cdp_request(&mut socket);
                send_mock_dialog(&mut socket, "prompt", "enter");
                let handle = read_cdp_request(&mut socket);
                assert_eq!(handle["method"], "Page.handleJavaScriptDialog");
                assert_eq!(handle["params"]["accept"], true);
                assert_eq!(handle["params"]["promptText"], "ok-token");
                send_mock_reply(&mut socket, &handle["id"], json!({}));
                send_mock_reply(
                    &mut socket,
                    &a2_loc["id"],
                    json!({ "result": { "objectId": "elem-2" } }),
                );
                let a2_vis = read_cdp_request(&mut socket);
                send_mock_reply(
                    &mut socket,
                    &a2_vis["id"],
                    json!({ "result": { "value": true } }),
                );
                let a2_href = read_cdp_request(&mut socket);
                send_mock_reply(
                    &mut socket,
                    &a2_href["id"],
                    json!({ "result": { "value": "http://127.0.0.1/app" } }),
                );

                mock_close_loop(&mut socket);
            },
        );
        assert_eq!(report.cases[0].status, CaseStatus::Passed, "{report:?}");
        assert_eq!(
            report.cases[0].steps[1].result,
            Some(json!({
                "url": "http://127.0.0.1/app",
                "dialog": {
                    "type": "prompt",
                    "message": "enter",
                    "accepted": true
                }
            }))
        );
    }
}
