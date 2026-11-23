//! Tests for `da schema` and its agreement with the case parser.
//!
//! `jsonschema` is a dev-dependency only: production code builds the schema
//! with `serde_json` and never validates anything at run time.

mod support;

#[cfg(any(feature = "browser", feature = "cli", feature = "desktop"))]
use std::fs;
#[cfg(any(feature = "browser", feature = "cli", feature = "desktop"))]
use std::path::Path;
#[cfg(feature = "cli")]
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::LazyLock;

use serde_json::{Value, json};
use support::TempDir;

/// The binary under test, as an absolute path.
fn da() -> &'static str {
    env!("CARGO_BIN_EXE_da")
}

fn run_da(args: &[&str]) -> Output {
    let work = TempDir::new("schema-cwd");
    Command::new(da())
        .current_dir(work.path())
        .args(args)
        .output()
        .expect("failed to run the da binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The schema as `--output json` prints it, parsed once per test process.
fn schema() -> &'static Value {
    static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
        let output = run_da(&["--output", "json", "schema"]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "`da schema` must exit 0, stderr: {:?}",
            stderr(&output)
        );
        assert!(
            stderr(&output).is_empty(),
            "`da schema` must not write diagnostics, got: {:?}",
            stderr(&output)
        );
        serde_json::from_slice(&output.stdout).expect("stdout must contain exactly one JSON value")
    });
    &SCHEMA
}

#[cfg(any(
    feature = "browser",
    feature = "cli",
    feature = "desktop",
    feature = "fs",
    feature = "http",
    feature = "proc"
))]
fn validator(schema: &'static Value) -> jsonschema::Validator {
    jsonschema::validator_for(schema).expect("the emitted schema must compile")
}

/// A complete case that uses a registered step, as a JSON instance.
#[cfg(feature = "cli")]
fn valid_instance() -> Value {
    json!({
        "name": "Example",
        "project": "shop",
        "suite": "smoke",
        "layer": "api",
        "tags": ["smoke", "fast"],
        "vars": {"port": 8080, "verbose": true},
        "steps": [
            {
                "cli.run": {"command": "da", "args": ["--version"]},
                "expect": [{"path": "$.exit_code", "equals": 0}],
                "save": {"version": "$.stdout"}
            }
        ]
    })
}

#[test]
fn the_schema_is_one_json_object_in_both_output_modes() {
    let output = run_da(&["schema"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "`da schema` must exit 0, stderr: {:?}",
        stderr(&output)
    );

    let text: Value = serde_json::from_str(&stdout(&output))
        .expect("text output must be one JSON object, pretty-printed or not");
    assert_eq!(text, *schema(), "both modes must describe the same schema");
    assert_eq!(
        text["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert!(text["$defs"].is_object(), "got: {text}");
    assert_eq!(text["type"], "object");
}

#[test]
fn the_schema_is_valid_against_the_2020_12_meta_schema() {
    jsonschema::meta::validate(schema()).expect("the schema must satisfy its own meta-schema");
}

#[cfg(feature = "cli")]
#[test]
fn the_classification_fields_match_the_parser() {
    let validator = validator(schema());
    let instance = valid_instance();
    assert!(
        validator.is_valid(&instance),
        "the parser accepts classification, so the schema must too"
    );

    for field in ["project", "suite", "layer"] {
        for blank in ["", "   "] {
            let mut empty = instance.clone();
            empty[field] = json!(blank);
            assert!(
                !validator.is_valid(&empty),
                "the parser rejects a blank `{field}` ({blank:?}), so the schema must too"
            );
        }
    }

    let mut duplicate = instance.clone();
    duplicate["tags"] = json!(["smoke", "smoke"]);
    assert!(
        !validator.is_valid(&duplicate),
        "the parser rejects a duplicate tag, so the schema must too"
    );

    for blank in ["", "   "] {
        let mut empty_tag = instance.clone();
        empty_tag["tags"] = json!([blank]);
        assert!(
            !validator.is_valid(&empty_tag),
            "the parser rejects a blank tag ({blank:?}), so the schema must too"
        );
    }
}

#[cfg(feature = "cli")]
#[test]
fn an_unknown_top_level_field_is_rejected() {
    let validator = validator(schema());
    let mut instance = valid_instance();
    assert!(validator.is_valid(&instance), "the control case must pass");

    instance["naem"] = json!("typo");
    let errors: Vec<String> = validator
        .iter_errors(&instance)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.iter().any(|error| error.contains("naem")),
        "the error must name the field, got: {errors:?}"
    );
}

#[cfg(feature = "cli")]
#[test]
fn non_string_case_names_pass_the_schema_and_lint() {
    let dir = TempDir::new("schema-name-scalars");
    let validator = validator(schema());

    for (yaml, label) in [("123", "number"), ("true", "boolean")] {
        let path = dir.write(
            &format!("name-{label}.yaml"),
            &format!("name: {yaml}\nsteps:\n  - cli.run:\n      command: da\n"),
        );
        let text = fs::read_to_string(&path).expect("the case must be readable");
        let instance: Value = serde_saphyr::from_str(&text).expect("the case must be valid YAML");
        assert!(
            !instance["name"].is_string(),
            "the regression must use a non-string YAML scalar, got: {instance}"
        );
        assert!(
            validator.is_valid(&instance),
            "the schema rejected a case name the parser accepts: {instance}"
        );

        let output = run_da(&["lint", path.to_str().expect("the path must be UTF-8")]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "`da lint` must accept `name: {yaml}`, stderr: {:?}",
            stderr(&output)
        );
    }
}

#[test]
fn an_empty_step_list_is_rejected() {
    let instance = json!({"name": "No steps", "steps": []});

    assert!(
        !jsonschema::is_valid(schema(), &instance),
        "the parser requires at least one step, so the schema must too"
    );
}

#[cfg(feature = "cli")]
#[test]
fn an_unregistered_step_is_rejected() {
    let validator = validator(schema());
    let instance = json!({
        "name": "Unknown",
        "steps": [{"no.such.step": {}}]
    });

    assert!(
        !validator.is_valid(&instance),
        "an unregistered step must not validate"
    );
}

#[cfg(feature = "cli")]
#[test]
fn an_unknown_comparison_is_rejected() {
    let validator = validator(schema());
    let instance = json!({
        "name": "Unknown comparison",
        "steps": [{
            "cli.run": {"command": "da"},
            "expect": [{"path": "$.exit_code", "matches": 0}]
        }]
    });

    assert!(
        !validator.is_valid(&instance),
        "an unknown comparison must not validate"
    );
}

#[cfg(feature = "cli")]
#[test]
fn a_cli_timeout_of_zero_is_rejected_by_the_schema_and_by_validate() {
    let definition = step_definition("cli.run");
    assert_eq!(
        definition.args_schema["properties"]["timeout_ms"]["minimum"], 1,
        "the step schema must declare the bound validate enforces: {}",
        definition.args_schema
    );
    assert_eq!(
        schema()["$defs"]["cli.run"]["properties"]["timeout_ms"]["minimum"],
        1,
        "`da schema` must carry the same bound"
    );

    let zero = json!({"command": "da", "timeout_ms": 0});
    assert!(
        definition.validate(&zero).is_err(),
        "validate must reject `timeout_ms: 0`"
    );
    assert!(
        !jsonschema::is_valid(definition.args_schema, &zero),
        "the schema must reject `timeout_ms: 0`: {}",
        definition.args_schema
    );
}

#[cfg(feature = "http")]
#[test]
fn an_http_timeout_of_zero_is_rejected_by_the_schema_and_by_validate() {
    let definition = step_definition("http.request");
    assert_eq!(
        definition.args_schema["properties"]["timeout_ms"]["minimum"], 1,
        "the step schema must declare the bound validate enforces: {}",
        definition.args_schema
    );
    assert_eq!(
        schema()["$defs"]["http.request"]["properties"]["timeout_ms"]["minimum"],
        1,
        "`da schema` must carry the same bound"
    );

    let zero = json!({"url": "http://127.0.0.1:1/health", "timeout_ms": 0});
    assert!(
        definition.validate(&zero).is_err(),
        "validate must reject `timeout_ms: 0`"
    );
    assert!(
        !jsonschema::is_valid(definition.args_schema, &zero),
        "the schema must reject `timeout_ms: 0`: {}",
        definition.args_schema
    );
}

/// A registered step definition, for tests that compare the schema with
/// `validate`.
#[cfg(any(feature = "cli", feature = "http"))]
fn step_definition(name: &str) -> do_anything::core::registry::StepRef<'static> {
    static REGISTRY: std::sync::OnceLock<do_anything::core::registry::Registry> =
        std::sync::OnceLock::new();
    REGISTRY
        .get_or_init(built_in_registry)
        .get(name)
        .unwrap_or_else(|| panic!("`{name}` must be registered"))
}

/// Every registered step definition, keyed by name.
#[cfg(any(
    feature = "browser",
    feature = "cli",
    feature = "db",
    feature = "desktop",
    feature = "fs",
    feature = "http",
    feature = "log",
    feature = "proc"
))]
fn built_in_registry() -> do_anything::core::registry::Registry {
    let mut registry = do_anything::core::registry::Registry::new();
    do_anything::drivers::register(&mut registry).expect("the built-in drivers must register");
    do_anything::drivers::register_observers(&mut registry)
        .expect("the built-in observers must register");
    registry
}

/// Arguments each built-in step accepts, before one argument is overridden.
///
/// The values only have to pass `validate`; it checks arguments without
/// starting the program or opening a connection.
#[cfg(any(
    feature = "browser",
    feature = "cli",
    feature = "db",
    feature = "desktop",
    feature = "http",
    feature = "proc"
))]
fn valid_arguments(step: &str) -> Value {
    match step {
        "browser.open" => json!({"url": "http://127.0.0.1:1/"}),
        "browser.click" => json!({"css": "#name"}),
        "browser.type" => json!({"css": "#name", "text": "hello"}),
        "browser.upload" => json!({"css": "#name", "files": ["fixture.txt"]}),
        "browser.press" => json!({"key": "Enter"}),
        "browser.wait" => json!({"css": "#name"}),
        "browser.read" => json!({"css": "#name"}),
        "browser.screenshot" => json!({"path": "screen.png"}),
        "browser.set_cookies" => {
            json!({"cookies": [{"name": "sid", "value": "abc", "url": "http://127.0.0.1:1/"}]})
        }
        "browser.save_state" => json!({"path": "state.json"}),
        "browser.switch_tab" => json!({}),
        "browser.scroll" => json!({"css": "#list", "delta_y": 200}),
        "browser.hover" => json!({"css": "#name"}),
        "browser.drag" => json!({"from": {"css": "#source"}, "to": {"css": "#target"}}),
        "browser.set_clipboard" => json!({"text": "hello"}),
        "browser.read_clipboard" => json!({}),
        "cli.run" => json!({"command": "da"}),
        "http.request" => json!({"url": "http://127.0.0.1:1/health"}),
        "db.query" => json!({"url": "sqlite:app.db", "sql": "SELECT 1"}),
        "android.launch" => json!({"package": "com.android.example"}),
        "android.tap" => json!({"x": 0, "y": 0}),
        "android.input_text" => json!({"text": "hello"}),
        "android.screenshot" => json!({"path": "screen.png"}),
        "android.dump" => json!({}),
        "android.tap_on" => json!({"text": "OK"}),
        "desktop.launch" => json!({"command": "notepad.exe"}),
        "desktop.attach" => json!({"pid": 1234}),
        "desktop.click" => json!({"automation_id": "ok"}),
        "desktop.type" => json!({"automation_id": "user", "text": "alice"}),
        "desktop.press" => json!({"key": "Enter"}),
        "desktop.wait" => json!({"automation_id": "status"}),
        "desktop.read" => json!({"automation_id": "status"}),
        "desktop.screenshot" => json!({"path": "out.png"}),
        "proc.start" => json!({"command": "da"}),
        "proc.send" => json!({"input": "x"}),
        "proc.wait_output" => json!({"pattern": "ok"}),
        "proc.stop" => json!({}),
        other => panic!("add the valid arguments of `{other}` to this test"),
    }
}

/// The bounds of one argument schema that this test can turn into a value.
///
/// A bound that the schema can express but this list cannot construct is
/// caught by the assertion on the constructed value, which fails loudly.
#[cfg(any(
    feature = "browser",
    feature = "cli",
    feature = "db",
    feature = "desktop",
    feature = "http",
    feature = "proc"
))]
fn declared_bounds(schema: &Value) -> Vec<(&'static str, Value)> {
    ["minimum", "minLength", "pattern"]
        .into_iter()
        .filter_map(|keyword| schema.get(keyword).map(|bound| (keyword, bound.clone())))
        .collect()
}

/// A value that violates one bound, and a value on the bound when one can be
/// constructed generically.
#[cfg(any(
    feature = "browser",
    feature = "cli",
    feature = "db",
    feature = "desktop",
    feature = "http",
    feature = "proc"
))]
fn bound_values(keyword: &str, bound: &Value) -> (Value, Option<Value>) {
    match keyword {
        "minimum" => {
            let minimum = bound
                .as_i64()
                .unwrap_or_else(|| panic!("`minimum` must be an integer, got: {bound}"));
            (json!(minimum - 1), Some(json!(minimum)))
        }
        "minLength" => {
            let minimum = bound
                .as_u64()
                .unwrap_or_else(|| panic!("`minLength` must be an integer, got: {bound}"));
            let length = usize::try_from(minimum).unwrap_or(usize::MAX);
            (
                json!("x".repeat(length.saturating_sub(1))),
                Some(json!("x".repeat(length))),
            )
        }
        // No generic string matches an arbitrary pattern, so only the
        // violating side is constructed: the empty string is refused by every
        // pattern this project writes, and the assertion says so otherwise.
        "pattern" => (json!(""), None),
        other => panic!("add a value for the `{other}` bound to this test"),
    }
}

/// The schema and `validate` agree on every bound a step declares.
///
/// This is the regression gate for a class of drift that has now happened three
/// times: the schema accepted a value that the driver later refused (`name`, a
/// blank classification value, `timeout_ms: 0`). A step's argument schema and
/// its `validate` function are two contracts for the same arguments, and
/// neither may be looser than the other. The reverse direction, a value that
/// `validate` refuses while the schema accepts it, is covered by the two
/// `*_timeout_of_zero_*` tests above.
#[cfg(any(
    feature = "browser",
    feature = "cli",
    feature = "db",
    feature = "desktop",
    feature = "http",
    feature = "proc"
))]
#[test]
fn every_declared_argument_bound_is_enforced_by_validate() {
    let registry = built_in_registry();
    let mut checked = 0;

    for name in registry.names() {
        let definition = registry
            .get(name)
            .expect("every listed step must be registered");
        let base = valid_arguments(name);
        assert!(
            definition.validate(&base).is_ok(),
            "the base arguments for `{name}` must be valid: {base}"
        );
        assert!(
            jsonschema::is_valid(definition.args_schema, &base),
            "the schema must accept the base arguments for `{name}`: {base}"
        );

        let properties = definition.args_schema["properties"]
            .as_object()
            .unwrap_or_else(|| {
                panic!(
                    "`{name}` must declare its arguments as object properties: {}",
                    definition.args_schema
                )
            });
        for (argument, schema) in properties {
            for (keyword, bound) in declared_bounds(schema) {
                let (violating, allowed) = bound_values(keyword, &bound);

                let mut args = base.clone();
                args[argument] = violating;
                assert!(
                    !jsonschema::is_valid(definition.args_schema, &args),
                    "the test must build a value that violates `{keyword}` for `{argument}` of `{name}`: {args}"
                );
                assert!(
                    definition.validate(&args).is_err(),
                    "`{name}` declares `{keyword}: {bound}` for `{argument}`, but validate accepts {args}"
                );

                // The value on the boundary itself must be accepted, so that
                // `da schema` does not promise a value that `da run` refuses.
                if let Some(allowed) = allowed {
                    let mut args = base.clone();
                    args[argument] = allowed;
                    assert!(
                        definition.validate(&args).is_ok(),
                        "`{name}` declares `{keyword}: {bound}` for `{argument}`, but validate rejects {args}"
                    );
                }
                checked += 1;
            }
        }
    }

    let drivers = usize::from(cfg!(feature = "browser"))
        + usize::from(cfg!(feature = "cli"))
        + usize::from(cfg!(feature = "desktop"))
        + usize::from(cfg!(feature = "http"))
        + usize::from(cfg!(feature = "proc"));
    assert!(
        checked >= drivers,
        "expected at least one bounded argument per built-in driver, checked {checked}"
    );
}

#[cfg(feature = "cli")]
#[test]
fn an_optional_step_takes_a_boolean_and_cannot_save() {
    let validator = validator(schema());
    let optional = json!({
        "name": "Optional",
        "steps": [{
            "cli.run": {"command": "da"},
            "optional": true,
            "expect": [{"path": "$.exit_code", "equals": 0}]
        }]
    });
    assert!(
        validator.is_valid(&optional),
        "the parser accepts an optional step, so the schema must too: {optional}"
    );

    let mut explicit = optional.clone();
    explicit["steps"][0]["optional"] = json!(false);
    explicit["steps"][0]["save"] = json!({"version": "$.stdout"});
    assert!(
        validator.is_valid(&explicit),
        "`optional: false` always runs, so it may save: {explicit}"
    );

    let mut saving = optional.clone();
    saving["steps"][0]["save"] = json!({"version": "$.stdout"});
    assert!(
        !validator.is_valid(&saving),
        "the parser refuses an optional step that saves, so the schema must too: {saving}"
    );

    for wrong in [json!("true"), json!(1), json!(null), json!(["true"])] {
        let mut instance = optional.clone();
        instance["steps"][0]["optional"] = wrong.clone();
        assert!(
            !validator.is_valid(&instance),
            "`optional: {wrong}` is not a boolean, so it must not validate"
        );
    }
}

#[cfg(feature = "http")]
#[test]
fn a_retry_block_requires_until_and_positive_durations() {
    let validator = validator(schema());
    let valid = json!({
        "name": "Retry polling",
        "steps": [{
            "http.request": {"url": "http://127.0.0.1:8080/jobs/1"},
            "retry": {
                "until": [{"path": "$.body.status", "equals": "done"}],
                "timeout_ms": 2000
            }
        }]
    });
    assert!(
        validator.is_valid(&valid),
        "the parser accepts retry without interval_ms, so the schema must too: {valid}"
    );

    let mut with_interval = valid.clone();
    with_interval["steps"][0]["retry"]["interval_ms"] = json!(50);
    assert!(
        validator.is_valid(&with_interval),
        "the parser accepts explicit interval_ms, so the schema must too: {with_interval}"
    );

    for (label, bad_retry) in [
        ("missing until", json!({"timeout_ms": 100})),
        ("empty until", json!({"until": [], "timeout_ms": 100})),
        (
            "missing timeout_ms",
            json!({"until": [{"path": "$.status", "equals": 200}]}),
        ),
        (
            "zero timeout_ms",
            json!({"until": [{"path": "$.status", "equals": 200}], "timeout_ms": 0}),
        ),
        (
            "zero interval_ms",
            json!({"until": [{"path": "$.status", "equals": 200}], "timeout_ms": 100, "interval_ms": 0}),
        ),
        (
            "unknown field",
            json!({"until": [{"path": "$.status", "equals": 200}], "timeout_ms": 100, "extra": 1}),
        ),
        ("non-object", json!(true)),
    ] {
        let mut instance = valid.clone();
        instance["steps"][0]["retry"] = bad_retry;
        assert!(
            !validator.is_valid(&instance),
            "`retry` ({label}) must not validate: {instance}"
        );
    }
}

#[cfg(feature = "cli")]
#[test]
fn an_unknown_step_argument_is_rejected() {
    let validator = validator(schema());
    let instance = json!({
        "name": "Unknown argument",
        "steps": [{"cli.run": {"command": "da", "bogus": true}}]
    });

    assert!(
        !validator.is_valid(&instance),
        "an unknown step argument must not validate"
    );
}

/// The `observe` block and the observers behind it, as `da schema` describes
/// them.
#[cfg(all(feature = "log", feature = "cli"))]
#[test]
fn the_schema_describes_observe_and_every_registered_observer() {
    let schema = schema();
    let observe = &schema["properties"]["observe"];

    assert_eq!(observe["type"], "object");
    assert_eq!(
        observe["propertyNames"]["pattern"], "^[a-z0-9._-]+$",
        "the name becomes a key of `observed`, so the schema states the rule"
    );
    let branches = observe["additionalProperties"]["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("registered observers must produce a oneOf, got: {observe}"));
    assert!(
        branches.iter().any(|branch| {
            branch["additionalProperties"] == false
                && branch["properties"]["log.file"]["$ref"] == "#/$defs/log.file"
        }),
        "`observe` must reference the observer definition, got: {branches:?}"
    );
    let definition = &schema["$defs"]["log.file"];
    assert_eq!(definition["type"], "object");
    assert_eq!(definition["properties"]["path"]["minLength"], 1);
    assert!(
        definition["description"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "the observer definition must keep its description, got: {definition}"
    );

    let validator = validator(schema);
    let valid = json!({
        "name": "Observed",
        "observe": {"app_log": {"log.file": {"path": "app.log"}}},
        "steps": [{"cli.run": {"command": "da"}}]
    });
    assert!(
        validator.is_valid(&valid),
        "a case that declares an observation must validate: {valid}"
    );

    for (label, observe, fragment) in [
        (
            "unknown observer",
            json!({"app_log": {"log.fil": {"path": "app.log"}}}),
            "log.fil",
        ),
        (
            "two observers",
            json!({"app_log": {"log.file": {"path": "a.log"}, "log.fil": {"path": "b.log"}}}),
            "log.fil",
        ),
        (
            "empty path",
            json!({"app_log": {"log.file": {"path": ""}}}),
            "path",
        ),
        (
            "unknown argument",
            json!({"app_log": {"log.file": {"path": "a.log", "location": "here"}}}),
            "location",
        ),
        (
            "name outside the rule",
            json!({"App_log": {"log.file": {"path": "a.log"}}}),
            "App_log",
        ),
    ] {
        let mut instance = valid.clone();
        instance["observe"] = observe;
        let errors: Vec<String> = validator
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect();
        assert!(
            !errors.is_empty(),
            "{label}: the schema must reject {instance}"
        );
        assert!(
            errors.iter().any(|error| error.contains(fragment)),
            "{label}: the error must mention `{fragment}`, got: {errors:?}"
        );
    }
}

/// The schema and `validate` agree on every bound an observer declares.
///
/// This is the same gate as [`every_declared_argument_bound_is_enforced_by_validate`],
/// applied to the second kind of extension point the registry holds.
#[cfg(all(feature = "log", any(feature = "cli", feature = "http")))]
#[test]
fn every_declared_observer_argument_bound_is_enforced_by_validate() {
    let registry = built_in_registry();
    let mut checked = 0;

    for name in registry.observer_names() {
        let definition = registry
            .observer(name)
            .expect("every listed observer must be registered");
        let base = observer_arguments(name);
        assert!(
            definition.validate(&base).is_ok(),
            "the base arguments for `{name}` must be valid: {base}"
        );
        assert!(
            jsonschema::is_valid(definition.args_schema, &base),
            "the schema must accept the base arguments for `{name}`: {base}"
        );
        assert_eq!(
            schema()["$defs"][name]["properties"],
            definition.args_schema["properties"],
            "`da schema` must carry the observer's arguments"
        );

        let properties = definition.args_schema["properties"]
            .as_object()
            .unwrap_or_else(|| {
                panic!(
                    "`{name}` must declare its arguments as object properties: {}",
                    definition.args_schema
                )
            });
        for (argument, schema) in properties {
            for (keyword, bound) in declared_bounds(schema) {
                let (violating, allowed) = bound_values(keyword, &bound);

                let mut args = base.clone();
                args[argument] = violating;
                assert!(
                    !jsonschema::is_valid(definition.args_schema, &args),
                    "the test must build a value that violates `{keyword}` for `{argument}` of `{name}`: {args}"
                );
                assert!(
                    definition.validate(&args).is_err(),
                    "`{name}` declares `{keyword}: {bound}` for `{argument}`, but validate accepts {args}"
                );

                if let Some(allowed) = allowed {
                    let mut args = base.clone();
                    args[argument] = allowed;
                    assert!(
                        definition.validate(&args).is_ok(),
                        "`{name}` declares `{keyword}: {bound}` for `{argument}`, but validate rejects {args}"
                    );
                }
                checked += 1;
            }
        }
    }

    assert!(
        checked > 0,
        "expected at least one bounded observer argument"
    );
}

/// Arguments each built-in observer accepts, before one is overridden.
#[cfg(all(feature = "log", any(feature = "cli", feature = "http")))]
fn observer_arguments(observer: &str) -> Value {
    match observer {
        "browser.network" => json!({}),
        "fs.watch" => json!({"path": "."}),
        "log.file" => json!({"path": "app.log"}),
        other => panic!("add the valid arguments of `{other}` to this test"),
    }
}

#[cfg(feature = "cli")]
#[test]
fn the_schema_accepts_every_valid_repository_case() {
    let validator = validator(schema());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    for file in files_with_extension(root.join("tests/self"), "yaml") {
        let text = fs::read_to_string(&file).expect("the self case must be readable");
        let instance: Value =
            serde_saphyr::from_str(&text).expect("the self case must be valid YAML");
        // A build without the observer cannot describe a case that declares
        // one; the case itself is still checked by every build that has it.
        if instance.get("observe").is_some() && !describes_observations(schema()) {
            continue;
        }
        assert!(
            validator.is_valid(&instance),
            "the schema rejected the valid self case {}",
            file.display()
        );
    }

    for file in files_with_extension(root.join("tests/self/fixtures"), "case") {
        let text = fs::read_to_string(&file).expect("the fixture must be readable");
        if do_anything::core::case::parse(&text).is_err() {
            let instance: Value =
                serde_saphyr::from_str(&text).expect("an invalid fixture is still valid YAML");
            assert!(
                !validator.is_valid(&instance),
                "the schema accepted the invalid fixture {}",
                file.display()
            );
            continue;
        }
        let instance: Value =
            serde_saphyr::from_str(&text).expect("a parsable fixture must be valid YAML");
        assert!(
            validator.is_valid(&instance),
            "the schema rejected the valid fixture {}",
            file.display()
        );
    }
}

/// Whether this build can describe an observation at all.
#[cfg(any(feature = "browser", feature = "cli", feature = "desktop"))]
fn describes_observations(schema: &Value) -> bool {
    schema["properties"]["observe"]["additionalProperties"] != Value::Bool(false)
}

#[cfg(feature = "browser")]
#[test]
fn browser_dialog_schema_agrees_with_validate() {
    let validator = validator(schema());
    let registry = built_in_registry();
    let bases: [(&str, Value); 5] = [
        ("browser.open", json!({"url": "http://127.0.0.1:1/"})),
        ("browser.click", json!({"css": "#go"})),
        ("browser.type", json!({"css": "#go", "text": "hello"})),
        ("browser.press", json!({"key": "Enter"})),
        ("browser.wait", json!({"css": "#go"})),
    ];
    for (step_name, base) in bases {
        let def = registry.get(step_name).expect("browser step must exist");
        for dialog in [
            json!("accept"),
            json!("dismiss"),
            json!({"action": "accept"}),
            json!({"action": "dismiss"}),
            json!({"action": "accept", "prompt_text": "token"}),
        ] {
            let mut step_args = base.clone();
            step_args["dialog"] = dialog.clone();
            let case = json!({
                "name": "Dialog schema check",
                "steps": [{ step_name: step_args.clone() }]
            });
            assert!(
                validator.is_valid(&case),
                "`{step_name}` schema must accept {dialog}: {case}"
            );
            assert!(
                def.validate(&step_args).is_ok(),
                "`{step_name}` validate must accept {dialog}: {step_args}"
            );
        }
        for bad in [
            json!(null),
            json!("cancel"),
            json!({}),
            json!({"action": "accept", "prompt_text": null}),
            json!({"action": "accept", "extra": true}),
        ] {
            let mut step_args = base.clone();
            step_args["dialog"] = bad.clone();
            let case = json!({
                "name": "Invalid dialog schema check",
                "steps": [{ step_name: step_args.clone() }]
            });
            assert!(
                !validator.is_valid(&case),
                "`{step_name}` schema must reject {bad}: {case}"
            );
            assert!(
                def.validate(&step_args).is_err(),
                "`{step_name}` validate must reject {bad}: {step_args}"
            );
        }
    }
}

/// Whether this build registers every step a documented case uses.
///
/// A build with `http` and `cli` but no `browser` cannot describe the browser
/// example, and that is not a schema defect: the same rule already applies to
/// an example that declares an observer.
#[cfg(any(
    feature = "browser",
    feature = "desktop",
    all(feature = "http", feature = "cli")
))]
fn describes_steps(schema: &Value, instance: &Value) -> bool {
    instance["steps"].as_array().is_none_or(|steps| {
        steps.iter().all(|step| {
            step.as_object().is_none_or(|fields| {
                fields
                    .keys()
                    .all(|name| schema["$defs"].get(name).is_some())
            })
        })
    })
}

#[cfg(any(
    feature = "browser",
    feature = "desktop",
    all(feature = "http", feature = "cli")
))]
#[test]
fn the_schema_accepts_every_case_example_in_the_docs() {
    let validator = validator(schema());
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut checked = 0;
    for doc in ["README.md", "README.zh-CN.md", "docs/guide.md"] {
        let text = fs::read_to_string(root.join(doc)).expect("the document must be readable");
        for example in case_examples(&text) {
            assert!(
                do_anything::core::case::parse(example).is_ok(),
                "an example in {doc} must parse as a case:
{example}"
            );
            let instance: Value =
                serde_saphyr::from_str(example).expect("the example must be valid YAML");
            // A build without a driver cannot describe an example that uses
            // its steps; every build that has the driver checks it.
            if !describes_steps(schema(), &instance) {
                continue;
            }
            // A build without the observer cannot describe an example that
            // declares one; every build that has the observer checks it.
            if instance.get("observe").is_some() && !describes_observations(schema()) {
                continue;
            }
            assert!(
                validator.is_valid(&instance),
                "the schema rejected an example in {doc}:
{example}"
            );
            checked += 1;
        }
    }
    // The http and cli build checks every example that has those drivers; the
    // browser-only build checks the browser example and skips the rest.
    let minimum = if cfg!(all(feature = "http", feature = "cli")) {
        5
    } else {
        1
    };
    assert!(
        checked >= minimum,
        "expected the docs to hold at least {minimum} example(s), found {checked}"
    );
}

#[cfg(feature = "cli")]
#[test]
fn the_schema_embeds_each_registered_step_with_its_description() {
    let schema = schema();
    let definition = &schema["$defs"]["cli.run"];
    assert!(
        definition["description"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "the step definition must keep its description, got: {definition}"
    );
    assert_eq!(definition["type"], "object");

    let branches = schema["properties"]["steps"]["items"]["oneOf"]
        .as_array()
        .unwrap_or_else(|| panic!("the step schema must be a oneOf, got: {schema}"));
    assert!(
        branches.iter().any(|branch| {
            branch["properties"]["cli.run"]["$ref"]
                .as_str()
                .is_some_and(|reference| reference == "#/$defs/cli.run")
        }),
        "the steps schema must reference the step definition, got: {branches:?}"
    );
}

#[cfg(not(any(
    feature = "android",
    feature = "browser",
    feature = "cli",
    feature = "db",
    feature = "desktop",
    feature = "http",
    feature = "proc"
)))]
#[test]
fn the_schema_is_valid_when_no_step_is_registered() {
    let schema = schema();
    assert_eq!(
        schema["properties"]["steps"]["items"],
        Value::Bool(false),
        "without registered steps no step item can be valid"
    );
    let definitions = schema["$defs"]
        .as_object()
        .expect("$defs must be an object");
    assert!(
        definitions.contains_key("expect") && definitions.contains_key("save"),
        "a build without steps keeps the shared assertion and save definitions"
    );
    assert!(
        !definitions.contains_key("cli.run") && !definitions.contains_key("http.request"),
        "a build without steps must not describe a step: {:?}",
        definitions.keys().collect::<Vec<_>>()
    );
    jsonschema::meta::validate(schema).expect("the no-driver schema must be a valid schema");

    let instance = json!({"name": "No driver", "steps": [{"anything": {}}]});
    assert!(
        !jsonschema::is_valid(schema, &instance),
        "a build without steps must not accept a case with a step"
    );
}

#[cfg(feature = "browser")]
#[test]
fn browser_open_profile_and_set_cookies_schema_match_validate() {
    let validator = validator(schema());

    let with_profile = json!({
        "name": "Named profile",
        "steps": [{
            "browser.open": {
                "url": "http://127.0.0.1:8080/",
                "profile": "admin_ci-1",
                "headless": false
            }
        }]
    });
    assert!(
        validator.is_valid(&with_profile),
        "named profile must validate: {with_profile}"
    );

    for bad_profile in [
        json!(""),
        json!("../etc"),
        json!("a/b"),
        json!("a\\b"),
        json!("bad name"),
        json!(null),
    ] {
        let mut instance = with_profile.clone();
        instance["steps"][0]["browser.open"]["profile"] = bad_profile.clone();
        assert!(
            !validator.is_valid(&instance),
            "invalid profile {bad_profile} must be rejected by schema"
        );
    }

    let both_connect_and_profile = json!({
        "name": "Conflict",
        "steps": [{
            "browser.open": {
                "url": "http://127.0.0.1:8080/",
                "connect": "http://127.0.0.1:9222",
                "profile": "admin"
            }
        }]
    });
    assert!(
        !validator.is_valid(&both_connect_and_profile),
        "connect and profile must be mutually exclusive in schema"
    );

    let set_cookies_valid = json!({
        "name": "Set cookies and local storage",
        "steps": [{
            "browser.set_cookies": {
                "cookies": [{
                    "name": "sid",
                    "value": "tok123",
                    "url": "http://127.0.0.1:8080/",
                    "http_only": true,
                    "same_site": "Lax"
                }],
                "local_storage": {
                    "http://127.0.0.1:8080": {
                        "token": "tok123"
                    }
                }
            }
        }]
    });
    assert!(
        validator.is_valid(&set_cookies_valid),
        "valid browser.set_cookies must validate: {set_cookies_valid}"
    );

    for (label, bad_args) in [
        ("empty object", json!({})),
        ("empty cookies", json!({"cookies": []})),
        (
            "missing url and domain",
            json!({"cookies": [{"name": "sid", "value": "tok"}]}),
        ),
        (
            "empty cookie name",
            json!({"cookies": [{"name": "", "value": "tok", "url": "http://127.0.0.1:8080/"}]}),
        ),
        (
            "empty cookie value",
            json!({"cookies": [{"name": "sid", "value": "", "url": "http://127.0.0.1:8080/"}]}),
        ),
        (
            "null cookie url",
            json!({"cookies": [{"name": "sid", "value": "tok", "url": null, "domain": "127.0.0.1"}]}),
        ),
        ("empty local_storage map", json!({"local_storage": {}})),
        (
            "empty origin items",
            json!({"local_storage": {"current": {}}}),
        ),
    ] {
        let instance = json!({
            "name": "Invalid set_cookies",
            "steps": [{ "browser.set_cookies": bad_args }]
        });
        assert!(
            !validator.is_valid(&instance),
            "browser.set_cookies ({label}) must fail schema: {instance}"
        );
    }
}

#[cfg(feature = "browser")]
#[test]
fn browser_storage_state_schema_match_validate() {
    let validator = validator(schema());

    let loads_state = json!({
        "name": "Reuse a saved login",
        "steps": [{
            "browser.open": {
                "url": "http://127.0.0.1:8080/orders",
                "storage_state": "state/admin.json"
            }
        }]
    });
    assert!(
        validator.is_valid(&loads_state),
        "a storage-state file must validate: {loads_state}"
    );

    for bad_path in [json!(""), json!(null)] {
        let mut instance = loads_state.clone();
        instance["steps"][0]["browser.open"]["storage_state"] = bad_path.clone();
        assert!(
            !validator.is_valid(&instance),
            "invalid storage_state {bad_path} must be rejected by schema"
        );
    }

    let connected_with_state = json!({
        "name": "Conflict",
        "steps": [{
            "browser.open": {
                "url": "http://127.0.0.1:8080/",
                "connect": "http://127.0.0.1:9222",
                "storage_state": "state/admin.json"
            }
        }]
    });
    assert!(
        !validator.is_valid(&connected_with_state),
        "connect and storage_state must be mutually exclusive in schema"
    );

    let saves_state = json!({
        "name": "Save the login",
        "steps": [{
            "browser.save_state": {
                "path": "state/admin.json"
            }
        }]
    });
    assert!(
        validator.is_valid(&saves_state),
        "a valid browser.save_state must validate: {saves_state}"
    );

    for (label, bad_args) in [
        ("missing path", json!({})),
        ("empty path", json!({"path": ""})),
        ("null path", json!({"path": null})),
        (
            "unknown argument",
            json!({"path": "state.json", "cookies": []}),
        ),
    ] {
        let instance = json!({
            "name": "Invalid save_state",
            "steps": [{ "browser.save_state": bad_args }]
        });
        assert!(
            !validator.is_valid(&instance),
            "browser.save_state ({label}) must fail schema: {instance}"
        );
    }
}

#[cfg(feature = "browser")]
#[test]
fn browser_upload_schema_match_validate() {
    let validator = validator(schema());

    let uploads = json!({
        "name": "Upload an avatar",
        "steps": [{
            "browser.upload": {
                "css": "#avatar",
                "files": ["fixtures/avatar.png"]
            }
        }]
    });
    assert!(
        validator.is_valid(&uploads),
        "a valid browser.upload must validate: {uploads}"
    );

    for locator in [
        json!({"xpath": "//*[@id='avatar']"}),
        json!({"text": "Choose avatar"}),
        json!({"role": "button"}),
        json!({"testid": "avatar"}),
    ] {
        let mut instance = uploads.clone();
        instance["steps"][0]["browser.upload"] = locator.clone();
        instance["steps"][0]["browser.upload"]["files"] = json!(["fixtures/avatar.png"]);
        assert!(
            validator.is_valid(&instance),
            "every locator form must validate with files: {instance}"
        );
    }

    let clearing = json!({
        "name": "Clear the selection",
        "steps": [{ "browser.upload": { "css": "#avatar", "files": [] } }]
    });
    assert!(
        validator.is_valid(&clearing),
        "an empty files list clears the input and must validate: {clearing}"
    );

    for (label, bad_args) in [
        ("missing files", json!({"css": "#avatar"})),
        ("null files", json!({"css": "#avatar", "files": null})),
        ("empty path", json!({"css": "#avatar", "files": [""]})),
        (
            "two locators",
            json!({"css": "#avatar", "testid": "avatar", "files": ["a.txt"]}),
        ),
        (
            "name without role",
            json!({"name": "Avatar", "files": ["a.txt"]}),
        ),
        (
            "zero timeout",
            json!({"css": "#avatar", "files": ["a.txt"], "timeout_ms": 0}),
        ),
        (
            "unknown argument",
            json!({"css": "#avatar", "files": ["a.txt"], "accept": "*/*"}),
        ),
    ] {
        let instance = json!({
            "name": "Invalid upload",
            "steps": [{ "browser.upload": bad_args }]
        });
        assert!(
            !validator.is_valid(&instance),
            "browser.upload ({label}) must fail schema: {instance}"
        );
    }
}

#[cfg(feature = "desktop")]
#[test]
fn desktop_steps_schema_accepts_valid_cases_and_rejects_unknown_fields() {
    let validator = validator(schema());

    let valid = json!({
        "name": "Desktop schema check",
        "steps": [
            { "desktop.launch": { "command": "app.exe", "window_title_contains": "Main" } },
            { "desktop.attach": { "pid": 42, "timeout_ms": 5000 } },
            { "desktop.click": { "at": { "x": 10, "y": 20, "window": { "width": 440, "height": 320 } } } },
            { "desktop.type": { "automation_id": "userBox", "text": "alice" } },
            { "desktop.press": { "key": "Ctrl+S" } },
            { "desktop.wait": { "automation_id": "status", "state": "present" } },
            { "desktop.read": { "automation_id": "status" } },
            { "desktop.screenshot": { "path": "shot.png", "mask_inputs": true } }
        ]
    });
    assert!(
        validator.is_valid(&valid),
        "valid desktop.* case must satisfy emitted schema: {valid}"
    );

    for (label, bad_step) in [
        (
            "empty command",
            json!({ "desktop.launch": { "command": "" } }),
        ),
        ("zero pid", json!({ "desktop.attach": { "pid": 0 } })),
        (
            "negative x",
            json!({ "desktop.click": { "at": { "x": -1, "y": 0 } } }),
        ),
        (
            "zero window width",
            json!({ "desktop.click": { "at": { "x": 1, "y": 1, "window": { "width": 0, "height": 100 } } } }),
        ),
        (
            "session-only ref in case schema",
            json!({ "desktop.click": { "ref": "e1" } }),
        ),
        (
            "unknown field",
            json!({ "desktop.press": { "key": "Enter", "extra": true } }),
        ),
    ] {
        let invalid = json!({
            "name": "Invalid desktop step",
            "steps": [bad_step]
        });
        assert!(
            !validator.is_valid(&invalid),
            "desktop schema ({label}) must reject invalid step: {invalid}"
        );
    }
}

#[cfg(feature = "browser")]
#[test]
fn browser_computer_use_schemas_match_validate() {
    let validator = validator(schema());
    let registry = built_in_registry();

    let check_step = |step: &str, valid: Vec<Value>, invalid: Vec<(&str, Value)>| {
        let def = registry
            .get(step)
            .unwrap_or_else(|| panic!("`{step}` must be registered"));
        for args in valid {
            let instance = json!({
                "name": format!("Valid {step}"),
                "steps": [{ step: args.clone() }]
            });
            assert!(
                validator.is_valid(&instance),
                "`{step}` schema must accept {args}"
            );
            assert!(
                def.validate(&args).is_ok(),
                "`{step}` validate must accept {args}"
            );
        }
        for (label, args) in invalid {
            let instance = json!({
                "name": format!("Invalid {step}: {label}"),
                "steps": [{ step: args.clone() }]
            });
            assert!(
                !validator.is_valid(&instance),
                "`{step}` schema must reject ({label}): {args}"
            );
            assert!(
                def.validate(&args).is_err(),
                "`{step}` validate must reject ({label}): {args}"
            );
        }
    };

    check_step(
        "browser.scroll",
        vec![
            json!({"delta_y": 200}),
            json!({"delta_x": -120, "delta_y": 40}),
            json!({"css": "#list", "delta_y": 150}),
            json!({"testid": "footer"}),
            json!({"testid": "footer", "into_view": true}),
            json!({"role": "region", "name": "Feed", "delta_y": 200, "dialog": "accept"}),
            json!({"at": {"x": 10, "y": 20}, "delta_y": 100}),
            json!({"at": {"x": 10, "y": 20, "viewport": {"width": 800, "height": 600}}, "delta_x": -50}),
        ],
        vec![
            ("empty object", json!({})),
            ("into_view without locator", json!({"into_view": true})),
            ("into_view false", json!({"css": "#a", "into_view": false})),
            (
                "delta with into_view",
                json!({"css": "#a", "delta_y": 100, "into_view": true}),
            ),
            ("at without delta", json!({"at": {"x": 10, "y": 20}})),
            (
                "at with locator",
                json!({"at": {"x": 10, "y": 20}, "css": "#a", "delta_y": 100}),
            ),
            (
                "within without locator",
                json!({"within": {"css": "#box"}, "delta_y": 100}),
            ),
            ("name without role", json!({"name": "Btn", "delta_y": 100})),
            ("zero timeout", json!({"delta_y": 100, "timeout_ms": 0})),
            ("null delta_y", json!({"delta_y": null})),
            ("unknown field", json!({"delta_y": 100, "extra": true})),
            ("ref in case", json!({"ref": "e1", "delta_y": 100})),
        ],
    );

    check_step(
        "browser.hover",
        vec![
            json!({"css": "#menu"}),
            json!({"xpath": "//button"}),
            json!({"text": "More"}),
            json!({"role": "button", "name": "More"}),
            json!({"testid": "menu"}),
            json!({"at": {"x": 10, "y": 20}}),
            json!({"at": {"x": 10, "y": 20, "viewport": {"width": 800, "height": 600}}, "dialog": "dismiss"}),
        ],
        vec![
            ("empty object", json!({})),
            ("two locators", json!({"css": "#a", "testid": "b"})),
            (
                "at with locator",
                json!({"css": "#a", "at": {"x": 10, "y": 20}}),
            ),
            ("name without role", json!({"name": "More"})),
            ("negative at x", json!({"at": {"x": -1, "y": 0}})),
            ("zero timeout", json!({"css": "#a", "timeout_ms": 0})),
            ("null css", json!({"css": null})),
            ("ref in case", json!({"ref": "e1"})),
        ],
    );

    check_step(
        "browser.drag",
        vec![
            json!({"from": {"css": "#a"}, "to": {"testid": "b"}}),
            json!({
                "from": {"role": "button", "name": "Card"},
                "to": {"css": ".col", "within": {"testid": "board"}, "nth": 1}
            }),
            json!({
                "from": {"at": {"x": 10, "y": 20}},
                "to": {"at": {"x": 200, "y": 300, "viewport": {"width": 1280, "height": 800}}},
                "dialog": "accept"
            }),
            json!({"from": {"testid": "item"}, "to": {"at": {"x": 300, "y": 150}}}),
        ],
        vec![
            ("empty object", json!({})),
            ("missing to", json!({"from": {"css": "#a"}})),
            ("missing from", json!({"to": {"css": "#b"}})),
            ("empty from", json!({"from": {}, "to": {"css": "#b"}})),
            (
                "two locators in from",
                json!({"from": {"css": "#a", "testid": "a"}, "to": {"css": "#b"}}),
            ),
            (
                "at and locator in from",
                json!({"from": {"css": "#a", "at": {"x": 1, "y": 2}}, "to": {"css": "#b"}}),
            ),
            (
                "name without role",
                json!({"from": {"name": "Item"}, "to": {"css": "#b"}}),
            ),
            (
                "negative coord",
                json!({"from": {"at": {"x": -1, "y": 2}}, "to": {"css": "#b"}}),
            ),
            (
                "zero timeout",
                json!({"from": {"css": "#a"}, "to": {"css": "#b"}, "timeout_ms": 0}),
            ),
            (
                "ref in case",
                json!({"from": {"ref": "e1"}, "to": {"ref": "e2"}}),
            ),
        ],
    );

    check_step(
        "browser.set_clipboard",
        vec![
            json!({"text": "hello"}),
            json!({"text": ""}),
            json!({"text": "secret", "timeout_ms": 1000}),
        ],
        vec![
            ("empty object", json!({})),
            ("null text", json!({"text": null})),
            ("non-string text", json!({"text": 123})),
            ("zero timeout", json!({"text": "hi", "timeout_ms": 0})),
            ("unknown field", json!({"text": "hi", "extra": true})),
        ],
    );

    check_step(
        "browser.read_clipboard",
        vec![json!({}), json!({"timeout_ms": 1000})],
        vec![
            ("zero timeout", json!({"timeout_ms": 0})),
            ("null timeout", json!({"timeout_ms": null})),
            ("unknown field", json!({"extra": true})),
        ],
    );
}

#[cfg(feature = "proc")]
#[test]
fn proc_steps_schema_agrees_with_validate() {
    let validator = validator(schema());
    let registry = built_in_registry();

    let valid = json!({
        "name": "Process session",
        "steps": [
            {
                "proc.start": {
                    "name": "api_1.0",
                    "command": "da",
                    "args": ["--version"],
                    "cwd": ".",
                    "env": {"PORT": "8080"},
                    "ready_pattern": "Listening",
                    "ready_timeout_ms": 2000
                }
            },
            { "proc.send": { "name": "api_1.0", "input": "ping\n", "eof": true } },
            { "proc.send": { "eof": true } },
            {
                "proc.wait_output": {
                    "name": "api_1.0",
                    "pattern": "pong",
                    "stream": "stderr",
                    "timeout_ms": 1000
                }
            },
            { "proc.stop": { "name": "api_1.0", "timeout_ms": 500 } },
            { "proc.stop": {} }
        ]
    });
    assert!(
        validator.is_valid(&valid),
        "valid proc.* steps must validate against the case schema: {valid}"
    );

    for (step_name, bad_args) in [
        ("proc.start", json!({})),
        ("proc.start", json!({"command": ""})),
        ("proc.start", json!({"command": "da", "name": "../bad"})),
        ("proc.start", json!({"command": "da", "name": "."})),
        ("proc.start", json!({"command": "da", "ready_pattern": ""})),
        (
            "proc.start",
            json!({"command": "da", "ready_timeout_ms": 0}),
        ),
        ("proc.start", json!({"command": "da", "extra": true})),
        ("proc.send", json!({})),
        ("proc.send", json!({"eof": false})),
        ("proc.send", json!({"input": "x", "name": "/bad"})),
        ("proc.send", json!({"input": "x", "timeout_ms": 0})),
        ("proc.send", json!({"input": "x", "extra": 1})),
        ("proc.wait_output", json!({})),
        ("proc.wait_output", json!({"pattern": ""})),
        (
            "proc.wait_output",
            json!({"pattern": "ok", "stream": "invalid"}),
        ),
        (
            "proc.wait_output",
            json!({"pattern": "ok", "timeout_ms": 0}),
        ),
        ("proc.wait_output", json!({"pattern": "ok", "extra": 1})),
        ("proc.stop", json!({"name": "bad name"})),
        ("proc.stop", json!({"timeout_ms": 0})),
        ("proc.stop", json!({"extra": 1})),
    ] {
        let def = registry
            .get(step_name)
            .unwrap_or_else(|| panic!("`{step_name}` must be registered"));
        let instance = json!({
            "name": "Invalid proc",
            "steps": [{ step_name: bad_args.clone() }]
        });
        assert!(
            !validator.is_valid(&instance),
            "`{step_name}` schema must reject {bad_args}: {instance}"
        );
        assert!(
            (def.validate)(&bad_args).is_err(),
            "`{step_name}` validate must reject {bad_args}"
        );
    }
}

#[cfg(feature = "fs")]
#[test]
fn fs_watch_schema_agrees_with_validate() {
    let validator = validator(schema());
    let registry = built_in_registry();
    let def = registry
        .observer("fs.watch")
        .expect("`fs.watch` must be registered");

    let step = if cfg!(feature = "proc") {
        json!({"proc.stop": {}})
    } else if cfg!(feature = "cli") {
        json!({"cli.run": {"command": "da"}})
    } else if cfg!(feature = "http") {
        json!({"http.request": {"url": "http://127.0.0.1:1/"}})
    } else if cfg!(feature = "db") {
        json!({"db.query": {"url": "sqlite:app.db", "sql": "SELECT 1"}})
    } else if cfg!(feature = "browser") {
        json!({"browser.open": {"url": "http://127.0.0.1:1/"}})
    } else {
        json!(null)
    };

    if !step.is_null() {
        let valid = json!({
            "name": "Watch directory",
            "observe": {
                "out_dir": {
                    "fs.watch": {
                        "path": "artifacts",
                        "recursive": false,
                        "max_entries": 250,
                        "hash": true
                    }
                }
            },
            "steps": [step]
        });
        assert!(
            validator.is_valid(&valid),
            "valid fs.watch observation must validate: {valid}"
        );
    }

    for valid_args in [
        json!({"path": "artifacts"}),
        json!({"path": "artifacts", "recursive": true, "max_entries": 1, "hash": false}),
    ] {
        assert!(
            jsonschema::is_valid(&def.args_schema, &valid_args),
            "fs.watch schema must accept {valid_args}"
        );
        assert!(
            (def.validate)(&valid_args).is_ok(),
            "fs.watch validate must accept {valid_args}"
        );
    }

    for bad_args in [
        json!({}),
        json!({"path": ""}),
        json!({"path": "out", "max_entries": 0}),
        json!({"path": "out", "recursive": "yes"}),
        json!({"path": "out", "hash": 1}),
        json!({"path": "out", "extra": true}),
    ] {
        assert!(
            !jsonschema::is_valid(&def.args_schema, &bad_args),
            "fs.watch schema must reject {bad_args}"
        );
        assert!(
            (def.validate)(&bad_args).is_err(),
            "fs.watch validate must reject {bad_args}"
        );
    }
}

/// Every file directly inside `dir` with the given extension, sorted by path.
#[cfg(feature = "cli")]
fn files_with_extension(dir: PathBuf, extension: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("failed to list {}: {error}", dir.display()))
        .map(|entry| entry.expect("failed to read a directory entry").path())
        .filter(|path| path.extension().is_some_and(|value| value == extension))
        .collect();
    files.sort();
    files
}

/// Complete case files in a document: `yaml` blocks with a top-level `name:`.
///
/// Fragments that show only part of a case have no `name:` line and are skipped.
#[cfg(any(
    feature = "browser",
    feature = "desktop",
    all(feature = "http", feature = "cli")
))]
fn case_examples(text: &str) -> Vec<&str> {
    text.split(
        "```yaml
",
    )
    .skip(1)
    .filter_map(|rest| {
        rest.split_once(
            "
```",
        )
        .map(|(block, _)| block)
    })
    .filter(|block| block.lines().any(|line| line.starts_with("name:")))
    .collect()
}
