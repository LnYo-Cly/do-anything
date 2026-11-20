//! The `android.*` steps: drive one device through the `adb` binary.
//!
//! Plain adb is the cheapest surface an Android device offers, but it is also
//! the least semantic one: `input tap` takes pixels, and only `uiautomator`
//! knows which element sits under them. On the measured Android 14 container a
//! dump costs about 2.0 s and the cost is fixed — compression halves the bytes
//! and not the time — while `input tap` costs 24 ms. Every action therefore
//! stays a pure adb call, and reading the UI tree is its own step
//! ([`STEP_NAMES`]`[4]`) that only the cases needing text or resource ids pay
//! for. `android.tap_on` is the one convenience that spends the two seconds
//! knowingly: it dumps, resolves one node and taps its centre in one step.
//!
//! The device is reached through one configurable binary, `DA_ADB` (default
//! `adb`), and one optional serial, `DA_ANDROID_SERIAL` or the step's `serial`.
//! No step softens a missing adb, a missing device or a second attached device
//! into a skip: each fails with the action that fixes it, because a green run
//! that drove nothing is worse than a red one.

mod adb;
mod dump;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use self::adb::Adb;
use self::dump::{Filter, Node, ResolveError};
use crate::core::paths;
use crate::core::registry::{
    Registry, StepDefinition, StepError, allow_retry, disallow_retry, no_lint_warnings,
};
use crate::core::session::Sessions;

/// Step names, in the order they are registered.
pub const STEP_NAMES: [&str; 6] = [
    "android.launch",
    "android.tap",
    "android.input_text",
    "android.screenshot",
    "android.dump",
    "android.tap_on",
];

/// The eight bytes every PNG starts with.
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Registers every Android step.
///
/// No step keeps a session: each one runs one or two adb commands and holds no
/// resource between steps, so there is nothing for `da session` to reattach to
/// and no session validator to register beyond the step's own.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    for definition in [
        launch_step()?,
        tap_step()?,
        input_text_step()?,
        screenshot_step()?,
        dump_step()?,
        tap_on_step()?,
    ] {
        registry.register(definition)?;
    }
    Ok(())
}

/// Builds `android.launch`, which takes a package, a component, or both.
fn launch_step() -> Result<StepDefinition, String> {
    let mut definition = step::<LaunchArgs>(
        STEP_NAMES[0],
        "Starts `package`, or the component `package`/`activity`, on the device and returns the `package` and the `component` it started.",
        validate_launch,
        run_launch,
    )?;
    strings(
        &mut definition.args_schema,
        &["package", "activity", "serial"],
    );
    Ok(definition)
}

/// Builds `android.tap`, which takes only pixels.
fn tap_step() -> Result<StepDefinition, String> {
    let mut definition = step::<TapArgs>(
        STEP_NAMES[1],
        "Taps the screen at `x`, `y` in pixels and returns the point; it reads no UI tree, so it costs one adb call.",
        validate_tap,
        run_tap,
    )?;
    strings(&mut definition.args_schema, &["serial"]);
    Ok(definition)
}

/// Builds `android.input_text`, which types into the focused field.
fn input_text_step() -> Result<StepDefinition, String> {
    let mut definition = step::<InputTextArgs>(
        STEP_NAMES[2],
        "Types `text` into the focused field with `input text` and returns how many `characters` it sent.",
        validate_input_text,
        run_input_text,
    )?;
    strings(&mut definition.args_schema, &["serial"]);
    Ok(definition)
}

/// Builds `android.screenshot`, which takes only the output path.
fn screenshot_step() -> Result<StepDefinition, String> {
    let mut definition = step::<ScreenshotArgs>(
        STEP_NAMES[3],
        "Writes a PNG of the screen to `path`, which resolves against the case file's directory, and returns the `path` and the number of `bytes` written.",
        validate_screenshot,
        run_screenshot,
    )?;
    strings(&mut definition.args_schema, &["serial"]);
    Ok(definition)
}

/// Builds `android.dump`, the one step that pays the dump cost on purpose.
fn dump_step() -> Result<StepDefinition, String> {
    let mut definition = step::<DumpArgs>(
        STEP_NAMES[4],
        "Dumps the UI hierarchy and returns its `nodes` — each with `text`, `resource_id`, `content_desc`, `class`, `bounds` and `center` — plus, when `text` or `resource_id` is given, the one node it names as `target`; one dump costs about two seconds.",
        validate_dump,
        run_dump,
    )?;
    strings(
        &mut definition.args_schema,
        &["text", "resource_id", "serial"],
    );
    definition.check_retry = allow_retry;
    Ok(definition)
}

/// Builds `android.tap_on`, which dumps, resolves one node and taps its centre.
fn tap_on_step() -> Result<StepDefinition, String> {
    let mut definition = step::<TapOnArgs>(
        STEP_NAMES[5],
        "Dumps the UI hierarchy, resolves the one node `text` or `resource_id` names and taps its centre, returning the `target` and the `center` it tapped; it costs the two seconds of `android.dump`.",
        validate_tap_on,
        run_tap_on,
    )?;
    strings(
        &mut definition.args_schema,
        &["text", "resource_id", "serial"],
    );
    // The two targets are exclusive, so one declared minimum length per field
    // would disagree with the other branch; `validate` refuses a blank value
    // and the schema's `oneOf` refuses the shape.
    if let Value::Object(entries) = &mut definition.args_schema {
        entries.insert(
            "oneOf".to_string(),
            json!([{ "required": ["text"] }, { "required": ["resource_id"] }]),
        );
    }
    Ok(definition)
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
        lint_warnings: no_lint_warnings,
        run,
    })
}

/// Makes optional string properties plain strings in the emitted schema.
///
/// `Option<String>` would otherwise also accept `null`; `validate` refuses it,
/// so the schema must refuse it too.
fn strings(schema: &mut Value, fields: &[&str]) {
    let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
        return;
    };
    for field in fields {
        if let Some(property) = properties.get_mut(*field).and_then(Value::as_object_mut) {
            property.insert("type".to_string(), json!("string"));
        }
    }
}

/// Arguments of `android.launch`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LaunchArgs {
    /// Package to launch, for example `com.android.settings`. Alone it starts
    /// the package's launcher activity; with `activity` it names the component.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    package: Option<String>,
    /// Activity to start: `.MainActivity`, `com.example.app.MainActivity`, or a
    /// full `com.example.app/.MainActivity` component.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    activity: Option<String>,
    /// Device serial for this step; overrides `DA_ANDROID_SERIAL`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    serial: Option<String>,
}

/// Arguments of `android.tap`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TapArgs {
    /// Horizontal pixel, from the left edge of the screen.
    x: u32,
    /// Vertical pixel, from the top edge of the screen.
    y: u32,
    /// Device serial for this step; overrides `DA_ANDROID_SERIAL`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    serial: Option<String>,
}

/// Arguments of `android.input_text`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InputTextArgs {
    /// Text to type into the field that currently has focus.
    #[schemars(extend("minLength" = 1))]
    text: String,
    /// Device serial for this step; overrides `DA_ANDROID_SERIAL`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    serial: Option<String>,
}

/// Arguments of `android.screenshot`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotArgs {
    /// PNG file to write; a relative path resolves against the case file's directory.
    #[schemars(extend("minLength" = 1))]
    path: String,
    /// Device serial for this step; overrides `DA_ANDROID_SERIAL`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    serial: Option<String>,
}

/// Arguments of `android.dump`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DumpArgs {
    /// Text that must name exactly one node, for example `Search settings`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    text: Option<String>,
    /// Resource id that must name exactly one node, for example
    /// `com.android.settings:id/search_bar`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    resource_id: Option<String>,
    /// Device serial for this step; overrides `DA_ANDROID_SERIAL`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    serial: Option<String>,
}

/// Arguments of `android.tap_on`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TapOnArgs {
    /// Text that must name exactly one node.
    #[serde(default)]
    text: Option<String>,
    /// Resource id that must name exactly one node.
    #[serde(default)]
    resource_id: Option<String>,
    /// Device serial for this step; overrides `DA_ANDROID_SERIAL`.
    #[schemars(extend("minLength" = 1))]
    #[serde(default)]
    serial: Option<String>,
}

fn validate_launch(args: &Value) -> Result<(), String> {
    check_launch(&parse_for_validate(args)?)
}

fn validate_tap(args: &Value) -> Result<(), String> {
    check_tap(&parse_for_validate(args)?)
}

fn validate_input_text(args: &Value) -> Result<(), String> {
    check_input_text(&parse_for_validate(args)?)
}

fn validate_screenshot(args: &Value) -> Result<(), String> {
    check_screenshot(&parse_for_validate(args)?)
}

fn validate_dump(args: &Value) -> Result<(), String> {
    check_dump(&parse_for_validate(args)?)
}

fn validate_tap_on(args: &Value) -> Result<(), String> {
    check_tap_on(&parse_for_validate(args)?)
}

/// Checks the arguments of `android.launch`.
///
/// `am start` takes a component, so a package alone and a full component are
/// the two forms that reach the device; everything else is refused before an
/// adb call is made.
fn check_launch(args: &LaunchArgs) -> Result<(), String> {
    let package = text_argument(args.package.as_deref(), "package", "the package to launch")?;
    let activity = text_argument(
        args.activity.as_deref(),
        "activity",
        "the activity to start",
    )?;
    check_serial(args.serial.as_deref())?;
    let Some(activity) = activity else {
        if package.is_none() {
            return Err(
                "invalid arguments: `android.launch` needs `package` and/or `activity`".to_string(),
            );
        }
        return Ok(());
    };
    match activity.split_once('/') {
        Some((component_package, component_activity)) => {
            if component_package.trim().is_empty() || component_activity.trim().is_empty() {
                return Err(format!(
                    "invalid arguments: `activity` `{activity}` must name both the package and the activity, as `com.example.app/.MainActivity`"
                ));
            }
            if let Some(package) = package
                && package != component_package
            {
                return Err(format!(
                    "invalid arguments: `activity` `{activity}` names package `{component_package}`, which is not `package` `{package}`; pass the component alone, or make both name the same package"
                ));
            }
        }
        None => {
            if package.is_none() {
                return Err(format!(
                    "invalid arguments: `activity` `{activity}` is not a component; give `package` as well, or write `com.example.app/.MainActivity`"
                ));
            }
        }
    }
    Ok(())
}

/// Checks the arguments of `android.tap`.
///
/// The coordinates are typed as unsigned integers, so a negative or fractional
/// pixel is refused while the arguments are parsed; only the serial is left.
fn check_tap(args: &TapArgs) -> Result<(), String> {
    check_serial(args.serial.as_deref())
}

/// Checks the arguments of `android.input_text`.
///
/// `input text` sends key events, so it has no key for a newline or a tab, and
/// it reads `%s` as a space, so a literal percent sign cannot be typed through
/// it. Refusing those values is better than sending text that the device
/// silently changes.
fn check_input_text(args: &InputTextArgs) -> Result<(), String> {
    check_serial(args.serial.as_deref())?;
    if args.text.is_empty() {
        return Err(
            "invalid arguments: `text` must not be empty; `input text` types characters into the field that has focus"
                .to_string(),
        );
    }
    if let Some(character) = args.text.chars().find(|character| character.is_control()) {
        return Err(format!(
            "invalid arguments: `text` contains the control character `{}`; `input text` has no key for a newline, a tab or another control character",
            character.escape_default()
        ));
    }
    if args.text.contains('%') {
        return Err(
            "invalid arguments: `text` contains `%`; `input text` reads `%s` as a space and has no escape for a literal percent sign"
                .to_string(),
        );
    }
    Ok(())
}

fn check_screenshot(args: &ScreenshotArgs) -> Result<(), String> {
    check_serial(args.serial.as_deref())?;
    if args.path.trim().is_empty() {
        return Err("invalid arguments: `path` must name the PNG file to write".to_string());
    }
    Ok(())
}

/// Checks the arguments of `android.dump`.
fn check_dump(args: &DumpArgs) -> Result<(), String> {
    check_serial(args.serial.as_deref())?;
    let text = text_argument(args.text.as_deref(), "text", "the text to resolve")?;
    let resource_id = text_argument(
        args.resource_id.as_deref(),
        "resource_id",
        "the resource id to resolve",
    )?;
    if text.is_some() && resource_id.is_some() {
        return Err(
            "invalid arguments: give `text` or `resource_id`, not both; a dump resolves one target"
                .to_string(),
        );
    }
    Ok(())
}

/// Checks the arguments of `android.tap_on`.
fn check_tap_on(args: &TapOnArgs) -> Result<(), String> {
    check_serial(args.serial.as_deref())?;
    let text = text_argument(args.text.as_deref(), "text", "the text to resolve")?;
    let resource_id = text_argument(
        args.resource_id.as_deref(),
        "resource_id",
        "the resource id to resolve",
    )?;
    match (text, resource_id) {
        (Some(_), Some(_)) => Err(
            "invalid arguments: give `text` or `resource_id`, not both; a tap has one target"
                .to_string(),
        ),
        (None, None) => Err(
            "invalid arguments: `android.tap_on` needs `text` or `resource_id` to name the node to tap"
                .to_string(),
        ),
        _ => Ok(()),
    }
}

/// One text argument, trimmed, or `None` when it is absent.
///
/// A value that is present but blank is refused: a blank package or a blank
/// selector would otherwise reach the device as an empty argument.
fn text_argument<'a>(
    value: Option<&'a str>,
    field: &str,
    expectation: &str,
) -> Result<Option<&'a str>, String> {
    match value {
        None => Ok(None),
        Some(text) if text.trim().is_empty() => Err(format!(
            "invalid arguments: `{field}` must not be blank; give {expectation}"
        )),
        Some(text) => Ok(Some(text.trim())),
    }
}

fn check_serial(serial: Option<&str>) -> Result<(), String> {
    text_argument(
        serial,
        "serial",
        "the device serial, or omit it to use `DA_ANDROID_SERIAL`",
    )
    .map(|_| ())
}

/// Parses a step's arguments.
fn parse<T: DeserializeOwned>(args: &Value) -> Result<T, StepError> {
    if let Some(message) = boolean_key_hint(args) {
        return Err(StepError::InvalidArguments(message));
    }
    reject_null_fields(args).map_err(StepError::InvalidArguments)?;
    serde_json::from_value(args.clone())
        .map_err(|error| StepError::InvalidArguments(format!("invalid arguments: {error}")))
}

/// Parses a step's arguments for `validate`.
fn parse_for_validate<T: DeserializeOwned>(args: &Value) -> Result<T, String> {
    if let Some(message) = boolean_key_hint(args) {
        return Err(message);
    }
    reject_null_fields(args)?;
    serde_json::from_value(args.clone()).map_err(|error| format!("invalid arguments: {error}"))
}

/// Explains the one YAML trap this step's arguments can hit.
///
/// The case format reads YAML 1.1 scalars, where an unquoted `y`, `n`, `on` or
/// `off` is a boolean. `android.tap` names its vertical pixel `y`, so a case
/// that writes it without quotes reaches the step as the key `true`, and serde
/// alone would report an unknown field called `true`.
fn boolean_key_hint(value: &Value) -> Option<String> {
    let key = value
        .as_object()?
        .keys()
        .find(|key| matches!(key.as_str(), "true" | "false"))?;
    Some(format!(
        "invalid arguments: a key was read as the boolean `{key}`; the case format reads an unquoted `y`, `n`, `on` or `off` key as a boolean, so quote it, for example `\"y\": 640`"
    ))
}

/// Rejects explicit `null` values in step arguments.
///
/// `serde` maps `null` into `Option::None` while the emitted schema refuses it,
/// so the two would disagree about `serial:` written with no value.
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

fn run_launch(args: &Value, _sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: LaunchArgs = parse(args)?;
    check_launch(&args).map_err(StepError::InvalidArguments)?;
    let adb = Adb::from_arguments(args.serial.as_deref());
    let package = args.package.as_deref().map(str::trim);
    let activity = args.activity.as_deref().map(str::trim);
    match (package, activity) {
        (Some(package), Some(activity)) => {
            let component = if activity.contains('/') {
                activity.to_string()
            } else {
                format!("{package}/{activity}")
            };
            start_component(&adb, &component, Some(package))
        }
        (Some(package), None) => launch_package(&adb, package),
        (None, Some(activity)) => start_component(&adb, activity, None),
        (None, None) => Err(StepError::InvalidArguments(
            "invalid arguments: `android.launch` needs `package` and/or `activity`".to_string(),
        )),
    }
}

/// Starts one component with `am start`.
///
/// `-W` makes `am` wait until the launch finishes, so the step after this one
/// sees the screen the launch produced instead of the one it replaced.
fn start_component(adb: &Adb, component: &str, package: Option<&str>) -> Result<Value, StepError> {
    let action = format!("start `{component}`");
    let output = adb.shell(&["am", "start", "-W", "-n", component], &action)?;
    check_device_refusal(&output, &action)?;
    Ok(json!({
        "package": package.or_else(|| component.split_once('/').map(|(package, _)| package)),
        "component": component,
    }))
}

/// Trims a device message for an error: first non-empty line, bounded length.
fn excerpt(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(200).collect())
        .unwrap_or_default()
}

/// Starts a package's launcher activity.
///
/// `am start` needs a component, so a package without an `activity` is resolved
/// on the device with `cmd package resolve-activity` and then started with
/// `am start -W -n`. Resolution is used instead of `monkey` because `monkey` is
/// absent from container Android images, where it fails with an unhelpful
/// argument dump. A case that needs a different entry point passes `activity`
/// and reaches `am start` directly.
fn launch_package(adb: &Adb, package: &str) -> Result<Value, StepError> {
    let action = format!("launch package `{package}`");
    let resolved = adb.shell(
        &[
            "cmd",
            "package",
            "resolve-activity",
            "--brief",
            "-c",
            "android.intent.category.LAUNCHER",
            package,
        ],
        &format!("resolve the launcher activity of `{package}`"),
    )?;
    let component = resolved
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| line.contains('/'))
        .unwrap_or_default()
        .to_string();
    if component.is_empty() {
        let detail = excerpt(&resolved);
        return Err(StepError::Execution(format!(
            "could not {action}: the device resolved no launcher activity for it; check the package name, or pass `activity` to name the activity to start{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" (the device said: {detail})")
            }
        )));
    }
    let output = adb.shell(&["am", "start", "-W", "-n", &component], &action)?;
    if let Some(line) = output
        .lines()
        .map(str::trim)
        .find(|line| line.contains("Error") || line.contains("does not exist"))
    {
        return Err(StepError::Execution(format!(
            "could not {action}: the device refused to start `{component}` (it said: {line})"
        )));
    }
    Ok(json!({ "package": package, "component": component }))
}

/// Fails when the device printed an error while the command still exited 0.
///
/// `am start` reports a component that does not exist on standard output and
/// exits successfully, so the exit code alone would make a launch that started
/// nothing look like a pass.
fn check_device_refusal(output: &str, action: &str) -> Result<(), StepError> {
    let refusal = output.lines().map(str::trim).find(|line| {
        line.starts_with("Error:") || line.starts_with("Error type") || line.contains("Exception")
    });
    match refusal {
        Some(line) => Err(StepError::Execution(format!(
            "could not {action}: the device said: {line}"
        ))),
        None => Ok(()),
    }
}

fn run_tap(args: &Value, _sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: TapArgs = parse(args)?;
    check_tap(&args).map_err(StepError::InvalidArguments)?;
    let adb = Adb::from_arguments(args.serial.as_deref());
    let x = args.x.to_string();
    let y = args.y.to_string();
    adb.shell(
        &["input", "tap", &x, &y],
        &format!("tap ({}, {})", args.x, args.y),
    )?;
    Ok(json!({ "x": args.x, "y": args.y }))
}

fn run_input_text(args: &Value, _sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: InputTextArgs = parse(args)?;
    check_input_text(&args).map_err(StepError::InvalidArguments)?;
    let adb = Adb::from_arguments(args.serial.as_deref());
    adb.shell(
        &["input", "text", &escape_input_text(&args.text)],
        "type the text into the field that has focus",
    )?;
    Ok(json!({ "characters": args.text.chars().count() }))
}

/// Prepares text for `input text`.
///
/// Two layers read the value: adb joins its arguments into one command line for
/// the device's shell, and `input text` reads `%s` as a space. The text is
/// therefore quoted for the shell and every space becomes `%s`.
fn escape_input_text(text: &str) -> String {
    shell_quote(&text.replace(' ', "%s"))
}

/// Quotes one argument for the device's `sh`.
fn shell_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('\'');
    for character in value.chars() {
        if character == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

fn run_screenshot(args: &Value, sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: ScreenshotArgs = parse(args)?;
    check_screenshot(&args).map_err(StepError::InvalidArguments)?;
    let adb = Adb::from_arguments(args.serial.as_deref());
    let png = adb.exec_out(&["screencap", "-p"], "capture the screen")?;
    if !png.starts_with(PNG_SIGNATURE) {
        return Err(StepError::Execution(format!(
            "could not capture the screen: the device returned {} bytes that are not a PNG file; check that the screen is on",
            png.len()
        )));
    }
    let path = resolve_path(args.path.trim(), sessions);
    write_bytes(&path, &png)?;
    Ok(json!({
        "path": paths::display(&path),
        "bytes": png.len(),
    }))
}

fn run_dump(args: &Value, _sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: DumpArgs = parse(args)?;
    check_dump(&args).map_err(StepError::InvalidArguments)?;
    let adb = Adb::from_arguments(args.serial.as_deref());
    let nodes = read_hierarchy(&adb)?;
    let mut result = json!({
        "count": nodes.len(),
        "nodes": nodes_value(&nodes),
    });
    if let Some(filter) = filter_of(args.text.as_deref(), args.resource_id.as_deref()) {
        result["target"] = node_value(resolve_one(&nodes, &filter)?);
    }
    Ok(result)
}

fn run_tap_on(args: &Value, _sessions: &mut Sessions) -> Result<Value, StepError> {
    let args: TapOnArgs = parse(args)?;
    check_tap_on(&args).map_err(StepError::InvalidArguments)?;
    let Some(filter) = filter_of(args.text.as_deref(), args.resource_id.as_deref()) else {
        return Err(StepError::InvalidArguments(
            "invalid arguments: `android.tap_on` needs `text` or `resource_id` to name the node to tap"
                .to_string(),
        ));
    };
    let adb = Adb::from_arguments(args.serial.as_deref());
    let nodes = read_hierarchy(&adb)?;
    let target = resolve_one(&nodes, &filter)?;
    let Some(bounds) = target.bounds else {
        return Err(StepError::Execution(format!(
            "the node {} has no bounds in the dump, so there is no point to tap; run `android.dump` to see the tree",
            filter.describe()
        )));
    };
    let (x, y) = bounds.center();
    adb.shell(
        &["input", "tap", &x.to_string(), &y.to_string()],
        &format!("tap the node {}", filter.describe()),
    )?;
    Ok(json!({
        "target": node_value(target),
        "center": { "x": x, "y": y },
    }))
}

/// Reads the UI hierarchy: one `uiautomator dump`, then one read of its file.
///
/// A dump costs about two seconds on the measured container and the cost is
/// fixed, which is why nothing calls this except the two steps that say so. The
/// file goes to `/data/local/tmp`, which an adb shell can always write, and is
/// removed again: `exec-out uiautomator dump` mixes a status line into the XML
/// on some builds, while the file holds the document alone.
fn read_hierarchy(adb: &Adb) -> Result<Vec<Node>, StepError> {
    let path = remote_dump_path();
    let dumped = adb.shell(&["uiautomator", "dump", &path], "dump the UI hierarchy")?;
    if let Some(line) = dumped
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("ERROR") || line.contains("Exception"))
    {
        return Err(StepError::Execution(format!(
            "could not dump the UI hierarchy: the device said: {line}; turn the screen on and let the animation finish, then run the step again"
        )));
    }
    let xml = adb.exec_out(&["cat", &path], "read the UI hierarchy the device dumped")?;
    // The file is only an intermediate: a failure to remove it must not hide a
    // dump that was read successfully.
    let _ = adb.shell(&["rm", "-f", &path], "remove the dumped UI hierarchy");
    let xml = String::from_utf8(xml).map_err(|_| {
        StepError::Execution(
            "the UI hierarchy the device returned is not UTF-8 text; the dump may be cut off"
                .to_string(),
        )
    })?;
    dump::parse(&xml)
        .map_err(|error| StepError::Execution(format!("could not read the UI hierarchy: {error}")))
}

/// A device path for the dump file that two cases cannot share.
fn remote_dump_path() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "/data/local/tmp/da-dump-{}-{}.xml",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The one target a case asked for, or `None` when it asked for none.
///
/// `validate` refuses both fields at once, so the order here is only about
/// reading one value.
fn filter_of(text: Option<&str>, resource_id: Option<&str>) -> Option<Filter> {
    match (text, resource_id) {
        (Some(text), _) => Some(Filter::Text(text.trim().to_string())),
        (None, Some(resource_id)) => Some(Filter::ResourceId(resource_id.trim().to_string())),
        (None, None) => None,
    }
}

/// Resolves exactly one node, with the message a case author needs when the
/// filter names none or several.
fn resolve_one<'a>(nodes: &'a [Node], filter: &Filter) -> Result<&'a Node, StepError> {
    match dump::resolve(nodes, filter) {
        Ok(node) => Ok(node),
        Err(ResolveError::NotFound) => Err(StepError::Execution(format!(
            "no node {} in the current UI tree ({} nodes); check the value, or run `android.dump` without a filter to see the tree",
            filter.describe(),
            nodes.len()
        ))),
        Err(ResolveError::Ambiguous(indexes)) => Err(StepError::Execution(format!(
            "{} matched {} nodes; use `android.tap` with the centre of the one to tap: {}",
            filter.describe(),
            indexes.len(),
            candidates(nodes, &indexes)
        ))),
    }
}

/// Up to five of the nodes a filter matched, as one line of an error message.
fn candidates(nodes: &[Node], indexes: &[usize]) -> String {
    const SHOWN: usize = 5;
    let shown: Vec<String> = indexes
        .iter()
        .take(SHOWN)
        .map(|index| {
            let node = &nodes[*index];
            let centre = match node.bounds {
                Some(bounds) => {
                    let (x, y) = bounds.center();
                    format!(" at ({x}, {y})")
                }
                None => String::new(),
            };
            format!(
                "{index}: <{}> id `{}` text `{}`{centre}",
                node.class.as_deref().unwrap_or("unknown"),
                node.resource_id.as_deref().unwrap_or("-"),
                node.text.as_deref().unwrap_or("-")
            )
        })
        .collect();
    let mut summary = shown.join("; ");
    if indexes.len() > SHOWN {
        summary.push_str(&format!(" (showing {SHOWN} of {})", indexes.len()));
    }
    summary
}

fn nodes_value(nodes: &[Node]) -> Value {
    Value::Array(nodes.iter().map(node_value).collect())
}

fn node_value(node: &Node) -> Value {
    let (bounds, center) = match node.bounds {
        Some(bounds) => {
            let (x, y) = bounds.center();
            (
                json!({
                    "left": bounds.left,
                    "top": bounds.top,
                    "right": bounds.right,
                    "bottom": bounds.bottom,
                }),
                json!({ "x": x, "y": y }),
            )
        }
        None => (Value::Null, Value::Null),
    };
    json!({
        "text": &node.text,
        "resource_id": &node.resource_id,
        "content_desc": &node.content_desc,
        "class": &node.class,
        "bounds": bounds,
        "center": center,
    })
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

/// Writes one screenshot, creating its directory when it is missing.
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StepError> {
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
    std::fs::write(path, bytes).map_err(|error| {
        StepError::Environment(format!(
            "could not write the screenshot `{}`: {error}",
            paths::display(path)
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        let mut registry = Registry::new();
        register(&mut registry).expect("the android steps must register");
        registry
    }

    fn rejects(step: &str, args: Value) -> String {
        let registry = registry();
        let definition = registry
            .get(step)
            .unwrap_or_else(|| panic!("`{step}` must be registered"));
        (definition.validate)(&args).expect_err("the arguments must be refused")
    }

    fn accepts(step: &str, args: Value) {
        let registry = registry();
        let definition = registry
            .get(step)
            .unwrap_or_else(|| panic!("`{step}` must be registered"));
        if let Err(message) = (definition.validate)(&args) {
            panic!("`{step}` must accept {args}: {message}");
        }
    }

    #[test]
    fn every_step_is_registered() {
        let registry = registry();
        let mut names = STEP_NAMES.to_vec();
        names.sort_unstable();
        assert_eq!(registry.names(), names);
        assert_eq!(
            registry.names().len(),
            STEP_NAMES.len(),
            "every android step must be registered exactly once"
        );
    }

    #[test]
    fn every_step_accepts_its_documented_arguments() {
        accepts("android.launch", json!({"package": "com.android.settings"}));
        accepts(
            "android.launch",
            json!({"package": "com.android.settings", "activity": ".Settings"}),
        );
        accepts(
            "android.launch",
            json!({"activity": "com.android.settings/.Settings"}),
        );
        accepts("android.tap", json!({"x": 360, "y": 640}));
        accepts("android.input_text", json!({"text": "hello world"}));
        accepts("android.screenshot", json!({"path": "screens/device.png"}));
        accepts("android.dump", json!({}));
        accepts("android.dump", json!({"text": "OK"}));
        accepts(
            "android.dump",
            json!({"resource_id": "com.android.settings:id/search_bar"}),
        );
        accepts("android.tap_on", json!({"text": "OK"}));
        accepts("android.tap_on", json!({"resource_id": "app:id/ok"}));
        accepts(
            "android.tap",
            json!({"x": 1, "y": 2, "serial": "emulator-5554"}),
        );
    }

    #[test]
    fn launch_needs_a_package_or_an_activity() {
        let message = rejects("android.launch", json!({}));
        assert!(
            message.contains("needs `package` and/or `activity`"),
            "got: {message}"
        );
        assert!(message.contains("invalid arguments"), "got: {message}");
    }

    #[test]
    fn launch_refuses_an_activity_that_names_no_component() {
        let message = rejects("android.launch", json!({"activity": ".Settings"}));
        assert!(message.contains("is not a component"), "got: {message}");
        assert!(
            message.contains("com.example.app/.MainActivity"),
            "got: {message}"
        );
    }

    #[test]
    fn launch_refuses_a_component_that_disagrees_with_the_package() {
        let message = rejects(
            "android.launch",
            json!({"package": "com.example.other", "activity": "com.android.settings/.Settings"}),
        );
        assert!(
            message.contains("names package `com.android.settings`"),
            "got: {message}"
        );
        assert!(message.contains("com.example.other"), "got: {message}");
    }

    #[test]
    fn launch_refuses_an_incomplete_component() {
        let message = rejects(
            "android.launch",
            json!({"activity": "com.android.settings/"}),
        );
        assert!(
            message.contains("must name both the package and the activity"),
            "got: {message}"
        );
    }

    #[test]
    fn a_blank_value_is_refused_for_every_text_argument() {
        for (step, args, expected) in [
            (
                "android.launch",
                json!({"package": "   "}),
                "must not be blank",
            ),
            (
                "android.launch",
                json!({"package": "com.android.settings", "activity": " "}),
                "must not be blank",
            ),
            (
                "android.input_text",
                json!({"text": ""}),
                "must not be empty",
            ),
            (
                "android.screenshot",
                json!({"path": "  "}),
                "must name the PNG file",
            ),
            ("android.dump", json!({"text": "  "}), "must not be blank"),
            (
                "android.dump",
                json!({"resource_id": "  "}),
                "must not be blank",
            ),
            ("android.tap_on", json!({"text": "  "}), "must not be blank"),
            (
                "android.tap",
                json!({"x": 1, "y": 2, "serial": " "}),
                "must not be blank",
            ),
        ] {
            let message = rejects(step, args.clone());
            assert!(
                message.contains(expected),
                "`{step}` must refuse {args} with `{expected}`, got: {message}"
            );
        }
    }

    #[test]
    fn tap_refuses_a_coordinate_that_is_not_an_unsigned_pixel() {
        for args in [
            json!({"x": -1, "y": 0}),
            json!({"x": 0, "y": -5}),
            json!({"x": 1.5, "y": 0}),
            json!({"x": "left", "y": 0}),
            json!({"x": 0}),
        ] {
            let message = rejects("android.tap", args.clone());
            assert!(
                message.contains("invalid arguments"),
                "`android.tap` must refuse {args}, got: {message}"
            );
        }
    }

    #[test]
    fn input_text_refuses_what_input_text_cannot_type() {
        let newline = rejects("android.input_text", json!({"text": "a\nb"}));
        assert!(newline.contains("control character"), "got: {newline}");

        let tab = rejects("android.input_text", json!({"text": "a\tb"}));
        assert!(tab.contains("control character"), "got: {tab}");

        let percent = rejects("android.input_text", json!({"text": "100%"}));
        assert!(percent.contains("literal percent sign"), "got: {percent}");
    }

    #[test]
    fn dump_takes_one_target_at_most() {
        let message = rejects(
            "android.dump",
            json!({"text": "OK", "resource_id": "app:id/ok"}),
        );
        assert!(message.contains("not both"), "got: {message}");

        let message = rejects("android.tap_on", json!({}));
        assert!(
            message.contains("needs `text` or `resource_id`"),
            "got: {message}"
        );

        let message = rejects(
            "android.tap_on",
            json!({"text": "OK", "resource_id": "app:id/ok"}),
        );
        assert!(message.contains("not both"), "got: {message}");
    }

    #[test]
    fn an_explicit_null_is_refused_even_though_the_field_is_optional() {
        let message = rejects(
            "android.launch",
            json!({"package": "com.android.settings", "serial": null}),
        );
        assert!(
            message.contains("`serial` must not be null"),
            "got: {message}"
        );
    }

    #[test]
    fn a_key_yaml_read_as_a_boolean_is_explained() {
        // `y: 640` in a case file reaches the step as the key `true`, because
        // the case format reads YAML 1.1 scalars.
        let message = rejects("android.tap", json!({"x": 360, "true": 640}));
        assert!(
            message.contains("read as the boolean `true`"),
            "got: {message}"
        );
        assert!(message.contains("quote it"), "got: {message}");
        assert!(message.contains("\"y\": 640"), "got: {message}");
    }

    #[test]
    fn the_emitted_schema_refuses_null_for_every_optional_string() {
        let registry = registry();
        for (step, fields) in [
            ("android.launch", vec!["package", "activity", "serial"]),
            ("android.tap", vec!["serial"]),
            ("android.input_text", vec!["serial"]),
            ("android.screenshot", vec!["serial"]),
            ("android.dump", vec!["text", "resource_id", "serial"]),
            ("android.tap_on", vec!["text", "resource_id", "serial"]),
        ] {
            let definition = registry
                .get(step)
                .unwrap_or_else(|| panic!("`{step}` must be registered"));
            for field in fields {
                assert_eq!(
                    definition.args_schema["properties"][field]["type"], "string",
                    "`{step}.{field}` must be a string in the schema: {}",
                    definition.args_schema
                );
                let mut args = json!({});
                args[field] = Value::Null;
                assert!(
                    !jsonschema::is_valid(&definition.args_schema, &args),
                    "the schema of `{step}` must refuse a null `{field}`"
                );
            }
        }
    }

    #[test]
    fn the_steps_validate_inside_an_interactive_session_too() {
        let registry = registry();
        assert!(
            registry
                .validate_for_session("android.tap", &json!({"x": 1, "y": 2}))
                .is_ok()
        );
        assert!(
            registry
                .validate_for_session("android.tap", &json!({"x": -1, "y": 2}))
                .is_err()
        );
    }

    #[test]
    fn only_the_dump_step_may_be_retried() {
        let registry = registry();
        let allows = |name: &str| {
            let definition = registry.get(name).expect("the step must be registered");
            (definition.check_retry)(&json!({})).is_ok()
        };
        assert!(allows("android.dump"), "a dump changes nothing");
        for name in [
            "android.launch",
            "android.tap",
            "android.input_text",
            "android.screenshot",
            "android.tap_on",
        ] {
            assert!(!allows(name), "`{name}` changes state and must not retry");
        }
    }

    #[test]
    fn input_text_quotes_for_the_device_shell_and_spells_spaces_as_percent_s() {
        assert_eq!(escape_input_text("hello world"), "'hello%sworld'");
        assert_eq!(escape_input_text("it's"), "'it'\\''s'");
        assert_eq!(escape_input_text("a;b"), "'a;b'");
        assert_eq!(escape_input_text("$(rm -rf /)"), "'$(rm%s-rf%s/)'");
        assert_eq!(escape_input_text("plain"), "'plain'");
    }

    #[test]
    fn every_dump_writes_a_file_no_other_step_can_share() {
        let first = remote_dump_path();
        let second = remote_dump_path();
        assert_ne!(first, second);
        assert!(
            first.starts_with("/data/local/tmp/da-dump-"),
            "got: {first}"
        );
        assert!(first.ends_with(".xml"), "got: {first}");
    }

    #[test]
    fn a_screenshot_path_resolves_against_the_case_directory() {
        let mut sessions = Sessions::new();
        sessions.set_base_directory("cases/shop");
        assert_eq!(
            resolve_path("screens/a.png", &sessions),
            PathBuf::from("cases/shop/screens/a.png")
        );
        assert_eq!(
            resolve_path("/tmp/a.png", &sessions),
            PathBuf::from("/tmp/a.png")
        );
    }

    #[test]
    fn a_screenshot_is_written_into_a_directory_that_did_not_exist() {
        let dir = std::env::temp_dir().join(format!("da-android-write-{}", std::process::id()));
        let path = dir.join("screens").join("shot.png");
        write_bytes(&path, PNG_SIGNATURE)
            .expect("the screenshot must be written even when its directory is new");
        assert_eq!(
            std::fs::read(&path).expect("the screenshot must be readable"),
            PNG_SIGNATURE
        );
        std::fs::remove_dir_all(&dir).expect("the test directory must be removable");
    }
}
