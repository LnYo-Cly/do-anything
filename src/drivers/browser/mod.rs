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

use self::cdp::{CdpError, Connection, Deadline, RemoteResult};
use self::connect::Endpoint;
use self::launch::Launched;
use crate::core::paths;
use crate::core::registry::{Registry, StepDefinition, StepError};
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
        locator_step::<ClickArgs>(
            STEP_NAMES[1],
            "Clicks the element the locator names and returns its `text` when it has one.",
            validate_click,
            run_click,
        )?,
        type_step()?,
        step::<PressArgs>(
            STEP_NAMES[3],
            "Presses `key`, such as `Enter` or `Tab`, on the focused element.",
            validate_press,
            run_press,
        )?,
        wait_step(
            STEP_NAMES[4],
            "Waits for the element the locator names, or for the URL to contain `url_contains`, and returns the `url`.",
            validate_wait,
            run_wait,
        )?,
        locator_step::<ReadArgs>(
            STEP_NAMES[5],
            "Returns the `text` of the element the locator names, plus its `value` when `attribute` is given.",
            validate_read,
            run_read,
        )?,
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
    definition.args_schema["oneOf"] = json!([
        { "required": ["css"] },
        { "required": ["xpath"] },
        { "required": ["text", "value"] },
        { "required": ["role"] },
        { "required": ["testid"] }
    ]);
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
    apply_locator_schema(&mut definition.args_schema, name);
    string_property(&mut definition.args_schema, "url_contains");
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
}

/// Arguments of `browser.press`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PressArgs {
    /// Key name, for example `Enter` or `Tab`.
    #[schemars(extend("minLength" = 1))]
    key: String,
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

/// The body of a center lookup, run with the located element as `el`.
const CENTER_BODY: &str = "if (!el) return null; el.scrollIntoView({ block: 'center', inline: 'center' }); var rect = el.getBoundingClientRect(); return { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 };";

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

/// One matched element summary returned for ambiguous-locator errors.
#[derive(Debug, Deserialize)]
struct CandidateInfo {
    tag: String,
    id: String,
    text: String,
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
        let mut label = format!("{}: <{}", index + 1, candidate.tag);
        if !candidate.id.is_empty() {
            label.push('#');
            label.push_str(&candidate.id);
        }
        label.push('>');
        let text = truncate_chars(candidate.text.trim(), 60);
        if !text.is_empty() {
            label.push_str(&format!(" \"{text}\""));
        }
        parts.push(label);
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
    Connected { target_id: String },
}

/// One case's browser, its page, and the options that created it.
struct BrowserSession {
    /// Taken by [`Session::close`], which releases it before the case report is
    /// written. `Drop` only matters on the fallback path.
    connection: Option<Connection>,
    origin: Option<Origin>,
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
        let (page, _) = attach(&mut connection, Deadline::after(LAUNCH_TIMEOUT))?;
        let network = network::NetworkState::new(&page);

        Ok(Self {
            connection: Some(connection),
            origin: Some(Origin::Launched(browser)),
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
            origin: Some(Origin::Connected { target_id }),
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
    fn navigate(&mut self, url: &str, timeout: Duration) -> Result<(String, String), StepError> {
        let page = self.page.clone();
        let deadline = Deadline::after(timeout);
        // Network events that arrive before a navigation still belong to a
        // request of this page, so they are merged before the rest is cleared.
        self.absorb_network();
        let connection = self.connection_mut()?;
        // Events from the about:blank page must not be mistaken for this
        // navigation's load event.
        connection.take_messages();
        let reply = connection
            .call(
                "Page.navigate",
                json!({ "url": url }),
                Some(&page),
                deadline,
            )
            .map_err(|error| match error {
                CdpError::Timeout { .. } => StepError::Timeout(format!(
                    "could not open `{url}` within {} ms: {error}",
                    timeout.as_millis()
                )),
                error => StepError::Environment(format!("could not open `{url}`: {error}")),
            })?;
        if let Some(error_text) = reply.get("errorText").and_then(Value::as_str) {
            return Err(StepError::Environment(format!(
                "could not open `{url}`: {error_text}"
            )));
        }
        // Chrome omits `loaderId` for a same-document navigation (a fragment
        // or history change): no new document loads, so no load event fires.
        if reply.get("loaderId").is_some() {
            wait_for_load(connection, &page, url, deadline)?;
        }

        let current = connection
            .evaluate(&page, "location.href", deadline)
            .map(|value| value.as_str().unwrap_or(url).to_string())
            .map_err(|error| load_error(error, url, deadline, "read the URL of"))?;
        let title = connection
            .evaluate(&page, "document.title", deadline)
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .map_err(|error| load_error(error, url, deadline, "read the title of"))?;
        Ok((current, title))
    }

    /// Waits for the target element to exist, match uniquely (or `nth`), and be visible.
    fn locate_target(
        &mut self,
        target: &ElementTarget,
        deadline: Deadline,
    ) -> Result<Located, StepError> {
        let target_desc = target.describe_with(&self.testid_attribute);
        let mut last_pending: Option<(String, PendingReason)> = None;
        loop {
            let poll = match self.resolve_target_once(target, deadline) {
                Ok(poll) => poll,
                Err(LocateError::Timeout) => {
                    let (desc, reason) =
                        last_pending.unwrap_or((target_desc, PendingReason::Missing));
                    return Err(pending_timeout_error(&desc, reason, deadline));
                }
                Err(LocateError::Step(error)) => return Err(error),
            };
            match poll {
                ResolvePoll::Found(object_id) => {
                    let visible =
                        if matches!(target.locator, Locator::Text(_) | Locator::Role { .. }) {
                            true
                        } else {
                            let page = self.page.clone();
                            let function =
                                format!("function () {{ var el = this; {VISIBLE_BODY} }}");
                            match self
                                .connection_mut()?
                                .call_function(&page, &object_id, &function, deadline)
                            {
                                Ok(value) => value == Value::Bool(true),
                                Err(CdpError::Timeout { .. }) => {
                                    return Err(pending_timeout_error(
                                        &target_desc,
                                        PendingReason::Hidden,
                                        deadline,
                                    ));
                                }
                                Err(error) => {
                                    return Err(evaluate_error(
                                        &target_desc,
                                        error,
                                        "check the visibility of",
                                        deadline,
                                    ));
                                }
                            }
                        };
                    if visible {
                        return Ok(Located(object_id));
                    }
                    last_pending = Some((target_desc.clone(), PendingReason::Hidden));
                }
                ResolvePoll::Pending {
                    target_desc: desc,
                    reason,
                } => {
                    last_pending = Some((desc, reason));
                }
            }
            if deadline.passed() {
                let (desc, reason) = last_pending.unwrap_or((target_desc, PendingReason::Missing));
                return Err(pending_timeout_error(&desc, reason, deadline));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Runs one non-blocking resolution pass over `target` and its `within` chain.
    fn resolve_target_once(
        &mut self,
        target: &ElementTarget,
        deadline: Deadline,
    ) -> Result<ResolvePoll, LocateError> {
        let root = match &target.within {
            Some(container) => match self.resolve_target_once(container, deadline)? {
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
        let page = self.page.clone();
        let remote = match root.as_deref() {
            Some(root_id) => {
                let fn_decl = format!("function () {{ var root = this; {body} }}");
                self.connection_mut()
                    .map_err(LocateError::Step)?
                    .call_function_remote(&page, root_id, &fn_decl, deadline)
            }
            None => {
                let expr = format!("(function () {{ var root = document; {body} }})()");
                self.connection_mut()
                    .map_err(LocateError::Step)?
                    .evaluate_remote(&page, &expr, deadline)
            }
        }
        .map_err(|error| locate_error(&target_desc, error))?;

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
    fn resolve_role_once(
        &mut self,
        target_desc: &str,
        role: &str,
        name: Option<&str>,
        nth: Option<usize>,
        root: Option<&str>,
        deadline: Deadline,
    ) -> Result<ResolvePoll, LocateError> {
        let (matched, objects) = self.role_objects(target_desc, role, name, root, deadline)?;
        let mut visible_objects = Vec::new();
        for object_id in objects {
            let visible = self
                .call_function_on(
                    target_desc,
                    &object_id,
                    VISIBLE_BODY,
                    "check the visibility of",
                    deadline,
                )
                .map_err(LocateError::Step)?;
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
                        let summary_val = self
                            .call_function_on(
                                target_desc,
                                object_id,
                                SUMMARIZE_ELEMENT_BODY,
                                "summarize",
                                deadline,
                            )
                            .map_err(LocateError::Step)?;
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
    ) -> Result<(bool, Vec<String>), LocateError> {
        let page = self.page.clone();
        let mut params = if let Some(root_id) = root {
            json!({ "objectId": root_id, "role": role })
        } else {
            let document = self
                .connection_mut()
                .map_err(LocateError::Step)?
                .call(
                    "DOM.getDocument",
                    json!({ "depth": 0 }),
                    Some(&page),
                    deadline,
                )
                .map_err(|error| locate_error(target_desc, error))?;
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
        let reply = self
            .connection_mut()
            .map_err(LocateError::Step)?
            .call("Accessibility.queryAXTree", params, Some(&page), deadline)
            .map_err(|error| locate_error(target_desc, error))?;
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
            let resolved = self
                .connection_mut()
                .map_err(LocateError::Step)?
                .call(
                    "DOM.resolveNode",
                    json!({ "backendNodeId": backend_node_id }),
                    Some(&page),
                    deadline,
                )
                .map_err(|error| locate_error(target_desc, error))?;
            if let Some(object_id) = resolved.pointer("/object/objectId").and_then(Value::as_str) {
                if let Some(root_id) = root {
                    let inside = self
                        .connection_mut()
                        .map_err(LocateError::Step)?
                        .call_function_with_object(
                            &page,
                            root_id,
                            object_id,
                            "function (el) { return Boolean(el) && this !== el && this.contains(el); }",
                            deadline,
                        )
                        .map_err(|error| locate_error(target_desc, error))?;
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
    ) -> Result<Value, StepError> {
        self.call_function_on(target_desc, &located.0, body, action, deadline)
    }

    /// Runs one action body with a resolved DOM node as `this`.
    fn call_function_on(
        &mut self,
        target_desc: &str,
        object_id: &str,
        body: &str,
        action: &str,
        deadline: Deadline,
    ) -> Result<Value, StepError> {
        let page = self.page.clone();
        let function = format!("function () {{ var el = this; {body} }}");
        self.connection_mut()?
            .call_function(&page, object_id, &function, deadline)
            .map_err(|error| evaluate_error(target_desc, error, action, deadline))
    }

    /// Sends one page command for a step and maps its errors.
    fn dispatch(
        &mut self,
        target_desc: &str,
        method: &str,
        params: Value,
        action: &str,
        deadline: Deadline,
    ) -> Result<(), StepError> {
        let page = self.page.clone();
        self.connection_mut()?
            .call(method, params, Some(&page), deadline)
            .map(|_| ())
            .map_err(|error| action_error(target_desc, action, error))
    }

    /// The open connection, or the error a step reports after a close.
    fn connection_mut(&mut self) -> Result<&mut Connection, StepError> {
        self.connection.as_mut().ok_or_else(|| {
            StepError::Execution(
                "the browser session is already closed; run `browser.open` again".to_string(),
            )
        })
    }

    /// Covers every editable field, captures a PNG and removes the covers.
    ///
    /// Cleanup runs after a failed capture as well; a browser that has already
    /// gone cannot be cleaned, and then the capture error is the one reported.
    fn capture_masked_png(&mut self, deadline: Deadline) -> Result<Vec<u8>, StepError> {
        let page = self.page.clone();
        self.connection_mut()?
            .evaluate(&page, MASK_INPUTS_SCRIPT, deadline)
            .map_err(|error| {
                StepError::Environment(format!(
                    "could not cover the editable fields before the screenshot: {error}"
                ))
            })?;

        let captured = self.connection_mut()?.call(
            "Page.captureScreenshot",
            json!({ "format": "png" }),
            Some(&page),
            deadline,
        );
        let cleaned = self.connection_mut()?.evaluate(
            &page,
            REMOVE_MASKS_SCRIPT,
            Deadline::after(MASK_CLEANUP_TIMEOUT),
        );
        if let Err(error) = cleaned {
            let mut message =
                format!("could not remove the screenshot covers from the page: {error}");
            if let Err(capture_error) = captured {
                message.push_str(&format!("; the screenshot also failed: {capture_error}"));
            }
            return Err(StepError::Environment(message));
        }

        let reply = captured.map_err(|error| match error {
            CdpError::Timeout { .. } => StepError::Timeout(format!(
                "the screenshot did not finish within {} ms",
                deadline.timeout_ms()
            )),
            error => StepError::Environment(format!("could not capture the screenshot: {error}")),
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

/// Opens one new tab and attaches it as the session's page.
fn attach(connection: &mut Connection, deadline: Deadline) -> Result<(String, String), String> {
    let target = connection
        .call(
            "Target.createTarget",
            json!({ "url": "about:blank" }),
            None,
            deadline,
        )
        .map_err(|error| format!("could not open a page in the browser: {error}"))?;
    let target_id = target
        .get("targetId")
        .and_then(Value::as_str)
        .ok_or_else(|| "the browser did not return a target id".to_string())?
        .to_string();
    let attached = connection
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
            deadline,
        )
        .map_err(|error| format!("could not attach to the page the browser opened: {error}"))?;
    let page = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| "the browser did not return a page session id".to_string())?
        .to_string();
    connection
        .call("Page.enable", json!({}), Some(&page), deadline)
        .map_err(|error| format!("could not enable page events: {error}"))?;
    connection
        .call("Runtime.enable", json!({}), Some(&page), deadline)
        .map_err(|error| format!("could not enable page scripts: {error}"))?;
    // The first navigation may start right after this call, so the network
    // domain is enabled before the page can issue a request; an observer
    // enabled later would miss the document request.
    connection
        .call("Network.enable", json!({}), Some(&page), deadline)
        .map_err(|error| format!("could not enable network events: {error}"))?;
    Ok((page, target_id))
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

/// Why one poll did not yield an element yet.
enum PendingReason {
    Missing,
    Hidden,
    OutOfRange { index: usize, count: usize },
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
    match reason {
        PendingReason::Missing => StepError::Execution(format!(
            "could not find {target_desc} within {} ms; check the page and the locator",
            deadline.timeout_ms()
        )),
        PendingReason::Hidden => StepError::Execution(format!(
            "{target_desc} was found but not visible within {} ms; check the page and the locator",
            deadline.timeout_ms()
        )),
        PendingReason::OutOfRange { index, count } => {
            let noun = if count == 1 { "element" } else { "elements" };
            StepError::Execution(format!(
                "could not find {target_desc} within {} ms: `nth` index {index} is out of range for {count} matched {noun}",
                deadline.timeout_ms()
            ))
        }
    }
}

/// Builds a step failure that names the target and what was attempted.
fn action_error(target_desc: &str, action: &str, error: impl std::fmt::Display) -> StepError {
    StepError::Execution(format!("could not {action} {target_desc}: {error}"))
}

/// Maps a failure while looking for an element into a poll-time error.
fn locate_error(target_desc: &str, error: CdpError) -> LocateError {
    match error {
        CdpError::Script(message) => LocateError::Step(StepError::Execution(format!(
            "could not use {target_desc}: {message}"
        ))),
        CdpError::Timeout { .. } => LocateError::Timeout,
        error => LocateError::Step(StepError::Environment(format!(
            "the browser failed while waiting for {target_desc}: {error}"
        ))),
    }
}

/// Maps a failure of one element action to the error class a step uses.
fn evaluate_error(
    target_desc: &str,
    error: CdpError,
    action: &str,
    deadline: Deadline,
) -> StepError {
    match error {
        CdpError::Timeout { .. } => StepError::Timeout(format!(
            "could not {action} {target_desc} within {} ms",
            deadline.timeout_ms()
        )),
        error => action_error(target_desc, action, error),
    }
}

/// Waits for the loaded document, skipping an event that belongs to the page
/// the session started on.
fn wait_for_load(
    connection: &mut Connection,
    page: &str,
    url: &str,
    deadline: Deadline,
) -> Result<(), StepError> {
    loop {
        match connection.wait_for_event("Page.loadEventFired", deadline) {
            Ok(_) => {}
            Err(CdpError::Timeout { .. }) => {
                return Err(StepError::Timeout(format!(
                    "the page `{url}` did not finish loading within {} ms",
                    deadline.timeout_ms()
                )));
            }
            Err(error) => {
                return Err(StepError::Environment(format!(
                    "the browser failed while loading `{url}`: {error}"
                )));
            }
        }
        let current = connection
            .evaluate(page, "location.href", deadline)
            .map_err(|error| load_error(error, url, deadline, "read the URL of"))?;
        let ready = connection
            .evaluate(page, "document.readyState", deadline)
            .map_err(|error| load_error(error, url, deadline, "read the state of"))?;
        if current.as_str() != Some("about:blank") && ready.as_str() == Some("complete") {
            return Ok(());
        }
    }
}

/// Maps a failed read after navigation to the error class the step reports.
fn load_error(error: CdpError, url: &str, deadline: Deadline, action: &str) -> StepError {
    match error {
        CdpError::Timeout { .. } => StepError::Timeout(format!(
            "the page `{url}` did not finish loading within {} ms",
            deadline.timeout_ms()
        )),
        error => StepError::Environment(format!("could not {action} `{url}`: {error}")),
    }
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
            let page = self.page.clone();
            match self.connection_mut().and_then(|connection| {
                connection
                    .evaluate(&page, "document.documentElement.outerHTML", deadline)
                    .map_err(|error| {
                        StepError::Environment(format!("could not read the DOM snapshot: {error}"))
                    })
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
            Some(Origin::Connected { target_id }) => {
                // Only this case's tab is ours. `Browser.close` is never sent:
                // the browser belongs to the user and must keep running.
                if let Err(error) = connection.call(
                    "Target.closeTarget",
                    json!({ "targetId": target_id }),
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
        if key == "within" {
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
    let (url, title) = session.navigate(&args.url, Duration::from_millis(args.timeout_ms))?;
    Ok(json!({ "url": url, "title": title }))
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
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let located = session.locate_target(&target, deadline)?;
    let center = session.run_on(&target_desc, &located, CENTER_BODY, "click", deadline)?;
    let x = center
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| action_error(&target_desc, "click", "the element has no position"))?;
    let y = center
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| action_error(&target_desc, "click", "the element has no position"))?;
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
        )?;
    }
    // A click may navigate, which detaches the element. The click itself
    // succeeded, so a text that can no longer be read is not a failure.
    let text = session
        .run_on(
            &target_desc,
            &located,
            TEXT_BODY,
            "read the text of",
            deadline,
        )
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    match text {
        Some(text) if !text.trim().is_empty() => Ok(json!({ "text": text.trim() })),
        _ => Ok(json!({})),
    }
}

fn run_type(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: TypeArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let text = type_text(&args)
        .map_err(StepError::InvalidArguments)?
        .to_string();
    let target = type_target(&args).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let located = session.locate_target(&target, deadline)?;
    let focused = session.run_on(&target_desc, &located, FOCUS_BODY, "focus", deadline)?;
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
    )?;
    Ok(json!({}))
}

fn run_press(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: PressArgs = parse(args)?;
    let key = Key::parse(&args.key).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_secs(5));
    let session = session(sessions)?;
    let page = session.page.clone();
    for params in [key.down_params(), key.up_params()] {
        session
            .connection_mut()?
            .call("Input.dispatchKeyEvent", params, Some(&page), deadline)
            .map_err(|error| match error {
                CdpError::Timeout { .. } => StepError::Timeout(format!(
                    "could not press `{}` within {} ms",
                    args.key,
                    deadline.timeout_ms()
                )),
                error => StepError::Execution(format!("could not press `{}`: {error}", args.key)),
            })?;
    }
    Ok(json!({}))
}

fn run_wait(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: WaitArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let target = WaitTarget::from_args(&args).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let session = session(sessions)?;
    let url = match target {
        WaitTarget::Element(target) => {
            session.locate_target(&target, deadline)?;
            let page = session.page.clone();
            session
                .connection_mut()?
                .evaluate(&page, "location.href", deadline)
                .map_err(|error| {
                    StepError::Environment(format!("could not read the current URL: {error}"))
                })?
        }
        WaitTarget::Url(wanted) => wait_for_url(session, &wanted, deadline)?,
    };
    Ok(json!({ "url": url.as_str().unwrap_or_default() }))
}

/// Waits until the page's URL contains `wanted`.
fn wait_for_url(
    session: &mut BrowserSession,
    wanted: &str,
    deadline: Deadline,
) -> Result<Value, StepError> {
    let page = session.page.clone();
    loop {
        match session
            .connection_mut()?
            .evaluate(&page, "location.href", deadline)
        {
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
            Err(CdpError::Timeout { .. }) => {
                return Err(StepError::Execution(format!(
                    "the URL did not contain `{wanted}` within {} ms",
                    deadline.timeout_ms()
                )));
            }
            Err(error) => {
                return Err(StepError::Environment(format!(
                    "the browser failed while waiting for the URL: {error}"
                )));
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
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let located = session.locate_target(&target, deadline)?;
    let text = session.run_on(
        &target_desc,
        &located,
        TEXT_BODY,
        "read the text of",
        deadline,
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
        )?;
        result["value"] = value;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
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
}
