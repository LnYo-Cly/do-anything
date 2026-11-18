//! Deterministic export of executed session steps into a replayable YAML case.

use serde_json::{Map, Value, json};

use crate::core::case;
use crate::session::store::StepRecord;

/// Options controlling how a session is exported to YAML.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    /// Case name written into `name:`. Defaults to a name derived from the session id.
    pub case_name: Option<String>,
    /// Include `browser.read` steps that have neither `expect` nor `save`.
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
        let exportable_skipped = record.optional
            && record.status == "skipped"
            && (record.used_ref.is_none() || record.synthesized_locator.is_some());
        if record.status != "passed" && !exportable_skipped {
            skipped_steps.push(json!({
                "index": record.index,
                "action": record.action,
                "reason": format!("step {}", record.status),
            }));
            continue;
        }
        if !options.include_reads
            && record.action == "browser.read"
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

        let args = materialize_args(record)?;
        let exported_index = exported.len() + 1;
        if record.fragile {
            fragile.push(json!({
                "step_index": record.index,
                "exported_index": exported_index,
                "action": record.action,
                "ref": record.used_ref,
                "locator": record.synthesized_locator,
            }));
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
    yaml.push_str("steps:\n");

    for (record, args) in &exported {
        if record.fragile {
            let ref_label = record.used_ref.as_deref().unwrap_or("target");
            yaml.push_str(&format!(
                "  # fragile: structural CSS path fallback for ref `{ref_label}`\n"
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

/// Replaces `ref` with the synthesized deterministic locator while preserving
/// original `${env:...}` and `${var}` placeholders.
fn materialize_args(record: &StepRecord) -> Result<Value, String> {
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

    if record.action == "browser.type" {
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

    let mut merged = Map::new();
    for (k, v) in loc_map {
        merged.insert(k.clone(), v.clone());
    }
    for (k, v) in base {
        merged.entry(k).or_insert(v);
    }
    Ok(Value::Object(merged))
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
