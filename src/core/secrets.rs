//! The values read from the environment during a run, and their redaction.
//!
//! A value is a secret because of where it came from — a `${env:NAME}`
//! reference — not because the case file declared it one. The set is filled
//! while the cases are validated, which happens before the first case runs, so
//! every output written after that point can be redacted with the whole set.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// What replaces a secret in output and in a failure bundle.
pub const MARKER: &str = "***";

/// The secret values of one run.
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    values: BTreeSet<String>,
}

impl Secrets {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one resolved environment value.
    ///
    /// An empty value is not a secret: replacing every empty string would
    /// rewrite the whole output.
    ///
    /// A value that contains a backslash is recorded a second time with every
    /// backslash replaced by a forward slash. [`crate::core::paths::display`]
    /// rewrites separators that way in every path it shows, so a message may
    /// hold a transformed copy of the value; redaction replaces exact
    /// substrings, so the transformed copy has to be a secret as well.
    pub fn record(&mut self, value: &str) {
        if value.is_empty() {
            return;
        }
        if value.contains('\\') {
            self.values.insert(value.replace('\\', "/"));
        }
        self.values.insert(value.to_string());
    }

    /// Whether nothing has been read from the environment yet.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Replaces every occurrence of every secret with [`MARKER`].
    pub fn redact(&self, text: &str) -> String {
        if self.values.is_empty() {
            return text.to_string();
        }
        // Longest first, so a secret that contains another one is replaced as a
        // whole instead of leaving a fragment behind.
        let mut ordered: Vec<&str> = self.values.iter().map(String::as_str).collect();
        ordered.sort_by_key(|value| std::cmp::Reverse(value.len()));
        let mut redacted = text.to_string();
        for value in ordered {
            redacted = redact_value(&redacted, value);
        }
        redacted
    }

    /// Replaces every secret inside a JSON value.
    ///
    /// It runs on the tree rather than on the serialized text: a secret that
    /// contains `"` or `\` is escaped in the text, so a textual replacement
    /// would miss it.
    ///
    /// A string keeps its own text and is redacted in place. A number, a
    /// boolean or null has no inner text to replace, so its JSON text is
    /// redacted and the result becomes a string: `10` with the secret `1`
    /// becomes `"***0"`, and `true` with the secret `true` becomes `"***"`.
    pub fn redact_json(&self, value: &Value) -> Value {
        if self.values.is_empty() {
            return value.clone();
        }
        match value {
            Value::String(text) => Value::String(self.redact(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.redact_json(item)).collect())
            }
            Value::Object(entries) => {
                let mut redacted = Map::with_capacity(entries.len());
                for (key, item) in entries {
                    redacted.insert(self.redact(key), self.redact_json(item));
                }
                Value::Object(redacted)
            }
            Value::Number(number) => self.redact_scalar(&number.to_string(), value),
            Value::Bool(flag) => self.redact_scalar(&flag.to_string(), value),
            Value::Null => self.redact_scalar("null", value),
        }
    }

    /// Redacts the JSON text of a scalar that cannot hold text of its own.
    ///
    /// A token that contains no secret keeps its type; one that does becomes a
    /// string, because a partly replaced number or boolean is not a valid token
    /// of its original type.
    fn redact_scalar(&self, text: &str, original: &Value) -> Value {
        let redacted = self.redact(text);
        if redacted == text {
            original.clone()
        } else {
            Value::String(redacted)
        }
    }
}

/// Encoded forms are only searched for secrets this long. A short secret would
/// match too much of the output when its bytes are matched one by one.
const ENCODED_MIN_BYTES: usize = 8;

/// A half-open byte range of the text that is replaced as one marker.
type Span = (usize, usize);

const BASE64_STANDARD: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_URL_SAFE: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Redacts one secret, including its percent-encoded and base64 forms.
fn redact_value(text: &str, value: &str) -> String {
    if value.len() < ENCODED_MIN_BYTES {
        return text.replace(value, MARKER);
    }

    let mut encoded = percent_spans(text, value.as_bytes());
    encoded.extend(base64_spans(text, value.as_bytes()));
    if encoded.is_empty() {
        // The encoded forms must not change the behavior of the exact match.
        return text.replace(value, MARKER);
    }

    let mut spans: Vec<Span> = text
        .match_indices(value)
        .map(|(start, matched)| (start, start + matched.len()))
        .collect();
    spans.extend(encoded);
    replace_spans(text, &merge_spans(spans))
}

/// Replaces every span with one marker.
fn replace_spans(text: &str, spans: &[Span]) -> String {
    let mut redacted = String::with_capacity(text.len() + spans.len() * MARKER.len());
    let mut cursor = 0;
    for &(start, end) in spans {
        redacted.push_str(&text[cursor..start]);
        redacted.push_str(MARKER);
        cursor = end;
    }
    redacted.push_str(&text[cursor..]);
    redacted
}

/// Sorts spans and joins the ones that overlap. Adjacent spans stay separate,
/// so two different secrets next to each other still produce two markers.
fn merge_spans(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_unstable();
    let mut merged: Vec<Span> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        if start >= end {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && start < last.1
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }
    merged
}

/// The spans of `secret` written with a percent sign before some of its bytes.
///
/// Each byte may appear as itself, as `%XX` with either case, or, for a space,
/// as `+`. The matcher follows every interpretation instead of assuming which
/// bytes a particular encoder escapes.
fn percent_spans(text: &str, secret: &[u8]) -> Vec<Span> {
    let bytes = text.as_bytes();
    if !bytes.iter().any(|byte| *byte == b'%' || *byte == b'+') {
        return Vec::new();
    }

    let mut spans = Vec::new();
    let mut start = 0;
    while start < bytes.len() {
        if let Some(end) = percent_end(bytes, secret, start)
            && has_encoded_token(&bytes[start..end])
        {
            spans.push((start, end));
            start = end;
            continue;
        }
        start += 1;
    }
    spans
}

/// The furthest end reachable from `start`, or `None`.
fn percent_end(text: &[u8], secret: &[u8], start: usize) -> Option<usize> {
    let mut positions = vec![start];
    let mut next = Vec::new();
    for &byte in secret {
        next.clear();
        for &position in &positions {
            if position >= text.len() {
                continue;
            }
            if text[position] == b'%' {
                let high = text.get(position + 1).and_then(|byte| hex_value(*byte));
                let low = text.get(position + 2).and_then(|byte| hex_value(*byte));
                if let (Some(high), Some(low)) = (high, low)
                    && (high << 4 | low) == byte
                {
                    next.push(position + 3);
                }
            }
            if byte == b' ' && text[position] == b'+' {
                next.push(position + 1);
            }
            if text[position] == byte {
                next.push(position + 1);
            }
        }
        if next.is_empty() {
            return None;
        }
        next.sort_unstable();
        next.dedup();
        std::mem::swap(&mut positions, &mut next);
    }
    positions.into_iter().max()
}

/// Whether a span contains at least one byte written as an escape.
fn has_encoded_token(bytes: &[u8]) -> bool {
    bytes.contains(&b'+')
        || bytes.windows(3).any(|window| {
            window[0] == b'%' && hex_value(window[1]).is_some() && hex_value(window[2]).is_some()
        })
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The spans of `secret` encoded with base64.
///
/// A secret can start at any of the three byte positions of a four-character
/// group. For each position the function computes the characters that depend
/// only on the secret, searches for them in both alphabets, and includes the
/// one adjacent character on each side that also carries secret bits.
fn base64_spans(text: &str, secret: &[u8]) -> Vec<Span> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    for offset in 0..3 {
        let values = base64_fixed_values(secret, offset);
        if values.is_empty() {
            continue;
        }
        let include_before = !(offset * 8).is_multiple_of(6);
        let include_after = !(offset * 8 + secret.len() * 8).is_multiple_of(6);
        let standard = base64_needle(&values, BASE64_STANDARD);
        let url_safe = base64_needle(&values, BASE64_URL_SAFE);
        // The alphabets differ only at values 62 and 63. Most secrets never
        // reach them, and searching the same text twice would only cost time.
        let mut needles = vec![standard];
        if url_safe != needles[0] {
            needles.push(url_safe);
        }
        for needle in needles {
            let mut from = 0;
            while from < text.len() {
                let Some(relative) = text[from..].find(&needle) else {
                    break;
                };
                let start = from + relative;
                let end = start + needle.len();
                let mut span_start = start;
                if include_before && start > 0 && is_base64(bytes[start - 1]) {
                    span_start = start - 1;
                }
                let mut span_end = end;
                if include_after && span_end < bytes.len() && is_base64(bytes[span_end]) {
                    span_end += 1;
                }
                while span_end < bytes.len() && bytes[span_end] == b'=' {
                    span_end += 1;
                }
                spans.push((span_start, span_end));
                from = start + 1;
            }
        }
    }
    merge_spans(spans)
}

/// Writes fixed six-bit values with one base64 alphabet.
fn base64_needle(values: &[u8], alphabet: &[u8; 64]) -> String {
    values
        .iter()
        .map(|value| alphabet[*value as usize] as char)
        .collect()
}

/// The six-bit values of the characters fully determined by `secret`.
fn base64_fixed_values(secret: &[u8], offset: usize) -> Vec<u8> {
    let start_bit = offset * 8;
    let end_bit = start_bit + secret.len() * 8;
    let first = start_bit.div_ceil(6);
    let last = end_bit / 6;
    (first..last)
        .map(|character| {
            let mut value = 0;
            for bit in 0..6 {
                let absolute = character * 6 + bit;
                let relative = absolute - start_bit;
                let byte = secret[relative / 8];
                value = (value << 1) | ((byte >> (7 - relative % 8)) & 1);
            }
            value
        })
        .collect()
}

fn is_base64(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'-' | b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_set_leaves_text_and_json_alone() {
        let secrets = Secrets::new();

        assert!(secrets.is_empty());
        assert_eq!(secrets.redact("nothing to hide"), "nothing to hide");
        assert_eq!(
            secrets.redact_json(&json!({"a": ["b", 1]})),
            json!({"a": ["b", 1]})
        );
    }

    #[test]
    fn every_occurrence_of_every_secret_is_replaced() {
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t");
        secrets.record("token-1");

        assert_eq!(
            secrets.redact("a s3cr3t and token-1 and s3cr3t again"),
            "a *** and *** and *** again"
        );
    }

    #[test]
    fn an_empty_value_is_not_recorded() {
        let mut secrets = Secrets::new();
        secrets.record("");

        assert!(secrets.is_empty());
        assert_eq!(secrets.redact("unchanged"), "unchanged");
    }

    #[test]
    fn a_longer_secret_wins_over_a_shorter_one_it_contains() {
        let mut secrets = Secrets::new();
        secrets.record("abc");
        secrets.record("abcdef");

        assert_eq!(secrets.redact("abcdef"), "***");
    }

    #[test]
    fn a_value_with_backslashes_is_redacted_in_its_forward_slash_form_too() {
        let mut secrets = Secrets::new();
        secrets.record(r"C:\dir\s3cr3t\app.log");

        assert_eq!(secrets.redact(r"C:\dir\s3cr3t\app.log"), "***");
        assert_eq!(
            secrets.redact("C:/dir/s3cr3t/app.log"),
            "***",
            "a path display rewrites the separators of the value, so the rewritten form is a secret"
        );
        assert_eq!(
            secrets.redact("C:/dir/other/app.log"),
            "C:/dir/other/app.log",
            "a value that differs from the secret is untouched"
        );
    }

    #[test]
    fn json_strings_keys_and_numbers_are_redacted() {
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t");
        secrets.record("4242");
        let value = json!({
            "s3cr3t": "a s3cr3t value",
            "token": "s3cr3t",
            "count": 4242,
            "list": ["s3cr3t", 1, true, null],
        });

        assert_eq!(
            secrets.redact_json(&value),
            json!({
                "***": "a *** value",
                "token": "***",
                "count": "***",
                "list": ["***", 1, true, null],
            })
        );
    }

    #[test]
    fn a_secret_that_needs_json_escaping_is_still_replaced() {
        let mut secrets = Secrets::new();
        secrets.record("a\"b\\c");
        let value = json!({"echo": "a\"b\\c"});

        let redacted = secrets.redact_json(&value);
        assert_eq!(redacted, json!({"echo": "***"}));
        assert!(
            !serde_json::to_string(&redacted)
                .expect("the redacted value must serialize")
                .contains("a\\\"b")
        );
    }

    #[test]
    fn a_secret_inside_a_json_number_replaces_its_text() {
        let mut secrets = Secrets::new();
        secrets.record("1");
        secrets.record("4242");

        // A JSON scalar has no text to replace in place, so it becomes the
        // redacted text: `10` with the secret `1` is `"***0"`, not `"***"`.
        // `2` contains neither secret and stays a number.
        assert_eq!(
            secrets.redact_json(&json!({
                "ten": 10,
                "float": 1.5,
                "negative": -21,
                "embedded": 42424242,
                "two": 2,
            })),
            json!({
                "ten": "***0",
                "float": "***.5",
                "negative": "-2***",
                "embedded": "******",
                "two": 2,
            })
        );
    }

    #[test]
    fn a_secret_that_is_a_json_boolean_or_null_replaces_its_text() {
        let mut secrets = Secrets::new();
        secrets.record("true");
        secrets.record("null");

        assert_eq!(
            secrets.redact_json(&json!({"yes": true, "no": false, "nothing": null})),
            json!({"yes": "***", "no": false, "nothing": "***"})
        );
    }

    #[test]
    fn a_secret_is_redacted_when_every_byte_is_percent_encoded() {
        let mut secrets = Secrets::new();
        secrets.record("abcdefgh");

        assert_eq!(secrets.redact("%61%62%63%64%65%66%67%68"), "***");
    }

    #[test]
    fn a_partially_percent_encoded_secret_is_redacted_as_one_marker() {
        let mut secrets = Secrets::new();
        secrets.record("a+b/c=123");

        assert_eq!(secrets.redact("a%2Bb%2Fc%3D123"), "***");
        assert_eq!(secrets.redact("a%2bb%2fc%3d123"), "***");
        assert_eq!(secrets.redact("a+b/c=123"), "***");
    }

    #[test]
    fn lowercase_hexadecimal_and_plus_spaces_are_percent_encoded() {
        let mut secrets = Secrets::new();
        secrets.record("token value: 42");

        assert_eq!(secrets.redact("token%20value%3A%2042"), "***");
        assert_eq!(secrets.redact("token+value:+42"), "***");
    }

    #[test]
    fn a_secret_that_contains_a_percent_sign_is_matched_byte_by_byte() {
        let mut secrets = Secrets::new();
        secrets.record("100%25valid-key");

        assert_eq!(secrets.redact("100%2525valid-key"), "***");
        assert_eq!(secrets.redact("100%25valid-key"), "***");
    }

    #[test]
    fn percent_encoding_applies_only_to_secrets_of_at_least_eight_bytes() {
        let mut secrets = Secrets::new();
        secrets.record("short-7");

        assert_eq!(
            secrets.redact("%73%68%6F%72%74%2D%37"),
            "%73%68%6F%72%74%2D%37"
        );
        assert_eq!(secrets.redact("c2hvcnQtNw=="), "c2hvcnQtNw==");
        assert_eq!(secrets.redact("short-7"), "***");
    }

    #[test]
    fn a_non_ascii_secret_is_redacted_in_percent_and_base64_forms() {
        let mut secrets = Secrets::new();
        secrets.record("münchen-密钥");

        assert_eq!(
            secrets.redact("%6D%C3%BC%6E%63%68%65%6E%2D%E5%AF%86%E9%92%A5"),
            "***"
        );
        assert_eq!(secrets.redact("bcO8bmNoZW4t5a+G6ZKl"), "***");
        assert_eq!(secrets.redact("bcO8bmNoZW4t5a-G6ZKl"), "***");
    }

    #[test]
    fn a_secret_is_redacted_in_all_three_base64_offsets() {
        let mut secrets = Secrets::new();
        secrets.record("abcdefgh");

        // The encoded secret alone, and the same bytes after one or two other
        // bytes. The one or two leading characters before the marker encode
        // only those other bytes.
        assert_eq!(secrets.redact("YWJjZGVmZ2g="), "***");
        assert_eq!(secrets.redact("WGFiY2RlZmdo"), "W***");
        assert_eq!(secrets.redact("WFlhYmNkZWZnaA=="), "WF***");
    }

    #[test]
    fn both_base64_alphabets_are_redacted_with_and_without_padding() {
        let mut secrets = Secrets::new();
        secrets.record("münchen-密钥!");

        for encoded in [
            "bcO8bmNoZW4t5a+G6ZKlIQ==",
            "bcO8bmNoZW4t5a-G6ZKlIQ==",
            "bcO8bmNoZW4t5a-G6ZKlIQ",
        ] {
            assert_eq!(
                secrets.redact(encoded),
                "***",
                "{encoded:?} must be replaced"
            );
        }

        let mut unpadded = Secrets::new();
        unpadded.record("abcdefgh");
        assert_eq!(unpadded.redact("YWJjZGVmZ2g="), "***");
        assert_eq!(unpadded.redact("YWJjZGVmZ2g"), "***");
    }

    #[test]
    fn base64_adjacent_characters_are_replaced_with_the_match() {
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-DA-LEAK-4242");

        assert_eq!(
            secrets.redact("Basic YWxpY2U6czNjcjN0LURBLUxFQUstNDI0Mg=="),
            "Basic YWxpY2U6***"
        );
        assert_eq!(secrets.redact("WHMzY3IzdC1EQS1MRUFLLTQyNDI="), "W***");
        assert_eq!(secrets.redact("WFlzM2NyM3QtREEtTEVBSy00MjQy"), "WF***");
    }

    #[test]
    fn a_secret_that_appears_literally_and_encoded_produces_only_markers() {
        let mut secrets = Secrets::new();
        secrets.record("s3cr3t-DA-LEAK-4242");

        assert_eq!(
            secrets.redact(
                "literal s3cr3t-DA-LEAK-4242 and encoded %73%33%63%72%33%74%2D%44%41%2D%4C%45%41%4B%2D%34%32%34%32"
            ),
            "literal *** and encoded ***"
        );
        assert_eq!(
            secrets.redact("s3cr3t-DA-LEAK-4242"),
            "***",
            "the literal form also satisfies the zero-encoded-byte form and must not be replaced twice"
        );
    }
}
