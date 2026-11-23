//! The `browser.*` steps: drive one Chrome page across a case.
//!
//! The first `browser.open` of a case creates a [`BrowserSession`] through the
//! session contract. Every later step of the same case asks [`Sessions`] for the
//! same session, so it sees the page the first step opened; the runner closes
//! the browser when the case ends, whatever the case's result was.
//!
//! A case either starts its own browser or, with `connect`, drives a browser the
//! user already runs with a debug port. The connected browser is never closed:
//! only the tabs this case opened and the tabs its pages opened are.
//!
//! A page action such as `<a target="_blank">` or `window.open` opens a tab the
//! session does not follow by itself; `browser.switch_tab` moves the session to
//! one of them. [`tabs`] keeps the set of tabs this case owns, which is also the
//! set teardown closes.
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
mod snapshot;
mod synthesis;
mod tabs;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::cdp::{
    CdpError, Connection, Deadline, DialogPolicy, DialogWatch, OpenedDialog, RemoteResult,
};
use self::connect::Endpoint;
use self::launch::Launched;
use crate::core::paths;
use crate::core::registry::{
    Registry, SessionDefinition, StepDefinition, StepError, allow_retry, disallow_retry,
};
use crate::core::session::{Evidence, Session, Sessions};

/// Session kind the browser steps share.
pub const SESSION_KIND: &str = "browser";

/// Step names, in the order they are registered.
pub const STEP_NAMES: [&str; 11] = [
    "browser.open",
    "browser.click",
    "browser.type",
    "browser.upload",
    "browser.press",
    "browser.wait",
    "browser.read",
    "browser.screenshot",
    "browser.set_cookies",
    "browser.save_state",
    "browser.switch_tab",
];

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_NAVIGATION_TIMEOUT_MS: u64 = 30_000;

/// Environment variable that overrides the default navigation budget.
const NAVIGATION_TIMEOUT_ENV: &str = "DA_BROWSER_NAVIGATION_TIMEOUT_MS";

/// Environment variable that overrides the browser launch budget.
const LAUNCH_TIMEOUT_ENV: &str = "DA_BROWSER_LAUNCH_TIMEOUT_MS";
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;
/// How long starting the browser and opening its page may take.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum number of fresh browser launches tried when post-startup connect/attach stalls.
const MAX_LAUNCH_ATTEMPTS: usize = 2;
/// How often an element or URL is looked at again.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Registers every browser step and the network observer.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    for definition in [
        open_step()?,
        click_step()?,
        type_step()?,
        upload_step()?,
        press_step()?,
        wait_step(
            STEP_NAMES[5],
            "Waits for the element the locator names, or for the URL to contain `url_contains`, and returns the `url`.",
            validate_wait,
            run_wait,
        )?,
        read_step()?,
        screenshot_step()?,
        set_cookies_step()?,
        save_state_step()?,
        switch_tab_step()?,
    ] {
        registry.register(definition)?;
    }
    network::register(registry)?;
    registry.register_session(SessionDefinition {
        kind: SESSION_KIND,
        reattach: BrowserSession::reattach_boxed,
        dispose_handle: BrowserSession::dispose_handle,
    })?;
    registry.register_session_validator("browser.click", validate_click_session)?;
    registry.register_session_validator("browser.type", validate_type_session)?;
    registry.register_session_validator("browser.upload", validate_upload_session)?;
    registry.register_session_validator("browser.wait", validate_wait_session)?;
    registry.register_session_validator("browser.read", validate_read_session)?;
    registry.register_arg_redactor("browser.set_cookies", redact_set_cookies_args)?;
    Ok(())
}

/// Default HTML attribute matched by `testid` locators.
const DEFAULT_TESTID_ATTRIBUTE: &str = "data-testid";

/// Builds `browser.open`, whose `connect`, `profile` and `storage_state` are
/// plain strings in the schema.
fn open_step() -> Result<StepDefinition, String> {
    let mut definition = step::<OpenArgs>(
        STEP_NAMES[0],
        "Opens `url` in the case's browser, in a named persistent profile when `profile` is given, or in a browser the user already runs when `connect` is given, and returns the page's `url` and `title`. A `storage_state` file supplies cookies and `localStorage` from an earlier `browser.save_state` before the navigation.",
        validate_open,
        run_open,
    )?;
    string_property(&mut definition.args_schema, "connect");
    string_property(&mut definition.args_schema, "profile");
    string_property(&mut definition.args_schema, "storage_state");
    string_property(&mut definition.args_schema, "testid_attribute");
    apply_dialog_schema(&mut definition.args_schema);
    if let Value::Object(entries) = &mut definition.args_schema {
        entries.insert(
            "allOf".to_string(),
            json!([
                { "not": { "required": ["connect", "profile"] } },
                { "not": { "required": ["connect", "storage_state"] } },
            ]),
        );
    }
    Ok(definition)
}

/// Builds `browser.save_state`, whose `path` names the JSON file to write.
fn save_state_step() -> Result<StepDefinition, String> {
    let mut definition = step::<SaveStateArgs>(
        STEP_NAMES[9],
        "Saves the browser's cookies and the current origin's `localStorage` to `path` so a later run can load them with `browser.open: { storage_state: ... }`, and returns the `path` and how many items it wrote.",
        validate_save_state,
        run_save_state,
    )?;
    string_property(&mut definition.args_schema, "path");
    Ok(definition)
}

/// Builds `browser.switch_tab`, which moves the session to another tab of the
/// case, selected by a URL or title fragment or by creation order.
fn switch_tab_step() -> Result<StepDefinition, String> {
    let mut definition = step::<SwitchTabArgs>(
        STEP_NAMES[10],
        "Switches the session to a tab this case's pages opened, selected by `url_contains`, `title_contains` or `index`, waits up to `timeout_ms` for it to appear and returns its `url` and `title`; every later `browser.*` step, the failure evidence and the network observation then apply to that tab.",
        validate_switch_tab,
        run_switch_tab,
    )?;
    string_property(&mut definition.args_schema, "url_contains");
    string_property(&mut definition.args_schema, "title_contains");
    Ok(definition)
}

/// Builds `browser.set_cookies`, keeping its nested schema self-contained without `$defs`.
fn set_cookies_step() -> Result<StepDefinition, String> {
    let args_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "cookies": {
                "type": "array",
                "minItems": 1,
                "description": "Cookies to inject into the browser session via CDP `Network.setCookies`.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["name", "value"],
                    "anyOf": [
                        { "required": ["url"] },
                        { "required": ["domain"] }
                    ],
                    "properties": {
                        "name": { "type": "string", "minLength": 1 },
                        "value": { "type": "string", "minLength": 1 },
                        "url": { "type": "string", "minLength": 1 },
                        "domain": { "type": "string", "minLength": 1 },
                        "path": { "type": "string", "minLength": 1 },
                        "http_only": { "type": "boolean" },
                        "secure": { "type": "boolean" },
                        "same_site": { "type": "string", "enum": ["Strict", "Lax", "None"] }
                    }
                }
            },
            "local_storage": {
                "type": "object",
                "minProperties": 1,
                "description": "Per-origin `localStorage` entries (origin URL or `current`) to inject into the browser session.",
                "additionalProperties": {
                    "type": "object",
                    "minProperties": 1,
                    "additionalProperties": { "type": "string" }
                }
            }
        },
        "anyOf": [
            { "required": ["cookies"] },
            { "required": ["local_storage"] }
        ]
    });
    Ok(StepDefinition {
        name: STEP_NAMES[8],
        description: "Injects cookies and optional `localStorage` entries into the browser session, returning `cookies_set` and `local_storage_set`.",
        args_schema,
        validate: validate_set_cookies,
        check_retry: disallow_retry,
        lint_warnings: crate::core::registry::no_lint_warnings,
        run: run_set_cookies,
    })
}

/// Builds `browser.click`, with optional `dialog` handling.
fn click_step() -> Result<StepDefinition, String> {
    let mut definition = locator_step::<ClickArgs>(
        STEP_NAMES[1],
        "Clicks the element the locator names, or explicit `at` coordinates for content that has no element to name, and returns the element's `text` when it has one or the clicked `x`, `y` and `viewport` for coordinates.",
        validate_click,
        run_click,
    )?;
    apply_at_schema(
        &mut definition.args_schema,
        at_branch(&[json!({ "required": ["text"] })]),
    );
    apply_dialog_schema(&mut definition.args_schema);
    definition.lint_warnings = coordinate_lint_warnings;
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
        "Types `text`, or `value` when `text` names the element, into the element the locator names, or after the focus click `at` coordinates name.",
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
    // With `at` there is no element for `text` to name, so it is the typing
    // text; giving both `text` and `value` would silently drop one.
    let mut coordinates = at_branch(&[json!({ "required": ["text", "value"] })]);
    coordinates["anyOf"] = json!([{ "required": ["text"] }, { "required": ["value"] }]);
    apply_at_schema(&mut definition.args_schema, coordinates);
    definition.lint_warnings = coordinate_lint_warnings;
    Ok(definition)
}

/// Builds `browser.upload`, whose `files` list becomes the input's selection.
///
/// An empty `files` list clears the selection, so it stays a valid value and
/// the schema does not declare a minimum length.
fn upload_step() -> Result<StepDefinition, String> {
    locator_step::<UploadArgs>(
        STEP_NAMES[3],
        "Sets the files of the `<input type=\"file\">` element the locator names from the `files` paths, which resolve against the case file's directory, and returns the number of `files` set and their `names`; an empty `files` clears the selection.",
        validate_upload,
        run_upload,
    )
}

/// Builds `browser.press`, with optional `dialog` handling.
fn press_step() -> Result<StepDefinition, String> {
    let mut definition = step::<PressArgs>(
        STEP_NAMES[4],
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
        STEP_NAMES[6],
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
        STEP_NAMES[7],
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
        lint_warnings: crate::core::registry::no_lint_warnings,
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

/// Adds the `at` coordinate form to a step schema.
///
/// Every existing `oneOf` branch refuses `at`, and `at_branch` refuses every
/// locator and modifier, so a step with `at` and a locator matches no branch.
fn apply_at_schema(schema: &mut Value, at_branch: Value) {
    if let Value::Object(entries) = schema {
        entries.remove("$defs");
    }
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert("at".to_string(), at_schema_value());
    }
    let Some(branches) = schema.get_mut("oneOf").and_then(Value::as_array_mut) else {
        return;
    };
    for branch in branches.iter_mut() {
        if let Value::Object(entries) = branch {
            entries.insert("not".to_string(), json!({"required": ["at"]}));
        }
    }
    branches.push(at_branch);
}

/// One `oneOf` branch that accepts `at` alone, plus the extra combinations the
/// step refuses alongside coordinates.
fn at_branch(extra_excluded: &[Value]) -> Value {
    let mut excluded = vec![
        json!({"required": ["css"]}),
        json!({"required": ["xpath"]}),
        json!({"required": ["role"]}),
        json!({"required": ["testid"]}),
        json!({"required": ["within"]}),
        json!({"required": ["nth"]}),
        json!({"required": ["name"]}),
    ];
    excluded.extend(extra_excluded.iter().cloned());
    json!({
        "required": ["at"],
        "not": { "anyOf": excluded },
    })
}

/// Self-contained JSON Schema for the optional `at` coordinate argument.
fn at_schema_value() -> Value {
    json!({
        "description": "Explicit viewport coordinates, in CSS pixels, for content that has no element to name, such as a `<canvas>`. `viewport` records the size the coordinates were measured at: when it is given and the live viewport differs, the step fails instead of clicking the wrong place.",
        "type": "object",
        "additionalProperties": false,
        "required": ["x", "y"],
        "properties": {
            "x": { "type": "number", "minimum": 0 },
            "y": { "type": "number", "minimum": 0 },
            "viewport": {
                "description": "Width and height in CSS pixels the coordinates were recorded at.",
                "type": "object",
                "additionalProperties": false,
                "required": ["width", "height"],
                "properties": {
                    "width": { "type": "integer", "minimum": 1 },
                    "height": { "type": "integer", "minimum": 1 }
                }
            }
        }
    })
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
    /// closes only that tab. Cannot be combined with `headless`, `width`,
    /// `height`, or `profile`.
    #[serde(default)]
    connect: Option<String>,
    /// Named persistent browser profile directory under `.da/profiles/<name>`,
    /// preserved across runs. Cannot be combined with `connect`.
    #[schemars(extend("minLength" = 1, "pattern" = "^[A-Za-z0-9_-]+$"))]
    #[serde(default)]
    profile: Option<String>,
    /// JSON file written by `browser.save_state` whose cookies and
    /// `localStorage` are applied before the navigation. Cannot be combined
    /// with `connect`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    storage_state: Option<String>,
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

/// Cookie `SameSite` attribute for `browser.set_cookies` and `browser.save_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
enum CookieSameSite {
    Strict,
    Lax,
    None,
}

impl CookieSameSite {
    fn as_cdp(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }

    fn from_cdp(value: Option<&str>) -> Option<Self> {
        match value {
            Some("Strict") => Some(Self::Strict),
            Some("Lax") => Some(Self::Lax),
            Some("None") => Some(Self::None),
            _ => None,
        }
    }
}

/// One cookie passed to `browser.set_cookies`, or stored in a storage-state file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CookieArg {
    name: String,
    value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    domain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    http_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    same_site: Option<CookieSameSite>,
}

/// Arguments of `browser.set_cookies`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetCookiesArgs {
    #[serde(default)]
    cookies: Option<Vec<CookieArg>>,
    #[serde(default)]
    local_storage: Option<BTreeMap<String, BTreeMap<String, String>>>,
}

/// Version of the storage-state file this `da` reads and writes.
const STORAGE_STATE_VERSION: u32 = 1;

/// Largest storage-state file `browser.open` reads, in bytes.
const STORAGE_STATE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// A JSON file that carries cookies and `localStorage` between runs.
///
/// `browser.save_state` writes it and `browser.open: { storage_state: ... }`
/// reads it. The shape intentionally matches the `browser.set_cookies`
/// arguments, so a file can also be copied into a case.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StorageStateFile {
    #[serde(default = "storage_state_version")]
    version: u32,
    #[serde(default)]
    cookies: Vec<CookieArg>,
    #[serde(default)]
    local_storage: BTreeMap<String, BTreeMap<String, String>>,
}

fn storage_state_version() -> u32 {
    STORAGE_STATE_VERSION
}

/// Arguments of `browser.save_state`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SaveStateArgs {
    /// File that receives the cookies and `localStorage`. A relative path is
    /// resolved against the directory that holds the case file.
    #[schemars(extend("minLength" = 1))]
    path: String,
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

/// A viewport size a coordinate step was recorded at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ViewportSize {
    /// Width in CSS pixels.
    #[schemars(extend("minimum" = 1))]
    width: u32,
    /// Height in CSS pixels.
    #[schemars(extend("minimum" = 1))]
    height: u32,
}

/// Explicit viewport coordinates for content that has no DOM node to name, such
/// as a `<canvas>`, a WebGL scene or a map control.
///
/// `viewport` records the viewport the coordinates were measured at. When it is
/// present the step fails if the live viewport differs, so a different window
/// size can never silently click the wrong place.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AtArgs {
    /// Horizontal offset in CSS pixels from the left edge of the viewport.
    #[schemars(extend("minimum" = 0))]
    x: f64,
    /// Vertical offset in CSS pixels from the top edge of the viewport.
    #[schemars(extend("minimum" = 0))]
    y: f64,
    /// Viewport the coordinates were recorded at; the step refuses to run when
    /// the live viewport differs.
    #[serde(default)]
    viewport: Option<ViewportSize>,
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
    /// Explicit viewport coordinates, for content that has no element to name,
    /// for example a `<canvas>`. Cannot be combined with a locator.
    #[serde(default)]
    at: Option<AtArgs>,
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
    /// Explicit viewport coordinates, for content that has no element to name,
    /// for example a `<canvas>`. Cannot be combined with a locator.
    #[serde(default)]
    at: Option<AtArgs>,
    /// How long to wait for the element, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// How to respond if typing opens a JavaScript dialog.
    #[serde(default)]
    dialog: Option<DialogArg>,
}

/// Arguments of `browser.upload`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UploadArgs {
    /// CSS selector of the `<input type="file">` element.
    #[serde(default)]
    css: Option<String>,
    /// XPath expression of the `<input type="file">` element.
    #[serde(default)]
    xpath: Option<String>,
    /// Visible text of the `<input type="file">` element, with whitespace
    /// normalized.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role of the `<input type="file">` element, for example
    /// `button`.
    #[serde(default)]
    role: Option<String>,
    /// Accessibility name of the element; only meaningful with `role`.
    #[serde(default)]
    name: Option<String>,
    /// Test id attribute value of the element.
    #[serde(default)]
    testid: Option<String>,
    /// Local files to set on the input, in order. A relative path resolves
    /// against the directory that holds the case file; an empty list clears the
    /// current selection.
    #[schemars(extend("items" = { "type": "string", "minLength": 1 }))]
    files: Vec<String>,
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

/// Arguments of `browser.switch_tab`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SwitchTabArgs {
    /// Text the URL of the tab must contain.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    url_contains: Option<String>,
    /// Text the title of the tab must contain.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    title_contains: Option<String>,
    /// Zero-based position of the tab in the order this case created its tabs;
    /// `0` is the tab `browser.open` opened. Defaults to the tab a page action
    /// opened most recently.
    #[schemars(extend("minimum" = 0))]
    #[serde(default)]
    index: Option<usize>,
    /// How long to wait for a matching tab, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
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
    navigation_timeout_default()
}

/// The navigation budget a case gets when it does not name one.
///
/// A contended CI runner can spend longer than the built-in thirty seconds in
/// `Page.navigate` on a page that loads instantly elsewhere, so the default is
/// configurable through `DA_BROWSER_NAVIGATION_TIMEOUT_MS`. An unset, malformed
/// or implausible value falls back to the built-in default instead of failing a
/// case: the variable is an operator knob, not case content.
fn launch_timeout() -> Duration {
    static TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
        std::env::var(LAUNCH_TIMEOUT_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| (1_000..=600_000).contains(value))
            .map(Duration::from_millis)
            .unwrap_or(LAUNCH_TIMEOUT)
    });
    *TIMEOUT
}

fn navigation_timeout_default() -> u64 {
    static TIMEOUT: LazyLock<u64> = LazyLock::new(|| {
        std::env::var(NAVIGATION_TIMEOUT_ENV)
            .ok()
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .filter(|value| (1_000..=600_000).contains(value))
            .unwrap_or(DEFAULT_NAVIGATION_TIMEOUT_MS)
    });
    *TIMEOUT
}

/// How long all failure evidence of one browser session may take.
const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one explicit `browser.screenshot` may take.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long removing the masks may take after the screenshot itself.
const MASK_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Shared JavaScript helpers for frame traversal, coordinate transforms, and visibility.
const FRAME_GEOMETRY_HELPERS_JS: &str = r#"
var isBoxVisible = function (node, win) {
  if (!node || node.nodeType !== 1) return false;
  var view = win || (node.ownerDocument && node.ownerDocument.defaultView) || window;
  var style = view.getComputedStyle(node);
  var rect = node.getBoundingClientRect();
  return style.display !== 'none' &&
    style.visibility !== 'hidden' &&
    style.visibility !== 'collapse' &&
    rect.width > 0 &&
    rect.height > 0;
};
var collectEnclosingFrames = function (node) {
  var frames = [];
  var doc = node.nodeType === 9 ? node : node.ownerDocument;
  var win = doc && doc.defaultView;
  var topWin = window;
  try {
    topWin = window.top || window;
  } catch (e) {
    return null;
  }
  while (win && win !== topWin) {
    var frame = null;
    try {
      frame = win.frameElement;
    } catch (e) {
      return null;
    }
    if (!frame || !frame.isConnected) return null;
    frames.unshift(frame);
    doc = frame.ownerDocument;
    win = doc && doc.defaultView;
  }
  return frames;
};
var areEnclosingFramesVisible = function (frames) {
  for (var i = 0; i < frames.length; i++) {
    if (!isBoxVisible(frames[i], null)) return false;
  }
  return true;
};
var checkAxisAlignedTransform = function (node, win) {
  if (win.SVGElement && node instanceof win.SVGElement) {
    return { ok: false, reason: 'SVG ancestor transforms cannot be mapped to viewport coordinates' };
  }
  var cs = win.getComputedStyle(node);
  var sx = 1;
  var sy = 1;
  if (cs.transform && cs.transform !== 'none') {
    var m = null;
    try {
      m = new (win.DOMMatrix || DOMMatrix)(cs.transform);
    } catch (e) {
      return { ok: false, reason: 'CSS transform `' + cs.transform + '` could not be parsed' };
    }
    if (!m.is2D || !isFinite(m.a) || !isFinite(m.b) || !isFinite(m.c) || !isFinite(m.d) || !isFinite(m.e) || !isFinite(m.f)) {
      return { ok: false, reason: '3D CSS transform `' + cs.transform + '` cannot be mapped as a 2D viewport coordinate' };
    }
    if (Math.abs(m.b) > 1e-6 || Math.abs(m.c) > 1e-6) {
      return { ok: false, reason: 'rotated or skewed CSS transform `' + cs.transform + '` cannot be mapped to axis-aligned viewport coordinates' };
    }
    if (m.a <= 0 || m.d <= 0) {
      return { ok: false, reason: 'non-positive or reflected CSS transform `' + cs.transform + '` cannot be mapped' };
    }
    sx *= m.a;
    sy *= m.d;
  }
  if (cs.rotate && cs.rotate !== 'none') {
    var rm = String(cs.rotate).trim().match(/^([+-]?(?:\d+\.?\d*|\.\d+)(?:e[+-]?\d+)?)(deg|rad|grad|turn)$/i);
    if (!rm || !isFinite(parseFloat(rm[1])) || Math.abs(parseFloat(rm[1])) > 1e-6) {
      return { ok: false, reason: 'CSS `rotate: ' + cs.rotate + '` cannot be mapped to axis-aligned viewport coordinates' };
    }
  }
  if (cs.scale && cs.scale !== 'none') {
    var sp = String(cs.scale).trim().split(/\s+/);
    var s0 = Number(sp[0]);
    var s1 = sp.length > 1 ? Number(sp[1]) : s0;
    var s2 = sp.length > 2 ? Number(sp[2]) : 1;
    if (sp.length > 3 || !isFinite(s0) || !isFinite(s1) || !isFinite(s2) || s0 <= 0 || s1 <= 0 || Math.abs(s2 - 1) > 1e-6) {
      return { ok: false, reason: 'CSS `scale: ' + cs.scale + '` cannot be mapped to 2D viewport coordinates' };
    }
    sx *= s0;
    sy *= s1;
  }
  if (cs.translate && cs.translate !== 'none') {
    var tp = String(cs.translate).trim().split(/\s+/);
    if (tp.length > 2) {
      var tz = parseFloat(tp[2]);
      if (!isFinite(tz) || Math.abs(tz) > 1e-6) {
        return { ok: false, reason: '3D CSS `translate: ' + cs.translate + '` cannot be mapped' };
      }
    }
  }
  if (cs.offsetPath && cs.offsetPath !== 'none') {
    return { ok: false, reason: 'CSS `offset-path` transform cannot be mapped' };
  }
  if (cs.zoom && cs.zoom !== 'normal') {
    var zm = parseFloat(cs.zoom);
    if (String(cs.zoom).indexOf('%') !== -1) zm /= 100;
    if (!isFinite(zm) || zm <= 0) {
      return { ok: false, reason: 'non-positive CSS `zoom: ' + cs.zoom + '` cannot be mapped' };
    }
    sx *= zm;
    sy *= zm;
  }
  return { ok: true, sx: sx, sy: sy };
};
var checkTransformChain = function (startNode, doc) {
  var win = (doc && doc.defaultView) || window;
  var sx = 1;
  var sy = 1;
  var cur = startNode;
  while (cur && cur.nodeType === 1) {
    var step = checkAxisAlignedTransform(cur, win);
    if (!step.ok) return step;
    sx *= step.sx;
    sy *= step.sy;
    cur = cur.parentElement || (cur.parentNode && cur.parentNode.host) || null;
  }
  return { ok: true, sx: sx, sy: sy };
};
var frameTransform = function (frame) {
  var doc = frame.ownerDocument || document;
  var fv = doc.defaultView || window;
  var chain = checkTransformChain(frame, doc);
  if (!chain.ok) return chain;
  var fr = frame.getBoundingClientRect();
  var fs = fv.getComputedStyle(frame);
  var cssBl = parseFloat(fs.borderLeftWidth) || 0;
  var cssBt = parseFloat(fs.borderTopWidth) || 0;
  var borderLeft = frame.clientLeft > cssBl + 0.5 ? frame.clientLeft : (cssBl || frame.clientLeft || 0);
  var borderTop = frame.clientTop > cssBt + 0.5 ? frame.clientTop : (cssBt || frame.clientTop || 0);
  var padLeft = parseFloat(fs.paddingLeft) || 0;
  var padTop = parseFloat(fs.paddingTop) || 0;
  var borderRight = parseFloat(fs.borderRightWidth) || 0;
  var borderBottom = parseFloat(fs.borderBottomWidth) || 0;
  var padRight = parseFloat(fs.paddingRight) || 0;
  var padBottom = parseFloat(fs.paddingBottom) || 0;
  var cssW = parseFloat(fs.width);
  var cssH = parseFloat(fs.height);
  var isBorderBox = fs.boxSizing === 'border-box';
  var localW = isFinite(cssW) && cssW > 0
    ? (isBorderBox ? cssW : cssW + borderLeft + borderRight + padLeft + padRight)
    : frame.offsetWidth;
  var localH = isFinite(cssH) && cssH > 0
    ? (isBorderBox ? cssH : cssH + borderTop + borderBottom + padTop + padBottom)
    : frame.offsetHeight;
  if (!isFinite(localW) || localW <= 0 || !isFinite(localH) || localH <= 0 || !isFinite(fr.width) || fr.width <= 0 || !isFinite(fr.height) || fr.height <= 0) {
    return { ok: false, reason: 'enclosing frame has non-positive layout dimensions' };
  }
  if (Math.abs(fr.width - localW * chain.sx) > Math.max(2, localW * chain.sx * 0.02) ||
      Math.abs(fr.height - localH * chain.sy) > Math.max(2, localH * chain.sy * 0.02)) {
    return { ok: false, reason: 'enclosing frame bounding box does not match axis-aligned transform geometry' };
  }
  var sx = fr.width / localW;
  var sy = fr.height / localH;
  return {
    ok: true,
    rect: fr,
    sx: sx,
    sy: sy,
    ox: fr.left + sx * (borderLeft + padLeft),
    oy: fr.top + sy * (borderTop + padTop),
    contentRight: fr.right - sx * (borderRight + padRight),
    contentBottom: fr.bottom - sy * (borderBottom + padBottom)
  };
};
"#;

/// Composed-tree helpers shared by every script that must see into open shadow
/// roots.
///
/// `collectDeepElements` walks `root` in composed pre-order: it yields each
/// element, then the elements of the element's open `shadowRoot`, then the
/// element's light children. That is the order `querySelectorAll` would use if it
/// pierced roots, so `nth` keeps naming the element a reader sees first.
///
/// `composedContains` answers `Node.contains` across shadow boundaries by
/// climbing through a `ShadowRoot` to its host. A closed root has no accessible
/// `shadowRoot`, so both helpers stop at the host, which is why the driver
/// reports closed roots instead of pretending to search them.
const COMPOSED_HELPERS_JS: &str = r#"
var collectDeepElements = function (root) {
  if (!root) return [];
  var out = [];
  var walk = function (node) {
    var kids = node ? node.children : null;
    if (!kids) return;
    for (var i = 0; i < kids.length; i++) {
      var child = kids[i];
      out.push(child);
      if (child.shadowRoot) walk(child.shadowRoot);
      walk(child);
    }
  };
  if (root.shadowRoot) walk(root.shadowRoot);
  walk(root);
  return out;
};
var collectDeepMatches = function (root, selector) {
  // querySelectorAll keeps standard `:scope` semantics per search root, so it is
  // evaluated once on the root and once on every open shadow root instead of
  // testing candidates with matches(), where `:scope` would name the candidate.
  if (!root) return [];
  var out = [];
  var push = function (nodes) {
    for (var i = 0; i < nodes.length; i++) {
      if (out.indexOf(nodes[i]) === -1) out.push(nodes[i]);
    }
  };
  push(root.querySelectorAll(selector));
  // A `within` container can itself be a shadow host, and its own root's
  // contents are not part of `root.querySelectorAll`.
  if (root.shadowRoot) push(root.shadowRoot.querySelectorAll(selector));
  var elements = collectDeepElements(root);
  for (var i = 0; i < elements.length; i++) {
    if (elements[i].shadowRoot) push(elements[i].shadowRoot.querySelectorAll(selector));
  }
  return out;
};
var composedContains = function (ancestor, node) {
  if (!ancestor || !node) return false;
  var cur = node;
  while (cur) {
    if (cur === ancestor) return true;
    if (cur.parentNode) {
      cur = cur.parentNode;
      continue;
    }
    cur = cur.nodeType === 11 && cur.host ? cur.host : null;
  }
  return false;
};
"#;

/// Frame traversal, coordinate transforms and visibility checks, plus the
/// composed-tree helpers.
static FRAME_HELPERS_JS: LazyLock<String> =
    LazyLock::new(|| format!("{FRAME_GEOMETRY_HELPERS_JS}\n{COMPOSED_HELPERS_JS}"));

/// JavaScript that covers every visible editable field with an opaque box,
/// including fields inside accessible same-origin and nested iframes.
///
/// `position: fixed` uses top-level viewport coordinates (`ox + sx * rect.left`,
/// `oy + sy * rect.top`) so a single overlay at the highest `z-index` covers fields
/// across nested scaled frames, except in `<frameset>` documents where masks live on
/// each frame document itself. Fails clearly instead of leaving uncovered pixels if
/// a frame or root element has a CSS transform that cannot be mapped accurately.
static MASK_INPUTS_SCRIPT: LazyLock<String> = LazyLock::new(|| {
    let helpers = FRAME_HELPERS_JS.as_str();
    format!(
        r#"(function () {{
{helpers}
  var skipped = ['button', 'submit', 'reset', 'checkbox', 'radio', 'image', 'hidden', 'range', 'color'];
  var masks = [];
  var isFrameset = Boolean(document.body && document.body.tagName === 'FRAMESET');
  var fail = function (reason) {{
    for (var m = 0; m < masks.length; m++) {{
      masks[m].remove();
    }}
    return {{ ok: false, reason: reason }};
  }};
  var measureHostMap = function (hostDoc) {{
    var root = hostDoc && hostDoc.documentElement;
    if (!root) return {{ ok: false, reason: 'document root element is missing' }};
    var win = hostDoc.defaultView || window;
    var chain = checkTransformChain(root, hostDoc);
    if (!chain.ok) return chain;
    var rcs = win.getComputedStyle(root);
    var rootMoved = (rcs.transform && rcs.transform !== 'none') ||
      (rcs.scale && rcs.scale !== 'none') ||
      (rcs.translate && rcs.translate !== 'none') ||
      Math.abs(chain.sx - 1) > 1e-6 ||
      Math.abs(chain.sy - 1) > 1e-6;
    if (!rootMoved) {{
      return {{ ok: true, ox: 0, oy: 0, sx: 1, sy: 1 }};
    }}
    var probe = hostDoc.createElement('div');
    probe.style.cssText = 'position:fixed;box-sizing:border-box;left:0px;top:0px;width:100px;height:100px;border:0;padding:0;margin:0;visibility:hidden;pointer-events:none;';
    root.appendChild(probe);
    var pr = probe.getBoundingClientRect();
    probe.remove();
    if (!isFinite(pr.width) || !isFinite(pr.height) || pr.width <= 0 || pr.height <= 0) {{
      return {{ ok: false, reason: 'document root has collapsed transform geometry' }};
    }}
    return {{ ok: true, ox: pr.left, oy: pr.top, sx: pr.width / 100, sy: pr.height / 100 }};
  }};
  var topHost = isFrameset ? {{ ok: true, ox: 0, oy: 0, sx: 1, sy: 1 }} : measureHostMap(document);
  var appendMask = function (hostDoc, hostMap, left, top, width, height) {{
    if (!hostDoc || !hostDoc.documentElement || width <= 0 || height <= 0) return true;
    var cssLeft = (left - hostMap.ox) / hostMap.sx;
    var cssTop = (top - hostMap.oy) / hostMap.sy;
    var cssW = width / hostMap.sx;
    var cssH = height / hostMap.sy;
    var mask = hostDoc.createElement('div');
    mask.setAttribute('data-da-screenshot-mask', '');
    mask.style.cssText = 'position:fixed;box-sizing:border-box;left:' + cssLeft + 'px;top:' + cssTop + 'px;width:' + cssW + 'px;height:' + cssH + 'px;background:#000;opacity:1;border:0;padding:0;margin:0;z-index:2147483647;pointer-events:none;';
    hostDoc.documentElement.appendChild(mask);
    masks.push(mask);
    var mr = mask.getBoundingClientRect();
    if (!isFinite(mr.width) || mr.width <= 0 || !isFinite(mr.height) || mr.height <= 0 ||
        mr.left > left + 1 || mr.top > top + 1 ||
        mr.right < left + width - 1 || mr.bottom < top + height - 1) {{
      return false;
    }}
    return true;
  }};
  var collectDoc = function (doc, ox, oy, sx, sy, clip, blockedReason) {{
    var win = doc.defaultView || window;
    var all = collectDeepElements(doc);
    for (var i = 0; i < all.length; i++) {{
      var el = all[i];
      if (el.nodeType !== 1 || !el.matches('input, textarea, select, [contenteditable]')) continue;
      var tag = el.tagName;
      var field = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT';
      if (!field && !el.isContentEditable) continue;
      if (tag === 'INPUT' && skipped.indexOf((el.type || 'text').toLowerCase()) !== -1) continue;
      if (!isBoxVisible(el, win)) continue;
      if (blockedReason) return fail(blockedReason);
      var rect = el.getBoundingClientRect();
      if (isFrameset && doc !== document) {{
        var frameHost = measureHostMap(doc);
        if (!frameHost.ok) return fail(frameHost.reason);
        if (!appendMask(doc, frameHost, rect.left, rect.top, rect.width, rect.height)) {{
          return fail('screenshot mask could not cover transformed field');
        }}
        continue;
      }}
      var left = ox + sx * rect.left;
      var top = oy + sy * rect.top;
      var right = ox + sx * rect.right;
      var bottom = oy + sy * rect.bottom;
      if (clip) {{
        left = Math.max(left, clip.left);
        top = Math.max(top, clip.top);
        right = Math.min(right, clip.right);
        bottom = Math.min(bottom, clip.bottom);
      }}
      if (right <= left || bottom <= top) continue;
      if (!appendMask(document, topHost, left, top, right - left, bottom - top)) {{
        return fail('screenshot mask could not cover transformed field');
      }}
    }}
    var frames = collectDeepElements(doc);
    for (var j = 0; j < frames.length; j++) {{
      var frame = frames[j];
      if (frame.nodeType !== 1 || !frame.matches('iframe, frame')) continue;
      if (!isBoxVisible(frame, win)) continue;
      var childDoc = null;
      try {{
        childDoc = frame.contentDocument;
      }} catch (e) {{
        childDoc = null;
      }}
      if (!childDoc) continue;
      if (blockedReason) {{
        var blockedErr = collectDoc(childDoc, 0, 0, 1, 1, null, blockedReason);
        if (blockedErr) return blockedErr;
        continue;
      }}
      var fTr = frameTransform(frame);
      if (!fTr.ok) {{
        var trErr = collectDoc(childDoc, 0, 0, 1, 1, null, fTr.reason);
        if (trErr) return trErr;
        continue;
      }}
      var childOx = ox + sx * fTr.ox;
      var childOy = oy + sy * fTr.oy;
      var childSx = sx * fTr.sx;
      var childSy = sy * fTr.sy;
      var nextClip = {{
        left: childOx,
        top: childOy,
        right: ox + sx * fTr.contentRight,
        bottom: oy + sy * fTr.contentBottom
      }};
      if (clip) {{
        nextClip.left = Math.max(nextClip.left, clip.left);
        nextClip.top = Math.max(nextClip.top, clip.top);
        nextClip.right = Math.min(nextClip.right, clip.right);
        nextClip.bottom = Math.min(nextClip.bottom, clip.bottom);
      }}
      if (nextClip.right <= nextClip.left || nextClip.bottom <= nextClip.top) continue;
      var childErr = collectDoc(childDoc, childOx, childOy, childSx, childSy, nextClip, null);
      if (childErr) return childErr;
    }}
    return null;
  }};
  var rootBlocked = topHost.ok ? null : topHost.reason;
  var err = collectDoc(document, 0, 0, 1, 1, null, rootBlocked);
  if (err) return err;
  return {{ ok: true, count: masks.length }};
}})()"#
    )
});

/// JavaScript that removes every mask this module added across the top document
/// and any accessible same-origin frames, including frames inside open shadow
/// roots.
static REMOVE_MASKS_SCRIPT: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"(function () {{
  {COMPOSED_HELPERS_JS}
  var removed = 0;
  var cleanDoc = function (doc) {{
    var all = collectDeepElements(doc);
    for (var i = 0; i < all.length; i++) {{
      if (all[i].hasAttribute('data-da-screenshot-mask')) {{
        all[i].remove();
        removed++;
      }}
    }}
    for (var j = 0; j < all.length; j++) {{
      var frame = all[j];
      if (!frame.matches('iframe, frame')) continue;
      var childDoc = null;
      try {{
        childDoc = frame.contentDocument;
      }} catch (e) {{
        childDoc = null;
      }}
      if (childDoc) cleanDoc(childDoc);
    }}
  }};
  cleanDoc(document);
  return removed;
}})()"#
    )
});

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
    ///
    /// `css` and `testid` walk the composed tree, so they reach elements inside
    /// any number of nested open shadow roots. `xpath` deliberately does not:
    /// `document.evaluate` cannot see into a shadow root (see `docs/guide.md`).
    fn resolve_body_with(&self, testid_attribute: &str, nth: Option<usize>) -> Option<String> {
        let collect = match self {
            Self::Css(selector) => format!(
                "{COMPOSED_HELPERS_JS}
var selector = {};
var matches = collectDeepMatches(root, selector);
var hiddenFound = false;",
                json_string(selector)
            ),
            Self::Xpath(expression) => format!(
                "var doc = root.nodeType === 9 ? root : (root.ownerDocument || document);\nvar snap = doc.evaluate({}, root, null, XPathResult.ORDERED_NODE_SNAPSHOT_TYPE, null);\nvar matches = [];\nfor (var i = 0; i < snap.snapshotLength; i++) {{\n  var node = snap.snapshotItem(i);\n  if (node && node.nodeType === 1 && (root === doc || (node !== root && root.contains(node)))) matches.push(node);\n}}\nvar hiddenFound = false;",
                json_string(expression)
            ),
            Self::TestId(id) => format!(
                "{COMPOSED_HELPERS_JS}\nvar attr = {};\nvar wanted = {};\nvar all = collectDeepElements(root);\nvar matches = [];\nfor (var i = 0; i < all.length; i++) {{\n  if (all[i].getAttribute(attr) === wanted) matches.push(all[i]);\n}}\nvar hiddenFound = false;",
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
///
/// `document.activeElement` reports the outermost shadow host, so the check
/// descends each open root to find the element that really holds focus.
const FOCUS_BODY: &str = "if (!el || !el.isConnected) return false; el.focus(); var doc = el.ownerDocument || document; var active = doc.activeElement; while (active && active.shadowRoot && active.shadowRoot.activeElement) active = active.shadowRoot.activeElement; return active === el;";

/// The body of a text read, run with the located element as `el`.
const TEXT_BODY: &str =
    "if (!el) return ''; return String(el.textContent === null ? '' : el.textContent).trim();";

/// The body of a visibility read, run with the located element as `el`.
static VISIBLE_BODY: LazyLock<String> = LazyLock::new(|| {
    let helpers = FRAME_HELPERS_JS.as_str();
    format!(
        r#"{helpers}
if (!el || el.nodeType !== 1 || !el.isConnected) return false;
var frames = collectEnclosingFrames(el);
if (!frames || !areEnclosingFramesVisible(frames)) return false;
return isBoxVisible(el, null);"#
    )
});

/// Inspects the element `browser.upload` resolved and reports whether it can
/// take files: a `<input type="file">` that is present and enabled.
///
/// A status object keeps "this is not a file input" and "this file input is
/// disabled" distinguishable without throwing, so the step can name the element
/// it actually matched.
const FILE_INPUT_STATE_BODY: &str = r#"if (!el || el.nodeType !== 1 || !el.isConnected) return { status: 'detached' };
var tag = el.tagName ? el.tagName.toLowerCase() : '';
var type = String(el.getAttribute('type') || '').toLowerCase();
if (tag !== 'input' || type !== 'file') {
  return { status: 'not_file_input', tag: tag, type: type };
}
var disabled = false;
try {
  disabled = el.matches(':disabled');
} catch (e) {
  disabled = Boolean(el.disabled);
}
return { status: 'ok', disabled: disabled, multiple: Boolean(el.multiple) };"#;

/// Reads back the file names the input holds after `DOM.setFileInputFiles`.
///
/// The names are the browser's own view of the selection, so the step reports
/// what the page sees instead of what it asked for.
const FILE_INPUT_NAMES_BODY: &str = r#"var names = [];
if (el && el.files) {
  for (var i = 0; i < el.files.length; i++) names.push(String(el.files[i].name));
}
return names;"#;

/// Replaces a matched `<label>` with the control it labels.
///
/// A file input is usually hidden behind its label or a styled button, so a
/// locator that names the visible label — by its `text`, for example — must
/// upload to the input the label controls. Anything else is returned unchanged.
const FOLLOW_LABEL_SCRIPT: &str = r#"function () {
  var el = this;
  if (!el || el.nodeType !== 1) return null;
  var tag = el.tagName ? el.tagName.toUpperCase() : '';
  if (tag === 'LABEL' && el.control) return el.control;
  return el;
}"#;

/// Clears the selection of the file input `el`.
///
/// `DOM.setFileInputFiles` with an empty list leaves the current files in place
/// (Chrome treats an empty list as "nothing to do"), so an empty `files` list is
/// applied in the page instead: assigning `value` empties the `FileList`, and
/// the `input` and `change` events the browser fires for a new selection are
/// dispatched so the page sees the clear. An input that held nothing keeps its
/// handlers quiet.
const CLEAR_FILE_INPUT_BODY: &str = r#"var hadSelection = Boolean(el && el.files && el.files.length > 0);
el.value = '';
if (hadSelection) {
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
}
return true;"#;

/// The status [`FILE_INPUT_STATE_BODY`] reports.
#[derive(Debug, PartialEq, Eq, Deserialize)]
struct FileInputState {
    status: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    multiple: bool,
}

/// Inspects a matched `within` container (`this`) and returns either the
/// container element itself, the `contentDocument` of a ready same-origin frame,
/// or a JSON status string (`missing`, `loading`, `cross_origin`).
const ENTER_CONTAINER_SCRIPT: &str = r#"function () {
  var el = this;
  if (!el || el.nodeType !== 1) {
    return JSON.stringify({ status: 'missing' });
  }
  var tag = el.tagName ? el.tagName.toUpperCase() : '';
  if (tag !== 'IFRAME' && tag !== 'FRAME') {
    return el;
  }
  if (!el.isConnected) {
    return JSON.stringify({ status: 'missing' });
  }
  var doc = null;
  try {
    doc = el.contentDocument;
  } catch (e) {
    return JSON.stringify({ status: 'cross_origin' });
  }
  if (!doc) {
    var hasWindow = false;
    try {
      hasWindow = el.contentWindow !== null;
    } catch (e) {
      hasWindow = true;
    }
    if (hasWindow || el.hasAttribute('sandbox')) {
      return JSON.stringify({ status: 'cross_origin' });
    }
    return JSON.stringify({ status: 'missing' });
  }
  if (!el.hasAttribute('srcdoc')) {
    var rawSrc = el.getAttribute('src');
    if (rawSrc !== null) {
      var trimmed = String(rawSrc).trim();
      if (trimmed !== '' && trimmed.toLowerCase().indexOf('javascript:') !== 0) {
        try {
          var base = (el.ownerDocument && el.ownerDocument.baseURI) || location.href;
          var targetUrl = new URL(trimmed, base);
          var docUrl = String(doc.URL || '');
          if (targetUrl.href !== 'about:blank' && docUrl === 'about:blank') {
            var hostOrigin = (el.ownerDocument && el.ownerDocument.defaultView && el.ownerDocument.defaultView.location)
              ? el.ownerDocument.defaultView.location.origin
              : location.origin;
            if (targetUrl.origin !== hostOrigin) {
              return JSON.stringify({ status: 'cross_origin' });
            }
            return JSON.stringify({ status: 'loading' });
          }
        } catch (e) {
        }
      }
    }
  }
  if (doc.readyState === 'loading') {
    return JSON.stringify({ status: 'loading' });
  }
  return doc;
}"#;

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
///
/// The candidates come from the composed walk, and "innermost" is composed
/// containment: a host whose `innerText` includes the text of its open shadow
/// root therefore yields to the element that actually renders it instead of
/// making the locator look ambiguous.
const TEXT_COLLECT_TEMPLATE: &str = r#"var wanted = __DA_WANTED__;
var normalize = function (value) {
  return String(value === null || value === undefined ? '' : value).replace(/\s+/g, ' ').trim();
};
var rootFrames = collectEnclosingFrames(root);
var framesVisible = rootFrames !== null && areEnclosingFramesVisible(rootFrames);
var all = collectDeepElements(root);
var hiddenFound = false;
var visibleMatches = [];
for (var i = 0; i < all.length; i++) {
  var el = all[i];
  if (normalize(el.innerText) === wanted) {
    if (framesVisible && isBoxVisible(el, null)) visibleMatches.push(el);
    else hiddenFound = true;
  }
}
var matches = [];
for (var j = 0; j < visibleMatches.length; j++) {
  var candidate = visibleMatches[j];
  var innermost = true;
  for (var k = 0; k < visibleMatches.length; k++) {
    if (k !== j && composedContains(candidate, visibleMatches[k])) { innermost = false; break; }
  }
  if (innermost) matches.push(candidate);
}"#;

/// Builds the text candidate collection snippet for one value.
fn text_collect_script(text: &str) -> String {
    let helpers = FRAME_HELPERS_JS.as_str();
    format!(
        "{helpers}\n{}",
        TEXT_COLLECT_TEMPLATE.replace("__DA_WANTED__", &json_string(text))
    )
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

/// Inspects an element (`el`) for visibility, disabled state, geometry, and hit-test,
/// accumulating viewport offsets and checking `elementFromPoint` across enclosing frames.
static ACTIONABLE_INSPECT_BODY: LazyLock<String> = LazyLock::new(|| {
    let helpers = FRAME_HELPERS_JS.as_str();
    format!(
        r#"{helpers}
var summarizeNode = function (node) {{
  var tag = node.tagName ? node.tagName.toLowerCase() : 'element';
  var id = node.id ? String(node.id) : '';
  var raw = (node.innerText && node.innerText.trim()) ? node.innerText : node.textContent;
  var text = String(raw === null || raw === undefined ? '' : raw).replace(/\s+/g, ' ').trim();
  if (text.length > 240) text = text.slice(0, 240);
  return {{ tag: tag, id: id, text: text }};
}};
// `document.elementFromPoint` returns the outermost shadow host at the point.
// Descending each open root in turn reaches the element that really receives
// the click; a closed root has no `shadowRoot`, so the host stays the hit.
var deepHit = function (doc, x, y) {{
  var hit = doc.elementFromPoint(x, y);
  while (hit && hit.shadowRoot) {{
    var deeper = null;
    try {{
      deeper = hit.shadowRoot.elementFromPoint(x, y);
    }} catch (e) {{
      deeper = null;
    }}
    if (!deeper || deeper === hit) break;
    hit = deeper;
  }}
  return hit;
}};
if (!el || el.nodeType !== 1 || !el.isConnected) return {{ status: 'not_visible' }};
var frames = collectEnclosingFrames(el);
if (!frames || !areEnclosingFramesVisible(frames) || !isBoxVisible(el, null)) {{
  return {{ status: 'not_visible' }};
}}
var fs = typeof el.closest === 'function' ? el.closest('fieldset[disabled]') : null;
var inDisabledFieldset = false;
if (fs) {{
  var legend = fs.querySelector('legend');
  if (!legend || !legend.contains(el)) inDisabledFieldset = true;
}}
var ariaNode = typeof el.closest === 'function' ? el.closest('[aria-disabled]') : null;
var ariaDisabled = ariaNode !== null && String(ariaNode.getAttribute('aria-disabled') || '').toLowerCase() === 'true';
if (el.disabled === true || el.hasAttribute('disabled') || inDisabledFieldset || ariaDisabled) {{
  return {{ status: 'disabled' }};
}}
var elDoc = el.ownerDocument || document;
var elWin = elDoc.defaultView || window;
var rect = el.getBoundingClientRect();
var elVw = elWin.innerWidth || (elDoc.documentElement && elDoc.documentElement.clientWidth) || 0;
var elVh = elWin.innerHeight || (elDoc.documentElement && elDoc.documentElement.clientHeight) || 0;
var lx0 = rect.left + rect.width / 2;
var ly0 = rect.top + rect.height / 2;
if (lx0 < 0 || lx0 >= elVw || ly0 < 0 || ly0 >= elVh) {{
  el.scrollIntoView({{ block: 'center', inline: 'center' }});
  rect = el.getBoundingClientRect();
}}
var curX = rect.left + rect.width / 2;
var curY = rect.top + rect.height / 2;
for (var i = frames.length - 1; i >= 0; i--) {{
  var f = frames[i];
  var fDoc = f.ownerDocument || document;
  var fWin = fDoc.defaultView || window;
  var fb = frameTransform(f);
  if (!fb.ok) {{
    return {{ status: 'unmappable_transform', reason: fb.reason }};
  }}
  var px = fb.ox + fb.sx * curX;
  var py = fb.oy + fb.sy * curY;
  var fVw = fWin.innerWidth || (fDoc.documentElement && fDoc.documentElement.clientWidth) || 0;
  var fVh = fWin.innerHeight || (fDoc.documentElement && fDoc.documentElement.clientHeight) || 0;
  if (px < 0 || px >= fVw || py < 0 || py >= fVh) {{
    f.scrollIntoView({{ block: 'center', inline: 'center' }});
    fb = frameTransform(f);
    if (!fb.ok) {{
      return {{ status: 'unmappable_transform', reason: fb.reason }};
    }}
    px = fb.ox + fb.sx * curX;
    py = fb.oy + fb.sy * curY;
    if (px < 0 || px >= fVw || py < 0 || py >= fVh) {{
      fWin.scrollBy(px - fVw / 2, py - fVh / 2);
      fb = frameTransform(f);
      if (!fb.ok) {{
        return {{ status: 'unmappable_transform', reason: fb.reason }};
      }}
      px = fb.ox + fb.sx * curX;
      py = fb.oy + fb.sy * curY;
    }}
  }}
  curX = px;
  curY = py;
}}
rect = el.getBoundingClientRect();
var lx = rect.left + rect.width / 2;
var ly = rect.top + rect.height / 2;
var accX = lx;
var accY = ly;
var accLeft = rect.left;
var accTop = rect.top;
var accW = rect.width;
var accH = rect.height;
var levelChecks = [];
for (var j = frames.length - 1; j >= 0; j--) {{
  var frame = frames[j];
  var fTr = frameTransform(frame);
  if (!fTr.ok) {{
    return {{ status: 'unmappable_transform', reason: fTr.reason }};
  }}
  accX = fTr.ox + fTr.sx * accX;
  accY = fTr.oy + fTr.sy * accY;
  accLeft = fTr.ox + fTr.sx * accLeft;
  accTop = fTr.oy + fTr.sy * accTop;
  accW = fTr.sx * accW;
  accH = fTr.sy * accH;
  levelChecks.unshift({{
    doc: frame.ownerDocument || document,
    x: accX,
    y: accY,
    target: frame
  }});
}}
var topX = accX;
var topY = accY;
var topRect = {{ x: accLeft, y: accTop, width: accW, height: accH }};
for (var k = 0; k < levelChecks.length; k++) {{
  var check = levelChecks[k];
  var fHit = deepHit(check.doc, check.x, check.y);
  if (fHit !== check.target) {{
    return {{
      status: 'ready_candidate',
      rect: topRect,
      x: topX,
      y: topY,
      hit_ok: false,
      blocker: fHit ? summarizeNode(fHit) : null
    }};
  }}
}}
var hit = deepHit(elDoc, lx, ly);
var hitOk = hit !== null && (hit === el || composedContains(el, hit));
var blocker = !hitOk && hit ? summarizeNode(hit) : null;
return {{
  status: 'ready_candidate',
  rect: topRect,
  x: topX,
  y: topY,
  hit_ok: hitOk,
  blocker: blocker
}};
"#
    )
});

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
    UnmappableTransform {
        reason: String,
    },
    ReadyCandidate {
        rect: ElementRect,
        x: f64,
        y: f64,
        hit_ok: bool,
        blocker: Option<CandidateInfo>,
    },
}

/// JavaScript returning the frame-index path from `window.top` to the document
/// holding `this`, or `null` if detached / cross-origin.
const FRAME_PATH_BODY: &str = r#"function () {
  var doc = this && this.nodeType === 9 ? this : (this && this.ownerDocument);
  var win = doc && doc.defaultView;
  if (!win) return null;
  var path = [];
  try {
    var topWin = window.top;
    while (win !== topWin) {
      var parent = win.parent;
      var idx = -1;
      for (var i = 0; i < parent.frames.length; i++) {
        if (parent.frames[i] === win) {
          idx = i;
          break;
        }
      }
      if (idx < 0) return null;
      path.unshift(idx);
      win = parent;
    }
  } catch (e) {
    return null;
  }
  return path;
}"#;

/// Status returned by `ENTER_CONTAINER_SCRIPT` when a `within` frame is missing, loading, or cross-origin.
#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WithinContainerStatus {
    Missing,
    Loading,
    CrossOrigin,
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
    Launched(Box<Launched>),
    /// Reattached to a detached browser process started by an earlier `da session`.
    Detached { pid: u32, profile: PathBuf },
    /// The case connected to the user's browser and closes only its own tab.
    Connected,
}

/// One case's browser, its page, and the options that created it.
struct BrowserSession {
    /// Taken by [`Session::close`], which releases it before the case report is
    /// written. `Drop` only matters on the fallback path.
    connection: Option<Connection>,
    origin: Option<Origin>,
    /// Every tab this case owns, and the one the session drives now.
    tabs: tabs::Tabs,
    /// The target session every page command is sent with.
    page: String,
    /// The requests this page reported, bounded and merged by request id.
    network: network::NetworkState,
    port: u16,
    websocket_url: String,
    headless: bool,
    width: u32,
    height: u32,
    /// The connected endpoint, as `connect` named it; `None` when the case
    /// started its own browser.
    connect: Option<String>,
    /// Named persistent profile directory name, if `profile` was given.
    profile: Option<String>,
    /// Pending one-shot `Page.addScriptToEvaluateOnNewDocument` `(origin, script_id)`
    /// pairs registered by `browser.set_cookies` for origins not yet loaded.
    pending_local_storage_scripts: Vec<(String, String)>,
    /// Attribute name matched by `testid` locators in this session.
    testid_attribute: String,
    /// Snapshot `ref` table for interactive sessions.
    refs: snapshot::RefStore,
    /// Per-step metadata (such as synthesized locator) collected by `da session`.
    step_metadata: Option<Value>,
    /// Whether a browser step ran during the current interactive command.
    snapshot_pending: bool,
}

/// One element resolved from a target, held as a CDP DOM `objectId`.
struct Located(String);

impl BrowserSession {
    /// Creates the session described by `browser.open`.
    fn open(args: &OpenArgs, connect: Option<&Endpoint>, detached: bool) -> Result<Self, String> {
        match connect {
            Some(endpoint) => Self::connect_to(endpoint, args.testid_attribute.as_deref()),
            None => Self::launch(args, detached),
        }
    }

    /// Creates a default headless session when `browser.set_cookies` runs before
    /// `browser.open`.
    fn open_default(detached: bool) -> Result<Self, String> {
        let defaults = OpenArgs {
            url: "about:blank".to_string(),
            connect: None,
            profile: None,
            storage_state: None,
            headless: default_headless(),
            width: default_width(),
            height: default_height(),
            timeout_ms: default_navigation_timeout_ms(),
            testid_attribute: None,
            dialog: None,
        };
        Self::launch(&defaults, detached)
    }

    /// Starts the browser described by `browser.open` and attaches its page.
    fn launch(args: &OpenArgs, detached: bool) -> Result<Self, String> {
        let deadline = Deadline::after(launch_timeout());
        let (browser, (connection, page, tabs)) = launch_browser_with_retry(
            deadline,
            MAX_LAUNCH_ATTEMPTS,
            |attempt_deadline| {
                Launched::launch_with_mode_and_profile(
                    args.headless,
                    args.width,
                    args.height,
                    attempt_deadline,
                    detached,
                    args.profile.as_deref(),
                )
            },
            |browser, attempt_deadline| {
                let mut connection = Connection::connect(browser.websocket_url(), attempt_deadline)
                    .map_err(|error| {
                        format!("could not connect to the browser it started: {error}; set `DA_CHROME` to the Chrome or Chromium executable")
                    })?;
                let (page, target_id) = attach(&mut connection, attempt_deadline)?;
                let tabs = tabs::Tabs::start(
                    &mut connection,
                    &target_id,
                    attempt_deadline.capped(TAB_QUERY_TIMEOUT),
                )?;
                Ok((connection, page, tabs))
            },
            Launched::abort,
        )?;
        let port = browser.port();
        let websocket_url = browser.websocket_url().to_string();
        let network = network::NetworkState::new(&page);

        Ok(Self {
            connection: Some(connection),
            origin: Some(Origin::Launched(Box::new(browser))),
            tabs,
            page,
            network,
            port,
            websocket_url,
            headless: args.headless,
            width: args.width,
            height: args.height,
            connect: None,
            profile: args.profile.clone(),
            pending_local_storage_scripts: Vec::new(),
            testid_attribute: args
                .testid_attribute
                .clone()
                .unwrap_or_else(|| DEFAULT_TESTID_ATTRIBUTE.to_string()),
            refs: snapshot::RefStore::default(),
            step_metadata: None,
            snapshot_pending: false,
        })
    }

    /// Attaches a page in a browser the user already runs.
    fn connect_to(endpoint: &Endpoint, testid_attribute: Option<&str>) -> Result<Self, String> {
        let websocket_url = endpoint.websocket_url(connect::CONNECT_TIMEOUT)?;
        let mut connection = Connection::connect(&websocket_url, Deadline::after(launch_timeout()))
            .map_err(|error| {
                endpoint.explain(format!("could not open the DevTools connection: {error}"))
            })?;
        let deadline = Deadline::after(launch_timeout());
        let (page, target_id) = attach(&mut connection, deadline)?;
        let tabs = tabs::Tabs::start(
            &mut connection,
            &target_id,
            deadline.capped(TAB_QUERY_TIMEOUT),
        )?;
        let network = network::NetworkState::new(&page);

        Ok(Self {
            connection: Some(connection),
            origin: Some(Origin::Connected),
            tabs,
            page,
            network,
            port: endpoint.port(),
            websocket_url,
            headless: true,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            connect: Some(endpoint.name()),
            profile: None,
            pending_local_storage_scripts: Vec::new(),
            testid_attribute: testid_attribute
                .unwrap_or(DEFAULT_TESTID_ATTRIBUTE)
                .to_string(),
            refs: snapshot::RefStore::default(),
            step_metadata: None,
            snapshot_pending: false,
        })
    }

    fn reattach_boxed(handle: &Value) -> Result<Box<dyn Session>, String> {
        Self::reattach(handle).map(|session| Box::new(session) as Box<dyn Session>)
    }

    fn reattach(handle: &Value) -> Result<Self, String> {
        let parsed: DetachedBrowserHandle = serde_json::from_value(handle.clone())
            .map_err(|error| format!("invalid browser session handle: {error}"))?;
        let (origin, port, websocket_url, target_id, profile_name) =
            match validate_detached_handle(&parsed, &std::env::temp_dir())? {
                ValidatedHandle::Launched {
                    pid,
                    profile,
                    profile_name,
                    port,
                    websocket_url,
                    target_id,
                } => (
                    Origin::Detached {
                        pid,
                        profile: profile.to_path_buf(),
                    },
                    port,
                    websocket_url.to_string(),
                    target_id.to_string(),
                    profile_name.map(str::to_string),
                ),
                ValidatedHandle::Connected {
                    port,
                    websocket_url,
                    ..
                } => (
                    Origin::Connected,
                    port,
                    websocket_url.to_string(),
                    parsed.target_id.clone(),
                    None,
                ),
            };
        let deadline = Deadline::after(connect::CONNECT_TIMEOUT);
        let mut connection = Connection::connect(&websocket_url, deadline).map_err(|error| {
            format!("browser session is no longer reachable at `{websocket_url}`: {error}")
        })?;
        let page = attach_existing_target(
            &mut connection,
            &target_id,
            "could not attach to the page this session drives",
            deadline,
            ATTACH_ENABLE_TIMEOUT,
        )
        .map_err(|failure| failure.message)?;
        let mut tabs = tabs::Tabs::restored(&target_id, &parsed.tabs);
        tabs.enable_discovery(&mut connection, deadline)?;
        tabs.refresh(&mut connection, deadline)?;
        let network = network::NetworkState::new(&page);
        Ok(Self {
            connection: Some(connection),
            origin: Some(origin),
            tabs,
            page,
            network,
            port,
            websocket_url,
            headless: parsed.headless,
            width: parsed.width,
            height: parsed.height,
            connect: parsed.connect,
            profile: profile_name,
            pending_local_storage_scripts: parsed.pending_local_storage_scripts,
            testid_attribute: parsed.testid_attribute,
            refs: parsed.refs,
            step_metadata: None,
            snapshot_pending: false,
        })
    }

    fn dispose_handle(handle: &Value) -> Result<(), String> {
        dispose_handle_with(
            handle,
            &std::env::temp_dir(),
            record_close,
            launch::is_port_listening,
            launch::verify_launched_process_before_close,
            |websocket_url, errors| {
                let deadline = Deadline::after(Duration::from_secs(2));
                if let Ok(mut connection) = Connection::connect(websocket_url, deadline) {
                    send_browser_close(&mut connection, errors);
                }
            },
            |websocket_url, target_id, errors| {
                let deadline = Deadline::after(Duration::from_secs(2));
                if let Ok(mut connection) = Connection::connect(websocket_url, deadline) {
                    send_target_close(&mut connection, target_id, errors);
                }
            },
            launch::dispose_detached,
        )
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
        if args.profile.as_deref() != self.profile.as_deref() {
            changes.push(format!(
                "`profile` from `{}` to `{}`",
                self.profile.as_deref().unwrap_or("none"),
                args.profile.as_deref().unwrap_or("none")
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
            "`browser.open` changed {} after the browser session was created; the initial browser step sets `connect`, `profile`, `headless`, `width` and `height`, so call `browser.open` first with those options or open the page in a new case",
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
        // A preload script can only be retired after the page that runs it
        // loaded, so the origin is read only while one is pending.
        let has_pending_scripts = !self.pending_local_storage_scripts.is_empty();
        // Network events that arrive before a navigation still belong to a
        // request of this page, so they are merged before the rest is cleared.
        self.absorb_network();
        // Every tab a page action opened before this navigation keeps its place
        // in the case's tab set; the events that reported it must not be
        // cleared with the page events of the page being left.
        self.absorb_targets();
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
        let loaded_origin = if has_pending_scripts {
            page_evaluate(connection, &scope, "location.origin", deadline, dialog)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
        } else {
            None
        };
        if let Some(origin) = loaded_origin {
            self.cleanup_pending_local_storage_scripts(&origin, deadline, dialog);
        }
        // The page is what `browser.switch_tab` reports for this tab and what a
        // `url_contains` or `title_contains` filter matches, so it is recorded
        // without waiting for the browser to report the same change again.
        let current_target = self.tabs.current().to_string();
        self.tabs.set_current(&current_target, &current, &title);
        Ok((current, title))
    }

    /// Merges the target events already received into the case's tab set.
    fn absorb_targets(&mut self) {
        if let Some(connection) = self.connection.as_mut() {
            self.tabs.absorb(connection);
        }
    }

    /// Switches the session to the tab `selector` names and returns its `url`
    /// and `title`.
    fn switch_tab(
        &mut self,
        selector: &tabs::Selector,
        deadline: Deadline,
    ) -> Result<Value, StepError> {
        let index = self.await_tab(selector, deadline)?;
        let (url, title) = self.activate_tab(index, deadline)?;
        Ok(json!({ "url": url, "title": title }))
    }

    /// Waits inside `deadline` for a tab the selector names.
    ///
    /// The browser's target list is read at least once and then every
    /// [`TAB_REFRESH_ROUNDS`] rounds, because a title often changes without an
    /// event that says so; between reads the target events Chrome already sent
    /// are merged, which is what keeps the creation order.
    fn await_tab(
        &mut self,
        selector: &tabs::Selector,
        deadline: Deadline,
    ) -> Result<usize, StepError> {
        let mut round: u32 = 0;
        loop {
            if let Some(connection) = self.connection.as_mut() {
                self.tabs.absorb(connection);
            }
            if round.is_multiple_of(TAB_REFRESH_ROUNDS) && !deadline.passed() {
                let refresh = match self.connection.as_mut() {
                    Some(connection) => self
                        .tabs
                        .refresh(connection, deadline.capped(TAB_QUERY_TIMEOUT)),
                    None => return Err(Self::no_connection_error()),
                };
                if let Err(message) = refresh {
                    if deadline.passed() {
                        return Err(StepError::Execution(
                            self.tabs.timeout_message(selector, deadline.timeout_ms()),
                        ));
                    }
                    return Err(StepError::Environment(message));
                }
            }
            round += 1;
            match self.tabs.select(selector) {
                tabs::Selection::Tab(index) => return Ok(index),
                tabs::Selection::Ambiguous(candidates) => {
                    return Err(StepError::Execution(
                        self.tabs.ambiguous_message(selector, &candidates),
                    ));
                }
                tabs::Selection::None => {}
            }
            if deadline.passed() {
                return Err(StepError::Execution(
                    self.tabs.timeout_message(selector, deadline.timeout_ms()),
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Attaches the tab at `index`, waits for its document and makes it the
    /// session's page.
    fn activate_tab(
        &mut self,
        index: usize,
        deadline: Deadline,
    ) -> Result<(String, String), StepError> {
        let target_id = self
            .tabs
            .target(index)
            .ok_or_else(|| {
                StepError::Execution(format!(
                    "the tab at position {index} is no longer open; run `browser.switch_tab` again"
                ))
            })?
            .to_string();
        // Switching to the tab the session already drives changes nothing; it
        // must not detach and re-attach the same page.
        if target_id == self.tabs.current() {
            return Ok(self.tabs.info(index).unwrap_or_default());
        }
        let connection = self.connection_mut()?;
        let page = attach_existing_target(
            connection,
            &target_id,
            &format!("could not switch to the tab `{target_id}`"),
            deadline.capped(TAB_QUERY_TIMEOUT),
            ATTACH_ENABLE_TIMEOUT,
        )
        .map_err(|failure| StepError::Execution(failure.message))?;
        let tracked = self.tabs.info(index).unwrap_or_default();
        let (url, title) = self.read_ready_tab(&target_id, &page, tracked, deadline);
        let previous = std::mem::replace(&mut self.page, page);
        self.tabs.set_current(&target_id, &url, &title);
        // The observer of the tab being left keeps nothing: `browser.network`
        // reports the requests of the tab the session now drives.
        self.network = network::NetworkState::new(&self.page);
        // A snapshot `ref` names an element of the document being left, and a
        // preload script was registered on the page session being left.
        self.refs = snapshot::RefStore::default();
        self.pending_local_storage_scripts.clear();
        self.step_metadata = None;
        if let Some(connection) = self.connection.as_mut() {
            // The previous session would otherwise keep reporting network and
            // page events of a tab no step looks at, filling the message queue.
            let _ = connection.call(
                "Target.detachFromTarget",
                json!({ "sessionId": previous }),
                None,
                deadline.capped(TAB_DETACH_TIMEOUT),
            );
            // `Target.detachedFromTarget` for a tab this session left is stale
            // news; reading it now keeps it from aborting a later attach.
            let _ = connection.poll_messages();
            self.tabs.absorb(connection);
        }
        Ok((url, title))
    }

    /// Reads the URL and title of a freshly attached tab once its document is
    /// the tab's own, not the blank document it was created with.
    ///
    /// The returned page may still be loading when `deadline` runs out; the
    /// switch itself succeeded, and a later `browser.wait` or `browser.read`
    /// waits for content. What must not be returned is the `about:blank`
    /// document of a tab whose navigation only just started.
    fn read_ready_tab(
        &mut self,
        target_id: &str,
        page: &str,
        tracked: (String, String),
        deadline: Deadline,
    ) -> (String, String) {
        let scope = PageScope {
            target_id: target_id.to_string(),
            session_id: page.to_string(),
        };
        let script = "[document.readyState, location.href, document.title]";
        let blank_expected = self.tabs.is_blank(target_id);
        let mut last = tracked.clone();
        loop {
            let mut dialog = DialogContext::fail_fast();
            let read = match self.connection.as_mut() {
                Some(connection) => {
                    page_evaluate(connection, &scope, script, deadline, &mut dialog)
                }
                None => return last,
            };
            match read {
                Ok(Value::Array(values)) => {
                    let ready = values.first().and_then(Value::as_str).unwrap_or("complete");
                    let url = values
                        .get(1)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let title = values
                        .get(2)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    last = (url.clone(), title.clone());
                    if ready != "loading"
                        && (url != "about:blank" || blank_expected || deadline.passed())
                    {
                        return last;
                    }
                }
                // A navigation that commits while the session is attached
                // destroys the document the read addressed; the next read sees
                // the new one. A page that died is reported by the next step.
                Ok(_) => return last,
                Err(_) if deadline.passed() => return last,
                Err(_) => {}
            }
            if deadline.passed() {
                return last;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn cleanup_pending_local_storage_scripts(
        &mut self,
        loaded_origin: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) {
        if self.pending_local_storage_scripts.is_empty() {
            return;
        }
        let scope = self.scope();
        let mut remaining = Vec::new();
        let scripts = std::mem::take(&mut self.pending_local_storage_scripts);
        for (origin, identifier) in scripts {
            if origin == loaded_origin {
                if let Some(connection) = self.connection.as_mut() {
                    let _ = page_call(
                        connection,
                        &scope,
                        "Page.removeScriptToEvaluateOnNewDocument",
                        json!({ "identifier": identifier }),
                        deadline,
                        dialog,
                    );
                }
            } else {
                remaining.push((origin, identifier));
            }
        }
        self.pending_local_storage_scripts = remaining;
    }

    /// Applies a storage-state file's cookies and `localStorage` to this
    /// session. Called before the first navigation, so the opened page is
    /// already signed in.
    fn apply_storage_state(
        &mut self,
        state: &StorageStateFile,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<(), StepError> {
        if let Some(connect) = &self.connect {
            return Err(StepError::InvalidArguments(format!(
                "invalid arguments: `storage_state` cannot be combined with `connect` (`{connect}`); a state file writes cookies into the browser it is loaded into"
            )));
        }
        if !state.cookies.is_empty() {
            apply_cookies(self, &state.cookies, deadline, dialog)?;
        }
        if !state.local_storage.is_empty() {
            apply_local_storage(self, &state.local_storage, deadline, dialog)?;
        }
        Ok(())
    }

    /// Reads the cookies of the current page and the current origin's
    /// `localStorage` into a storage-state value.
    fn capture_storage_state(
        &mut self,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<StorageStateFile, StepError> {
        let scope = self.scope();
        let connection = self.connection_mut()?;
        let url = page_evaluate(connection, &scope, "location.href", deadline, dialog)
            .map_err(|failure| {
                failure.step(
                    "could not read the current page URL",
                    timeout_or_environment,
                )
            })?
            .as_str()
            .unwrap_or_default()
            .to_string();
        let origin = page_evaluate(connection, &scope, "location.origin", deadline, dialog)
            .map_err(|failure| {
                failure.step(
                    "could not read the current page origin",
                    timeout_or_environment,
                )
            })?
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !origin.starts_with("http://") && !origin.starts_with("https://") {
            return Err(StepError::Execution(format!(
                "`browser.save_state` needs an open http:// or https:// page; the current page is `{url}`, so open the signed-in page with `browser.open` first"
            )));
        }
        let reply = page_call(
            connection,
            &scope,
            "Network.getCookies",
            json!({ "urls": [url] }),
            deadline,
            dialog,
        )
        .map_err(|failure| {
            failure.step("could not read the browser cookies", timeout_or_environment)
        })?;
        let cookies = cookies_from_cdp(&reply, &origin);
        let storage = page_evaluate(
            connection,
            &scope,
            LOCAL_STORAGE_DUMP_BODY,
            deadline,
            dialog,
        )
        .map_err(|failure| failure.step("could not read `localStorage`", timeout_or_environment))?;
        let mut local_storage = BTreeMap::new();
        if let Some(entries) = storage.as_object() {
            let items: BTreeMap<String, String> = entries
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|text| (key.clone(), text.to_string()))
                })
                .collect();
            if !items.is_empty() {
                local_storage.insert(origin.clone(), items);
            }
        }
        if cookies.is_empty() && local_storage.is_empty() {
            return Err(StepError::Execution(format!(
                "`browser.save_state` found no cookies or `localStorage` for `{origin}`; run it after the page signs in"
            )));
        }
        Ok(StorageStateFile {
            version: STORAGE_STATE_VERSION,
            cookies,
            local_storage,
        })
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
                        VISIBLE_BODY.as_str(),
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

    /// Waits for the target element to exist, match uniquely (or `nth`), without
    /// requiring it to be painted.
    ///
    /// A file input is usually hidden behind a styled label or button, so the
    /// element the locator names must exist and be usable, not visible.
    fn locate_target_any_visibility(
        &mut self,
        target: &ElementTarget,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Located, StepError> {
        self.poll_target(
            target,
            deadline,
            dialog,
            |_session, _target_desc, object_id, _dialog| Ok(Ok(Located(object_id))),
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
                session.inspect_actionable_once(
                    target_desc,
                    object_id,
                    &mut previous_rect,
                    deadline,
                    dialog,
                )
            },
        )
    }

    /// Runs one non-blocking actionability check on a resolved `object_id`.
    fn inspect_actionable_once(
        &mut self,
        target_desc: &str,
        object_id: String,
        previous_rect: &mut Option<ElementRect>,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Result<ActionableTarget, PendingReason>, LocateError> {
        let raw = self.call_function_poll(
            target_desc,
            &object_id,
            ACTIONABLE_INSPECT_BODY.as_str(),
            "inspect the actionability of",
            deadline,
            dialog,
        )?;
        let inspect: ActionableInspect = serde_json::from_value(raw).map_err(|error| {
            LocateError::Step(StepError::Environment(format!(
                "the browser returned an invalid actionability state for {target_desc}: {error}"
            )))
        })?;
        match evaluate_actionable_inspect(previous_rect, inspect) {
            ActionablePoll::Ready(x, y) => Ok(Ok(ActionableTarget {
                located: Located(object_id),
                x,
                y,
            })),
            ActionablePoll::Pending(reason) => Ok(Err(reason)),
            ActionablePoll::UnmappableTransform(reason) => {
                Err(LocateError::Step(StepError::Execution(format!(
                    "could not map {target_desc} into viewport coordinates: {reason}"
                ))))
            }
        }
    }

    /// Resolves `ref_id` to a live element and its deterministic locator,
    /// without requiring the element to be painted.
    fn locate_ref_any_visibility(
        &mut self,
        ref_id: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Located, StepError> {
        let object_id = self.resolve_and_synthesize_ref(ref_id, deadline, dialog)?;
        Ok(Located(object_id))
    }

    /// Resolves `ref_id` to a live element, synthesizes its deterministic locator,
    /// and waits for visibility within `deadline`.
    fn locate_ref_visible(
        &mut self,
        ref_id: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Located, StepError> {
        let target_desc = format!("ref `{ref_id}`");
        let object_id = self.resolve_and_synthesize_ref(ref_id, deadline, dialog)?;
        loop {
            match self.call_function_poll(
                &target_desc,
                &object_id,
                VISIBLE_BODY.as_str(),
                "check the visibility of",
                deadline,
                dialog,
            ) {
                Ok(Value::Bool(true)) => return Ok(Located(object_id)),
                Ok(_) => {
                    if deadline.passed() {
                        return Err(pending_timeout_error(
                            &target_desc,
                            PendingReason::Hidden,
                            deadline,
                        ));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(LocateError::Timeout) => {
                    return Err(pending_timeout_error(
                        &target_desc,
                        PendingReason::Hidden,
                        deadline,
                    ));
                }
                Err(LocateError::Step(err)) => return Err(err),
            }
        }
    }

    /// Resolves `ref_id` to a live element, synthesizes its deterministic locator,
    /// and waits for full actionability within `deadline`.
    fn locate_ref_actionable(
        &mut self,
        ref_id: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<ActionableTarget, StepError> {
        let target_desc = format!("ref `{ref_id}`");
        let object_id = self.resolve_and_synthesize_ref(ref_id, deadline, dialog)?;
        let mut previous_rect: Option<ElementRect> = None;
        let mut last_reason = PendingReason::Hidden;
        loop {
            match self.inspect_actionable_once(
                &target_desc,
                object_id.clone(),
                &mut previous_rect,
                deadline,
                dialog,
            ) {
                Ok(Ok(ready)) => return Ok(ready),
                Ok(Err(reason)) => {
                    last_reason = reason;
                    if deadline.passed() {
                        return Err(pending_timeout_error(&target_desc, last_reason, deadline));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(LocateError::Timeout) => {
                    return Err(pending_timeout_error(&target_desc, last_reason, deadline));
                }
                Err(LocateError::Step(err)) => return Err(err),
            }
        }
    }

    fn resolve_and_synthesize_ref(
        &mut self,
        ref_id: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<String, StepError> {
        let scope = self.scope();
        let refs = self.refs.clone();
        let (object_id, _) = {
            let connection = self.connection_mut()?;
            snapshot::resolve_ref_object_id(connection, &scope, &refs, ref_id, deadline, dialog)?
        };
        let synth = synthesis::synthesize_for_object(self, &object_id, deadline, dialog)?;
        self.step_metadata = Some(json!({
            "ref": ref_id,
            "synthesized_locator": synth.locator,
            "fragile": synth.fragile,
        }));
        Ok(synth.object_id)
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
            // Every terminal return names the same pending reason, including the
            // timeout that ends the last poll, so a closed shadow root is
            // explained however the wait happened to expire.
            let poll = match self.resolve_target_once(target, deadline, dialog) {
                Ok(poll) => poll,
                Err(LocateError::Timeout) => {
                    return Err(self.pending_error(
                        &last_pending.0,
                        &last_pending.1,
                        deadline,
                        dialog,
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
                        return Err(self.pending_error(
                            &last_pending.0,
                            &last_pending.1,
                            deadline,
                            dialog,
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
                return Err(self.pending_error(&last_pending.0, &last_pending.1, deadline, dialog));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Builds the terminal timeout error, naming a closed shadow root when that
    /// is why a locator could not reach the element.
    fn pending_error(
        &mut self,
        target_desc: &str,
        reason: &PendingReason,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> StepError {
        let error = pending_timeout_error(target_desc, reason.clone(), deadline);
        if !matches!(reason, PendingReason::Missing | PendingReason::Occluded(_))
            || !self.page_has_closed_shadow_root(dialog)
        {
            return error;
        }
        match error {
            StepError::Execution(message) => {
                StepError::Execution(format!("{message}{CLOSED_SHADOW_ROOT_HINT}"))
            }
            other => other,
        }
    }

    /// Whether the page holds a closed shadow root, which only DevTools can see.
    fn page_has_closed_shadow_root(&mut self, dialog: &mut DialogContext) -> bool {
        let scope = self.scope();
        let deadline = Deadline::after(CLOSED_SHADOW_PROBE_TIMEOUT);
        let Some(connection) = self.connection.as_mut() else {
            return false;
        };
        let document = page_call(
            connection,
            &scope,
            "DOM.getDocument",
            json!({ "depth": -1, "pierce": true }),
            deadline,
            dialog,
        );
        match document {
            Ok(value) => value
                .get("root")
                .is_some_and(nodes_include_closed_shadow_root),
            Err(_) => false,
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
        let testid_attribute = self.testid_attribute.clone();
        let root = match &target.within {
            Some(container) => match self.resolve_target_once(container, deadline, dialog)? {
                ResolvePoll::Found(container_id) => {
                    let container_desc = container.describe_with(&testid_attribute);
                    match self.enter_within_container(
                        &container_desc,
                        container_id,
                        deadline,
                        dialog,
                    )? {
                        ResolvePoll::Found(entered_id) => Some(entered_id),
                        pending @ ResolvePoll::Pending { .. } => return Ok(pending),
                    }
                }
                pending @ ResolvePoll::Pending { .. } => return Ok(pending),
            },
            None => None,
        };
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

    /// Resolves a matched `within` container into the DOM node whose subtree the
    /// inner locator searches: the element itself, or `contentDocument` when the
    /// element is an `<iframe>` or `<frame>`.
    fn enter_within_container(
        &mut self,
        container_desc: &str,
        object_id: String,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<ResolvePoll, LocateError> {
        let scope = self.scope();
        let connection = self.connection_mut().map_err(LocateError::Step)?;
        let remote = page_call_function_remote(
            connection,
            &scope,
            &object_id,
            ENTER_CONTAINER_SCRIPT,
            deadline,
            dialog,
        )
        .map_err(|failure| locate_error(container_desc, failure))?;

        match remote {
            RemoteResult::Object(root_id) => Ok(ResolvePoll::Found(root_id)),
            RemoteResult::Value(Value::String(raw_json)) => {
                let status: WithinContainerStatus = serde_json::from_str(&raw_json).map_err(|error| {
                    LocateError::Step(StepError::Environment(format!(
                        "the browser returned an invalid container status for {container_desc}: {error}"
                    )))
                })?;
                match status {
                    WithinContainerStatus::Missing => Ok(ResolvePoll::Pending {
                        target_desc: container_desc.to_string(),
                        reason: PendingReason::Missing,
                    }),
                    WithinContainerStatus::Loading => Ok(ResolvePoll::Pending {
                        target_desc: container_desc.to_string(),
                        reason: PendingReason::FrameLoading,
                    }),
                    WithinContainerStatus::CrossOrigin => {
                        Err(LocateError::Step(StepError::Execution(format!(
                            "cannot enter {container_desc}: cross-origin iframes are not supported; `within` only supports same-origin iframes"
                        ))))
                    }
                }
            }
            RemoteResult::Value(other) => Err(LocateError::Step(StepError::Environment(format!(
                "the browser returned an unexpected value while entering {container_desc}: {other}"
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
                VISIBLE_BODY.as_str(),
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

        let root_context = match root {
            Some(root_id) => {
                match self.resolve_role_root_context(target_desc, root_id, deadline, dialog)? {
                    Some(context) => Some(context),
                    None => return Ok((false, Vec::new())),
                }
            }
            None => None,
        };

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
            if let Some(object_id) = resolved.pointer("/object/objectId").and_then(Value::as_str)
                && self.node_matches_role_root(
                    target_desc,
                    object_id,
                    root_context.as_ref(),
                    deadline,
                    dialog,
                )?
            {
                matched = true;
                objects.push(object_id.to_string());
            }
        }
        Ok((matched, objects))
    }

    /// Resolves a `role` query's `within` root into its frame-index path and an
    /// `objectId` in that frame's own JavaScript world so the composed
    /// containment check runs in the same context as nodes returned by
    /// `DOM.resolveNode`.
    fn resolve_role_root_context(
        &mut self,
        target_desc: &str,
        root_id: &str,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<Option<(Vec<Value>, String)>, LocateError> {
        let scope = self.scope();
        let connection = self.connection_mut().map_err(LocateError::Step)?;
        let path = page_call_function(
            connection,
            &scope,
            root_id,
            FRAME_PATH_BODY,
            deadline,
            dialog,
        )
        .map_err(|failure| locate_error(target_desc, failure))?;
        let Some(path_array) = path.as_array().cloned() else {
            return Ok(None);
        };
        if path_array.is_empty() {
            return Ok(Some((path_array, root_id.to_string())));
        }
        let described = page_call(
            connection,
            &scope,
            "DOM.describeNode",
            json!({ "objectId": root_id, "depth": 0 }),
            deadline,
            dialog,
        )
        .map_err(|failure| locate_error(target_desc, failure))?;
        let Some(root_backend_id) = described
            .pointer("/node/backendNodeId")
            .and_then(Value::as_i64)
        else {
            return Ok(None);
        };
        let resolved_root = page_call(
            connection,
            &scope,
            "DOM.resolveNode",
            json!({ "backendNodeId": root_backend_id }),
            deadline,
            dialog,
        )
        .map_err(|failure| locate_error(target_desc, failure))?;
        Ok(resolved_root
            .pointer("/object/objectId")
            .and_then(Value::as_str)
            .map(|own_id| (path_array, own_id.to_string())))
    }

    /// Checks that an AX node belongs to the top-level document (when unscoped)
    /// or to the exact frame and composed subtree of `root_context` (when
    /// `within` is set), so a node inside the container's open shadow root
    /// counts as inside it.
    fn node_matches_role_root(
        &mut self,
        target_desc: &str,
        object_id: &str,
        root_context: Option<&(Vec<Value>, String)>,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<bool, LocateError> {
        let scope = self.scope();
        let inside = match root_context {
            Some((expected_path, root_id)) => {
                let connection = self.connection_mut().map_err(LocateError::Step)?;
                let el_path = page_call_function(
                    connection,
                    &scope,
                    object_id,
                    FRAME_PATH_BODY,
                    deadline,
                    dialog,
                )
                .map_err(|failure| locate_error(target_desc, failure))?;
                if el_path.as_array() != Some(expected_path) {
                    return Ok(false);
                }
                let connection = self.connection_mut().map_err(LocateError::Step)?;
                let contains_script = format!(
                    "function (el) {{ {COMPOSED_HELPERS_JS} return Boolean(el) && this !== el && composedContains(this, el); }}"
                );
                page_call_function_with_object(
                    connection,
                    &scope,
                    root_id,
                    object_id,
                    &contains_script,
                    deadline,
                    dialog,
                )
                .map_err(|failure| locate_error(target_desc, failure))?
            }
            None => {
                let connection = self.connection_mut().map_err(LocateError::Step)?;
                page_call_function(
                    connection,
                    &scope,
                    object_id,
                    "function () { return window === window.top && this.ownerDocument === document; }",
                    deadline,
                    dialog,
                )
                .map_err(|failure| locate_error(target_desc, failure))?
            }
        };
        Ok(inside == Value::Bool(true))
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

    /// Reads the CSS viewport size explicit coordinates are measured in.
    fn viewport_size(
        &mut self,
        deadline: Deadline,
        dialog: &mut DialogContext,
    ) -> Result<(u32, u32), StepError> {
        let scope = self.scope();
        let value = page_evaluate(
            self.connection_mut()?,
            &scope,
            "({ width: window.innerWidth, height: window.innerHeight })",
            deadline,
            dialog,
        )
        .map_err(|failure| {
            failure.step("could not read the page viewport", timeout_or_execution)
        })?;
        let width = viewport_dimension(&value, "width");
        let height = viewport_dimension(&value, "height");
        match (width, height) {
            (Some(width), Some(height)) => Ok((width, height)),
            _ => Err(StepError::Environment(
                "the page reported no measurable viewport; wait for the page to finish loading and try again"
                    .to_string(),
            )),
        }
    }

    /// The open connection, or the error a step reports after a close.
    fn connection_mut(&mut self) -> Result<&mut Connection, StepError> {
        self.connection
            .as_mut()
            .ok_or_else(Self::no_connection_error)
    }

    /// The error a step reports once the session's browser is gone.
    fn no_connection_error() -> StepError {
        StepError::Execution(
            "the browser session is already closed; run `browser.open` again".to_string(),
        )
    }

    /// The target and session every page-session wait watches.
    fn scope(&self) -> PageScope {
        PageScope {
            target_id: self.tabs.current().to_string(),
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
        let mask_result = page_evaluate(
            self.connection_mut()?,
            &scope,
            MASK_INPUTS_SCRIPT.as_str(),
            deadline,
            &mut dialog,
        )
        .map_err(|failure| {
            failure.environment("could not cover the editable fields before the screenshot")
        })?;
        if mask_result.get("ok").and_then(Value::as_bool) != Some(true) {
            let reason = mask_result
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("enclosing frame geometry could not be mapped");
            return Err(StepError::Execution(format!(
                "could not cover editable fields before the screenshot: {reason}"
            )));
        }

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
            REMOVE_MASKS_SCRIPT.as_str(),
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
/// Per-attempt budget for `Target.createTarget`, `Target.attachToTarget`, and
/// domain setup (`Page.enable` / `Runtime.enable` / `Network.enable`).
///
/// Healthy targets answer in tens of milliseconds; capping each step at 5 s
/// lets a silent renderer or startup attach hang abort and retry within the 30 s
/// `LAUNCH_TIMEOUT`.
const ATTACH_ENABLE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `attach` waits for `Target.closeTarget` when cleaning up a failed fresh tab.
const ATTACH_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// Longest one `Target.getTargets` or `Target.setDiscoverTargets` may take.
///
/// They only list or subscribe; a browser that cannot answer them in this
/// budget is failing, and the wait for a tab must not spend its whole budget
/// on one listing.
const TAB_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long detaching the previous tab's page session may take.
const TAB_DETACH_TIMEOUT: Duration = Duration::from_secs(1);
/// How many polling rounds pass before `browser.switch_tab` re-reads the
/// browser's target list, so a title that never arrives as an event is still
/// seen.
const TAB_REFRESH_ROUNDS: u32 = 5;

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
                let mut attempt_error = failure.message;
                if let Some(target_id) = failure.target_id.as_deref()
                    && !failure.target_destroyed
                    && effective_kind == AttachFailureKind::TargetFailed
                    && !deadline.passed()
                    && attempt < MAX_ATTACH_ATTEMPTS
                {
                    if let Err(close_error) = connection.call(
                        "Target.closeTarget",
                        json!({ "targetId": target_id }),
                        None,
                        deadline.capped(ATTACH_CLOSE_TIMEOUT),
                    ) {
                        match close_error {
                            // The target may have closed itself right after failing.
                            CdpError::Method { message, .. } if message.contains("No target") => {}
                            other => {
                                effective_kind = AttachFailureKind::BrowserFailed;
                                attempt_error = format!(
                                    "{attempt_error}; cleanup failed (`Target.closeTarget` on `{target_id}`): {other}"
                                );
                            }
                        }
                    }
                    let _ = connection.take_messages();
                }
                attempt_errors.push(attempt_error);
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

/// Attempts to launch and attach a fresh browser up to `max_attempts` times
/// inside `deadline`.
///
/// Failures before a browser process reports its debug port (such as a missing
/// binary) fail immediately; post-startup connect/attach failures immediately
/// abort the wedged process and retry while `deadline` has budget left, or stop
/// and report both errors if abort/cleanup itself fails.
fn launch_browser_with_retry<B, C>(
    deadline: Deadline,
    max_attempts: usize,
    mut spawn_browser: impl FnMut(Deadline) -> Result<B, String>,
    mut connect_and_attach: impl FnMut(&B, Deadline) -> Result<C, String>,
    mut abort_browser: impl FnMut(B) -> Result<(), String>,
) -> Result<(B, C), String>
where
    C: Sized,
{
    let mut errors = Vec::with_capacity(max_attempts);
    for attempt in 1..=max_attempts {
        let browser = spawn_browser(deadline)?;
        match connect_and_attach(&browser, deadline) {
            Ok(attached) => return Ok((browser, attached)),
            Err(error) => {
                if let Err(cleanup_error) = abort_browser(browser) {
                    errors.push(format!("{error}; browser cleanup failed: {cleanup_error}"));
                    break;
                }
                errors.push(error);
                if attempt >= max_attempts || deadline.passed() {
                    break;
                }
            }
        }
    }
    if errors.len() == 1 {
        return Err(errors.pop().unwrap_or_default());
    }
    let summary = errors
        .into_iter()
        .enumerate()
        .map(|(index, error)| format!("launch attempt {}: {error}", index + 1))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "could not attach to a freshly started browser: {summary}"
    ))
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
        deadline.capped(enable_timeout),
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
    let page = attach_existing_target(
        connection,
        &target_id,
        "could not attach to the page the browser opened",
        deadline,
        enable_timeout,
    )?;
    Ok((page, target_id))
}

/// Attaches `target_id` and enables the domains every page step needs.
///
/// `action` starts the messages of a failed attach, so the error names what the
/// step was doing.
fn attach_existing_target(
    connection: &mut Connection,
    target_id: &str,
    action: &str,
    deadline: Deadline,
    enable_timeout: Duration,
) -> Result<String, AttachAttemptFailure> {
    let attached = attach_call(
        connection,
        AttachStep::AttachToTarget,
        action,
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
        AttachScope {
            target_id: Some(target_id),
            session_id: None,
        },
        deadline.capped(enable_timeout),
    )?;
    let page = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| AttachAttemptFailure {
            target_id: Some(target_id.to_string()),
            kind: AttachFailureKind::TargetFailed,
            message: "the browser did not return a page session id".to_string(),
            target_destroyed: false,
        })?
        .to_string();
    let domain_deadline = deadline.capped(enable_timeout);
    let page_scope = AttachScope {
        target_id: Some(target_id),
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
    Ok(page)
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
#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingReason {
    Missing,
    Hidden,
    FrameLoading,
    OutOfRange { index: usize, count: usize },
    Disabled,
    Unstable,
    Occluded(Option<CandidateInfo>),
}

/// Outcome of evaluating one actionability snapshot against the prior poll's geometry.
#[derive(Debug, PartialEq)]
enum ActionablePoll {
    Ready(f64, f64),
    Pending(PendingReason),
    UnmappableTransform(String),
}

/// Evaluates one actionability snapshot against the prior poll's geometry.
fn evaluate_actionable_inspect(
    previous_rect: &mut Option<ElementRect>,
    inspect: ActionableInspect,
) -> ActionablePoll {
    match inspect {
        ActionableInspect::NotVisible => {
            *previous_rect = None;
            ActionablePoll::Pending(PendingReason::Hidden)
        }
        ActionableInspect::Disabled => {
            *previous_rect = None;
            ActionablePoll::Pending(PendingReason::Disabled)
        }
        ActionableInspect::UnmappableTransform { reason } => {
            *previous_rect = None;
            ActionablePoll::UnmappableTransform(reason)
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
                return ActionablePoll::Pending(PendingReason::Occluded(blocker));
            }
            if !stable {
                return ActionablePoll::Pending(PendingReason::Unstable);
            }
            ActionablePoll::Ready(x, y)
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
        PendingReason::FrameLoading => StepError::Execution(format!(
            "frame {target_desc} did not finish loading within {ms} ms"
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

/// The sentence a terminal lookup failure adds when the page holds a closed
/// shadow root, which no page script can look into.
const CLOSED_SHADOW_ROOT_HINT: &str = "; the page contains a closed shadow root, which `da` cannot pierce: open it with `attachShadow({ mode: 'open' })` or select its host";

/// How long proving a closed shadow root may take on a terminal failure path.
const CLOSED_SHADOW_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Whether a `DOM.getDocument` node tree holds a closed shadow root.
///
/// `pierce: true` reports every shadow root of a host under `shadowRoots` with
/// its `shadowRootType`, which is the only way to see a closed root: JavaScript
/// reports `element.shadowRoot === null` for it.
fn nodes_include_closed_shadow_root(node: &Value) -> bool {
    if node.get("shadowRootType").and_then(Value::as_str) == Some("closed") {
        return true;
    }
    if ["shadowRoots", "children"]
        .iter()
        .filter_map(|key| node.get(*key).and_then(Value::as_array))
        .flatten()
        .any(nodes_include_closed_shadow_root)
    {
        return true;
    }
    ["contentDocument", "templateContent"]
        .iter()
        .filter_map(|key| node.get(*key))
        .any(nodes_include_closed_shadow_root)
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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct DetachedBrowserHandle {
    mode: String,
    websocket_url: String,
    target_id: String,
    port: u16,
    pid: Option<u32>,
    profile: Option<String>,
    #[serde(default)]
    profile_name: Option<String>,
    headless: bool,
    width: u32,
    height: u32,
    connect: Option<String>,
    testid_attribute: String,
    #[serde(default)]
    refs: snapshot::RefStore,
    #[serde(default)]
    pending_local_storage_scripts: Vec<(String, String)>,
    /// Every tab this case owns, in creation order; the first entry is the tab
    /// `browser.open` opened. A handle written before tabs were tracked lists
    /// only `target_id`.
    #[serde(default)]
    tabs: Vec<String>,
}

/// The tabs a handle lists, as one entry per owned target.
///
/// The current target must be one of them: a handle that names it out of the
/// set would detach a session whose tab teardown never closes.
fn managed_tabs(parsed: &DetachedBrowserHandle) -> Result<Vec<&str>, String> {
    if parsed.tabs.is_empty() {
        return Ok(vec![parsed.target_id.as_str()]);
    }
    if parsed.tabs.len() > tabs::MAX_TABS {
        return Err(format!(
            "browser session handle lists {} tabs; at most {} are managed",
            parsed.tabs.len(),
            tabs::MAX_TABS
        ));
    }
    for target_id in &parsed.tabs {
        if target_id.trim().is_empty() {
            return Err("browser session handle has an empty `tabs` entry".to_string());
        }
    }
    if !parsed.tabs.iter().any(|id| id == &parsed.target_id) {
        return Err(
            "browser session handle `target_id` is not one of its managed `tabs`".to_string(),
        );
    }
    Ok(parsed.tabs.iter().map(String::as_str).collect())
}

#[derive(Debug, PartialEq, Eq)]
enum ValidatedHandle<'a> {
    Launched {
        pid: u32,
        profile: &'a Path,
        profile_name: Option<&'a str>,
        port: u16,
        websocket_url: &'a str,
        target_id: &'a str,
    },
    Connected {
        port: u16,
        websocket_url: &'a str,
        /// Every tab the case owns, so teardown closes exactly those.
        target_ids: Vec<&'a str>,
    },
}

fn validate_detached_handle<'a>(
    parsed: &'a DetachedBrowserHandle,
    temp_root: &Path,
) -> Result<ValidatedHandle<'a>, String> {
    if parsed.port == 0 {
        return Err("browser session handle has invalid port 0".to_string());
    }
    if parsed.target_id.trim().is_empty() {
        return Err("browser session handle has empty `target_id`".to_string());
    }
    match parsed.mode.as_str() {
        "launched" => {
            if parsed.connect.is_some() {
                return Err(
                    "launched browser session handle must not specify `connect`".to_string()
                );
            }
            let pid = parsed
                .pid
                .ok_or_else(|| "launched browser session handle is missing `pid`".to_string())?;
            if pid == 0 {
                return Err("launched browser session handle has invalid pid 0".to_string());
            }
            let profile_raw = parsed.profile.as_deref().ok_or_else(|| {
                "launched browser session handle is missing `profile`".to_string()
            })?;
            let profile = Path::new(profile_raw);
            if let Some(name) = parsed.profile_name.as_deref() {
                launch::validate_named_profile_path(profile, name)?;
            } else {
                launch::validate_owned_profile_in(profile, temp_root)?;
            }
            validate_launched_ws_url(&parsed.websocket_url, parsed.port)?;
            // The tab list is checked for both modes: a launched handle also
            // drives tabs through `da session`, even though stopping it closes
            // the whole browser.
            managed_tabs(parsed)?;
            Ok(ValidatedHandle::Launched {
                pid,
                profile,
                profile_name: parsed.profile_name.as_deref(),
                port: parsed.port,
                websocket_url: &parsed.websocket_url,
                target_id: &parsed.target_id,
            })
        }
        "connected" => {
            if parsed.pid.is_some() || parsed.profile.is_some() || parsed.profile_name.is_some() {
                return Err(
                    "connected browser session handle must not specify `pid` or `profile`"
                        .to_string(),
                );
            }
            let connect = parsed.connect.as_deref().ok_or_else(|| {
                "connected browser session handle is missing `connect`".to_string()
            })?;
            let endpoint = Endpoint::parse(connect)?;
            if endpoint.port() != parsed.port {
                return Err(format!(
                    "connected browser session handle `port` ({}) does not match `connect` port ({})",
                    parsed.port,
                    endpoint.port()
                ));
            }
            validate_connected_ws_url(&parsed.websocket_url)?;
            Ok(ValidatedHandle::Connected {
                port: parsed.port,
                websocket_url: &parsed.websocket_url,
                target_ids: managed_tabs(parsed)?,
            })
        }
        other => Err(format!("unknown browser session mode `{other}`")),
    }
}

fn validate_launched_ws_url(url: &str, port: u16) -> Result<(), String> {
    if url.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(format!(
            "launched browser session handle has invalid `websocket_url` `{url}`"
        ));
    }
    let expected_prefix = format!("ws://127.0.0.1:{port}/");
    if !url.starts_with(&expected_prefix) || url.len() == expected_prefix.len() {
        return Err(format!(
            "launched browser session handle `websocket_url` `{url}` does not match loopback port {port}"
        ));
    }
    Ok(())
}

fn validate_connected_ws_url(url: &str) -> Result<(), String> {
    if url.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(format!(
            "connected browser session handle has invalid `websocket_url` `{url}`"
        ));
    }
    let Some(rest) = url.strip_prefix("ws://") else {
        return Err(format!(
            "connected browser session handle `websocket_url` `{url}` must use `ws://`"
        ));
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return Err(format!(
            "connected browser session handle `websocket_url` `{url}` is missing a path"
        ));
    };
    if path.is_empty() {
        return Err(format!(
            "connected browser session handle `websocket_url` `{url}` is missing a path"
        ));
    }
    let Some((host, port_str)) = authority.rsplit_once(':') else {
        return Err(format!(
            "connected browser session handle `websocket_url` `{url}` is missing a port"
        ));
    };
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(format!(
            "connected browser session handle `websocket_url` `{url}` must use a loopback host"
        ));
    }
    if !port_str.parse::<u16>().is_ok_and(|port| port > 0) {
        return Err(format!(
            "connected browser session handle `websocket_url` `{url}` has an invalid port"
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "internal callback seam for verifying handle disposal without real sockets or disk"
)]
fn dispose_handle_with<R, P, V, CB, CT, D>(
    handle: &Value,
    temp_root: &Path,
    mut on_record_close: R,
    port_listening: P,
    verify_before_close: V,
    mut close_browser: CB,
    mut close_target: CT,
    dispose_detached_fn: D,
) -> Result<(), String>
where
    R: FnMut(u16),
    P: FnMut(u16) -> bool,
    V: FnMut(u32, u16, &Path) -> Result<bool, String>,
    CB: FnMut(&str, &mut Vec<String>),
    CT: FnMut(&str, &str, &mut Vec<String>),
    D: FnMut(Option<u32>, Option<u16>, Option<&Path>) -> Result<(), String>,
{
    let parsed: DetachedBrowserHandle = serde_json::from_value(handle.clone())
        .map_err(|error| format!("invalid browser session handle: {error}"))?;
    let validated = validate_detached_handle(&parsed, temp_root)?;

    match validated {
        ValidatedHandle::Connected {
            port,
            websocket_url,
            target_ids,
        } => {
            on_record_close(port);
            let mut errors = Vec::new();
            for target_id in target_ids {
                close_target(websocket_url, target_id, &mut errors);
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        }
        ValidatedHandle::Launched {
            pid,
            profile,
            profile_name,
            port,
            websocket_url,
            ..
        } => {
            on_record_close(port);
            let mut errors = Vec::new();
            if let Some(name) = profile_name {
                close_and_dispose_detached_with(
                    pid,
                    port,
                    profile,
                    &mut errors,
                    port_listening,
                    |p, port, prof| launch::verify_named_process_before_close(p, port, prof, name),
                    |errs| close_browser(websocket_url, errs),
                    |p, port, prof| launch::dispose_detached_named(p, port, prof, name),
                );
            } else {
                close_and_dispose_detached_with(
                    pid,
                    port,
                    profile,
                    &mut errors,
                    port_listening,
                    verify_before_close,
                    |errs| close_browser(websocket_url, errs),
                    dispose_detached_fn,
                );
            }
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "shared teardown helper used by both dispose_handle_with and BrowserSession::close"
)]
fn close_and_dispose_detached_with<P, V, CB, D>(
    pid: u32,
    port: u16,
    profile: &Path,
    errors: &mut Vec<String>,
    mut port_listening: P,
    mut verify_before_close: V,
    mut close_browser: CB,
    mut dispose_detached_fn: D,
) where
    P: FnMut(u16) -> bool,
    V: FnMut(u32, u16, &Path) -> Result<bool, String>,
    CB: FnMut(&mut Vec<String>),
    D: FnMut(Option<u32>, Option<u16>, Option<&Path>) -> Result<(), String>,
{
    if port_listening(port) {
        match verify_before_close(pid, port, profile) {
            Ok(true) => close_browser(errors),
            Ok(false) => {}
            Err(error) => {
                errors.push(error);
                return;
            }
        }
    }
    if let Err(error) = dispose_detached_fn(Some(pid), Some(port), Some(profile)) {
        errors.push(error);
    }
}

impl Session for BrowserSession {
    fn detach(&mut self) -> Result<Option<Value>, String> {
        // A tab a page action opened after the last step is still reported by
        // the browser; it must be in the handle, or the next command neither
        // switches to it nor closes it.
        if let Some(connection) = self.connection.as_mut() {
            let _ = connection.poll_messages();
            self.tabs.absorb(connection);
        }
        let _ = self.connection.take();
        let (mode, pid, profile, profile_name, port, websocket_url) = match self.origin.take() {
            Some(Origin::Launched(browser)) => {
                let detached = browser
                    .detach()
                    .ok_or_else(|| "the browser process handle was already released".to_string())?;
                (
                    "launched".to_string(),
                    Some(detached.pid),
                    Some(detached.profile.to_string_lossy().into_owned()),
                    detached.profile_name,
                    detached.port,
                    detached.websocket_url,
                )
            }
            Some(Origin::Detached { pid, profile }) => (
                "launched".to_string(),
                Some(pid),
                Some(profile.to_string_lossy().into_owned()),
                self.profile.clone(),
                self.port,
                self.websocket_url.clone(),
            ),
            Some(Origin::Connected) => (
                "connected".to_string(),
                None,
                None,
                None,
                self.port,
                self.websocket_url.clone(),
            ),
            None => {
                return Err("the browser session is already closed".to_string());
            }
        };

        let handle = DetachedBrowserHandle {
            mode,
            websocket_url,
            target_id: self.tabs.current().to_string(),
            port,
            pid,
            profile,
            profile_name,
            headless: self.headless,
            width: self.width,
            height: self.height,
            connect: self.connect.clone(),
            testid_attribute: self.testid_attribute.clone(),
            refs: self.refs.clone(),
            pending_local_storage_scripts: self.pending_local_storage_scripts.clone(),
            tabs: self.tabs.ids(),
        };
        serde_json::to_value(handle)
            .map(Some)
            .map_err(|error| format!("could not serialize browser session handle: {error}"))
    }

    fn local_only_reason(&self) -> Option<String> {
        if self.connect.is_some() {
            Some(
                "browser session connected to an external browser whose state is not managed by `da`"
                    .to_string(),
            )
        } else {
            self.profile.as_ref().map(|profile| {
                format!(
                    "browser session used persistent profile `{profile}` whose stored state is not managed by `da`"
                )
            })
        }
    }

    fn snapshot(
        &mut self,
        scope_ref: Option<&str>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
    ) -> Result<Option<Value>, String> {
        let scope = self.scope();
        let existing_refs = self.refs.clone();
        let deadline = Deadline::after(SCREENSHOT_TIMEOUT);
        let mut dialog = DialogContext::fail_fast();
        let connection = self.connection_mut().map_err(step_error_text)?;
        let (snap, new_refs) = snapshot::capture_snapshot(
            connection,
            &scope,
            &existing_refs,
            scope_ref,
            max_depth,
            max_nodes,
            deadline,
            &mut dialog,
        )?;
        self.refs = new_refs;
        Ok(Some(snap))
    }

    fn snapshot_after_step(&mut self) -> Result<Option<Value>, String> {
        if !self.snapshot_pending {
            return Ok(None);
        }
        self.snapshot_pending = false;
        self.snapshot(None, None, None)
    }

    fn take_step_metadata(&mut self) -> Option<Value> {
        self.step_metadata.take()
    }

    fn step_evidence(&mut self) -> Result<Vec<Evidence>, String> {
        if self.connection.is_none() {
            return Ok(Vec::new());
        }
        self.evidence()
    }

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
                send_browser_close(&mut connection, &mut errors);
                drop(connection);
                if let Err(error) = browser.shutdown() {
                    errors.push(error);
                }
            }
            Some(Origin::Detached { pid, profile }) => {
                let mut connection = Some(connection);
                if let Some(name) = self.profile.as_deref() {
                    close_and_dispose_detached_with(
                        pid,
                        self.port,
                        &profile,
                        &mut errors,
                        launch::is_port_listening,
                        |p, port, prof| {
                            launch::verify_named_process_before_close(p, port, prof, name)
                        },
                        |errs| {
                            if let Some(mut conn) = connection.take() {
                                send_browser_close(&mut conn, errs);
                            }
                        },
                        |p, port, prof| launch::dispose_detached_named(p, port, prof, name),
                    );
                } else {
                    close_and_dispose_detached_with(
                        pid,
                        self.port,
                        &profile,
                        &mut errors,
                        launch::is_port_listening,
                        launch::verify_launched_process_before_close,
                        |errs| {
                            if let Some(mut conn) = connection.take() {
                                send_browser_close(&mut conn, errs);
                            }
                        },
                        launch::dispose_detached,
                    );
                }
            }
            Some(Origin::Connected) => {
                // Only the tabs this case owns are ours: the one `browser.open`
                // created and the ones its pages opened. `Browser.close` is
                // never sent: the browser belongs to the user, whose own tabs
                // are not in the set and must keep running.
                let _ = connection.poll_messages();
                self.tabs.absorb(&mut connection);
                for target_id in self.tabs.ids() {
                    send_target_close(&mut connection, &target_id, &mut errors);
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

fn send_browser_close(connection: &mut Connection, errors: &mut Vec<String>) {
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
}

fn send_target_close(connection: &mut Connection, target_id: &str, errors: &mut Vec<String>) {
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

/// Rejects explicit `null` values in step arguments (including nested objects
/// and arrays), because `serde` otherwise maps `null` into `Option::None` while
/// JSON Schema refuses it.
fn reject_null_fields(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(entries) => {
            for (key, field) in entries {
                if field.is_null() {
                    return Err(format!("invalid arguments: `{key}` must not be null"));
                }
                reject_null_fields(field)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.is_null() {
                    return Err("invalid arguments: array elements must not be null".to_string());
                }
                reject_null_fields(item)?;
            }
        }
        _ => {}
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
    if raw.get("storage_state").is_some_and(Value::is_null) {
        return Err(
            "invalid arguments: `storage_state` must be a file path like `state.json`".to_string(),
        );
    }
    if let Some(path) = &args.storage_state
        && path.trim().is_empty()
    {
        return Err(
            "invalid arguments: `storage_state` must name the JSON file `browser.save_state` wrote"
                .to_string(),
        );
    }
    if let Some(connect) = &args.connect {
        if args.profile.is_some() {
            return Err(
                "invalid arguments: `profile` and `connect` cannot both be set; remove one"
                    .to_string(),
            );
        }
        if args.storage_state.is_some() {
            return Err(
                "invalid arguments: `storage_state` and `connect` cannot both be set; a state file writes cookies into your everyday browser, so load it into a browser `da` starts instead"
                    .to_string(),
            );
        }
        for field in ["headless", "width", "height"] {
            if raw.get(field).is_some() {
                return Err(format!(
                    "invalid arguments: `connect` cannot be combined with `{field}`; the connected browser keeps its own window"
                ));
            }
        }
        Endpoint::parse(connect)?;
    }
    if let Some(profile) = &args.profile {
        launch::validate_profile_name(profile)?;
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
    if args.at.is_some() {
        return check_click_at(&args);
    }
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

/// The `at` checks of `browser.click`, shared by `da lint` and execution.
fn check_click_at(args: &ClickArgs) -> Result<(), String> {
    let Some(at) = &args.at else {
        return Ok(());
    };
    check_at(
        at,
        &[
            ("css", args.css.is_some()),
            ("xpath", args.xpath.is_some()),
            ("text", args.text.is_some()),
            ("role", args.role.is_some()),
            ("testid", args.testid.is_some()),
            ("name", args.name.is_some()),
            ("within", args.within.is_some()),
            ("nth", args.nth.is_some()),
        ],
    )
}

/// Checks one explicit coordinate form.
///
/// Coordinates are a locator of their own, so any other locator or element
/// modifier would name a second target. Both coordinates must be able to
/// address a pixel inside a positive-sized viewport.
fn check_at(at: &AtArgs, combined: &[(&str, bool)]) -> Result<(), String> {
    if let Some((field, _)) = combined.iter().find(|(_, present)| *present) {
        return Err(format!(
            "invalid arguments: `at` cannot be combined with `{field}`; give explicit coordinates or a locator, not both"
        ));
    }
    for (name, value) in [("x", at.x), ("y", at.y)] {
        if !value.is_finite() || value < 0.0 {
            return Err(format!(
                "invalid arguments: `at.{name}` must be a finite number that is not negative"
            ));
        }
    }
    if let Some(viewport) = &at.viewport
        && (viewport.width == 0 || viewport.height == 0)
    {
        return Err(
            "invalid arguments: `at.viewport.width` and `at.viewport.height` must be at least 1"
                .to_string(),
        );
    }
    Ok(())
}

/// Warns that explicit coordinates are a fragile way to name a target.
///
/// Coordinates are not refused: `<canvas>`, WebGL and map controls have no DOM
/// node or accessibility node to name, so a coordinate click is the only way in.
/// Lint still says so, and says how to pin the step to one viewport, because a
/// coordinate goes stale whenever the layout or the window size changes.
fn coordinate_lint_warnings(args: &Value) -> Vec<String> {
    let Some(at) = args.get("at") else {
        return Vec::new();
    };
    let mut warnings = vec![
        "uses explicit coordinates `at`; coordinates are a last resort for content that has no DOM element or accessibility node (for example `<canvas>`) and they click the wrong place when the layout or viewport changes, so prefer `css`, `xpath`, `text`, `role` or `testid` when one can name the target".to_string(),
    ];
    if at.get("viewport").is_none() {
        warnings.push(
            "records no `at.viewport`; the step clicks at the live page size, so record the viewport the coordinates were measured at and the step will fail on a different size instead of clicking the wrong place".to_string(),
        );
    }
    warnings
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
    if args.at.is_some() {
        return check_type_at(&args);
    }
    type_target(&args).map(|_| ())
}

/// The target locator of `browser.upload`.
fn upload_target(args: &UploadArgs) -> Result<ElementTarget, String> {
    let locator = Locator::from_args(
        &args.css,
        &args.xpath,
        &args.text,
        &args.role,
        &args.name,
        &args.testid,
    )?;
    ElementTarget::from_parts(locator, args.within.as_deref(), args.nth)
}

/// Checks the `files` list: every entry must name a path.
///
/// Whether the path exists is a run-time fact the browser reports, so the
/// static check only refuses the values no file can ever have.
fn check_files(files: &[String]) -> Result<(), String> {
    for (index, file) in files.iter().enumerate() {
        if file.trim().is_empty() {
            return Err(format!(
                "invalid arguments: `files[{index}]` must not be empty; give a file path, or pass `files: []` to clear the input"
            ));
        }
    }
    Ok(())
}

fn validate_upload(args: &Value) -> Result<(), String> {
    let args: UploadArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    upload_target(&args)?;
    check_files(&args.files)
}

/// The `at` checks of `browser.type`, shared by `da lint` and execution.
fn check_type_at(args: &TypeArgs) -> Result<(), String> {
    let Some(at) = &args.at else {
        return Ok(());
    };
    // With `at` there is no element for `text` to name, so both `text` and
    // `value` would claim to be the typed text.
    if args.text.is_some() && args.value.is_some() {
        return Err(
            "invalid arguments: with `at`, `text` is the text to type, so give `text` or `value`, not both"
                .to_string(),
        );
    }
    check_at(
        at,
        &[
            ("css", args.css.is_some()),
            ("xpath", args.xpath.is_some()),
            ("role", args.role.is_some()),
            ("testid", args.testid.is_some()),
            ("name", args.name.is_some()),
            ("within", args.within.is_some()),
            ("nth", args.nth.is_some()),
        ],
    )
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

#[derive(Debug, PartialEq, Eq)]
enum StorageTarget {
    Current,
    Origin(String),
}

fn normalize_storage_origin(raw: &str) -> Result<StorageTarget, String> {
    if raw == "current" {
        return Ok(StorageTarget::Current);
    }
    if raw.is_empty() || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(format!(
            "invalid arguments: `local_storage` key `{raw}` must be `current` or an `http://` / `https://` origin"
        ));
    }
    let stripped = raw.strip_suffix('/').unwrap_or(raw);
    let authority = stripped
        .strip_prefix("http://")
        .or_else(|| stripped.strip_prefix("https://"))
        .ok_or_else(|| {
            format!(
                "invalid arguments: `local_storage` key `{raw}` must be `current` or an `http://` / `https://` origin"
            )
        })?;
    if authority.is_empty()
        || authority
            .chars()
            .any(|ch| matches!(ch, '/' | '?' | '#' | '@'))
    {
        return Err(format!(
            "invalid arguments: `local_storage` key `{raw}` must be an origin without path, query, or fragment"
        ));
    }
    Ok(StorageTarget::Origin(stripped.to_string()))
}

fn check_set_cookies(args: &SetCookiesArgs) -> Result<(), String> {
    if args.cookies.is_none() && args.local_storage.is_none() {
        return Err(
            "invalid arguments: `browser.set_cookies` requires at least one of `cookies` or `local_storage`"
                .to_string(),
        );
    }
    if let Some(cookies) = &args.cookies {
        check_cookie_list(cookies).map_err(|error| format!("invalid arguments: {error}"))?;
    }
    if let Some(storage) = &args.local_storage {
        check_local_storage(storage, true)
            .map_err(|error| format!("invalid arguments: {error}"))?;
    }
    Ok(())
}

/// Checks one `cookies` array, as given to `browser.set_cookies` or stored in a
/// storage-state file.
fn check_cookie_list(cookies: &[CookieArg]) -> Result<(), String> {
    if cookies.is_empty() {
        return Err("`cookies` must contain at least one cookie".to_string());
    }
    for (index, cookie) in cookies.iter().enumerate() {
        let label = format!("cookies[{index}]");
        if cookie.name.trim().is_empty() {
            return Err(format!("`{label}.name` must not be empty"));
        }
        if cookie.value.is_empty() {
            return Err(format!("`{label}.value` must not be empty"));
        }
        if cookie.url.is_none() && cookie.domain.is_none() {
            return Err(format!("`{label}` must specify `url` or `domain`"));
        }
        if let Some(url) = &cookie.url {
            if url.is_empty() || url.chars().any(char::is_whitespace) {
                return Err(format!(
                    "`{label}.url` must not be empty or contain whitespace"
                ));
            }
            let rest = url
                .strip_prefix("http://")
                .or_else(|| url.strip_prefix("https://"))
                .ok_or_else(|| format!("`{label}.url` must start with `http://` or `https://`"))?;
            if rest.is_empty() || rest.starts_with('/') {
                return Err(format!("`{label}.url` must include a host"));
            }
        }
        if let Some(domain) = &cookie.domain
            && (domain.trim().is_empty() || domain.chars().any(char::is_whitespace))
        {
            return Err(format!(
                "`{label}.domain` must not be empty or contain whitespace"
            ));
        }
        if let Some(path) = &cookie.path
            && (path.trim().is_empty() || path.chars().any(char::is_whitespace))
        {
            return Err(format!(
                "`{label}.path` must not be empty or contain whitespace"
            ));
        }
    }
    Ok(())
}

/// Checks one `local_storage` map. `allow_current` is false for a storage-state
/// file, which is applied before a page exists, so `current` means nothing there.
fn check_local_storage(
    storage: &BTreeMap<String, BTreeMap<String, String>>,
    allow_current: bool,
) -> Result<(), String> {
    if storage.is_empty() {
        return Err("`local_storage` must contain at least one origin entry".to_string());
    }
    for (origin, items) in storage {
        let target = normalize_storage_origin(origin)
            .map_err(|error| error.trim_start_matches("invalid arguments: ").to_string())?;
        if matches!(target, StorageTarget::Current) && !allow_current {
            return Err(format!(
                "`local_storage` key `{origin}` is not valid in a storage-state file; name the full `http://` or `https://` origin"
            ));
        }
        if items.is_empty() {
            return Err(format!(
                "`local_storage` for `{origin}` must contain at least one key"
            ));
        }
        for key in items.keys() {
            if key.is_empty() {
                return Err(format!(
                    "`local_storage` key for `{origin}` must not be empty"
                ));
            }
        }
    }
    Ok(())
}

/// Checks the content of a storage-state file after it was parsed.
fn check_storage_state(state: &StorageStateFile) -> Result<(), String> {
    if state.version != STORAGE_STATE_VERSION {
        return Err(format!(
            "unsupported `version` {}; this `da` reads version {STORAGE_STATE_VERSION}",
            state.version
        ));
    }
    if state.cookies.is_empty() && state.local_storage.is_empty() {
        return Err(
            "it contains neither `cookies` nor `local_storage`; save it with `browser.save_state` after signing in"
                .to_string(),
        );
    }
    if !state.cookies.is_empty() {
        check_cookie_list(&state.cookies)?;
    }
    if !state.local_storage.is_empty() {
        check_local_storage(&state.local_storage, false)?;
    }
    Ok(())
}

fn validate_set_cookies(args: &Value) -> Result<(), String> {
    let parsed: SetCookiesArgs = parse_for_validate(args)?;
    check_set_cookies(&parsed)
}

fn validate_save_state(args: &Value) -> Result<(), String> {
    let parsed: SaveStateArgs = parse_for_validate(args)?;
    if parsed.path.trim().is_empty() {
        return Err(
            "invalid arguments: `path` must name the storage-state file to write".to_string(),
        );
    }
    Ok(())
}

fn validate_switch_tab(args: &Value) -> Result<(), String> {
    let parsed: SwitchTabArgs = parse_for_validate(args)?;
    check_timeout(parsed.timeout_ms)?;
    for (field, value) in [
        ("url_contains", &parsed.url_contains),
        ("title_contains", &parsed.title_contains),
    ] {
        if let Some(text) = value
            && text.is_empty()
        {
            return Err(format!(
                "invalid arguments: `{field}` must not be empty; give the text the tab's {} contains",
                field.trim_end_matches("_contains")
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefClickWaitArgs {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default = "default_timeout_ms")]
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
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefReadArgs {
    #[serde(rename = "ref")]
    ref_id: String,
    #[serde(default)]
    attribute: Option<String>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefUploadArgs {
    #[serde(rename = "ref")]
    ref_id: String,
    files: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
}

fn has_ref_field(args: &Value) -> bool {
    args.as_object().is_some_and(|map| map.contains_key("ref"))
}

fn check_ref_id(ref_id: &str) -> Result<(), String> {
    if ref_id.trim().is_empty() {
        return Err("invalid arguments: `ref` must not be empty".to_string());
    }
    Ok(())
}

fn validate_click_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefClickWaitArgs = parse_for_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        return check_timeout(parsed.timeout_ms);
    }
    validate_click(args)
}

fn validate_type_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefTypeArgs = parse_for_validate(args)?;
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
            _ => {}
        }
        return Ok(());
    }
    validate_type(args)
}

fn validate_upload_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefUploadArgs = parse_for_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        check_timeout(parsed.timeout_ms)?;
        return check_files(&parsed.files);
    }
    validate_upload(args)
}

fn validate_wait_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefClickWaitArgs = parse_for_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        return check_timeout(parsed.timeout_ms);
    }
    validate_wait(args)
}

fn validate_read_session(args: &Value) -> Result<(), String> {
    if has_ref_field(args) {
        let parsed: RefReadArgs = parse_for_validate(args)?;
        check_ref_id(&parsed.ref_id)?;
        check_timeout(parsed.timeout_ms)?;
        if let Some(attribute) = &parsed.attribute
            && attribute.trim().is_empty()
        {
            return Err("invalid arguments: `attribute` must not be empty".to_string());
        }
        return Ok(());
    }
    validate_read(args)
}

/// Returns the case's browser session or explains how to open one.
fn session(sessions: &mut Sessions) -> Result<&mut BrowserSession, StepError> {
    let browser = sessions
        .get::<BrowserSession>(SESSION_KIND)
        .map_err(StepError::Execution)?
        .ok_or_else(|| {
            StepError::Execution(
                "no browser session is open; run `browser.open` before this step".to_string(),
            )
        })?;
    browser.snapshot_pending = true;
    Ok(browser)
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
    // The state file is read before the browser starts, so a missing or
    // malformed file fails closed without leaving a browser behind.
    let storage_state = match args.storage_state.as_deref() {
        Some(raw_path) => {
            let path = resolve_path(raw_path, sessions);
            Some(load_storage_state(&path)?)
        }
        None => None,
    };
    let existing = sessions
        .get::<BrowserSession>(SESSION_KIND)
        .map_err(StepError::Execution)?
        .is_some();
    let interactive = sessions.is_interactive();
    let session = sessions
        .get_or_create(SESSION_KIND, || {
            BrowserSession::open(&args, connect.as_ref(), interactive)
        })
        .map_err(StepError::Environment)?;
    if existing {
        session.ensure_same_options(&args, connect.as_ref())?;
        if let Some(attribute) = &args.testid_attribute {
            session.testid_attribute = attribute.clone();
        }
    }
    let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
    session.snapshot_pending = true;
    if let Some(state) = &storage_state {
        session.apply_storage_state(
            state,
            Deadline::after(Duration::from_millis(args.timeout_ms)),
            &mut dialog,
        )?;
    }
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
    if has_ref_field(args) {
        if !sessions.is_interactive() {
            return Err(StepError::InvalidArguments(
                "invalid arguments: `ref` is only available inside `da session`".to_string(),
            ));
        }
        let parsed: RefClickWaitArgs = parse(args)?;
        check_ref_id(&parsed.ref_id).map_err(StepError::InvalidArguments)?;
        check_timeout(parsed.timeout_ms).map_err(StepError::InvalidArguments)?;
        let deadline = Deadline::after(Duration::from_millis(parsed.timeout_ms));
        let mut dialog = DialogContext::fail_fast();
        let session = session(sessions)?;
        let target_desc = format!("ref `{}`", parsed.ref_id);
        let actionable = session.locate_ref_actionable(&parsed.ref_id, deadline, &mut dialog)?;
        return click_actionable(session, &target_desc, actionable, deadline, &mut dialog);
    }
    let args: ClickArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    if let Some(at) = &args.at {
        check_click_at(&args).map_err(StepError::InvalidArguments)?;
        let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
        let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
        let session = session(sessions)?;
        return click_at(session, at, deadline, &mut dialog);
    }
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
    click_actionable(session, &target_desc, actionable, deadline, &mut dialog)
}

fn click_actionable(
    session: &mut BrowserSession,
    target_desc: &str,
    actionable: ActionableTarget,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, StepError> {
    let located = actionable.located;
    let (x, y) = (actionable.x, actionable.y);
    for (event_type, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
        session.dispatch(
            target_desc,
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
            dialog,
        )?;
    }
    // A click may navigate, which detaches the element. The click itself
    // succeeded, so a text that can no longer be read is not a failure unless
    // an unhandled JavaScript dialog opened.
    let text = match session.run_on(
        target_desc,
        &located,
        TEXT_BODY,
        "read the text of",
        deadline,
        dialog,
    ) {
        Ok(value) => value.as_str().map(str::to_string),
        Err(error) if dialog.is_fail_fast_triggered() => return Err(error),
        Err(_) => None,
    };
    let result = match text {
        Some(text) if !text.trim().is_empty() => json!({ "text": text.trim() }),
        _ => json!({}),
    };
    Ok(attach_dialog(result, dialog))
}

fn run_type(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    if has_ref_field(args) {
        if !sessions.is_interactive() {
            return Err(StepError::InvalidArguments(
                "invalid arguments: `ref` is only available inside `da session`".to_string(),
            ));
        }
        validate_type_session(args).map_err(StepError::InvalidArguments)?;
        let parsed: RefTypeArgs = parse(args)?;
        let text = parsed
            .value
            .or(parsed.text)
            .ok_or_else(|| StepError::InvalidArguments("missing typing text".to_string()))?;
        let deadline = Deadline::after(Duration::from_millis(parsed.timeout_ms));
        let mut dialog = DialogContext::fail_fast();
        let session = session(sessions)?;
        let target_desc = format!("ref `{}`", parsed.ref_id);
        let actionable = session.locate_ref_actionable(&parsed.ref_id, deadline, &mut dialog)?;
        return type_actionable(
            session,
            &target_desc,
            &actionable.located,
            &text,
            deadline,
            &mut dialog,
        );
    }
    let args: TypeArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let text = type_text(&args)
        .map_err(StepError::InvalidArguments)?
        .to_string();
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let mut dialog = DialogContext::from_arg(args.dialog.as_ref());
    if let Some(at) = &args.at {
        check_type_at(&args).map_err(StepError::InvalidArguments)?;
        let session = session(sessions)?;
        return type_at(session, at, &text, deadline, &mut dialog);
    }
    let target = type_target(&args).map_err(StepError::InvalidArguments)?;
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let actionable = session.locate_actionable_target(&target, deadline, &mut dialog)?;
    type_actionable(
        session,
        &target_desc,
        &actionable.located,
        &text,
        deadline,
        &mut dialog,
    )
}

fn type_actionable(
    session: &mut BrowserSession,
    target_desc: &str,
    located: &Located,
    text: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, StepError> {
    let focused = session.run_on(target_desc, located, FOCUS_BODY, "focus", deadline, dialog)?;
    if focused != Value::Bool(true) {
        return Err(action_error(
            target_desc,
            "focus",
            "the element did not take focus",
        ));
    }
    session.dispatch(
        target_desc,
        "Input.insertText",
        json!({ "text": text }),
        "type into",
        deadline,
        dialog,
    )?;
    Ok(attach_dialog(json!({}), dialog))
}

fn run_upload(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    if has_ref_field(args) {
        if !sessions.is_interactive() {
            return Err(StepError::InvalidArguments(
                "invalid arguments: `ref` is only available inside `da session`".to_string(),
            ));
        }
        validate_upload_session(args).map_err(StepError::InvalidArguments)?;
        let parsed: RefUploadArgs = parse(args)?;
        let files = resolve_upload_files(&parsed.files, sessions)?;
        let deadline = Deadline::after(Duration::from_millis(parsed.timeout_ms));
        let mut dialog = DialogContext::fail_fast();
        let session = session(sessions)?;
        let target_desc = format!("ref `{}`", parsed.ref_id);
        let located = session.locate_ref_any_visibility(&parsed.ref_id, deadline, &mut dialog)?;
        return upload_to_located(
            session,
            &target_desc,
            &located,
            &files,
            deadline,
            &mut dialog,
        );
    }
    validate_upload(args).map_err(StepError::InvalidArguments)?;
    let args: UploadArgs = parse(args)?;
    let target = upload_target(&args).map_err(StepError::InvalidArguments)?;
    let files = resolve_upload_files(&args.files, sessions)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let mut dialog = DialogContext::fail_fast();
    let session = session(sessions)?;
    let target_desc = target.describe_with(&session.testid_attribute);
    let located = session.locate_target_any_visibility(&target, deadline, &mut dialog)?;
    upload_to_located(
        session,
        &target_desc,
        &located,
        &files,
        deadline,
        &mut dialog,
    )
}

/// Resolves and checks the `files` list of `browser.upload`.
///
/// A relative path resolves against the directory that holds the case file, the
/// same rule `browser.screenshot` uses for its output, and every path is checked
/// on this side first so a missing or unusable file is named in a step error
/// instead of surfacing as an opaque browser failure.
fn resolve_upload_files(files: &[String], sessions: &Sessions) -> Result<Vec<PathBuf>, StepError> {
    let mut resolved = Vec::with_capacity(files.len());
    for (index, raw) in files.iter().enumerate() {
        let path = PathBuf::from(raw);
        let candidate = match sessions.base_directory() {
            Some(directory) if !path.is_absolute() => directory.join(&path),
            _ => path,
        };
        let absolute = std::path::absolute(&candidate).map_err(|error| {
            StepError::Execution(format!(
                "could not upload `files[{index}]` `{raw}`: cannot make it absolute: {error}"
            ))
        })?;
        let metadata = std::fs::metadata(&absolute).map_err(|error| {
            let detail = if error.kind() == std::io::ErrorKind::NotFound {
                "does not exist; a relative `files` path resolves against the directory that holds the case file"
                    .to_string()
            } else {
                format!("cannot be read: {error}")
            };
            StepError::Execution(format!(
                "could not upload `files[{index}]` `{raw}`: it {detail} (looked at `{}`)",
                paths::display(&absolute)
            ))
        })?;
        if !metadata.is_file() {
            return Err(StepError::Execution(format!(
                "could not upload `files[{index}]` `{raw}`: `{}` is not a regular file; `files` must name files, not directories",
                paths::display(&absolute)
            )));
        }
        resolved.push(absolute);
    }
    Ok(resolved)
}

/// Sets `files` on the resolved element and reports what the page now holds.
fn upload_to_located(
    session: &mut BrowserSession,
    target_desc: &str,
    located: &Located,
    files: &[PathBuf],
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, StepError> {
    let target = follow_label(session, target_desc, located, deadline, dialog)?;
    let raw = session.run_on(
        target_desc,
        &target,
        FILE_INPUT_STATE_BODY,
        "inspect",
        deadline,
        dialog,
    )?;
    let state: FileInputState = serde_json::from_value(raw).map_err(|error| {
        StepError::Environment(format!(
            "the browser returned an invalid file input state for {target_desc}: {error}"
        ))
    })?;
    match state.status.as_str() {
        "ok" => {}
        "detached" => {
            return Err(StepError::Execution(format!(
                "could not upload to {target_desc}: the element was removed from the page before its files could be set"
            )));
        }
        _ => {
            let matched = match matched_element_label(&state.tag, &state.r#type) {
                Some(label) => {
                    format!("the matched element is {label}, not an `<input type=\"file\">`")
                }
                None => "the matched element is not an `<input type=\"file\">`".to_string(),
            };
            return Err(StepError::Execution(format!(
                "could not upload to {target_desc}: {matched}; point the locator at the file input"
            )));
        }
    }
    if state.disabled {
        return Err(StepError::Execution(format!(
            "could not upload to {target_desc}: the file input is disabled (its own `disabled` attribute or an enclosing disabled `<fieldset>`), so it cannot take files"
        )));
    }
    if files.len() > 1 && !state.multiple {
        return Err(StepError::Execution(format!(
            "could not upload {} files to {target_desc}: the file input has no `multiple` attribute, so it accepts one file; add `multiple` to the input or pass a single path",
            files.len()
        )));
    }
    if files.is_empty() {
        session.run_on(
            target_desc,
            &target,
            CLEAR_FILE_INPUT_BODY,
            "clear the files of",
            deadline,
            dialog,
        )?;
    } else {
        let paths: Vec<String> = files.iter().map(|path| paths::display(path)).collect();
        session.dispatch(
            target_desc,
            "DOM.setFileInputFiles",
            json!({ "objectId": target.0, "files": paths }),
            "set the files of",
            deadline,
            dialog,
        )?;
    }
    let names = session.run_on(
        target_desc,
        &target,
        FILE_INPUT_NAMES_BODY,
        "read the files of",
        deadline,
        dialog,
    )?;
    let names: Vec<String> = serde_json::from_value(names).map_err(|error| {
        StepError::Environment(format!(
            "the browser returned an invalid file selection for {target_desc}: {error}"
        ))
    })?;
    if names.len() != files.len() {
        return Err(StepError::Execution(format!(
            "could not upload {} file(s) to {target_desc}: the browser selected {}; check that the browser can read every file",
            files.len(),
            names.len()
        )));
    }
    Ok(json!({ "files": names.len(), "names": names }))
}

/// Names the element a locator actually matched, for a "not a file input" error.
fn matched_element_label(tag: &str, input_type: &str) -> Option<String> {
    let tag = tag.trim();
    if tag.is_empty() {
        return None;
    }
    let input_type = input_type.trim();
    if tag == "input" && !input_type.is_empty() {
        return Some(format!("`<input type=\"{input_type}\">`"));
    }
    Some(format!("`<{tag}>`"))
}

/// Follows a matched `<label>` to the control it labels.
///
/// The label is what a person sees and clicks, and what a `text` locator can
/// name, so uploads through it must reach the input the label opens.
fn follow_label(
    session: &mut BrowserSession,
    target_desc: &str,
    located: &Located,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Located, StepError> {
    let scope = session.scope();
    let connection = session.connection_mut()?;
    let remote = page_call_function_remote(
        connection,
        &scope,
        &located.0,
        FOLLOW_LABEL_SCRIPT,
        deadline,
        dialog,
    )
    .map_err(|failure| {
        failure.step(
            &format!("could not follow the label of {target_desc}"),
            timeout_or_execution,
        )
    })?;
    match remote {
        RemoteResult::Object(object_id) => Ok(Located(object_id)),
        RemoteResult::Value(_) => Err(StepError::Execution(format!(
            "could not upload to {target_desc}: the element was removed from the page before its files could be set"
        ))),
    }
}

/// Clicks explicit viewport coordinates after proving they still describe the
/// page they were measured on.
fn click_at(
    session: &mut BrowserSession,
    at: &AtArgs,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, StepError> {
    let (width, height) = resolve_coordinates(session, at, deadline, dialog)?;
    dispatch_pointer_click(session, at, deadline, dialog)?;
    Ok(attach_dialog(coordinate_result(at, width, height), dialog))
}

/// Clicks explicit viewport coordinates to focus the target, then types `text`.
fn type_at(
    session: &mut BrowserSession,
    at: &AtArgs,
    text: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, StepError> {
    let (width, height) = resolve_coordinates(session, at, deadline, dialog)?;
    dispatch_pointer_click(session, at, deadline, dialog)?;
    let target = coordinate_target(at);
    session.dispatch(
        &target,
        "Input.insertText",
        json!({ "text": text }),
        "type into",
        deadline,
        dialog,
    )?;
    Ok(attach_dialog(coordinate_result(at, width, height), dialog))
}

/// The `at` target as an error message names it.
fn coordinate_target(at: &AtArgs) -> String {
    format!("coordinates ({}, {})", at.x, at.y)
}

/// The JSON result of a coordinate step: where it clicked and at what size.
fn coordinate_result(at: &AtArgs, width: u32, height: u32) -> Value {
    json!({
        "x": coordinate_number(at.x),
        "y": coordinate_number(at.y),
        "viewport": { "width": width, "height": height },
    })
}

/// The JSON number for one coordinate.
///
/// A whole coordinate stays an integer so an `equals: 100` assertion matches the
/// number the case wrote instead of failing on `100.0`.
fn coordinate_number(value: f64) -> Value {
    let whole = value as i64;
    if value == whole as f64 {
        json!(whole)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// One positive viewport dimension from the page's evaluate reply.
fn viewport_dimension(value: &Value, name: &str) -> Option<u32> {
    let number = value.get(name)?;
    let pixels = number
        .as_u64()
        .or_else(|| number.as_f64().map(|number| number.max(0.0) as u64))?;
    u32::try_from(pixels).ok().filter(|pixels| *pixels > 0)
}

/// Reads the live viewport and refuses coordinates that cannot describe it.
///
/// A declared `at.viewport` pins the step to the size it was measured at, so a
/// different window size fails loudly instead of clicking the wrong place. Every
/// coordinate must also still fall inside the live viewport.
fn resolve_coordinates(
    session: &mut BrowserSession,
    at: &AtArgs,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<(u32, u32), StepError> {
    let (width, height) = session.viewport_size(deadline, dialog)?;
    if let Some(declared) = &at.viewport
        && (declared.width != width || declared.height != height)
    {
        return Err(StepError::Execution(format!(
            "the coordinates were recorded at viewport {}x{}, but the page viewport is {}x{}; record the coordinates at the run's viewport size or open the browser with `width: {}` and `height: {}`",
            declared.width, declared.height, width, height, declared.width, declared.height
        )));
    }
    if at.x >= f64::from(width) || at.y >= f64::from(height) {
        return Err(StepError::Execution(format!(
            "the coordinates ({}, {}) fall outside the {}x{} page viewport; both coordinates must be smaller than the viewport width and height",
            at.x, at.y, width, height
        )));
    }
    Ok((width, height))
}

/// Sends the move, press and release events a coordinate click is made of.
fn dispatch_pointer_click(
    session: &mut BrowserSession,
    at: &AtArgs,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<(), StepError> {
    let target = coordinate_target(at);
    for (event_type, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
        let params = if event_type == "mouseMoved" {
            json!({ "type": event_type, "x": at.x, "y": at.y })
        } else {
            json!({
                "type": event_type,
                "x": at.x,
                "y": at.y,
                "button": "left",
                "buttons": buttons,
                "clickCount": 1,
            })
        };
        session.dispatch(
            &target,
            "Input.dispatchMouseEvent",
            params,
            "click",
            deadline,
            dialog,
        )?;
    }
    Ok(())
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
    if has_ref_field(args) {
        if !sessions.is_interactive() {
            return Err(StepError::InvalidArguments(
                "invalid arguments: `ref` is only available inside `da session`".to_string(),
            ));
        }
        let parsed: RefClickWaitArgs = parse(args)?;
        check_ref_id(&parsed.ref_id).map_err(StepError::InvalidArguments)?;
        check_timeout(parsed.timeout_ms).map_err(StepError::InvalidArguments)?;
        let deadline = Deadline::after(Duration::from_millis(parsed.timeout_ms));
        let mut dialog = DialogContext::fail_fast();
        let session = session(sessions)?;
        session.locate_ref_visible(&parsed.ref_id, deadline, &mut dialog)?;
        let scope = session.scope();
        let connection = session.connection_mut()?;
        let url = page_evaluate(connection, &scope, "location.href", deadline, &mut dialog)
            .map_err(|failure| failure.environment("could not read the current URL"))?;
        return Ok(json!({ "url": url.as_str().unwrap_or_default() }));
    }
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
    if has_ref_field(args) {
        if !sessions.is_interactive() {
            return Err(StepError::InvalidArguments(
                "invalid arguments: `ref` is only available inside `da session`".to_string(),
            ));
        }
        validate_read_session(args).map_err(StepError::InvalidArguments)?;
        let parsed: RefReadArgs = parse(args)?;
        let deadline = Deadline::after(Duration::from_millis(parsed.timeout_ms));
        let mut dialog = DialogContext::fail_fast();
        let session = session(sessions)?;
        let target_desc = format!("ref `{}`", parsed.ref_id);
        let located = session.locate_ref_visible(&parsed.ref_id, deadline, &mut dialog)?;
        return read_located(
            session,
            &target_desc,
            &located,
            parsed.attribute.as_deref(),
            deadline,
            &mut dialog,
        );
    }
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
    read_located(
        session,
        &target_desc,
        &located,
        args.attribute.as_deref(),
        deadline,
        &mut dialog,
    )
}

fn read_located(
    session: &mut BrowserSession,
    target_desc: &str,
    located: &Located,
    attribute: Option<&str>,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<Value, StepError> {
    let text = session.run_on(
        target_desc,
        located,
        TEXT_BODY,
        "read the text of",
        deadline,
        dialog,
    )?;
    let mut result = json!({ "text": text.as_str().unwrap_or_default() });
    if let Some(attribute) = attribute {
        let body = format!(
            "if (!el) return null; return el.getAttribute({});",
            json_string(attribute)
        );
        let value = session.run_on(
            target_desc,
            located,
            &body,
            "read the attribute of",
            deadline,
            dialog,
        )?;
        result["value"] = value;
    }
    Ok(result)
}

fn is_template_placeholder(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return false;
    };
    if inner.is_empty() || inner.contains(['{', '}']) {
        return false;
    }
    match inner.strip_prefix("env:") {
        Some(env_name) => {
            let mut chars = env_name.chars();
            chars
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        }
        None => true,
    }
}

fn mask_sensitive_value(slot: &mut Value, preserve_placeholders: bool) {
    match slot {
        Value::String(s) if preserve_placeholders && is_template_placeholder(s) => {}
        _ => *slot = Value::String("***".to_string()),
    }
}

/// Replaces concrete cookie values and `local_storage` values in
/// `browser.set_cookies` arguments before they enter step reports, HTML
/// reports, or session logs. When `preserve_placeholders` is true (unexpanded
/// session `raw_args`), whole-string `${env:NAME}` and `${var}` placeholders
/// stay intact for deterministic `da session export`.
fn redact_set_cookies_args(args: &Value, preserve_placeholders: bool) -> Value {
    let mut redacted = args.clone();
    let Some(root) = redacted.as_object_mut() else {
        return redacted;
    };
    if let Some(Value::Array(cookies)) = root.get_mut("cookies") {
        for cookie in cookies {
            if let Some(obj) = cookie.as_object_mut()
                && let Some(val) = obj.get_mut("value")
            {
                mask_sensitive_value(val, preserve_placeholders);
            }
        }
    }
    if let Some(Value::Object(origins)) = root.get_mut("local_storage") {
        for entries in origins.values_mut() {
            if let Some(map) = entries.as_object_mut() {
                for val in map.values_mut() {
                    mask_sensitive_value(val, preserve_placeholders);
                }
            }
        }
    }
    redacted
}

fn run_set_cookies(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: SetCookiesArgs = parse(args)?;
    check_set_cookies(&args).map_err(StepError::InvalidArguments)?;

    let interactive = sessions.is_interactive();
    let session = sessions
        .get_or_create(SESSION_KIND, || BrowserSession::open_default(interactive))
        .map_err(StepError::Environment)?;

    let deadline = Deadline::after(Duration::from_millis(DEFAULT_NAVIGATION_TIMEOUT_MS));
    let mut dialog = DialogContext::fail_fast();

    let cookies_set = if let Some(cookies) = &args.cookies {
        apply_cookies(session, cookies, deadline, &mut dialog)?
    } else {
        0
    };
    let local_storage_set = if let Some(storage) = &args.local_storage {
        apply_local_storage(session, storage, deadline, &mut dialog)?
    } else {
        0
    };

    session.snapshot_pending = true;
    Ok(json!({
        "cookies_set": cookies_set,
        "local_storage_set": local_storage_set,
    }))
}

fn apply_cookies(
    session: &mut BrowserSession,
    cookies: &[CookieArg],
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<usize, StepError> {
    let mut params_list = Vec::with_capacity(cookies.len());
    for cookie in cookies {
        let mut entry = serde_json::Map::new();
        entry.insert("name".to_string(), json!(cookie.name));
        entry.insert("value".to_string(), json!(cookie.value));
        if let Some(url) = &cookie.url {
            entry.insert("url".to_string(), json!(url));
        }
        if let Some(domain) = &cookie.domain {
            entry.insert("domain".to_string(), json!(domain));
        }
        if let Some(path) = &cookie.path {
            entry.insert("path".to_string(), json!(path));
        }
        if let Some(http_only) = cookie.http_only {
            entry.insert("httpOnly".to_string(), json!(http_only));
        }
        if let Some(secure) = cookie.secure {
            entry.insert("secure".to_string(), json!(secure));
        }
        if let Some(same_site) = cookie.same_site {
            entry.insert("sameSite".to_string(), json!(same_site.as_cdp()));
        }
        params_list.push(Value::Object(entry));
    }

    let scope = session.scope();
    let connection = session.connection_mut()?;
    page_call(
        connection,
        &scope,
        "Network.setCookies",
        json!({ "cookies": params_list }),
        deadline,
        dialog,
    )
    .map_err(|failure| failure.step("could not set browser cookies", timeout_or_execution))?;
    Ok(cookies.len())
}

fn apply_local_storage(
    session: &mut BrowserSession,
    storage: &BTreeMap<String, BTreeMap<String, String>>,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<usize, StepError> {
    static GUARD_SEQ: AtomicU64 = AtomicU64::new(0);

    let scope = session.scope();
    let current_origin = {
        let connection = session.connection_mut()?;
        page_evaluate(connection, &scope, "location.origin", deadline, dialog)
            .map_err(|failure| failure.environment("could not read the current page origin"))?
            .as_str()
            .unwrap_or_default()
            .to_string()
    };

    let mut total = 0usize;
    for (raw_origin, entries) in storage {
        let target = normalize_storage_origin(raw_origin).map_err(StepError::InvalidArguments)?;
        let pairs: Vec<(&String, &String)> = entries.iter().collect();
        let encoded_pairs = serde_json::to_string(&pairs).map_err(|error| {
            StepError::Execution(format!("could not encode local_storage entries: {error}"))
        })?;

        let matches_current = match &target {
            StorageTarget::Current => {
                if current_origin.is_empty() || current_origin == "null" {
                    return Err(StepError::Execution(
                        "cannot set `local_storage` for `current` before opening an HTTP(S) page; pass an explicit origin such as `http://127.0.0.1:8080` or run `browser.open` first".to_string(),
                    ));
                }
                true
            }
            StorageTarget::Origin(origin) => current_origin == *origin,
        };

        if matches_current {
            let script = format!(
                "(function () {{ var items = {encoded_pairs}; for (var i = 0; i < items.length; i++) {{ window.localStorage.setItem(items[i][0], items[i][1]); }} return items.length; }})()"
            );
            let connection = session.connection_mut()?;
            page_evaluate(connection, &scope, &script, deadline, dialog).map_err(|failure| {
                failure.step(
                    &format!("could not set `local_storage` for `{raw_origin}`"),
                    timeout_or_execution,
                )
            })?;
        } else if let StorageTarget::Origin(origin) = target {
            let guard = format!(
                "__da_ls_{}_{}",
                std::process::id(),
                GUARD_SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let script = format!(
                "(function () {{ try {{ if (window.location.origin !== {}) return; var guard = {}; if (window.sessionStorage.getItem(guard)) return; window.sessionStorage.setItem(guard, '1'); var items = {encoded_pairs}; for (var i = 0; i < items.length; i++) {{ window.localStorage.setItem(items[i][0], items[i][1]); }} }} catch (_) {{}} }})();",
                json_string(&origin),
                json_string(&guard),
            );
            let connection = session.connection_mut()?;
            let reply = page_call(
                connection,
                &scope,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": script }),
                deadline,
                dialog,
            )
            .map_err(|failure| {
                failure.step(
                    &format!("could not register `local_storage` preload for `{origin}`"),
                    timeout_or_execution,
                )
            })?;
            if let Some(identifier) = reply.get("identifier").and_then(Value::as_str) {
                session
                    .pending_local_storage_scripts
                    .push((origin, identifier.to_string()));
            }
        }
        total += entries.len();
    }
    Ok(total)
}

/// Evaluates to an object with every current `localStorage` entry.
const LOCAL_STORAGE_DUMP_BODY: &str = "(function () { var out = {}; for (var index = 0; index < window.localStorage.length; index++) { var key = window.localStorage.key(index); out[key] = window.localStorage.getItem(key); } return out; })()";

/// Reads and validates a storage-state file, failing closed on anything a
/// later run could not apply.
fn load_storage_state(path: &Path) -> Result<StorageStateFile, StepError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        StepError::Environment(format!(
            "could not read browser storage state `{}`: {error}; save it with `browser.save_state` or fix the path",
            paths::display(path)
        ))
    })?;
    if !metadata.is_file() {
        return Err(StepError::Environment(format!(
            "browser storage state `{}` is not a regular file",
            paths::display(path)
        )));
    }
    if metadata.len() > STORAGE_STATE_MAX_BYTES {
        return Err(StepError::Environment(format!(
            "browser storage state `{}` is larger than {STORAGE_STATE_MAX_BYTES} bytes",
            paths::display(path)
        )));
    }
    let text = std::fs::read_to_string(path).map_err(|error| {
        StepError::Environment(format!(
            "could not read browser storage state `{}`: {error}",
            paths::display(path)
        ))
    })?;
    let state: StorageStateFile = serde_json::from_str(&text).map_err(|error| {
        StepError::Environment(format!(
            "browser storage state `{}` is malformed: {error}; expected a JSON object written by `browser.save_state`",
            paths::display(path)
        ))
    })?;
    check_storage_state(&state).map_err(|error| {
        StepError::Environment(format!(
            "browser storage state `{}` is invalid: {error}",
            paths::display(path)
        ))
    })?;
    Ok(state)
}

/// Writes a storage-state file, creating its directory and keeping the file
/// readable only by its owner where the platform allows it.
fn write_storage_state(path: &Path, state: &StorageStateFile) -> Result<(), StepError> {
    let mut encoded = serde_json::to_string_pretty(state).map_err(|error| {
        StepError::Execution(format!(
            "could not encode the browser storage state: {error}"
        ))
    })?;
    encoded.push('\n');
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            StepError::Environment(format!(
                "could not create the storage-state directory `{}`: {error}",
                paths::display(parent)
            ))
        })?;
    }
    write_private_file(path, encoded.as_bytes()).map_err(|error| {
        StepError::Environment(format!(
            "could not write the browser storage state `{}`: {error}",
            paths::display(path)
        ))
    })
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // `mode` only applies to a newly created file; an existing state file must
    // not keep a wider mode than the owner.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    std::io::Write::write_all(&mut file, bytes)
}

#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
}

/// Maps `Network.getCookies` cookies into the `browser.set_cookies` shape.
fn cookies_from_cdp(reply: &Value, origin: &str) -> Vec<CookieArg> {
    let Some(entries) = reply.get("cookies").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut cookies = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(name) = entry.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(value) = entry.get("value").and_then(Value::as_str) else {
            continue;
        };
        // A cookie with an empty name or value carries no state and the
        // `browser.set_cookies` shape refuses it; skip it instead of writing a
        // file that cannot be loaded.
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let path = entry.get("path").and_then(Value::as_str);
        let domain = entry
            .get("domain")
            .and_then(Value::as_str)
            .map(str::to_string);
        let url = if domain.is_none() {
            Some(match path {
                Some(path) => format!("{origin}{path}"),
                None => format!("{origin}/"),
            })
        } else {
            None
        };
        cookies.push(CookieArg {
            name: name.to_string(),
            value: value.to_string(),
            url,
            domain,
            path: path.map(str::to_string),
            http_only: entry.get("httpOnly").and_then(Value::as_bool),
            secure: entry.get("secure").and_then(Value::as_bool),
            same_site: CookieSameSite::from_cdp(entry.get("sameSite").and_then(Value::as_str)),
        });
    }
    cookies
}

fn run_save_state(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: SaveStateArgs = parse(args)?;
    if args.path.trim().is_empty() {
        return Err(StepError::InvalidArguments(
            "invalid arguments: `path` must name the storage-state file to write".to_string(),
        ));
    }
    let path = resolve_path(&args.path, sessions);
    let session = session(sessions)?;
    if let Some(connect) = &session.connect {
        return Err(StepError::Execution(format!(
            "refusing to save browser storage state from a connected browser (`{connect}`): its cookies belong to your everyday browser; use `browser.open` with `profile` or without `connect`"
        )));
    }
    let mut dialog = DialogContext::fail_fast();
    let state = session.capture_storage_state(Deadline::after(SCREENSHOT_TIMEOUT), &mut dialog)?;
    let cookies = state.cookies.len();
    let local_storage: usize = state.local_storage.values().map(BTreeMap::len).sum();
    write_storage_state(&path, &state)?;
    Ok(json!({
        "path": paths::display(&path),
        "cookies": cookies,
        "local_storage": local_storage,
    }))
}

fn run_switch_tab(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: SwitchTabArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let selector = tabs::Selector {
        url_contains: args.url_contains,
        title_contains: args.title_contains,
        index: args.index,
    };
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let session = session(sessions)?;
    session.switch_tab(&selector, deadline)
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

    /// Merges the keys of `extra` into the `base` mapping.
    fn add_fields(base: Value, extra: Value) -> Value {
        let mut merged = base;
        if let (Some(entries), Value::Object(extra)) = (merged.as_object_mut(), extra) {
            entries.extend(extra);
        }
        merged
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
        let within_args = json!({ "css": "#status", "within": { "css": "#embed" } });
        for name in ["browser.read", "browser.wait"] {
            let def = definition(name);
            for args in [json!({}), within_args.clone()] {
                assert!(
                    (def.check_retry)(&args).is_ok(),
                    "`{name}` must allow retry for {args}"
                );
            }
        }
        for name in [
            "browser.open",
            "browser.click",
            "browser.type",
            "browser.upload",
            "browser.press",
            "browser.screenshot",
            "browser.set_cookies",
            "browser.save_state",
            "browser.switch_tab",
        ] {
            let def = definition(name);
            for args in [json!({}), within_args.clone()] {
                let error = (def.check_retry)(&args)
                    .err()
                    .unwrap_or_else(|| panic!("`{name}` must reject retry for {args}"));
                assert!(error.contains("cannot use `retry`"), "{name}: {error}");
            }
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
        for step in [
            "browser.click",
            "browser.type",
            "browser.upload",
            "browser.read",
        ] {
            let definition = definition(step);
            let branches = definition.args_schema["oneOf"]
                .as_array()
                .unwrap_or_else(|| panic!("`{step}` must declare oneOf"));
            // Five locator forms, plus the `at` coordinate form where a step
            // supports it.
            let coordinate_branches = usize::from(matches!(step, "browser.click" | "browser.type"));
            assert_eq!(
                branches.len(),
                5 + coordinate_branches,
                "`{step}`: {branches:?}"
            );
            for property in ["css", "xpath", "text", "role", "name", "testid"] {
                assert_eq!(
                    definition.args_schema["properties"][property]["type"], "string",
                    "`{step}`: `{property}` must be a string"
                );
            }

            let files = json!({"files": ["fixture.txt"]});
            let base = if step == "browser.type" {
                json!({"css": "#name", "text": "hello"})
            } else if step == "browser.upload" {
                json!({"css": "#name", "files": ["fixture.txt"]})
            } else {
                json!({"css": "#name"})
            };
            assert!(
                (definition.validate)(&base).is_ok(),
                "`{step}` must accept one locator: {base}"
            );
            let text = if step == "browser.type" {
                json!({"text": "Later", "value": "hello"})
            } else if step == "browser.upload" {
                json!({"text": "Later", "files": ["fixture.txt"]})
            } else {
                json!({"text": "Later"})
            };
            assert!(
                (definition.validate)(&text).is_ok(),
                "`{step}` must accept a text locator: {text}"
            );
            let role = if step == "browser.type" {
                json!({"role": "textbox", "name": "Name", "text": "hello"})
            } else if step == "browser.upload" {
                json!({"role": "textbox", "name": "Name", "files": ["fixture.txt"]})
            } else {
                json!({"role": "textbox", "name": "Name"})
            };
            assert!(
                (definition.validate)(&role).is_ok(),
                "`{step}` must accept a role locator: {role}"
            );
            let both = if step == "browser.type" {
                json!({"css": "#a", "xpath": "//*[@id='a']", "text": "hello"})
            } else if step == "browser.upload" {
                json!({"css": "#a", "xpath": "//*[@id='a']", "files": ["fixture.txt"]})
            } else {
                json!({"css": "#a", "xpath": "//*[@id='a']"})
            };
            let error = (definition.validate)(&both).expect_err("both locators must be refused");
            assert!(error.contains("cannot both be set"), "{step}: {error}");
            let none = if step == "browser.type" {
                json!({"text": "hello"})
            } else if step == "browser.upload" {
                files.clone()
            } else {
                json!({})
            };
            let error =
                (definition.validate)(&none).expect_err("a missing locator must be refused");
            assert!(error.contains("one locator"), "{step}: {error}");
            let name = if step == "browser.type" {
                json!({"name": "Submit", "text": "hello"})
            } else if step == "browser.upload" {
                json!({"name": "Submit", "files": ["fixture.txt"]})
            } else {
                json!({"name": "Submit"})
            };
            let error =
                (definition.validate)(&name).expect_err("a name without a role must be refused");
            assert!(error.contains("role"), "{step}: {error}");
        }
    }

    #[test]
    fn upload_requires_a_files_list_and_accepts_every_locator() {
        let definition = definition("browser.upload");
        assert_eq!(
            definition.args_schema["required"],
            json!(["files"]),
            "`files` is required: {}",
            definition.args_schema
        );
        assert_eq!(
            definition.args_schema["properties"]["files"]["type"], "array",
            "`files` is a list: {}",
            definition.args_schema
        );
        assert_eq!(
            definition.args_schema["properties"]["files"]["items"]["minLength"], 1,
            "an empty path is refused by the schema: {}",
            definition.args_schema
        );

        for locator in [
            json!({"css": "#file"}),
            json!({"xpath": "//*[@id='file']"}),
            json!({"text": "Choose a file"}),
            json!({"role": "button", "name": "Choose a file"}),
            json!({"testid": "file"}),
        ] {
            let mut args = locator.clone();
            args["files"] = json!(["fixture.txt"]);
            assert!(
                (definition.validate)(&args).is_ok(),
                "every locator form must be accepted with `files`: {args}"
            );
        }
        assert!(
            (definition.validate)(&json!({"css": "#file", "files": []})).is_ok(),
            "an empty `files` list clears the input and is valid"
        );
        assert!(
            (definition.validate)(&json!({
                "css": "#file",
                "files": ["a.txt"],
                "within": {"css": "#form"},
                "nth": 1,
                "timeout_ms": 250
            }))
            .is_ok(),
            "the locator modifiers and `timeout_ms` are accepted"
        );

        let error = (definition.validate)(&json!({"css": "#file"}))
            .expect_err("a missing `files` list must be refused");
        assert!(error.contains("files"), "{error}");
        let error = (definition.validate)(&json!({"css": "#file", "files": [""]}))
            .expect_err("an empty path must be refused");
        assert!(error.contains("files[0]"), "{error}");
        let error =
            (definition.validate)(&json!({"css": "#file", "files": ["a.txt"], "timeout_ms": 0}))
                .expect_err("a zero timeout must be refused");
        assert!(error.contains("timeout_ms"), "{error}");
        let error = (definition.validate)(&json!({"css": "#file", "files": ["a.txt"], "bogus": 1}))
            .expect_err("an unknown argument must be refused");
        assert!(error.contains("bogus"), "{error}");
    }

    #[test]
    fn upload_paths_resolve_against_the_case_directory_before_the_browser_runs() {
        let base = std::env::temp_dir().join(format!("da-upload-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("cases/sub")).expect("the fixture directory must exist");
        std::fs::write(base.join("cases/fixture.txt"), "hello")
            .expect("the fixture must be written");

        let mut sessions = Sessions::new();
        sessions.set_base_directory(base.join("cases"));

        let resolved = resolve_upload_files(&["fixture.txt".to_string()], &sessions)
            .expect("a relative path must resolve against the case directory");
        assert_eq!(
            resolved,
            vec![
                std::path::absolute(base.join("cases/fixture.txt"))
                    .expect("the fixture path must become absolute")
            ],
            "a relative path must resolve against the case file's directory"
        );

        let absolute = std::path::absolute(base.join("cases/fixture.txt"))
            .expect("the fixture path must become absolute");
        let resolved = resolve_upload_files(&[paths::display(&absolute)], &sessions)
            .expect("an absolute path must be used as given");
        assert_eq!(resolved, vec![absolute], "an absolute path must be kept");

        let error = resolve_upload_files(&["missing.txt".to_string()], &sessions)
            .expect_err("a missing file must be refused");
        assert!(
            step_error_text(error).contains("does not exist"),
            "a missing file must say so"
        );
        let error = resolve_upload_files(&["sub".to_string()], &sessions)
            .expect_err("a directory must be refused");
        assert!(
            step_error_text(error).contains("not a regular file"),
            "a directory must be named as one"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_matched_element_is_labelled_by_its_tag_and_input_type() {
        assert_eq!(matched_element_label("div", "").as_deref(), Some("`<div>`"));
        assert_eq!(
            matched_element_label("input", "text").as_deref(),
            Some("`<input type=\"text\">`")
        );
        assert_eq!(
            matched_element_label("input", "").as_deref(),
            Some("`<input>`")
        );
        assert_eq!(matched_element_label("", ""), None);
    }

    #[test]
    fn upload_session_accepts_a_ref_and_still_requires_files() {
        let registry = registry();
        for args in [
            json!({"ref": "e1", "files": ["fixture.txt"]}),
            json!({"ref": "e1", "files": [], "timeout_ms": 250}),
        ] {
            assert!(
                registry
                    .validate_for_session("browser.upload", &args)
                    .is_ok(),
                "a ref upload must be accepted: {args}"
            );
        }
        for (label, args) in [
            ("empty ref", json!({"ref": "", "files": ["a.txt"]})),
            ("missing files", json!({"ref": "e1"})),
            ("empty path", json!({"ref": "e1", "files": [""]})),
            (
                "zero timeout",
                json!({"ref": "e1", "files": ["a.txt"], "timeout_ms": 0}),
            ),
            (
                "ref with a locator",
                json!({"ref": "e1", "css": "#file", "files": ["a.txt"]}),
            ),
        ] {
            let error = registry
                .validate_for_session("browser.upload", &args)
                .expect_err("the session arguments must be refused");
            assert!(!error.is_empty(), "{label}: {error}");
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
    fn coordinates_are_their_own_locator_form_on_click_and_type() {
        for step in ["browser.click", "browser.type"] {
            let definition = definition(step);
            let base = if step == "browser.type" {
                json!({"at": {"x": 12, "y": 34}, "text": "hello"})
            } else {
                json!({"at": {"x": 12, "y": 34}})
            };
            assert!(
                (definition.validate)(&base).is_ok(),
                "`{step}` must accept bare coordinates: {base}"
            );
            assert!(
                jsonschema::is_valid(&definition.args_schema, &base),
                "the schema must accept bare coordinates: {base}"
            );
            let pinned = if step == "browser.type" {
                json!({"at": {"x": 12, "y": 34, "viewport": {"width": 800, "height": 600}}, "value": "hello"})
            } else {
                json!({"at": {"x": 12, "y": 34, "viewport": {"width": 800, "height": 600}}})
            };
            assert!(
                (definition.validate)(&pinned).is_ok(),
                "`{step}` must accept a recorded viewport: {pinned}"
            );
            assert!(
                jsonschema::is_valid(&definition.args_schema, &pinned),
                "the schema must accept a recorded viewport: {pinned}"
            );

            for (label, extra) in [
                ("css", json!({"css": "#a"})),
                ("xpath", json!({"xpath": "//*[@id='a']"})),
                ("role", json!({"role": "button"})),
                ("testid", json!({"testid": "a"})),
                ("within", json!({"within": {"css": "#a"}})),
                ("nth", json!({"nth": 1})),
                ("name", json!({"name": "A"})),
            ] {
                let combined = add_fields(base.clone(), extra);
                let error = (definition.validate)(&combined)
                    .expect_err("a locator must not be combined with coordinates");
                assert!(
                    error.contains("cannot be combined"),
                    "{step}/{label}: {error}"
                );
                assert!(
                    !jsonschema::is_valid(&definition.args_schema, &combined),
                    "the schema must refuse `at` with {label} on `{step}`: {combined}"
                );
            }

            if step == "browser.click" {
                let combined = add_fields(base.clone(), json!({"text": "Later"}));
                assert!(
                    (definition.validate)(&combined).is_err(),
                    "`browser.click` must refuse `at` with a text locator"
                );
                assert!(
                    !jsonschema::is_valid(&definition.args_schema, &combined),
                    "the schema must refuse `at` with a text locator: {combined}"
                );
            } else {
                let combined = add_fields(base.clone(), json!({"value": "hello"}));
                assert!(
                    (definition.validate)(&combined).is_err(),
                    "`browser.type` must refuse both `text` and `value` with `at`"
                );
                assert!(
                    !jsonschema::is_valid(&definition.args_schema, &combined),
                    "the schema must refuse both `text` and `value` with `at`: {combined}"
                );
                let value_only = json!({"at": {"x": 1, "y": 2}, "value": "hello"});
                assert!(
                    (definition.validate)(&value_only).is_ok(),
                    "`browser.type` must accept `value` as the typed text with `at`"
                );
                assert!(
                    jsonschema::is_valid(&definition.args_schema, &value_only),
                    "the schema must accept `value` as the typed text with `at`: {value_only}"
                );
            }

            for (label, at) in [
                ("negative x", json!({"x": -1, "y": 2})),
                ("negative y", json!({"x": 1, "y": -2})),
                ("missing x", json!({"y": 2})),
                (
                    "zero viewport width",
                    json!({"x": 1, "y": 2, "viewport": {"width": 0, "height": 600}}),
                ),
                (
                    "zero viewport height",
                    json!({"x": 1, "y": 2, "viewport": {"width": 800, "height": 0}}),
                ),
                ("unknown at field", json!({"x": 1, "y": 2, "z": 3})),
            ] {
                let args = if step == "browser.type" {
                    json!({"at": at, "text": "hello"})
                } else {
                    json!({"at": at})
                };
                assert!(
                    (definition.validate)(&args).is_err(),
                    "`{step}` must refuse {label}: {args}"
                );
                assert!(
                    !jsonschema::is_valid(&definition.args_schema, &args),
                    "the schema must refuse {label} on `{step}`: {args}"
                );
            }
        }
    }

    #[test]
    fn coordinate_steps_warn_about_fragility_and_an_unrecorded_viewport() {
        for step in ["browser.click", "browser.type"] {
            let definition = definition(step);
            let base = if step == "browser.type" {
                json!({"at": {"x": 12, "y": 34}, "text": "hello"})
            } else {
                json!({"at": {"x": 12, "y": 34}})
            };
            let warnings = (definition.lint_warnings)(&base);
            assert_eq!(
                warnings.len(),
                2,
                "`{step}` must warn about coordinates and the missing viewport: {warnings:?}"
            );
            assert!(warnings[0].contains("coordinates"), "{warnings:?}");
            assert!(warnings[0].contains("canvas"), "{warnings:?}");
            assert!(warnings[1].contains("viewport"), "{warnings:?}");

            let pinned = if step == "browser.type" {
                json!({"at": {"x": 12, "y": 34, "viewport": {"width": 800, "height": 600}}, "text": "hello"})
            } else {
                json!({"at": {"x": 12, "y": 34, "viewport": {"width": 800, "height": 600}}})
            };
            let warnings = (definition.lint_warnings)(&pinned);
            assert_eq!(
                warnings.len(),
                1,
                "a recorded viewport must not warn about a missing one: {warnings:?}"
            );
            assert!(warnings[0].contains("coordinates"), "{warnings:?}");

            let locator = if step == "browser.type" {
                json!({"css": "#name", "text": "hello"})
            } else {
                json!({"css": "#name"})
            };
            assert!(
                (definition.lint_warnings)(&locator).is_empty(),
                "a locator step must not warn about coordinates"
            );
        }
        for step in [
            "browser.wait",
            "browser.read",
            "browser.open",
            "browser.press",
        ] {
            let definition = definition(step);
            assert!(
                (definition.lint_warnings)(&json!({})).is_empty(),
                "`{step}` has no fragile arguments"
            );
        }
    }

    #[test]
    fn a_coordinate_result_keeps_whole_numbers_and_names_the_viewport() {
        let at = AtArgs {
            x: 100.0,
            y: 12.5,
            viewport: None,
        };
        assert_eq!(coordinate_number(at.x), json!(100));
        assert_eq!(
            coordinate_result(&at, 1280, 800),
            json!({
                "x": 100,
                "y": 12.5,
                "viewport": {"width": 1280, "height": 800},
            })
        );
        assert_eq!(coordinate_target(&at), "coordinates (100, 12.5)");
    }

    #[test]
    fn timeouts_and_keys_are_checked_by_validate() {
        for step in [
            "browser.click",
            "browser.type",
            "browser.upload",
            "browser.wait",
            "browser.read",
        ] {
            let definition = definition(step);
            let mut args = match step {
                "browser.type" => json!({"css": "#a", "text": "x"}),
                "browser.upload" => json!({"css": "#a", "files": ["a.txt"]}),
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
            quoted.contains(r#"var selector = "a\"b\\c";"#)
                && quoted.contains("collectDeepElements(root)"),
            "a selector must be passed as a JSON string literal and matched over \
             the composed tree: {quoted}"
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
            at: None,
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
            at: None,
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
            at: None,
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
            let expected_branches = match step {
                "browser.wait" | "browser.click" | "browser.type" => 6,
                _ => 5,
            };
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
        assert_eq!(
            first,
            ActionablePoll::Pending(PendingReason::Occluded(Some(host.clone())))
        );
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
        assert_eq!(
            second,
            ActionablePoll::Pending(PendingReason::Occluded(Some(host)))
        );

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
        assert_eq!(
            moved_second,
            ActionablePoll::Pending(PendingReason::Unstable)
        );

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
            ActionablePoll::Pending(PendingReason::Unstable)
        );
        assert_eq!(
            evaluate_actionable_inspect(&mut fresh, ActionableInspect::NotVisible),
            ActionablePoll::Pending(PendingReason::Hidden)
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
            ActionablePoll::Pending(PendingReason::Unstable)
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
            ActionablePoll::Ready(88.0, 26.0)
        );
        assert_eq!(
            evaluate_actionable_inspect(
                &mut fresh,
                ActionableInspect::UnmappableTransform {
                    reason: "rotated CSS transform".to_string()
                }
            ),
            ActionablePoll::UnmappableTransform("rotated CSS transform".to_string())
        );
        assert!(fresh.is_none());
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
    fn attach_recovers_when_attach_to_target_stalls_silently() {
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
            assert_eq!(attach1["params"]["targetId"], "target-1");
            // Never answer Target.attachToTarget on target-1; wait for Target.closeTarget.
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
        let (page, target) = attach_with_enable_timeout(
            &mut conn,
            Deadline::after(Duration::from_secs(3)),
            Duration::from_millis(120),
        )
        .expect("attach must recover on target-2 after silent Target.attachToTarget stall");
        drop(conn);
        server.join().expect("mock server finish");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "Target.attachToTarget hang must abort at enable_timeout instead of outer deadline, took {:?}",
            started.elapsed()
        );
        assert_eq!((page.as_str(), target.as_str()), ("session-2", "target-2"));
    }

    #[test]
    fn attach_fails_fast_when_attach_to_target_stalls_every_attempt() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            for attempt in 1..=MAX_ATTACH_ATTEMPTS {
                let tid = format!("target-{attempt}");
                let create = read_cdp_request(&mut socket);
                assert_eq!(create["method"], "Target.createTarget");
                socket
                    .send(tungstenite::Message::text(
                        json!({ "id": create["id"], "result": { "targetId": tid } }).to_string(),
                    ))
                    .expect("create target");

                let attach_cmd = read_cdp_request(&mut socket);
                assert_eq!(attach_cmd["method"], "Target.attachToTarget");
                if attempt < MAX_ATTACH_ATTEMPTS {
                    let close_cmd = read_cdp_request(&mut socket);
                    assert_eq!(close_cmd["method"], "Target.closeTarget");
                    assert_eq!(close_cmd["params"]["targetId"], tid);
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
        let err = attach_with_enable_timeout(
            &mut conn,
            Deadline::after(Duration::from_secs(3)),
            Duration::from_millis(100),
        )
        .expect_err("attach must give up after MAX_ATTACH_ATTEMPTS");
        drop(conn);
        server.join().expect("mock server finish");

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "3 stalled Target.attachToTarget attempts must fail at 3 * enable_timeout, took {:?}",
            started.elapsed()
        );
        assert!(err.contains("attempt 1:"), "{err}");
        assert!(err.contains("attempt 3:"), "{err}");
        assert!(err.contains("Target.attachToTarget"), "{err}");
        assert!(err.contains("no CDP events"), "{err}");
    }

    #[test]
    fn launch_browser_with_retry_recovers_after_wedged_attach_and_respects_budget() {
        // 1. First launched browser stalls on attach; it is aborted and attempt 2 succeeds.
        let mut spawns = 0usize;
        let mut aborted = Vec::new();
        let (browser, session) = launch_browser_with_retry(
            Deadline::after(Duration::from_secs(5)),
            MAX_LAUNCH_ATTEMPTS,
            |_| {
                spawns += 1;
                Ok(format!("browser-{spawns}"))
            },
            |browser, _| {
                if browser == "browser-1" {
                    Err("Target.attachToTarget stalled".to_string())
                } else {
                    Ok("attached-2".to_string())
                }
            },
            |browser| {
                aborted.push(browser);
                Ok(())
            },
        )
        .expect("second browser launch must recover");
        assert_eq!(spawns, 2);
        assert_eq!(aborted, vec!["browser-1".to_string()]);
        assert_eq!(
            (browser.as_str(), session.as_str()),
            ("browser-2", "attached-2")
        );

        // 2. Pre-startup spawn failure (e.g. missing binary) does not retry.
        let mut spawn_calls = 0usize;
        let spawn_err = launch_browser_with_retry::<String, String>(
            Deadline::after(Duration::from_secs(5)),
            MAX_LAUNCH_ATTEMPTS,
            |_| {
                spawn_calls += 1;
                Err("could not find Chrome".to_string())
            },
            |_, _| unreachable!(),
            |_| unreachable!(),
        )
        .expect_err("spawn failure must fail immediately");
        assert_eq!(spawn_calls, 1);
        assert!(spawn_err.contains("could not find Chrome"), "{spawn_err}");

        // 3. Exhausting all launch attempts surfaces diagnostics from every attempt.
        let mut attempts = 0usize;
        let mut aborted_all = Vec::new();
        let err = launch_browser_with_retry::<String, String>(
            Deadline::after(Duration::from_secs(5)),
            MAX_LAUNCH_ATTEMPTS,
            |_| {
                attempts += 1;
                Ok(format!("b-{attempts}"))
            },
            |b, _| Err(format!("{b}: attach failed")),
            |b| {
                aborted_all.push(b);
                Ok(())
            },
        )
        .expect_err("exhausted launches must fail");
        assert_eq!(aborted_all, vec!["b-1".to_string(), "b-2".to_string()]);
        assert!(
            err.contains("launch attempt 1: b-1: attach failed"),
            "{err}"
        );
        assert!(
            err.contains("launch attempt 2: b-2: attach failed"),
            "{err}"
        );

        // 4. Exhausted outer deadline stops after attempt 1 instead of spawning past LAUNCH_TIMEOUT.
        let mut deadline_spawns = 0usize;
        let deadline_err = launch_browser_with_retry::<String, String>(
            Deadline::after(Duration::ZERO),
            MAX_LAUNCH_ATTEMPTS,
            |_| {
                deadline_spawns += 1;
                Ok("b-1".to_string())
            },
            |_, _| Err("stalled".to_string()),
            |_| Ok(()),
        )
        .expect_err("expired deadline must not spawn attempt 2");
        assert_eq!(deadline_spawns, 1);
        assert_eq!(deadline_err, "stalled");

        // 5. Abort/cleanup failure stops retrying and reports both attach and cleanup errors.
        let mut cleanup_spawns = 0usize;
        let cleanup_err = launch_browser_with_retry::<String, String>(
            Deadline::after(Duration::from_secs(5)),
            MAX_LAUNCH_ATTEMPTS,
            |_| {
                cleanup_spawns += 1;
                Ok(format!("b-{cleanup_spawns}"))
            },
            |_, _| Err("Target.attachToTarget stalled".to_string()),
            |b| Err(format!("could not kill {b}")),
        )
        .expect_err("abort failure must stop retrying and surface cleanup error");
        assert_eq!(cleanup_spawns, 1);
        assert!(
            cleanup_err.contains("Target.attachToTarget stalled"),
            "{cleanup_err}"
        );
        assert!(cleanup_err.contains("could not kill b-1"), "{cleanup_err}");
    }

    #[test]
    fn attach_stops_retrying_and_reports_target_close_cleanup_error() {
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
            // Never answer Target.attachToTarget; fail Target.closeTarget cleanup with a protocol error.
            let close1 = read_cdp_request(&mut socket);
            assert_eq!(close1["method"], "Target.closeTarget");
            assert_eq!(close1["params"]["targetId"], "target-1");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": close1["id"],
                        "error": { "code": -32000, "message": "close refused" }
                    })
                    .to_string(),
                ))
                .expect("send closeTarget error");
            while socket.read().is_ok() {}
        });

        let mut conn =
            Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2))).expect("connect");
        let err = attach_with_enable_timeout(
            &mut conn,
            Deadline::after(Duration::from_secs(3)),
            Duration::from_millis(100),
        )
        .expect_err("Target.closeTarget error must stop attach retry and report cleanup error");
        drop(conn);
        server.join().expect("mock server finish");

        assert!(err.contains("Target.attachToTarget"), "{err}");
        assert!(err.contains("Target.closeTarget"), "{err}");
        assert!(err.contains("close refused"), "{err}");
        assert!(
            !err.contains("attempt 2:"),
            "must not retry after cleanup failure: {err}"
        );
    }

    #[test]
    fn attach_treats_already_closed_target_on_cleanup_as_recoverable() {
        let (ws_url, server) = spawn_mock_cdp(|mut socket| {
            let create1 = read_cdp_request(&mut socket);
            socket
                .send(tungstenite::Message::text(
                    json!({ "id": create1["id"], "result": { "targetId": "target-1" } })
                        .to_string(),
                ))
                .expect("create target-1");

            let attach1 = read_cdp_request(&mut socket);
            assert_eq!(attach1["method"], "Target.attachToTarget");
            let close1 = read_cdp_request(&mut socket);
            assert_eq!(close1["method"], "Target.closeTarget");
            socket
                .send(tungstenite::Message::text(
                    json!({
                        "id": close1["id"],
                        "error": { "code": -32000, "message": "No target with given id found" }
                    })
                    .to_string(),
                ))
                .expect("send No target error");

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
                    .expect("reply domain enable");
            }
            while socket.read().is_ok() {}
        });

        let mut conn =
            Connection::connect(&ws_url, Deadline::after(Duration::from_secs(2))).expect("connect");
        let (page, target) = attach_with_enable_timeout(
            &mut conn,
            Deadline::after(Duration::from_secs(3)),
            Duration::from_millis(100),
        )
        .expect("already-closed target on cleanup must still allow retry");
        drop(conn);
        server.join().expect("mock server finish");
        assert_eq!((page.as_str(), target.as_str()), ("session-2", "target-2"));
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
            tabs: tabs::Tabs::attached(target_id),
            page: session_id.to_string(),
            network: network::NetworkState::new(session_id),
            port: 9222,
            websocket_url: "ws://127.0.0.1:9222/devtools/browser/mock".to_string(),
            headless: true,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            connect: Some("http://127.0.0.1:9222".to_string()),
            profile: None,
            pending_local_storage_scripts: Vec::new(),
            testid_attribute: DEFAULT_TESTID_ATTRIBUTE.to_string(),
            refs: snapshot::RefStore::default(),
            step_metadata: None,
            snapshot_pending: false,
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

        // `browser.open` starts tracking the tabs a page action opens.
        let discover = read_cdp_request(socket);
        assert_eq!(discover["method"], "Target.setDiscoverTargets");
        send_mock_reply(socket, &discover["id"], json!({}));

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
            &[LoadedCase::new("mock-browser-retry.yaml".into(), case)],
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

    fn send_mock_network_request(
        socket: &mut MockSocket,
        request_id: &str,
        url: &str,
        status: i64,
    ) {
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
    }

    #[test]
    fn retry_with_browser_network_discards_failed_attempt_requests() {
        use crate::core::case::{Expectation, Matcher, Observation, RetryPolicy};
        use crate::core::report::CaseStatus;
        use serde_saphyr::Location;

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
                    send_mock_network_request(sock, "req-doc", "http://127.0.0.1/app", 200);
                });

                // attempt 1: Execution (ambiguous)
                let attempt1 = read_cdp_request(&mut socket);
                assert_eq!(attempt1["method"], "Runtime.evaluate");
                send_mock_network_request(
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
                send_mock_network_request(
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

                // The terminal timeout names a closed shadow root when the page
                // has one, so it asks the DOM agent before the attempt fails.
                let probe = read_cdp_request(&mut socket);
                assert_eq!(probe["method"], "DOM.getDocument");
                send_mock_reply(
                    &mut socket,
                    &probe["id"],
                    json!({ "result": { "root": { "nodeId": 1, "nodeType": 9, "nodeName": "#document" } } }),
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
                send_mock_network_request(
                    &mut socket,
                    "req-ok",
                    "http://127.0.0.1/attempt3-ok",
                    200,
                );
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

    #[test]
    fn validate_detached_handle_rejects_incomplete_and_inconsistent_launched_handles() {
        let temp_root = std::env::temp_dir();
        let owned_profile = temp_root
            .join(format!("{}-9999-0", launch::PROFILE_PREFIX))
            .to_string_lossy()
            .into_owned();
        let base = || DetachedBrowserHandle {
            mode: "launched".to_string(),
            websocket_url: "ws://127.0.0.1:9222/devtools/browser/abc".to_string(),
            target_id: "target-1".to_string(),
            port: 9222,
            pid: Some(4242),
            profile: Some(owned_profile.clone()),
            profile_name: None,
            headless: true,
            width: 1280,
            height: 800,
            connect: None,
            testid_attribute: DEFAULT_TESTID_ATTRIBUTE.to_string(),
            refs: snapshot::RefStore::default(),
            pending_local_storage_scripts: Vec::new(),
            tabs: Vec::new(),
        };

        assert!(matches!(
            validate_detached_handle(&base(), &temp_root),
            Ok(ValidatedHandle::Launched {
                pid: 4242,
                port: 9222,
                ..
            })
        ));

        let mut missing_pid = base();
        missing_pid.pid = None;
        assert!(validate_detached_handle(&missing_pid, &temp_root).is_err());

        let mut zero_pid = base();
        zero_pid.pid = Some(0);
        assert!(validate_detached_handle(&zero_pid, &temp_root).is_err());

        let mut missing_profile = base();
        missing_profile.profile = None;
        assert!(validate_detached_handle(&missing_profile, &temp_root).is_err());

        let mut unowned_profile = base();
        unowned_profile.profile = Some(temp_root.join("unrelated").to_string_lossy().into_owned());
        assert!(validate_detached_handle(&unowned_profile, &temp_root).is_err());

        let mut zero_port = base();
        zero_port.port = 0;
        assert!(validate_detached_handle(&zero_port, &temp_root).is_err());

        let mut empty_target = base();
        empty_target.target_id = "  ".to_string();
        assert!(validate_detached_handle(&empty_target, &temp_root).is_err());

        let mut hybrid_connect = base();
        hybrid_connect.connect = Some("http://127.0.0.1:9222".to_string());
        assert!(validate_detached_handle(&hybrid_connect, &temp_root).is_err());

        let mut mismatched_port = base();
        mismatched_port.websocket_url = "ws://127.0.0.1:9333/devtools/browser/abc".to_string();
        assert!(validate_detached_handle(&mismatched_port, &temp_root).is_err());

        let mut remote_ws = base();
        remote_ws.websocket_url = "ws://192.0.2.10:9222/devtools/browser/abc".to_string();
        assert!(validate_detached_handle(&remote_ws, &temp_root).is_err());
    }

    #[test]
    fn validate_detached_handle_preserves_connected_semantics_and_rejects_hybrid_fields() {
        let temp_root = std::env::temp_dir();
        let owned_profile = temp_root
            .join(format!("{}-9999-1", launch::PROFILE_PREFIX))
            .to_string_lossy()
            .into_owned();
        let base = || DetachedBrowserHandle {
            mode: "connected".to_string(),
            websocket_url: "ws://127.0.0.1:9333/devtools/browser/mock".to_string(),
            target_id: "target-1".to_string(),
            port: 9222,
            pid: None,
            profile: None,
            profile_name: None,
            headless: true,
            width: 1280,
            height: 800,
            connect: Some("http://127.0.0.1:9222".to_string()),
            testid_attribute: DEFAULT_TESTID_ATTRIBUTE.to_string(),
            refs: snapshot::RefStore::default(),
            pending_local_storage_scripts: Vec::new(),
            tabs: Vec::new(),
        };

        assert!(matches!(
            validate_detached_handle(&base(), &temp_root),
            Ok(ValidatedHandle::Connected { port: 9222, .. })
        ));

        let mut with_pid = base();
        with_pid.pid = Some(4242);
        assert!(validate_detached_handle(&with_pid, &temp_root).is_err());

        let mut with_profile = base();
        with_profile.profile = Some(owned_profile);
        assert!(validate_detached_handle(&with_profile, &temp_root).is_err());

        let mut no_connect = base();
        no_connect.connect = None;
        assert!(validate_detached_handle(&no_connect, &temp_root).is_err());

        let mut remote_ws = base();
        remote_ws.websocket_url = "ws://192.0.2.10:9333/devtools/browser/mock".to_string();
        assert!(validate_detached_handle(&remote_ws, &temp_root).is_err());
    }

    #[test]
    fn dispose_handle_with_rejects_bad_handles_and_preserves_connected_ownership() {
        let temp_root = std::env::temp_dir();
        let owned_profile = temp_root
            .join(format!("{}-8888-0", launch::PROFILE_PREFIX))
            .to_string_lossy()
            .into_owned();

        let mut record_calls = 0usize;
        let mut close_browser_calls = 0usize;
        let mut close_target_calls = 0usize;
        let mut dispose_detached_calls = 0usize;

        // 1. Incomplete launched handle must fail before any callback runs.
        let incomplete = json!({
            "mode": "launched",
            "websocket_url": "ws://127.0.0.1:9222/devtools/browser/abc",
            "target_id": "target-1",
            "port": 9222,
            "pid": null,
            "profile": owned_profile,
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": null,
            "testid_attribute": "data-testid",
            "refs": {}
        });
        assert!(
            dispose_handle_with(
                &incomplete,
                &temp_root,
                |_| record_calls += 1,
                |_| true,
                |_, _, _| Ok(true),
                |_, _| close_browser_calls += 1,
                |_, _, _| close_target_calls += 1,
                |_, _, _| {
                    dispose_detached_calls += 1;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(record_calls, 0);
        assert_eq!(close_browser_calls, 0);
        assert_eq!(close_target_calls, 0);
        assert_eq!(dispose_detached_calls, 0);

        // 2. Open port with unverified pid must refuse Browser.close and dispose_detached.
        let unverified = json!({
            "mode": "launched",
            "websocket_url": "ws://127.0.0.1:9222/devtools/browser/abc",
            "target_id": "target-1",
            "port": 9222,
            "pid": 4242,
            "profile": owned_profile,
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": null,
            "testid_attribute": "data-testid",
            "refs": {}
        });
        assert!(
            dispose_handle_with(
                &unverified,
                &temp_root,
                |_| record_calls += 1,
                |_| true,
                |_, _, _| Err("pid 4242 does not match temporary profile".to_string()),
                |_, _| close_browser_calls += 1,
                |_, _, _| close_target_calls += 1,
                |_, _, _| {
                    dispose_detached_calls += 1;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(close_browser_calls, 0);
        assert_eq!(close_target_calls, 0);
        assert_eq!(dispose_detached_calls, 0);

        // 3. Connected handle closes only its target tab and never sends Browser.close or disposes.
        let connected = json!({
            "mode": "connected",
            "websocket_url": "ws://127.0.0.1:9222/devtools/browser/abc",
            "target_id": "target-tab-1",
            "port": 9222,
            "pid": null,
            "profile": null,
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": "http://127.0.0.1:9222",
            "testid_attribute": "data-testid",
            "refs": {}
        });
        assert!(
            dispose_handle_with(
                &connected,
                &temp_root,
                |_| record_calls += 1,
                |_| true,
                |_, _, _| Ok(true),
                |_, _| close_browser_calls += 1,
                |_, target_id, _| {
                    assert_eq!(target_id, "target-tab-1");
                    close_target_calls += 1;
                },
                |_, _, _| {
                    dispose_detached_calls += 1;
                    Ok(())
                },
            )
            .is_ok()
        );
        assert_eq!(close_target_calls, 1);
        assert_eq!(close_browser_calls, 0);
        assert_eq!(dispose_detached_calls, 0);
    }

    #[test]
    fn reattach_rejects_invalid_handle_before_connecting() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind listener");
        let port = listener.local_addr().expect("port").port();
        listener.set_nonblocking(true).expect("nonblocking");

        let bad_handle = json!({
            "mode": "launched",
            "websocket_url": format!("ws://127.0.0.1:{port}/devtools/browser/mock"),
            "target_id": "tab-1",
            "port": port,
            "pid": 4242,
            "profile": std::env::temp_dir().join("not-da-profile").to_string_lossy(),
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": null,
            "testid_attribute": DEFAULT_TESTID_ATTRIBUTE,
            "refs": {}
        });
        let err = match BrowserSession::reattach(&bad_handle) {
            Ok(_) => panic!("unowned profile handle must fail reattach before connecting"),
            Err(err) => err,
        };
        assert!(err.contains("refusing"), "{err}");
        assert!(
            matches!(listener.accept(), Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "reattach must not open a TCP/WS connection on an invalid handle"
        );
    }

    #[test]
    fn connected_session_detach_reattach_and_dispose_closes_only_target_tab() {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("must bind mock CDP listener");
        let port = listener.local_addr().expect("port").port();
        let ws_url = format!("ws://127.0.0.1:{port}/devtools/browser/mock");
        let server = std::thread::spawn(move || {
            // Connection 1: BrowserSession::reattach
            let (stream1, _) = listener.accept().expect("accept reattach connection");
            let _ = stream1.set_nodelay(true);
            let mut socket1 = tungstenite::accept(stream1).expect("handshake 1");
            let attach_cmd = read_cdp_request(&mut socket1);
            assert_eq!(attach_cmd["method"], "Target.attachToTarget");
            assert_eq!(attach_cmd["params"]["targetId"], "tab-connected");
            socket1
                .send(tungstenite::Message::text(
                    json!({
                        "id": attach_cmd["id"],
                        "result": { "sessionId": "session-1" }
                    })
                    .to_string(),
                ))
                .expect("send attach reply");

            for method in ["Page.enable", "Runtime.enable", "Network.enable"] {
                let req = read_cdp_request(&mut socket1);
                assert_eq!(req["method"], method);
                socket1
                    .send(tungstenite::Message::text(
                        json!({ "id": req["id"], "result": {} }).to_string(),
                    ))
                    .expect("send domain reply");
            }
            // A reattached session resumes tracking the tabs this case owns and
            // reads their current URL and title.
            let discover = read_cdp_request(&mut socket1);
            assert_eq!(discover["method"], "Target.setDiscoverTargets");
            socket1
                .send(tungstenite::Message::text(
                    json!({ "id": discover["id"], "result": {} }).to_string(),
                ))
                .expect("send discover reply");
            let targets = read_cdp_request(&mut socket1);
            assert_eq!(targets["method"], "Target.getTargets");
            socket1
                .send(tungstenite::Message::text(
                    json!({
                        "id": targets["id"],
                        "result": {
                            "targetInfos": [{
                                "targetId": "tab-connected",
                                "type": "page",
                                "url": "http://127.0.0.1/app",
                                "title": "App"
                            }]
                        }
                    })
                    .to_string(),
                ))
                .expect("send targets reply");
            while socket1.read().is_ok() {}

            // Connection 2: BrowserSession::dispose_handle
            let (stream2, _) = listener.accept().expect("accept dispose connection");
            let _ = stream2.set_nodelay(true);
            let mut socket2 = tungstenite::accept(stream2).expect("handshake 2");
            let close_target = read_cdp_request(&mut socket2);
            assert_eq!(
                close_target["method"], "Target.closeTarget",
                "connected dispose_handle must only close its tab, never Browser.close"
            );
            assert_eq!(close_target["params"]["targetId"], "tab-connected");
            socket2
                .send(tungstenite::Message::text(
                    json!({ "id": close_target["id"], "result": { "success": true } }).to_string(),
                ))
                .expect("send closeTarget reply");
            while socket2.read().is_ok() {}
        });

        let handle = json!({
            "mode": "connected",
            "websocket_url": ws_url,
            "target_id": "tab-connected",
            "port": port,
            "pid": null,
            "profile": null,
            "headless": true,
            "width": 1280,
            "height": 800,
            "connect": format!("http://127.0.0.1:{port}"),
            "testid_attribute": DEFAULT_TESTID_ATTRIBUTE,
            "refs": {}
        });

        let mut reattached =
            BrowserSession::reattach(&handle).expect("connected handle must reattach");
        let re_detached = reattached
            .detach()
            .expect("detach succeeds")
            .expect("detach returns handle");
        assert!(re_detached["pid"].is_null());
        assert!(re_detached["profile"].is_null());
        assert_eq!(re_detached["mode"], "connected");

        BrowserSession::dispose_handle(&re_detached).expect("dispose connected handle succeeds");
        server.join().expect("mock CDP server finishes");
    }

    #[test]
    fn iframe_container_status_and_frame_loading_timeout_messages_parse_and_format() {
        assert!(matches!(
            serde_json::from_str::<WithinContainerStatus>(r#"{"status":"missing"}"#).ok(),
            Some(WithinContainerStatus::Missing)
        ));
        assert!(matches!(
            serde_json::from_str::<WithinContainerStatus>(r#"{"status":"loading"}"#).ok(),
            Some(WithinContainerStatus::Loading)
        ));
        assert!(matches!(
            serde_json::from_str::<WithinContainerStatus>(r#"{"status":"cross_origin"}"#).ok(),
            Some(WithinContainerStatus::CrossOrigin)
        ));

        let err = pending_timeout_error(
            "CSS selector `#lazy-frame`",
            PendingReason::FrameLoading,
            Deadline::after(Duration::from_millis(1200)),
        );
        assert_eq!(
            step_error_text(err),
            "frame CSS selector `#lazy-frame` did not finish loading within 1200 ms"
        );

        let xpath = Locator::Xpath("//button".to_string())
            .resolve_body_with(DEFAULT_TESTID_ATTRIBUTE, None)
            .expect("xpath locator has a page expression");
        assert!(
            xpath.contains("root.nodeType === 9 ? root : (root.ownerDocument || document)")
                && xpath.contains("doc.evaluate("),
            "xpath expression must evaluate against root's ownerDocument: {xpath}"
        );

        let unmappable: ActionableInspect = serde_json::from_str(
            r#"{"status":"unmappable_transform","reason":"rotated CSS transform"}"#,
        )
        .expect("unmappable_transform status must deserialize");
        assert!(matches!(
            unmappable,
            ActionableInspect::UnmappableTransform { ref reason } if reason == "rotated CSS transform"
        ));
    }

    #[test]
    fn retry_within_iframe_isolates_network_and_recovers_from_unexpected_dialog() {
        use crate::core::case::{Expectation, Matcher, Observation, RetryPolicy};
        use crate::core::report::CaseStatus;
        use serde_saphyr::Location;

        let mut step = mock_read_step(
            5000,
            RetryPolicy {
                until: vec![Expectation {
                    path: "$.text".to_string(),
                    matcher: Matcher::Equals(json!("ready")),
                }],
                timeout_ms: 3000,
                interval_ms: 10,
            },
        );
        step.args = json!({
            "css": "#status",
            "within": { "css": "#embed" },
            "timeout_ms": 5000
        });
        step.expect = vec![Expectation {
            path: "$.observed.net.requests".to_string(),
            matcher: Matcher::Equals(json!([{
                "method": "GET",
                "url": "http://127.0.0.1/attempt3-iframe-ok",
                "status": 200,
                "type": "Fetch",
                "failed": null
            }])),
        }];

        let report = run_mock_browser_retry_case(
            "iframe retry network and dialog isolation",
            vec![Observation {
                name: "net".to_string(),
                kind: network::OBSERVER_NAME.to_string(),
                args: json!({}),
                location: Location::UNKNOWN,
                args_location: Location::UNKNOWN,
            }],
            step,
            |mut socket| {
                mock_open_handshake(&mut socket, |sock| {
                    send_mock_network_request(sock, "req-doc", "http://127.0.0.1/app", 200);
                });

                // Attempt 1: resolves #embed, then enter_within_container meets an unexpected alert
                let a1_frame = read_cdp_request(&mut socket);
                assert_eq!(a1_frame["method"], "Runtime.evaluate");
                send_mock_reply(
                    &mut socket,
                    &a1_frame["id"],
                    json!({ "result": { "objectId": "frame-1" } }),
                );
                let a1_enter = read_cdp_request(&mut socket);
                assert_eq!(a1_enter["method"], "Runtime.callFunctionOn");
                send_mock_network_request(
                    &mut socket,
                    "req-a1",
                    "http://127.0.0.1/attempt1-iframe-dialog",
                    500,
                );
                send_mock_dialog(&mut socket, "alert", "iframe-transient");
                let handle = read_cdp_request(&mut socket);
                assert_eq!(handle["method"], "Page.handleJavaScriptDialog");
                assert_eq!(handle["params"]["accept"], false);
                send_mock_reply(&mut socket, &handle["id"], json!({}));
                send_mock_reply(
                    &mut socket,
                    &a1_enter["id"],
                    json!({ "result": { "value": null } }),
                );

                // Attempt 2: enters #embed, reads "pending" -> `until` fails and drains req-a2
                let a2_frame = read_cdp_request(&mut socket);
                assert_eq!(a2_frame["method"], "Runtime.evaluate");
                send_mock_reply(
                    &mut socket,
                    &a2_frame["id"],
                    json!({ "result": { "objectId": "frame-2" } }),
                );
                let a2_enter = read_cdp_request(&mut socket);
                assert_eq!(a2_enter["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &a2_enter["id"],
                    json!({ "result": { "objectId": "frame-doc-2" } }),
                );
                let a2_inner = read_cdp_request(&mut socket);
                assert_eq!(a2_inner["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &a2_inner["id"],
                    json!({ "result": { "objectId": "elem-2" } }),
                );
                let a2_vis = read_cdp_request(&mut socket);
                assert_eq!(a2_vis["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &a2_vis["id"],
                    json!({ "result": { "value": true } }),
                );
                let a2_text = read_cdp_request(&mut socket);
                assert_eq!(a2_text["method"], "Runtime.callFunctionOn");
                send_mock_network_request(
                    &mut socket,
                    "req-a2",
                    "http://127.0.0.1/attempt2-iframe-pending",
                    200,
                );
                send_mock_reply(
                    &mut socket,
                    &a2_text["id"],
                    json!({ "result": { "value": "pending" } }),
                );

                // Attempt 3: enters #embed, reads "ready" -> succeeds with only req-a3 observed
                let a3_frame = read_cdp_request(&mut socket);
                assert_eq!(a3_frame["method"], "Runtime.evaluate");
                send_mock_reply(
                    &mut socket,
                    &a3_frame["id"],
                    json!({ "result": { "objectId": "frame-3" } }),
                );
                let a3_enter = read_cdp_request(&mut socket);
                assert_eq!(a3_enter["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &a3_enter["id"],
                    json!({ "result": { "objectId": "frame-doc-3" } }),
                );
                let a3_inner = read_cdp_request(&mut socket);
                assert_eq!(a3_inner["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &a3_inner["id"],
                    json!({ "result": { "objectId": "elem-3" } }),
                );
                let a3_vis = read_cdp_request(&mut socket);
                assert_eq!(a3_vis["method"], "Runtime.callFunctionOn");
                send_mock_reply(
                    &mut socket,
                    &a3_vis["id"],
                    json!({ "result": { "value": true } }),
                );
                let a3_text = read_cdp_request(&mut socket);
                assert_eq!(a3_text["method"], "Runtime.callFunctionOn");
                send_mock_network_request(
                    &mut socket,
                    "req-a3",
                    "http://127.0.0.1/attempt3-iframe-ok",
                    200,
                );
                send_mock_reply(
                    &mut socket,
                    &a3_text["id"],
                    json!({ "result": { "value": "ready" } }),
                );

                mock_close_loop(&mut socket);
            },
        );
        assert_eq!(report.cases[0].status, CaseStatus::Passed, "{report:?}");
        let result = report.cases[0].steps[1]
            .result
            .as_ref()
            .expect("browser.read result must exist");
        assert_eq!(result["text"], "ready");
        assert_eq!(
            result["observed"]["net"]["requests"],
            json!([{
                "method": "GET",
                "url": "http://127.0.0.1/attempt3-iframe-ok",
                "status": 200,
                "type": "Fetch",
                "failed": null
            }]),
            "stale iframe retry requests leaked into final observation: {result}"
        );
    }
    /// A fresh directory under the temporary root, removed by the caller.
    fn storage_temp_dir(label: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "da-storage-state-{}-{}-{label}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("storage-state test directory must be created");
        dir
    }

    fn sample_storage_state() -> StorageStateFile {
        StorageStateFile {
            version: STORAGE_STATE_VERSION,
            cookies: vec![CookieArg {
                name: "sid".to_string(),
                value: "s3cr3t-session".to_string(),
                url: None,
                domain: Some("127.0.0.1".to_string()),
                path: Some("/".to_string()),
                http_only: Some(true),
                secure: Some(false),
                same_site: Some(CookieSameSite::Lax),
            }],
            local_storage: BTreeMap::from([(
                "http://127.0.0.1:8080".to_string(),
                BTreeMap::from([("auth_token".to_string(), "s3cr3t-token".to_string())]),
            )]),
        }
    }

    #[test]
    fn storage_state_round_trips_through_a_private_file() {
        let dir = storage_temp_dir("round-trip");
        let path = dir.join("nested/state.json");

        write_storage_state(&path, &sample_storage_state())
            .expect("the storage-state file must be written");

        let text = std::fs::read_to_string(&path).expect("the state file must be readable");
        assert!(text.contains("\"version\": 1"), "got: {text}");
        assert!(text.contains("\"same_site\": \"Lax\""), "got: {text}");
        assert!(
            text.contains("http://127.0.0.1:8080"),
            "the origin must be recorded: {text}"
        );

        let loaded = load_storage_state(&path).expect("the written file must load");
        assert_eq!(loaded.version, STORAGE_STATE_VERSION);
        assert_eq!(loaded.cookies.len(), 1);
        assert_eq!(loaded.cookies[0].name, "sid");
        assert_eq!(loaded.cookies[0].value, "s3cr3t-session");
        assert_eq!(loaded.cookies[0].domain.as_deref(), Some("127.0.0.1"));
        assert_eq!(loaded.cookies[0].http_only, Some(true));
        assert_eq!(loaded.cookies[0].same_site, Some(CookieSameSite::Lax));
        assert_eq!(
            loaded.local_storage["http://127.0.0.1:8080"]["auth_token"],
            "s3cr3t-token"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("the state file metadata must be readable")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "only the owner may read the state file");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn storage_state_error(path: &Path) -> String {
        match load_storage_state(path) {
            Ok(_) => panic!("the storage state must be refused: {}", path.display()),
            Err(StepError::Environment(message)) => message,
            Err(other) => {
                panic!("a storage-state file must fail as an environment error: {other:?}")
            }
        }
    }

    #[test]
    fn loading_storage_state_fails_closed_without_leaking_values() {
        let dir = storage_temp_dir("fail-closed");

        let missing = dir.join("missing.json");
        let message = storage_state_error(&missing);
        assert!(
            message.contains("could not read browser storage state")
                && message.contains("missing.json"),
            "got: {message}"
        );

        let directory = dir.join("state-dir");
        std::fs::create_dir_all(&directory).expect("the fixture directory must be created");
        let message = storage_state_error(&directory);
        assert!(message.contains("is not a regular file"), "got: {message}");

        let malformed = dir.join("malformed.json");
        std::fs::write(&malformed, "{\"cookies\": [{\"name\": \"sid\", ").expect("fixture");
        let message = storage_state_error(&malformed);
        assert!(message.contains("is malformed"), "got: {message}");

        let cases = [
            (
                "version",
                json!({"version": 2, "cookies": [{"name": "sid", "value": "s3cr3t-session", "url": "http://127.0.0.1/"}]}),
                "unsupported `version`",
            ),
            ("empty", json!({}), "neither `cookies` nor `local_storage`"),
            (
                "cookie",
                json!({"cookies": [{"name": "sid", "value": "s3cr3t-session"}]}),
                "must specify `url` or `domain`",
            ),
            (
                "origin",
                json!({"local_storage": {"current": {"auth_token": "s3cr3t-token"}}}),
                "not valid in a storage-state file",
            ),
            (
                "unknown",
                json!({"cookies": [{"name": "sid", "value": "s3cr3t-session", "url": "http://127.0.0.1/"}], "other": []}),
                "is malformed",
            ),
        ];
        for (label, content, expected) in cases {
            let path = dir.join(format!("{label}.json"));
            std::fs::write(&path, content.to_string()).expect("fixture");
            let message = storage_state_error(&path);
            assert!(
                message.contains(expected),
                "case `{label}` must name `{expected}`: {message}"
            );
            assert!(
                !message.contains("s3cr3t-"),
                "the error must not repeat cookie or token values: {message}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn storage_state_arguments_are_checked_before_a_browser_starts() {
        let open = definition("browser.open");
        let save = definition("browser.save_state");

        assert!(
            (open.validate)(&json!({
                "url": "http://127.0.0.1:8080/orders",
                "storage_state": "state/admin.json"
            }))
            .is_ok()
        );
        for bad in [
            json!({"url": "http://127.0.0.1:8080/", "storage_state": null}),
            json!({"url": "http://127.0.0.1:8080/", "storage_state": ""}),
            json!({
                "url": "http://127.0.0.1:8080/",
                "connect": "http://127.0.0.1:9222",
                "storage_state": "state/admin.json"
            }),
        ] {
            assert!(
                (open.validate)(&bad).is_err(),
                "`browser.open` must refuse {bad}"
            );
        }

        assert!((save.validate)(&json!({"path": "state/admin.json"})).is_ok());
        for bad in [json!({}), json!({"path": ""}), json!({"path": null})] {
            assert!(
                (save.validate)(&bad).is_err(),
                "`browser.save_state` must refuse {bad}"
            );
        }
    }

    #[test]
    fn cookies_from_cdp_keep_the_set_cookies_shape() {
        let reply = json!({
            "cookies": [
                {
                    "name": "sid",
                    "value": "s3cr3t-session",
                    "domain": "127.0.0.1",
                    "path": "/app",
                    "httpOnly": true,
                    "secure": false,
                    "sameSite": "Strict",
                    "session": true,
                    "priority": "Medium"
                },
                {
                    "name": "empty",
                    "value": "",
                    "domain": "127.0.0.1",
                    "path": "/"
                }
            ]
        });

        let cookies = cookies_from_cdp(&reply, "http://127.0.0.1:8080");
        assert_eq!(cookies.len(), 1, "an empty cookie value must be skipped");
        assert_eq!(cookies[0].name, "sid");
        assert_eq!(cookies[0].domain.as_deref(), Some("127.0.0.1"));
        assert_eq!(cookies[0].path.as_deref(), Some("/app"));
        assert_eq!(cookies[0].url, None);
        assert_eq!(cookies[0].http_only, Some(true));
        assert_eq!(cookies[0].secure, Some(false));
        assert_eq!(cookies[0].same_site, Some(CookieSameSite::Strict));

        let host_only = cookies_from_cdp(
            &json!({"cookies": [{"name": "sid", "value": "v", "path": "/app"}]}),
            "http://127.0.0.1:8080",
        );
        assert_eq!(
            host_only[0].url.as_deref(),
            Some("http://127.0.0.1:8080/app")
        );
        assert_eq!(host_only[0].domain, None);
    }

    #[test]
    fn redact_set_cookies_args_masks_values_and_keeps_env_and_var_placeholders() {
        let input = json!({
            "cookies": [
                { "name": "sid", "value": "s3cr3t-session", "url": "http://127.0.0.1/" },
                { "name": "env_ck", "value": "${env:MY_COOKIE}", "url": "http://127.0.0.1/" },
                { "name": "var_ck", "value": "${tok}", "url": "http://127.0.0.1/" },
                { "name": "mixed_ck", "value": "Bearer ${tok}", "url": "http://127.0.0.1/" }
            ],
            "local_storage": {
                "http://127.0.0.1": {
                    "auth_token": "s3cr3t-token",
                    "env_tok": "${env:TOK}",
                    "var_tok": "${saved_tok}"
                },
                "current": { "k": "inline-val" }
            }
        });
        let scrubbed = redact_set_cookies_args(&input, true);
        assert_eq!(scrubbed["cookies"][0]["name"], "sid");
        assert_eq!(scrubbed["cookies"][0]["value"], "***");
        assert_eq!(scrubbed["cookies"][0]["url"], "http://127.0.0.1/");
        assert_eq!(scrubbed["cookies"][1]["value"], "${env:MY_COOKIE}");
        assert_eq!(scrubbed["cookies"][2]["value"], "${tok}");
        assert_eq!(scrubbed["cookies"][3]["value"], "***");
        assert_eq!(
            scrubbed["local_storage"]["http://127.0.0.1"]["auth_token"],
            "***"
        );
        assert_eq!(
            scrubbed["local_storage"]["http://127.0.0.1"]["env_tok"],
            "${env:TOK}"
        );
        assert_eq!(
            scrubbed["local_storage"]["http://127.0.0.1"]["var_tok"],
            "${saved_tok}"
        );
        assert_eq!(scrubbed["local_storage"]["current"]["k"], "***");

        let resolved_scrubbed = redact_set_cookies_args(&input, false);
        assert_eq!(resolved_scrubbed["cookies"][1]["value"], "***");
        assert_eq!(resolved_scrubbed["cookies"][2]["value"], "***");
        assert_eq!(
            resolved_scrubbed["local_storage"]["http://127.0.0.1"]["env_tok"],
            "***"
        );
        assert_eq!(
            resolved_scrubbed["local_storage"]["http://127.0.0.1"]["var_tok"],
            "***"
        );
    }
}

#[cfg(test)]
mod navigation_timeout_tests {
    /// The value is clamped to a sane range so a typo cannot hang or truncate a case.
    fn parsed(raw: &str) -> Option<u64> {
        raw.trim()
            .parse::<u64>()
            .ok()
            .filter(|value| (1_000..=600_000).contains(value))
    }

    #[test]
    fn the_navigation_override_accepts_only_sane_values() {
        assert_eq!(parsed("90000"), Some(90_000));
        assert_eq!(parsed(" 1500 "), Some(1_500));
        assert_eq!(parsed("999"), None, "under one second is a typo");
        assert_eq!(parsed("600001"), None, "over ten minutes is a typo");
        assert_eq!(parsed("soon"), None);
        assert_eq!(parsed(""), None);
    }
}
