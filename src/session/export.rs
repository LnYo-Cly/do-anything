//! Deterministic export of executed session steps into a replayable YAML case.

use serde_json::{Map, Value, json};

use crate::core::case;
use crate::session::model_hook::{
    STEP_ACTION_ACT, STEP_ACTION_EXTRACT, STEP_ACTION_JUDGE, STEP_ACTION_LOCATE,
    STEP_ACTION_SELECT_REF,
};
use crate::session::store::{StepRecord, StoredObservation};

/// Options controlling how a session is exported to YAML.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    /// Case name written into `name:`. Defaults to a name derived from the session id.
    pub case_name: Option<String>,
    /// Include `browser.read` and `desktop.read` steps that have neither `expect` nor `save`.
    pub include_reads: bool,
}

/// Output produced by [`export_session`].
#[derive(Debug, Clone)]
pub struct ExportResult {
    pub case_name: String,
    pub yaml: String,
    pub exported_steps: usize,
    pub skipped_steps: Vec<Value>,
    pub fragile: Vec<Value>,
}

/// Builds a deterministic, `da lint`-valid YAML case file from recorded session steps.
pub fn export_session(
    session_id: &str,
    observe: &[StoredObservation],
    records: &[StepRecord],
    options: &ExportOptions,
) -> Result<ExportResult, String> {
    let case_name = options
        .case_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("Session {session_id}"));

    let mut exported = Vec::new();
    let mut skipped_steps = Vec::new();
    let mut fragile = Vec::new();

    for record in records {
        if matches!(
            record.action.as_str(),
            STEP_ACTION_SELECT_REF
                | STEP_ACTION_LOCATE
                | STEP_ACTION_JUDGE
                | STEP_ACTION_EXTRACT
                | STEP_ACTION_ACT
        ) {
            skipped_steps.push(json!({
                "index": record.index,
                "action": record.action,
                "reason": "exploration-only model hook diagnostic",
            }));
            continue;
        }
        let exportable_skipped =
            record.optional && record.status == "skipped" && !has_unresolved_refs(record);
        if record.status != "passed" && !exportable_skipped {
            skipped_steps.push(json!({
                "index": record.index,
                "action": record.action,
                "reason": format!("step {}", record.status),
            }));
            continue;
        }
        if !options.include_reads
            && matches!(record.action.as_str(), "browser.read" | "desktop.read")
            && record.expect.is_empty()
            && record.save.is_empty()
        {
            skipped_steps.push(json!({
                "index": record.index,
                "action": record.action,
                "reason": "read-only step without expect or save",
            }));
            continue;
        }

        if record.redacted_args {
            return Err(format!(
                "step {} (`{}`) passed an inline secret value that was redacted to `***` in the session log; re-run it with a `${{env:NAME}}` or saved `${{var}}` placeholder so the exported case can replay without embedding secrets",
                record.index, record.action
            ));
        }

        let args = materialize_args(record)?;
        let exported_index = exported.len() + 1;
        let coord_bounds = coordinates_bounds(&record.action, &args);
        if record.fragile || coord_bounds.is_some() {
            let mut entry = json!({
                "step_index": record.index,
                "exported_index": exported_index,
                "action": record.action,
                "ref": record.used_ref,
                "locator": fragile_locator_payload(record),
            });
            match &coord_bounds {
                Some((field, bounds)) => {
                    entry["reason"] = json!("explicit_coordinates");
                    entry[*field] = bounds.clone();
                }
                None => entry["reason"] = json!("synthesized_locator"),
            }
            fragile.push(entry);
        }

        exported.push((record, args));
    }

    if exported.is_empty() {
        return Err(
            "no successful exportable steps in session; run at least one passing step before exporting"
                .to_string(),
        );
    }

    let mut yaml = String::new();
    yaml.push_str(&format!("name: {}\n", format_scalar_string(&case_name)));
    if !observe.is_empty() {
        yaml.push_str("observe:\n");
        for item in observe {
            yaml.push_str(&format!("  {}:\n", format_key(&item.name)));
            yaml.push_str(&format!("    {}:", format_key(&item.kind)));
            match &item.raw_args {
                Value::Null => yaml.push_str(" {}\n"),
                Value::Object(map) if map.is_empty() => yaml.push_str(" {}\n"),
                other => {
                    yaml.push('\n');
                    write_yaml_value(&mut yaml, other, 6);
                }
            }
        }
    }
    yaml.push_str("steps:\n");

    for (record, args) in &exported {
        if let Some((field, bounds)) = coordinates_bounds(&record.action, args) {
            let width = bounds.get("width").and_then(Value::as_u64).unwrap_or(0);
            let height = bounds.get("height").and_then(Value::as_u64).unwrap_or(0);
            yaml.push_str(&format!(
                "  # fragile: explicit coordinates recorded at {field} {width}x{height}; replay this case at the same size\n"
            ));
        } else if record.fragile {
            let ref_label = record.used_ref.as_deref().unwrap_or("target");
            let fallback_kind = if record.action.starts_with("desktop.") {
                "UIA"
            } else {
                "CSS"
            };
            yaml.push_str(&format!(
                "  # fragile: structural {fallback_kind} path fallback for ref `{ref_label}`\n"
            ));
        }
        yaml.push_str(&format!("  - {}:", record.action));
        if let Value::Object(map) = args
            && map.is_empty()
        {
            yaml.push_str(" {}\n");
        } else {
            yaml.push('\n');
            write_yaml_value(&mut yaml, args, 6);
        }

        if record.optional {
            yaml.push_str("    optional: true\n");
        }

        if !record.expect.is_empty() {
            yaml.push_str("    expect:\n");
            for item in &record.expect {
                yaml.push_str(&format!(
                    "      - path: {}\n",
                    format_scalar_string(&item.path)
                ));
                yaml.push_str(&format!(
                    "        {}: {}\n",
                    item.comparison,
                    format_inline_or_block(&item.expected, 8)
                ));
            }
        }

        if !record.save.is_empty() {
            yaml.push_str("    save:\n");
            for (var_name, json_path) in &record.save {
                yaml.push_str(&format!(
                    "      {}: {}\n",
                    format_key(var_name),
                    format_scalar_string(json_path)
                ));
            }
        }
    }

    // Verify that the exported YAML parses cleanly with the case parser.
    case::parse(&yaml).map_err(|error| {
        format!("internal export error: generated YAML failed case parsing: {error}")
    })?;

    Ok(ExportResult {
        case_name,
        yaml,
        exported_steps: exported.len(),
        skipped_steps,
        fragile,
    })
}

/// Returns whether a skipped optional step left any `ref` without a synthesized locator.
fn has_unresolved_refs(record: &StepRecord) -> bool {
    if record.action == "browser.drag" {
        let synth = record.synthesized_locator.as_ref();
        for side in ["from", "to"] {
            if record
                .raw_args
                .get(side)
                .and_then(|endpoint| endpoint.get("ref"))
                .is_some()
                && synth.and_then(|map| map.get(side)).is_none()
            {
                return true;
            }
        }
        return false;
    }
    record.used_ref.is_some() && record.synthesized_locator.is_none()
}

/// Strips internal `from_ref`/`to_ref`/`*_fragile` metadata keys from `fragile` export entries.
fn fragile_locator_payload(record: &StepRecord) -> Value {
    if record.action == "browser.drag"
        && let Some(Value::Object(map)) = &record.synthesized_locator
    {
        let mut clean = Map::new();
        for key in ["from", "to"] {
            if let Some(val) = map.get(key) {
                clean.insert(key.to_string(), val.clone());
            }
        }
        return Value::Object(clean);
    }
    record.synthesized_locator.clone().unwrap_or(Value::Null)
}

/// Replaces `ref` with the synthesized deterministic locator while preserving
/// original `${env:...}` and `${var}` placeholders, and records the viewport a
/// coordinate step ran at.
fn materialize_args(record: &StepRecord) -> Result<Value, String> {
    if record.action == "browser.drag" {
        return materialize_drag_args(record);
    }
    let mut args = match &record.synthesized_locator {
        Some(_) => synthesized_locator_args(record)?,
        None => record.raw_args.clone(),
    };
    pin_coordinate_viewport(record, &mut args)?;
    Ok(args)
}

/// Name of the recorded bounds field inside `at` (`window` for desktop, `viewport` for browser).
fn coordinate_bounds_field(action: &str) -> &'static str {
    if action.starts_with("desktop.") {
        "window"
    } else {
        "viewport"
    }
}

/// The `at.viewport` or `at.window` size a coordinate step exports, if present.
fn coordinates_bounds(action: &str, args: &Value) -> Option<(&'static str, Value)> {
    let field = coordinate_bounds_field(action);
    let bounds = args
        .get("at")
        .and_then(|at| at.get(field))
        .or_else(|| {
            args.get("from")
                .and_then(|endpoint| endpoint.get("at"))
                .and_then(|at| at.get(field))
        })
        .or_else(|| {
            args.get("to")
                .and_then(|endpoint| endpoint.get("at"))
                .and_then(|at| at.get(field))
        })?
        .clone();
    Some((field, bounds))
}

/// Merges synthesized locator keys with remaining step/endpoint arguments without
/// overwriting locator fields.
fn merge_locator_fields(
    loc_map: &Map<String, Value>,
    extra: Map<String, Value>,
) -> Map<String, Value> {
    let mut merged = Map::new();
    for (k, v) in loc_map {
        merged.insert(k.clone(), v.clone());
    }
    for (k, v) in extra {
        merged.entry(k).or_insert(v);
    }
    merged
}

/// Materializes both `from` and `to` endpoints of `browser.drag` (synthesized refs and pinned `at.viewport`).
fn materialize_drag_args(record: &StepRecord) -> Result<Value, String> {
    let Value::Object(mut base) = record.raw_args.clone() else {
        return Err(format!(
            "step {} (`{}`) arguments must be a mapping",
            record.index, record.action
        ));
    };
    let synth = record
        .synthesized_locator
        .as_ref()
        .and_then(Value::as_object);
    let observed_viewport = record
        .observed_viewport
        .as_ref()
        .or_else(|| synth.and_then(|map| map.get("viewport")));

    for side in ["from", "to"] {
        let Some(mut endpoint) = base.remove(side) else {
            return Err(format!(
                "step {} (`{}`) is missing `{side}` endpoint",
                record.index, record.action
            ));
        };
        let Value::Object(mut ep_map) = endpoint.clone() else {
            return Err(format!(
                "step {} (`{}`) endpoint `{side}` must be a mapping",
                record.index, record.action
            ));
        };
        if ep_map.remove("ref").is_some() {
            let Some(loc_map) = synth
                .and_then(|map| map.get(side))
                .and_then(Value::as_object)
            else {
                return Err(format!(
                    "step {} (`{}`) uses `{side}.ref` without a synthesized locator",
                    record.index, record.action
                ));
            };
            endpoint = Value::Object(merge_locator_fields(loc_map, ep_map));
        }
        pin_endpoint_at_viewport(record, &mut endpoint, observed_viewport)?;
        base.insert(side.to_string(), endpoint);
    }

    Ok(Value::Object(base))
}

/// Pins an unpinned `at` block inside `container` to `observed` viewport or window dimensions.
fn pin_endpoint_at_viewport(
    record: &StepRecord,
    container: &mut Value,
    observed: Option<&Value>,
) -> Result<(), String> {
    let field = coordinate_bounds_field(&record.action);
    let Some(at) = container.get_mut("at").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    if at.get(field).is_some() {
        return Ok(());
    }
    let bounds = observed.cloned().ok_or_else(|| {
        format!(
            "step {} (`{}`) uses `at` coordinates but recorded no {field} size, so the exported case could not replay at the same size",
            record.index, record.action
        )
    })?;
    at.insert(field.to_string(), bounds);
    Ok(())
}

/// Adds the recorded viewport to a bare `at`, so the exported case refuses to
/// replay at a different size instead of clicking the wrong place.
fn pin_coordinate_viewport(record: &StepRecord, args: &mut Value) -> Result<(), String> {
    pin_endpoint_at_viewport(record, args, record.observed_viewport.as_ref())
}

/// Replaces `ref` with the synthesized deterministic locator.
fn synthesized_locator_args(record: &StepRecord) -> Result<Value, String> {
    let Some(locator) = &record.synthesized_locator else {
        return Ok(record.raw_args.clone());
    };
    let Value::Object(mut base) = record.raw_args.clone() else {
        return Err(format!(
            "step {} (`{}`) arguments must be a mapping",
            record.index, record.action
        ));
    };
    let Value::Object(loc_map) = locator else {
        return Err(format!(
            "step {} (`{}`) synthesized locator must be a mapping",
            record.index, record.action
        ));
    };
    base.remove("ref");
    base.remove("at");

    if matches!(record.action.as_str(), "browser.type" | "desktop.type") {
        let typed = base.remove("value").or_else(|| base.remove("text"));
        let mut merged = Map::new();
        for (k, v) in loc_map {
            merged.insert(k.clone(), v.clone());
        }
        if let Some(typed_text) = typed {
            if loc_map.contains_key("text") {
                merged.insert("value".to_string(), typed_text);
            } else {
                merged.insert("text".to_string(), typed_text);
            }
        }
        for (k, v) in base {
            merged.entry(k).or_insert(v);
        }
        return Ok(Value::Object(merged));
    }

    Ok(Value::Object(merge_locator_fields(loc_map, base)))
}

fn format_inline_or_block(value: &Value, indent: usize) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(num) => num.to_string(),
        Value::String(text) => format_scalar_string(text),
        Value::Array(_) | Value::Object(_) => {
            let mut buf = String::from("\n");
            write_yaml_value(&mut buf, value, indent + 2);
            buf.trim_end_matches('\n').to_string()
        }
    }
}

fn write_yaml_value(out: &mut String, value: &Value, indent: usize) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str(&format!("{pad}{{}}\n"));
                return;
            }
            for (key, item) in map {
                match item {
                    Value::Object(child) if !child.is_empty() => {
                        out.push_str(&format!("{pad}{}:\n", format_key(key)));
                        write_yaml_value(out, item, indent + 2);
                    }
                    Value::Array(items) if !items.is_empty() => {
                        out.push_str(&format!("{pad}{}:\n", format_key(key)));
                        write_yaml_value(out, item, indent + 2);
                    }
                    Value::Object(_) => {
                        out.push_str(&format!("{pad}{}: {{}}\n", format_key(key)));
                    }
                    Value::Array(_) => {
                        out.push_str(&format!("{pad}{}: []\n", format_key(key)));
                    }
                    scalar => {
                        out.push_str(&format!(
                            "{pad}{}: {}\n",
                            format_key(key),
                            format_inline_or_block(scalar, indent)
                        ));
                    }
                }
            }
        }
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str(&format!("{pad}[]\n"));
                return;
            }
            for item in items {
                match item {
                    Value::Object(child) if !child.is_empty() => {
                        let mut iter = child.iter();
                        if let Some((first_k, first_v)) = iter.next() {
                            match first_v {
                                Value::Object(nested) if !nested.is_empty() => {
                                    out.push_str(&format!("{pad}- {}:\n", format_key(first_k)));
                                    write_yaml_value(out, first_v, indent + 4);
                                }
                                Value::Array(nested) if !nested.is_empty() => {
                                    out.push_str(&format!("{pad}- {}:\n", format_key(first_k)));
                                    write_yaml_value(out, first_v, indent + 4);
                                }
                                _ => {
                                    out.push_str(&format!(
                                        "{pad}- {}: {}\n",
                                        format_key(first_k),
                                        format_inline_or_block(first_v, indent + 2)
                                    ));
                                }
                            }
                            for (k, v) in iter {
                                match v {
                                    Value::Object(nested) if !nested.is_empty() => {
                                        out.push_str(&format!("{pad}  {}:\n", format_key(k)));
                                        write_yaml_value(out, v, indent + 4);
                                    }
                                    Value::Array(nested) if !nested.is_empty() => {
                                        out.push_str(&format!("{pad}  {}:\n", format_key(k)));
                                        write_yaml_value(out, v, indent + 4);
                                    }
                                    _ => {
                                        out.push_str(&format!(
                                            "{pad}  {}: {}\n",
                                            format_key(k),
                                            format_inline_or_block(v, indent + 2)
                                        ));
                                    }
                                }
                            }
                        }
                    }
                    Value::Object(_) => out.push_str(&format!("{pad}- {{}}\n")),
                    Value::Array(_) => out.push_str(&format!("{pad}- []\n")),
                    scalar => {
                        out.push_str(&format!(
                            "{pad}- {}\n",
                            format_inline_or_block(scalar, indent)
                        ));
                    }
                }
            }
        }
        scalar => {
            out.push_str(&format!(
                "{pad}{}\n",
                format_inline_or_block(scalar, indent)
            ));
        }
    }
}

fn format_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !is_yaml_reserved_or_number(key)
    {
        key.to_string()
    } else {
        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string())
    }
}

fn format_scalar_string(text: &str) -> String {
    if text.is_empty() {
        return "\"\"".to_string();
    }
    if text != text.trim() || text.chars().any(|c| c.is_control()) {
        return serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    }
    if is_yaml_reserved_or_number(text) {
        return serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    }
    let first = text.chars().next().unwrap_or(' ');
    if matches!(
        first,
        '#' | '-'
            | '?'
            | ':'
            | '['
            | ']'
            | '{'
            | '}'
            | ','
            | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | '`'
            | '$'
    ) {
        return serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    }
    if text.contains(": ")
        || text.contains(" #")
        || text.ends_with(':')
        || text.contains('\'')
        || text.contains('"')
        || text.contains('\\')
    {
        return serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    }
    text.to_string()
}

fn is_yaml_reserved_or_number(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "true"
            | "false"
            | "null"
            | "~"
            | "yes"
            | "no"
            | "on"
            | "off"
            | "y"
            | "n"
            | ".nan"
            | ".inf"
            | "+.inf"
            | "-.inf"
    ) {
        return true;
    }
    text.parse::<i64>().is_ok() || text.parse::<f64>().is_ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    fn sample_record(
        index: usize,
        action: &str,
        raw_args: Value,
        redacted_args: bool,
    ) -> StepRecord {
        StepRecord {
            index,
            action: action.to_string(),
            raw_args,
            redacted_args,
            expect: Vec::new(),
            save: BTreeMap::new(),
            optional: false,
            status: "passed".to_string(),
            error: None,
            used_ref: None,
            synthesized_locator: None,
            fragile: false,
            observed_viewport: None,
            started_at_ms: 1,
            duration_ms: 1,
            result: None,
            model: None,
            latency_ms: None,
            usage: None,
            trace_id: None,
            confidence: None,
        }
    }

    fn base_record(index: usize, action: &str, raw_args: Value) -> StepRecord {
        sample_record(index, action, raw_args, false)
    }

    #[test]
    fn export_preserves_saved_variable_placeholders_in_set_cookies() {
        let mut cli_step = sample_record(
            1,
            "cli.run",
            json!({ "command": "echo", "args": ["tok"] }),
            false,
        );
        cli_step
            .save
            .insert("tok".to_string(), "$.stdout".to_string());
        let cookie_step = sample_record(
            2,
            "browser.set_cookies",
            json!({
                "cookies": [{
                    "name": "sid",
                    "value": "${tok}",
                    "url": "http://127.0.0.1/"
                }]
            }),
            false,
        );
        let exported = export_session(
            "s1",
            &[],
            &[cli_step, cookie_step],
            &ExportOptions::default(),
        )
        .expect("export saved-var cookie");
        assert!(
            exported.yaml.contains("\"${tok}\"") || exported.yaml.contains("${tok}"),
            "expected ${{tok}} placeholder in exported YAML, got:\n{}",
            exported.yaml
        );
        assert!(
            !exported.yaml.contains("***"),
            "unexpected redacted placeholder in exported YAML:\n{}",
            exported.yaml
        );
    }

    #[test]
    fn export_refuses_inline_redacted_secret_arguments() {
        let cookie_step = sample_record(
            1,
            "browser.set_cookies",
            json!({
                "cookies": [{
                    "name": "sid",
                    "value": "***",
                    "url": "http://127.0.0.1/"
                }]
            }),
            true,
        );
        let err = export_session("s2", &[], &[cookie_step], &ExportOptions::default())
            .expect_err("inline redacted secret must fail export");
        assert!(err.contains("browser.set_cookies"), "{err}");
        assert!(
            err.contains("${env:NAME}") && err.contains("${var}"),
            "{err}"
        );
    }

    #[test]
    fn export_preserves_session_observers_and_env_placeholders() {
        let mut step = sample_record(
            1,
            "cli.run",
            json!({ "command": "echo", "args": ["ok"] }),
            false,
        );
        step.expect.push(crate::session::store::StoredExpectation {
            path: "$.observed.app_log.lines".to_string(),
            comparison: "equals".to_string(),
            expected: json!([]),
        });
        step.save.insert(
            "captured".to_string(),
            "$.observed.app_log.text".to_string(),
        );
        let observe = vec![
            StoredObservation {
                name: "app_log".to_string(),
                kind: "log.file".to_string(),
                raw_args: json!({ "path": "${env:APP_LOG_PATH}" }),
            },
            StoredObservation {
                name: "net".to_string(),
                kind: "browser.network".to_string(),
                raw_args: json!({}),
            },
        ];
        let exported = export_session("s3", &observe, &[step], &ExportOptions::default())
            .expect("export with observers");
        assert!(
            exported.yaml.contains("observe:\n  app_log:\n    log.file:\n      path: \"${env:APP_LOG_PATH}\"\n  net:\n    browser.network: {}\n"),
            "unexpected exported YAML:\n{}",
            exported.yaml
        );
    }

    #[test]
    fn export_drag_materializes_both_endpoints_pins_viewport_and_cleans_fragile() {
        let mut mixed_drag = base_record(
            2,
            "browser.drag",
            json!({
                "from": { "at": { "x": 40, "y": 60 } },
                "to": { "ref": "e9" }
            }),
        );
        mixed_drag.used_ref = Some("e9".to_string());
        mixed_drag.fragile = true;
        mixed_drag.observed_viewport = Some(json!({ "width": 1280, "height": 720 }));
        mixed_drag.synthesized_locator = Some(json!({
            "to": { "css": "#col > div:nth-of-type(2)" },
            "to_ref": "e9",
            "to_fragile": true,
            "viewport": { "width": 1280, "height": 720 }
        }));

        let mut two_ref_drag = base_record(
            3,
            "browser.drag",
            json!({
                "from": { "ref": "e1" },
                "to": { "ref": "e2" }
            }),
        );
        two_ref_drag.used_ref = Some("e1/e2".to_string());
        two_ref_drag.fragile = false;
        two_ref_drag.synthesized_locator = Some(json!({
            "from": { "testid": "card-a" },
            "from_ref": "e1",
            "from_fragile": false,
            "to": { "testid": "drop-b" },
            "to_ref": "e2",
            "to_fragile": false
        }));

        let read_clip = base_record(4, "browser.read_clipboard", json!({}));

        let records = vec![
            base_record(1, "browser.open", json!({ "url": "http://127.0.0.1/" })),
            mixed_drag,
            two_ref_drag,
            read_clip,
        ];

        let result = export_session(
            "drag-test",
            &[],
            &records,
            &ExportOptions {
                case_name: Some("Drag export".to_string()),
                include_reads: false,
            },
        )
        .expect("export must succeed");

        assert_eq!(result.exported_steps, 4);
        assert!(
            !result.yaml.contains("ref:"),
            "exported YAML must not contain raw ref:\n{}",
            result.yaml
        );
        assert!(
            !result.yaml.contains("to_ref")
                && !result.yaml.contains("from_ref")
                && !result.yaml.contains("to_fragile")
                && !result.yaml.contains("from_fragile"),
            "internal metadata helper keys must not leak into YAML:\n{}",
            result.yaml
        );
        assert!(
            result.yaml.contains("width: 1280") && result.yaml.contains("height: 720"),
            "explicit drag endpoint coordinates must pin observed viewport:\n{}",
            result.yaml
        );
        assert!(
            result.yaml.contains("testid: card-a") && result.yaml.contains("testid: drop-b"),
            "both drag refs must materialize into synthesized locators:\n{}",
            result.yaml
        );
        assert!(
            result.yaml.contains("  - browser.read_clipboard: {}\n"),
            "empty read_clipboard args must render cleanly:\n{}",
            result.yaml
        );
        assert_eq!(result.fragile.len(), 1, "{:?}", result.fragile);
        let fragile_loc = &result.fragile[0]["locator"];
        assert!(
            fragile_loc.get("from_fragile").is_none()
                && fragile_loc.get("to_fragile").is_none()
                && fragile_loc.get("from_ref").is_none()
                && fragile_loc.get("to_ref").is_none(),
            "fragile metadata must strip internal helper keys: {fragile_loc}"
        );
    }

    #[test]
    fn export_omits_optional_skipped_drag_with_any_unresolved_ref() {
        let mut partial_failed_drag = base_record(
            2,
            "browser.drag",
            json!({
                "from": { "ref": "e1" },
                "to": { "ref": "e99" }
            }),
        );
        partial_failed_drag.optional = true;
        partial_failed_drag.status = "skipped".to_string();
        partial_failed_drag.used_ref = Some("e1/e99".to_string());
        partial_failed_drag.synthesized_locator = Some(json!({
            "from": { "testid": "card-a" },
            "from_ref": "e1",
            "from_fragile": false
        }));

        let records = vec![
            base_record(1, "browser.open", json!({ "url": "http://127.0.0.1/" })),
            partial_failed_drag,
        ];

        let result = export_session("drag-optional", &[], &records, &ExportOptions::default())
            .expect("export must succeed");
        assert_eq!(result.exported_steps, 1);
        assert!(
            !result.yaml.contains("browser.drag") && !result.yaml.contains("ref:"),
            "optional skipped drag with unresolved `to.ref` must be omitted:\n{}",
            result.yaml
        );
    }
}
