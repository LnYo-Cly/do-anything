//! The `browser.*` steps: drive one Chrome page across a case.
//!
//! The first `browser.open` of a case creates a [`BrowserSession`] through the
//! session contract. Every later step of the same case asks [`Sessions`] for the
//! same session, so it sees the page the first step opened; the runner closes
//! the browser when the case ends, whatever the case's result was.
//!
//! Locators are deterministic on purpose: a CSS selector or an XPath
//! expression, never a coordinate or a screenshot. Element lookup waits for the
//! element to appear, so a step does not race the page's own scripts.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use headless_chrome::browser::tab::NoElementFound;
use headless_chrome::util::Timeout;
use headless_chrome::{Browser, Element, LaunchOptions, Tab};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

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
const DEFAULT_WIDTH: u32 = 1280;
const DEFAULT_HEIGHT: u32 = 800;

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

    /// Waits for the element and returns it.
    fn find<'a>(&self, tab: &'a Tab, timeout: Duration) -> Result<Element<'a>, StepError> {
        let result = match self {
            Self::Css(selector) => tab.wait_for_element_with_custom_timeout(selector, timeout),
            Self::Xpath(expression) => tab.wait_for_xpath_with_custom_timeout(expression, timeout),
        };
        result.map_err(|error| {
            // Running out of the wait is the case the step exists to report;
            // any other error means the browser itself is in trouble.
            if error.downcast_ref::<NoElementFound>().is_some()
                || error.downcast_ref::<Timeout>().is_some()
            {
                StepError::Execution(format!(
                    "could not find {} within {} ms; check the page and the locator",
                    self.describe(),
                    timeout.as_millis()
                ))
            } else {
                StepError::Environment(format!(
                    "the browser failed while waiting for {}: {error}",
                    self.describe()
                ))
            }
        })
    }

    /// A step failure that names the locator and what was attempted.
    fn action_error(&self, action: &str, error: impl std::fmt::Display) -> StepError {
        StepError::Execution(format!("could not {action} {}: {error}", self.describe()))
    }
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

/// One case's browser and the page every browser step of that case uses.
struct BrowserSession {
    /// Taken by [`Session::close`], which releases it before the case report is
    /// written. `Drop` only matters on the fallback path.
    browser: Option<Browser>,
    tab: std::sync::Arc<Tab>,
    headless: bool,
    width: u32,
    height: u32,
}

impl BrowserSession {
    /// Launches the browser described by `browser.open`.
    fn launch(args: &OpenArgs) -> Result<Self, String> {
        let path = chrome_path()?;
        let options = LaunchOptions::default_builder()
            .headless(args.headless)
            .window_size(Some((args.width, args.height)))
            .path(Some(path))
            .build()
            .map_err(|error| format!("could not build the Chrome launch options: {error}"))?;
        let browser = Browser::new(options).map_err(|error| {
            format!(
                "could not start Chrome: {error}; set `DA_CHROME` to the Chrome or Chromium executable if it is installed elsewhere"
            )
        })?;
        let tab = browser.new_tab().map_err(|error| {
            format!(
                "could not open a page in Chrome: {error}; set `DA_CHROME` to the Chrome or Chromium executable if it is installed elsewhere"
            )
        })?;
        Ok(Self {
            browser: Some(browser),
            tab,
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
    fn navigate(&self, url: &str) -> Result<(String, String), StepError> {
        self.tab
            .navigate_to(url)
            .map_err(|error| StepError::Environment(format!("could not open `{url}`: {error}")))?;
        self.tab.wait_until_navigated().map_err(|error| {
            StepError::Timeout(format!("the page `{url}` did not finish loading: {error}"))
        })?;
        let current = self.tab.get_url();
        let title = self.tab.get_title().map_err(|error| {
            StepError::Environment(format!("could not read the title of `{url}`: {error}"))
        })?;
        Ok((current, title))
    }
}

impl Session for BrowserSession {
    fn close(&mut self) -> Result<(), String> {
        let Some(browser) = self.browser.take() else {
            return Ok(());
        };
        record_close();
        let debug_url = browser.get_ws_url();
        drop(browser);
        if let Some(port) = debug_port(&debug_url) {
            let address = SocketAddr::from(([127, 0, 0, 1], port));
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(2) {
                if TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err() {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            return Err(format!(
                "the browser process is still listening on its debug port {port} after close"
            ));
        }
        Ok(())
    }
}

/// The Chrome executable to launch.
///
/// `DA_CHROME` wins; otherwise the library's own lookup is used. Both failure
/// messages name `DA_CHROME`, because that is the one thing the user can set.
fn chrome_path() -> Result<PathBuf, String> {
    match std::env::var_os("DA_CHROME") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if path.is_file() {
                Ok(path)
            } else {
                Err(format!(
                    "`DA_CHROME` points to `{}`, which is not a file; set `DA_CHROME` to the Chrome or Chromium executable",
                    path.display()
                ))
            }
        }
        _ => headless_chrome::browser::default_executable().map_err(|error| {
            format!(
                "could not find Chrome or Chromium: {error}; set `DA_CHROME` to the Chrome or Chromium executable"
            )
        }),
    }
}

/// The port headless_chrome's debug WebSocket listens on.
fn debug_port(url: &str) -> Option<u16> {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))?;
    let authority = rest.split('/').next()?;
    let (_, port) = authority.rsplit_once(':')?;
    port.parse().ok()
}

/// Records that [`Session::close`] ran.
///
/// This is a test hook for the case-end test: the process also ends when the
/// fallback `Drop` runs, so the record is what tells an explicit close from the
/// fallback. It writes nothing unless `DA_TEST_BROWSER_CLOSE_LOG` names a file.
fn record_close() {
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
        let _ = writeln!(file, "browser session closed");
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

/// Checks a timeout argument.
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
    Ok(())
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
    if args.key.trim().is_empty() {
        return Err(
            "invalid arguments: `key` must not be empty; use a key such as `Enter` or `Tab`"
                .to_string(),
        );
    }
    Ok(())
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
    let (url, title) = session.navigate(&args.url)?;
    Ok(json!({ "url": url, "title": title }))
}

fn run_click(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ClickArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator =
        Locator::from_args(&args.css, &args.xpath).map_err(StepError::InvalidArguments)?;
    let session = session(sessions)?;
    let element = locator.find(&session.tab, Duration::from_millis(args.timeout_ms))?;
    element
        .click()
        .map_err(|error| locator.action_error("click", error))?;
    // A click may navigate, which detaches the element. The click itself
    // succeeded, so a text that can no longer be read is not a failure.
    match element.get_inner_text() {
        Ok(text) if !text.trim().is_empty() => Ok(json!({ "text": text.trim() })),
        _ => Ok(json!({})),
    }
}

fn run_type(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: TypeArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator =
        Locator::from_args(&args.css, &args.xpath).map_err(StepError::InvalidArguments)?;
    let session = session(sessions)?;
    let element = locator.find(&session.tab, Duration::from_millis(args.timeout_ms))?;
    element
        .type_into(&args.text)
        .map_err(|error| locator.action_error("type into", error))?;
    Ok(json!({}))
}

fn run_press(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: PressArgs = parse(args)?;
    if args.key.trim().is_empty() {
        return Err(StepError::InvalidArguments(
            "invalid arguments: `key` must not be empty; use a key such as `Enter` or `Tab`"
                .to_string(),
        ));
    }
    let session = session(sessions)?;
    session.tab.press_key(&args.key).map_err(|error| {
        if error.to_string().starts_with("Key not found") {
            StepError::InvalidArguments(format!(
                "invalid arguments: `{}` is not a key name; use a key such as `Enter` or `Tab`",
                args.key
            ))
        } else {
            StepError::Execution(format!("could not press `{}`: {error}", args.key))
        }
    })?;
    Ok(json!({}))
}

fn run_wait(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: WaitArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let target = WaitTarget::from_args(&args).map_err(StepError::InvalidArguments)?;
    let timeout = Duration::from_millis(args.timeout_ms);
    let session = session(sessions)?;
    let url = match target {
        WaitTarget::Element(locator) => {
            locator.find(&session.tab, timeout)?;
            session.tab.get_url()
        }
        WaitTarget::Url(text) => wait_for_url(&session.tab, &text, timeout)?,
    };
    Ok(json!({ "url": url }))
}

/// Waits until the page's URL contains `wanted`.
fn wait_for_url(tab: &Tab, wanted: &str, timeout: Duration) -> Result<String, StepError> {
    let started = Instant::now();
    loop {
        if let Ok(value) = tab.evaluate("location.href", false)
            && let Some(Value::String(url)) = value.value
            && url.contains(wanted)
        {
            return Ok(url);
        }
        if started.elapsed() >= timeout {
            return Err(StepError::Execution(format!(
                "the URL did not contain `{wanted}` within {} ms",
                timeout.as_millis()
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_read(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ReadArgs = parse(args)?;
    check_timeout(args.timeout_ms).map_err(StepError::InvalidArguments)?;
    let locator =
        Locator::from_args(&args.css, &args.xpath).map_err(StepError::InvalidArguments)?;
    let session = session(sessions)?;
    let element = locator.find(&session.tab, Duration::from_millis(args.timeout_ms))?;
    let text = element
        .get_inner_text()
        .map_err(|error| locator.action_error("read the text of", error))?;
    let mut result = json!({ "text": text });
    if let Some(attribute) = &args.attribute {
        let value = element
            .get_attribute_value(attribute)
            .map_err(|error| locator.action_error("read the attribute of", error))?;
        result["value"] = value.map_or(Value::Null, Value::String);
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

        assert!((definition.validate)(&json!({"url": "http://127.0.0.1:1/"})).is_ok());
        for (label, args) in [
            ("missing url", json!({})),
            ("empty url", json!({"url": ""})),
            ("zero width", json!({"url": "http://x/", "width": 0})),
            ("zero height", json!({"url": "http://x/", "height": 0})),
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
        let error =
            (press.validate)(&json!({"key": ""})).expect_err("an empty key must be refused");
        assert!(error.contains("key"), "{error}");
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
    fn debug_ports_are_read_from_the_websocket_url() {
        assert_eq!(
            debug_port("ws://127.0.0.1:41234/devtools/browser/abc"),
            Some(41234)
        );
        assert_eq!(debug_port("wss://127.0.0.1:9/devtools/browser"), Some(9));
        assert_eq!(debug_port("browser is not running"), None);
        assert_eq!(debug_port("ws://127.0.0.1/devtools/browser"), None);
    }

    #[test]
    fn a_locator_names_itself_in_an_error() {
        assert_eq!(
            Locator::Css("#name".to_string()).describe(),
            "CSS selector `#name`"
        );
        assert_eq!(
            Locator::Xpath("//*[@id='name']".to_string()).describe(),
            "XPath `//*[@id='name']`"
        );
    }
}
