//! The `browser.*` steps: drive one Chrome page across a case.
//!
//! The first `browser.open` of a case creates a [`BrowserSession`] through the
//! session contract. Every later step of the same case asks [`Sessions`] for the
//! same session, so it sees the page the first step opened; the runner closes
//! the browser when the case ends, whatever the case's result was.
//!
//! Locators are deterministic on purpose: a CSS selector or an XPath
//! expression, never a coordinate or a screenshot. Element lookup waits for the
//! element to appear and to be visible, so a step does not race the page's own
//! scripts.
//!
//! The protocol lives in [`cdp`] and the process in [`launch`]; this module turns
//! their messages into step results and step errors.

mod cdp;
mod launch;

use std::io::Write;
use std::time::Duration;

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use self::cdp::{CdpError, Connection, Deadline};
use self::launch::Launched;
use crate::core::registry::{Registry, StepDefinition, StepError};
use crate::core::session::{Session, Sessions};

/// Session kind the browser steps share.
pub const SESSION_KIND: &str = "browser";

/// Step names, in the order they are registered.
pub const STEP_NAMES: [&str; 6] = [
    "browser.open",
    "browser.click",
    "browser.type",
    "browser.press",
    "browser.wait",
    "browser.read",
];

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_NAVIGATION_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;
/// How long starting the browser and opening its page may take.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
/// How often an element or URL is looked at again.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Registers every browser step.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    for definition in [
        step::<OpenArgs>(
            STEP_NAMES[0],
            "Opens `url` in the case's browser and returns the page's `url` and `title`.",
            validate_open,
            run_open,
        )?,
        locator_step::<ClickArgs>(
            STEP_NAMES[1],
            "Clicks the element the locator names and returns its `text` when it has one.",
            validate_click,
            run_click,
        )?,
        locator_step::<TypeArgs>(
            STEP_NAMES[2],
            "Types `text` into the element the locator names.",
            validate_type,
            run_type,
        )?,
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
    ] {
        registry.register(definition)?;
    }
    Ok(())
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

/// Builds a step whose locator arguments are exactly one of `css` and `xpath`.
fn locator_step<T: JsonSchema>(
    name: &'static str,
    description: &'static str,
    validate: fn(&Value) -> Result<(), String>,
    run: fn(&Value, &mut Sessions) -> Result<Value, StepError>,
) -> Result<StepDefinition, String> {
    let mut definition = step::<T>(name, description, validate, run)?;
    string_property(&mut definition.args_schema, "css");
    string_property(&mut definition.args_schema, "xpath");
    exactly_one_of(&mut definition.args_schema, &["css", "xpath"]);
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
    for property in ["css", "xpath", "url_contains"] {
        string_property(&mut definition.args_schema, property);
    }
    exactly_one_of(
        &mut definition.args_schema,
        &["css", "xpath", "url_contains"],
    );
    Ok(definition)
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
    /// How long to wait for the element, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// Text to type into the element.
    text: String,
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
    /// Text the current URL must contain.
    #[serde(default)]
    url_contains: Option<String>,
    /// How long to wait, in milliseconds. Defaults to 5000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
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

/// A deterministic way to name one element.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Locator {
    /// A CSS selector.
    Css(String),
    /// An XPath expression.
    Xpath(String),
}

impl Locator {
    /// Reads exactly one locator from two optional arguments.
    fn from_args(css: &Option<String>, xpath: &Option<String>) -> Result<Self, String> {
        match (css.as_deref(), xpath.as_deref()) {
            (Some(_), Some(_)) => Err(
                "invalid arguments: `css` and `xpath` cannot both be set; give exactly one locator"
                    .to_string(),
            ),
            (Some(selector), None) if selector.trim().is_empty() => {
                Err("invalid arguments: `css` must not be empty".to_string())
            }
            (None, Some(expression)) if expression.trim().is_empty() => {
                Err("invalid arguments: `xpath` must not be empty".to_string())
            }
            (Some(selector), None) => Ok(Self::Css(selector.to_string())),
            (None, Some(expression)) => Ok(Self::Xpath(expression.to_string())),
            (None, None) => {
                Err("invalid arguments: give one locator: `css` or `xpath`".to_string())
            }
        }
    }

    /// The locator as an error message names it.
    fn describe(&self) -> String {
        match self {
            Self::Css(selector) => format!("CSS selector `{selector}`"),
            Self::Xpath(expression) => format!("XPath `{expression}`"),
        }
    }

    /// A JavaScript expression that evaluates to the element or to `null`.
    ///
    /// The selector is passed through `serde_json` so a quote or a backslash in
    /// it can never end the string literal or add code.
    fn element_expression(&self) -> String {
        match self {
            Self::Css(selector) => {
                format!("document.querySelector({})", json_string(selector))
            }
            Self::Xpath(expression) => format!(
                "document.evaluate({}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue",
                json_string(expression)
            ),
        }
    }

    /// An expression that reports whether the element exists and is visible.
    fn locate_expression(&self) -> String {
        format!(
            "(function () {{ var el = {}; if (!el || el.nodeType !== 1) return {{ found: false }}; var style = window.getComputedStyle(el); var rect = el.getBoundingClientRect(); return {{ found: true, visible: style.display !== 'none' && style.visibility !== 'hidden' && style.visibility !== 'collapse' && rect.width > 0 && rect.height > 0 }}; }})()",
            self.element_expression()
        )
    }

    /// An expression that scrolls the element into view and returns its centre.
    fn center_expression(&self) -> String {
        format!(
            "(function () {{ var el = {}; if (!el) return null; el.scrollIntoView({{ block: 'center', inline: 'center' }}); var rect = el.getBoundingClientRect(); return {{ x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }}; }})()",
            self.element_expression()
        )
    }

    /// An expression that focuses the element and reports whether it took.
    fn focus_expression(&self) -> String {
        format!(
            "(function () {{ var el = {}; if (!el) return false; el.focus(); return document.activeElement === el; }})()",
            self.element_expression()
        )
    }

    /// An expression for the element's text content, trimmed.
    fn text_expression(&self) -> String {
        format!(
            "(function () {{ var el = {}; if (!el) return ''; return String(el.textContent === null ? '' : el.textContent).trim(); }})()",
            self.element_expression()
        )
    }

    /// An expression for one attribute, or `null` when it is absent.
    fn attribute_expression(&self, attribute: &str) -> String {
        format!(
            "(function () {{ var el = {}; if (!el) return null; return el.getAttribute({}); }})()",
            self.element_expression(),
            json_string(attribute)
        )
    }

    /// A step failure that names the locator and what was attempted.
    fn action_error(&self, action: &str, error: impl std::fmt::Display) -> StepError {
        StepError::Execution(format!("could not {action} {}: {error}", self.describe()))
    }
}

/// Encodes text as a JavaScript string literal.
fn json_string(value: &str) -> String {
    // `to_string` of a JSON string cannot fail: the value is already UTF-8.
    Value::String(value.to_string()).to_string()
}

/// What `browser.wait` waits for.
enum WaitTarget {
    /// An element.
    Element(Locator),
    /// The current URL containing some text.
    Url(String),
}

impl WaitTarget {
    /// Reads exactly one of the three accepted forms.
    fn from_args(args: &WaitArgs) -> Result<Self, String> {
        if args.css.is_some() && args.xpath.is_some() {
            return Err("invalid arguments: `css` and `xpath` cannot both be set; give exactly one of `css`, `xpath` and `url_contains`".to_string());
        }
        let locator = Locator::from_args(&args.css, &args.xpath);
        match (&locator, args.url_contains.as_deref()) {
            (Ok(_), Some(_)) => Err(
                "invalid arguments: `url_contains` cannot be combined with a locator; give exactly one of `css`, `xpath` and `url_contains`"
                    .to_string(),
            ),
            (Ok(locator), None) => Ok(Self::Element(locator.clone())),
            (Err(_), Some(text)) if text.trim().is_empty() => {
                Err("invalid arguments: `url_contains` must not be empty".to_string())
            }
            (Err(_), Some(text)) => Ok(Self::Url(text.to_string())),
            (Err(message), None) => Err(format!(
                "invalid arguments: give exactly one of `css`, `xpath` and `url_contains`; {message}"
            )),
        }
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

/// One case's browser, its page, and the options that created it.
struct BrowserSession {
    /// Taken by [`Session::close`], which releases it before the case report is
    /// written. `Drop` only matters on the fallback path.
    connection: Option<Connection>,
    browser: Option<Launched>,
    /// The target session every page command is sent with.
    page: String,
    port: u16,
    headless: bool,
    width: u32,
    height: u32,
}

impl BrowserSession {
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

        let setup = Deadline::after(LAUNCH_TIMEOUT);
        let target = connection
            .call(
                "Target.createTarget",
                json!({ "url": "about:blank" }),
                None,
                setup,
            )
            .map_err(|error| format!("could not open a page in the browser: {error}"))?;
        let target_id = target
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| "the browser did not return a target id".to_string())?;
        let attached = connection
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
                setup,
            )
            .map_err(|error| format!("could not attach to the page the browser opened: {error}"))?;
        let page = attached
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| "the browser did not return a page session id".to_string())?
            .to_string();
        connection
            .call("Page.enable", json!({}), Some(&page), setup)
            .map_err(|error| format!("could not enable page events: {error}"))?;
        connection
            .call("Runtime.enable", json!({}), Some(&page), setup)
            .map_err(|error| format!("could not enable page scripts: {error}"))?;

        Ok(Self {
            connection: Some(connection),
            browser: Some(browser),
            page,
            port,
            headless: args.headless,
            width: args.width,
            height: args.height,
        })
    }

    /// Refuses an option that only applies when the session is created.
    fn ensure_same_options(&self, args: &OpenArgs) -> Result<(), StepError> {
        let mut changes = Vec::new();
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
            "`browser.open` changed {} after the browser session was created; the first `browser.open` of a case sets `headless`, `width` and `height`, so repeat those values or open the page in a new case",
            changes.join(", ")
        )))
    }

    /// Navigates the session's page and returns its current URL and title.
    fn navigate(&mut self, url: &str, timeout: Duration) -> Result<(String, String), StepError> {
        let page = self.page.clone();
        let deadline = Deadline::after(timeout);
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
        wait_for_load(connection, &page, url, deadline)?;

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

    /// Waits for the element the locator names to exist and be visible.
    fn locate(&mut self, locator: &Locator, deadline: Deadline) -> Result<(), StepError> {
        let page = self.page.clone();
        let expression = locator.locate_expression();
        loop {
            let outcome = self
                .connection_mut()?
                .evaluate(&page, &expression, deadline);
            match outcome {
                Ok(value) => {
                    if value.get("found").and_then(Value::as_bool) == Some(true) {
                        if value.get("visible").and_then(Value::as_bool) == Some(true) {
                            return Ok(());
                        }
                        if deadline.passed() {
                            return Err(StepError::Execution(format!(
                                "{} was found but not visible within {} ms; check the page and the locator",
                                locator.describe(),
                                deadline.timeout_ms()
                            )));
                        }
                    } else if deadline.passed() {
                        return Err(StepError::Execution(format!(
                            "could not find {} within {} ms; check the page and the locator",
                            locator.describe(),
                            deadline.timeout_ms()
                        )));
                    }
                }
                Err(CdpError::Script(message)) => {
                    return Err(StepError::Execution(format!(
                        "could not use {}: {message}",
                        locator.describe()
                    )));
                }
                Err(CdpError::Timeout { .. }) => {
                    return Err(StepError::Execution(format!(
                        "could not find {} within {} ms; check the page and the locator",
                        locator.describe(),
                        deadline.timeout_ms()
                    )));
                }
                Err(error) => {
                    return Err(StepError::Environment(format!(
                        "the browser failed while waiting for {}: {error}",
                        locator.describe()
                    )));
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Evaluates one expression for a step and maps its errors.
    fn evaluate(
        &mut self,
        locator: &Locator,
        expression: &str,
        action: &str,
        deadline: Deadline,
    ) -> Result<Value, StepError> {
        let page = self.page.clone();
        self.connection_mut()?
            .evaluate(&page, expression, deadline)
            .map_err(|error| match error {
                CdpError::Timeout { .. } => StepError::Timeout(format!(
                    "could not {action} {} within {} ms",
                    locator.describe(),
                    deadline.timeout_ms()
                )),
                error => locator.action_error(action, error),
            })
    }

    /// Sends one page command for a step and maps its errors.
    fn dispatch(
        &mut self,
        locator: &Locator,
        method: &str,
        params: Value,
        action: &str,
        deadline: Deadline,
    ) -> Result<(), StepError> {
        let page = self.page.clone();
        self.connection_mut()?
            .call(method, params, Some(&page), deadline)
            .map(|_| ())
            .map_err(|error| locator.action_error(action, error))
    }

    /// The open connection, or the error a step reports after a close.
    fn connection_mut(&mut self) -> Result<&mut Connection, StepError> {
        self.connection.as_mut().ok_or_else(|| {
            StepError::Execution(
                "the browser session is already closed; run `browser.open` again".to_string(),
            )
        })
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
    fn close(&mut self) -> Result<(), String> {
        let Some(mut connection) = self.connection.take() else {
            return Ok(());
        };
        record_close(self.port);
        let mut errors = Vec::new();
        if let Err(error) = connection.call(
            "Browser.close",
            json!({}),
            None,
            Deadline::after(Duration::from_secs(2)),
        ) {
            match error {
                // Chrome may close the socket as it answers, or never answer.
                // The process check below decides whether the close worked.
                CdpError::Transport(_) | CdpError::Timeout { .. } => {}
                error => errors.push(format!("could not ask the browser to close: {error}")),
            }
        }
        drop(connection);
        if let Some(browser) = self.browser.take()
            && let Err(error) = browser.shutdown()
        {
            errors.push(error);
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

/// Parses a step's arguments.
fn parse<T: DeserializeOwned>(args: &Value) -> Result<T, StepError> {
    serde_json::from_value(args.clone())
        .map_err(|error| StepError::InvalidArguments(format!("invalid arguments: {error}")))
}

/// Parses a step's arguments for `validate`.
fn parse_for_validate<T: DeserializeOwned>(args: &Value) -> Result<T, String> {
    serde_json::from_value(args.clone()).map_err(|error| format!("invalid arguments: {error}"))
}

/// Checks a `timeout_ms` argument.
fn check_timeout(timeout_ms: u64) -> Result<(), String> {
    if timeout_ms == 0 {
        return Err("invalid arguments: `timeout_ms` must be at least 1".to_string());
    }
    Ok(())
}

fn check_open(args: &OpenArgs) -> Result<(), String> {
    if args.url.trim().is_empty() {
        return Err(
            "invalid arguments: `url` must not be empty; give the page to open".to_string(),
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

fn validate_open(args: &Value) -> Result<(), String> {
    check_open(&parse_for_validate(args)?)
}

fn validate_click(args: &Value) -> Result<(), String> {
    let args: ClickArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    Locator::from_args(&args.css, &args.xpath).map(|_| ())
}

fn validate_type(args: &Value) -> Result<(), String> {
    let args: TypeArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    Locator::from_args(&args.css, &args.xpath).map(|_| ())
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

fn validate_read(args: &Value) -> Result<(), String> {
    let args: ReadArgs = parse_for_validate(args)?;
    check_timeout(args.timeout_ms)?;
    Locator::from_args(&args.css, &args.xpath)?;
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
    let args: OpenArgs = parse(args)?;
    check_open(&args).map_err(StepError::InvalidArguments)?;
    let existing = sessions
        .get::<BrowserSession>(SESSION_KIND)
        .map_err(StepError::Execution)?
        .is_some();
    let session = sessions
        .get_or_create(SESSION_KIND, || BrowserSession::launch(&args))
        .map_err(StepError::Environment)?;
    if existing {
        session.ensure_same_options(&args)?;
    }
    let (url, title) = session.navigate(&args.url, Duration::from_millis(args.timeout_ms))?;
    Ok(json!({ "url": url, "title": title }))
}

fn run_click(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ClickArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator =
        Locator::from_args(&args.css, &args.xpath).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let session = session(sessions)?;
    session.locate(&locator, deadline)?;
    let center = session.evaluate(&locator, &locator.center_expression(), "click", deadline)?;
    let x = center
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| locator.action_error("click", "the element has no position"))?;
    let y = center
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| locator.action_error("click", "the element has no position"))?;
    for (event_type, buttons) in [("mousePressed", 1), ("mouseReleased", 0)] {
        session.dispatch(
            &locator,
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
    let page = session.page.clone();
    let text = session
        .connection
        .as_mut()
        .and_then(|connection| {
            connection
                .evaluate(&page, &locator.text_expression(), deadline)
                .ok()
        })
        .and_then(|value| value.as_str().map(str::to_string));
    match text {
        Some(text) if !text.trim().is_empty() => Ok(json!({ "text": text.trim() })),
        _ => Ok(json!({})),
    }
}

fn run_type(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: TypeArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator =
        Locator::from_args(&args.css, &args.xpath).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let session = session(sessions)?;
    session.locate(&locator, deadline)?;
    let focused = session.evaluate(&locator, &locator.focus_expression(), "focus", deadline)?;
    if focused != Value::Bool(true) {
        return Err(locator.action_error("focus", "the element did not take focus"));
    }
    session.dispatch(
        &locator,
        "Input.insertText",
        json!({ "text": args.text }),
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
        WaitTarget::Element(locator) => {
            session.locate(&locator, deadline)?;
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

fn run_read(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ReadArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator =
        Locator::from_args(&args.css, &args.xpath).map_err(StepError::InvalidArguments)?;
    let deadline = Deadline::after(Duration::from_millis(args.timeout_ms));
    let session = session(sessions)?;
    session.locate(&locator, deadline)?;
    let text = session.evaluate(
        &locator,
        &locator.text_expression(),
        "read the text of",
        deadline,
    )?;
    let mut result = json!({ "text": text.as_str().unwrap_or_default() });
    if let Some(attribute) = &args.attribute {
        let value = session.evaluate(
            &locator,
            &locator.attribute_expression(attribute),
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

        assert!((definition.validate)(&json!({"url": "http://127.0.0.1:1/"})).is_ok());
        assert!(
            (definition.validate)(&json!({"url": "http://127.0.0.1:1/", "timeout_ms": 30000}))
                .is_ok(),
            "a navigation timeout is a valid argument"
        );
        for (label, args) in [
            ("missing url", json!({})),
            ("empty url", json!({"url": ""})),
            ("zero width", json!({"url": "http://x/", "width": 0})),
            ("zero height", json!({"url": "http://x/", "height": 0})),
            ("zero timeout", json!({"url": "http://x/", "timeout_ms": 0})),
            ("unknown", json!({"url": "http://x/", "bogus": true})),
        ] {
            assert!(
                (definition.validate)(&args).is_err(),
                "{label} must be refused: {args}"
            );
        }
    }

    #[test]
    fn a_locator_is_exactly_one_of_css_and_xpath() {
        for step in ["browser.click", "browser.type", "browser.read"] {
            let definition = definition(step);
            let branches = definition.args_schema["oneOf"]
                .as_array()
                .unwrap_or_else(|| panic!("`{step}` must declare oneOf"));
            assert_eq!(branches.len(), 2, "`{step}`: {branches:?}");
            assert_eq!(
                definition.args_schema["properties"]["css"]["type"],
                "string"
            );
            assert_eq!(
                definition.args_schema["properties"]["xpath"]["type"],
                "string"
            );

            let base = if step == "browser.type" {
                json!({"css": "#name", "text": "hello"})
            } else {
                json!({"css": "#name"})
            };
            assert!(
                (definition.validate)(&base).is_ok(),
                "`{step}` must accept one locator: {base}"
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
        }
    }

    #[test]
    fn wait_accepts_a_locator_or_url_contains_but_not_both() {
        let definition = definition("browser.wait");
        let branches = definition.args_schema["oneOf"]
            .as_array()
            .expect("`browser.wait` must declare oneOf");
        assert_eq!(branches.len(), 3, "{branches:?}");

        for args in [
            json!({"css": "#late"}),
            json!({"xpath": "//*[@id='late']"}),
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
            Locator::Css("#name".to_string()).describe(),
            "CSS selector `#name`"
        );
        assert_eq!(
            Locator::Xpath("//*[@id='name']".to_string()).describe(),
            "XPath `//*[@id='name']`"
        );

        let quoted = Locator::Css("a\"b\\c".to_string()).locate_expression();
        assert!(
            quoted.contains(r#"document.querySelector("a\"b\\c")"#),
            "a selector must be passed as a JSON string literal: {quoted}"
        );
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
}
