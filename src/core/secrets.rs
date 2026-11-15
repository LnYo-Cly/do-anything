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
    ///
    /// Encoded matches are discovered against the original text for every
    /// secret before anything is replaced: one marker must never destroy
    /// another secret's needle. A literal match that shares bytes with an
    /// encoded one is replaced together with it, while literal matches that
    /// touch no encoded match still resolve longest-first as they always did.
    pub fn redact(&self, text: &str) -> String {
        if self.values.is_empty() {
            return text.to_string();
        }
        let ordered = self.ordered_values();
        let mut encoded = Vec::new();
        for value in &ordered {
            if value.len() >= ENCODED_MIN_BYTES {
                encoded.extend(percent_spans(text, value.as_bytes()));
                encoded.extend(base64_spans(text, value.as_bytes()));
            }
        }
        if encoded.is_empty() {
            return redact_exact(text, &ordered);
        }
        let covered = cover_encoded_matches(text, &ordered, encoded);
        redact_around(text, &ordered, &covered)
    }

    /// The recorded values, longest first, so a value that contains another
    /// one is replaced as a whole instead of leaving a fragment behind.
    fn ordered_values(&self) -> Vec<&str> {
        let mut ordered: Vec<&str> = self.values.iter().map(String::as_str).collect();
        ordered.sort_by_key(|value| std::cmp::Reverse(value.len()));
        ordered
    }

    /// Replaces every secret inside a JSON value.
    ///
    /// It runs on the tree rather than on the serialized text: a secret that
    /// contains `"` or `\` is escaped in the text, so a textual replacement
    /// would miss it.
    ///
    /// A string keeps its own text and is redacted in place. A number, a
    /// boolean or null has no inner text to replace, so its JSON text is
    /// redacted with the exact form only and the result becomes a string: `10`
    /// with the secret `1` becomes `"***0"`, and `true` with the secret `true`
    /// becomes `"***"`. Encoded forms apply to strings and keys, not to these
    /// scalars.
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
    /// of its original type. Only exact occurrences count here: the encoded
    /// forms are a string-and-key behavior, and a number that happens to spell
    /// a base64 form must keep its value and type.
    fn redact_scalar(&self, text: &str, original: &Value) -> Value {
        let redacted = redact_exact(text, &self.ordered_values());
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

/// Replaces the literal occurrences only, longest first.
///
/// This is the whole behavior before encoded forms existed, including how it
/// resolves two literal matches that overlap only partly: the longer value is
/// replaced and the other one is left where the marker did not reach.
fn redact_exact(text: &str, ordered: &[&str]) -> String {
    let mut redacted = text.to_string();
    for value in ordered {
        redacted = redacted.replace(value, MARKER);
    }
    redacted
}

/// Extends the encoded spans over every literal match they touch.
///
/// Literal matches that overlap an encoded match have to become one marker:
/// replacing either one first would leave the other's bytes split around the
/// marker. Components made only of literal matches are left out, so their
/// longest-first overlap behavior is untouched and the exact pass keeps it.
fn cover_encoded_matches(text: &str, ordered: &[&str], encoded: Vec<Span>) -> Vec<Span> {
    let mut spans: Vec<(usize, usize, bool)> = encoded
        .into_iter()
        .map(|(start, end)| (start, end, true))
        .collect();
    for value in ordered {
        spans.extend(
            text.match_indices(value)
                .map(|(start, matched)| (start, start + matched.len(), false)),
        );
    }
    spans.sort_unstable();

    let mut covered = Vec::new();
    let mut component: Option<(usize, usize, bool)> = None;
    for (start, end, is_encoded) in spans {
        match component {
            Some((component_start, component_end, has_encoded)) if start < component_end => {
                component = Some((
                    component_start.min(start),
                    component_end.max(end),
                    has_encoded || is_encoded,
                ));
            }
            Some((component_start, component_end, true)) => {
                covered.push((component_start, component_end));
                component = Some((start, end, is_encoded));
            }
            Some(_) => component = Some((start, end, is_encoded)),
            None => component = Some((start, end, is_encoded)),
        }
    }
    if let Some((start, end, true)) = component {
        covered.push((start, end));
    }
    covered
}

/// Replaces the covered intervals with one marker each and redacts the gaps
/// with exact matching only.
fn redact_around(text: &str, ordered: &[&str], covered: &[Span]) -> String {
    let mut redacted = String::with_capacity(text.len() + covered.len() * MARKER.len());
    let mut cursor = 0;
    for &(start, end) in covered {
        redacted.push_str(&redact_exact(&text[cursor..start], ordered));
        redacted.push_str(MARKER);
        cursor = end;
    }
    redacted.push_str(&redact_exact(&text[cursor..], ordered));
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
    // A span that is worth replacing contains at least one escape. Only `%` can
    // escape an arbitrary byte, and `+` escapes a space, so text without either
    // one has nothing to find; an all-literal span is left to the exact match.
    let has_percent = bytes.contains(&b'%');
    let has_plus_escape = secret.contains(&b' ') && bytes.contains(&b'+');
    if !has_percent && !has_plus_escape {
        return Vec::new();
    }

    // A match can only start on the first byte written as itself, on `%` for an
    // escaped first byte, or on `+` for a space. The position scan keeps this
    // linear in the text instead of allocating a candidate list at every byte.
    let first = secret[0];
    let is_space = first == b' ';
    let mut positions = Vec::new();
    let mut next = Vec::new();
    let mut spans = Vec::new();
    let mut start = 0;
    while let Some(candidate) = next_candidate(bytes, start, first, is_space) {
        if pure_literal_mismatch(bytes, secret, candidate) {
            start = next_boundary(text, candidate);
            continue;
        }
        let end = percent_end(bytes, secret, candidate, &mut positions, &mut next).filter(|end| {
            text.is_char_boundary(*end) && has_encoded_token(&bytes[candidate..*end])
        });
        if let Some(end) = end {
            spans.push((candidate, end));
            start = end;
        } else {
            start = next_boundary(text, candidate);
        }
    }
    spans
}

const BYTE_ONES: u64 = 0x0101_0101_0101_0101;
const BYTE_HIGHS: u64 = 0x8080_8080_8080_8080;

/// Sets the high bit of every byte of `word` that equals `target`.
fn bytes_equal(word: u64, target: u8) -> u64 {
    let difference = word ^ (u64::from(target) * BYTE_ONES);
    difference.wrapping_sub(BYTE_ONES) & !difference & BYTE_HIGHS
}

/// The next byte that can start a percent-encoded match.
///
/// The eight-byte steps matter: a scan that visits every byte of a one-mebibyte
/// log line costs more than the rest of redaction together.
fn next_candidate(bytes: &[u8], from: usize, first: u8, is_space: bool) -> Option<usize> {
    let mut index = from;
    while index + 8 <= bytes.len() {
        let Ok(chunk) = bytes[index..index + 8].try_into() else {
            break;
        };
        let word = u64::from_le_bytes(chunk);
        let mut matches = bytes_equal(word, first) | bytes_equal(word, b'%');
        if is_space {
            matches |= bytes_equal(word, b'+');
        }
        if matches != 0 {
            return Some(index + matches.trailing_zeros() as usize / 8);
        }
        index += 8;
    }
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == first || byte == b'%' || (is_space && byte == b'+') {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Whether a start cannot produce a span with an escape.
///
/// While the text and the secret agree and no escape character appears, the
/// only interpretation is the literal one. A mismatch then rules every
/// interpretation out, and a complete literal run contains no escape, so the
/// span would be left to the exact match anyway. The function stops at the
/// first escape character and leaves that start to the full matcher.
fn pure_literal_mismatch(text: &[u8], secret: &[u8], start: usize) -> bool {
    for (index, &byte) in secret.iter().enumerate() {
        let Some(&text_byte) = text.get(start + index) else {
            return true;
        };
        if text_byte == b'%' || (byte == b' ' && text_byte == b'+') {
            return false;
        }
        if text_byte != byte {
            return true;
        }
    }
    true
}

/// The first character boundary after `position`.
fn next_boundary(text: &str, position: usize) -> usize {
    let mut next = position + 1;
    while next < text.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    next
}

/// The furthest end reachable from `start`, or `None`.
///
/// An ambiguous byte can double the positions that still match, so the set has
/// no fixed size. The two vectors belong to the caller and are reused for every
/// candidate of one secret, which keeps the common single-position case free of
/// allocation after the first call.
fn percent_end(
    text: &[u8],
    secret: &[u8],
    start: usize,
    positions: &mut Vec<usize>,
    next: &mut Vec<usize>,
) -> Option<usize> {
    positions.clear();
    positions.push(start);
    for &byte in secret {
        next.clear();
        for &position in positions.iter() {
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
        std::mem::swap(positions, next);
    }
    positions.iter().copied().max()
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
    fn a_secret_that_starts_with_a_non_ascii_character_is_matched_at_boundaries() {
        let mut secrets = Secrets::new();
        secrets.record("密钥-token-42");
        let encoded: String = "密钥-token-42"
            .bytes()
            .map(|byte| format!("%{byte:02x}"))
            .collect();

        assert_eq!(secrets.redact(&format!("x{encoded}y")), "x***y");

        // A leading byte of another character equals the first byte of the
        // secret; the failed attempt must not split that character.
        assert_eq!(secrets.redact("密X密钥-token-42"), "密X***");
        assert_eq!(secrets.redact("密X密钥-token-42!"), "密X***!");
    }

    #[test]
    fn a_percent_encoded_match_crosses_an_eight_byte_boundary() {
        let mut secrets = Secrets::new();
        secrets.record("abcdefgh");

        assert_eq!(secrets.redact("xxxxxxx%61bcdefgh"), "xxxxxxx***");
        assert_eq!(
            secrets.redact("xxxxxxx%61%62%63%64%65%66%67%68"),
            "xxxxxxx***"
        );
    }

    /// Every ambiguous step doubles the positions that may still complete. The
    /// only branch that finishes this text is the fourth one, at offset 15;
    /// a matcher that keeps a fixed number of positions loses it.
    #[test]
    fn four_ambiguous_positions_still_complete_the_match() {
        let mut secrets = Secrets::new();
        let secret = "%2%252%352%253";
        secrets.record(secret);
        let text = "%25%32%252%352%25%335%32%25%25%25%2525%252%25%3253";

        let spans = percent_spans(text, secret.as_bytes());
        assert!(
            spans.iter().any(|&(start, end)| start == 0 && end == 20),
            "the branch that completes ends at 20; spans: {spans:?}"
        );

        let redacted = secrets.redact(text);
        assert_eq!(redacted, format!("***{}", &text[20..]));
        assert!(
            !redacted.contains(&text[..20]),
            "the complete encoding of the secret must not survive: {redacted}"
        );
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

    #[test]
    fn a_literal_match_does_not_hide_another_secret_base64_form() {
        let mut secrets = Secrets::new();
        secrets.record("YWJjZGVmZ2");
        secrets.record("abcdefgh");

        assert_eq!(secrets.redact("YWJjZGVmZ2g="), "***");
    }

    #[test]
    fn a_literal_match_does_not_hide_another_secret_percent_form() {
        let mut secrets = Secrets::new();
        secrets.record("prefix%61bc");
        secrets.record("abcdefgh");

        assert_eq!(secrets.redact("prefix%61bcdefgh"), "***");
    }

    #[test]
    fn overlapping_base64_matches_leave_no_encoded_tail() {
        let mut secrets = Secrets::new();
        secrets.record("abcdefghij");
        secrets.record("fghijklm");

        // Base64 of `abcdefghijklm`; both values occur inside the same encoding.
        assert_eq!(secrets.redact("YWJjZGVmZ2hpamtsbQ=="), "***");
    }

    #[test]
    fn overlapping_percent_matches_leave_no_encoded_tail() {
        let mut secrets = Secrets::new();
        secrets.record("abcdefghij");
        secrets.record("fghijklm");
        let encoded: String = "abcdefghijklm"
            .bytes()
            .map(|byte| format!("%{byte:02x}"))
            .collect();

        assert_eq!(secrets.redact(&encoded), "***");
    }

    #[test]
    fn partly_overlapping_literal_values_keep_their_longest_first_result() {
        let mut six = Secrets::new();
        six.record("abcdef");
        six.record("cdefgh");
        assert_eq!(six.redact("abcdefgh"), "***gh");

        let mut ten = Secrets::new();
        ten.record("abcdefghij");
        ten.record("efghijklmn");
        assert_eq!(ten.redact("abcdefghijklmn"), "***klmn");
    }

    #[test]
    fn a_separate_encoded_match_does_not_change_literal_resolution() {
        let mut secrets = Secrets::new();
        secrets.record("abcdefghij");
        secrets.record("efghijklmn");
        secrets.record("01234567");

        assert_eq!(
            secrets.redact("abcdefghijklmn | MDEyMzQ1Njc="),
            "***klmn | ***"
        );
    }

    #[test]
    fn an_encoded_only_match_does_not_change_a_json_number() {
        let mut secrets = Secrets::new();
        // U+3F7D three times is nine bytes; its base64 is 472947294729.
        secrets.record("\u{3f7d}\u{3f7d}\u{3f7d}");

        assert_eq!(
            secrets.redact_json(&json!({"amount": 472947294729_u64, "text": "472947294729"})),
            json!({"amount": 472947294729_u64, "text": "***"})
        );
    }

    #[test]
    fn exact_scalar_redaction_still_converts_the_number() {
        let mut secrets = Secrets::new();
        secrets.record("1234");

        assert_eq!(secrets.redact_json(&json!(12345678)), json!("***5678"));
    }

    fn base64(data: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
        let mut out = String::new();
        for chunk in data.chunks(3) {
            let bytes = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let bits =
                (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
            let characters = chunk.len() + 1;
            for index in 0..4 {
                if index < characters {
                    out.push(alphabet[((bits >> (18 - 6 * index)) & 63) as usize] as char);
                } else if pad {
                    out.push('=');
                }
            }
        }
        out
    }

    /// The strongest regression guard for the bit arithmetic: for every shift
    /// of the secret inside a larger base64 document, every character that
    /// carries secret bits is replaced and every character that does not is
    /// left alone.
    #[test]
    fn no_base64_character_with_secret_bits_survives_at_any_offset() {
        let secret = "s3cr3t-中~/+";
        let mut secrets = Secrets::new();
        secrets.record(secret);
        let filler = b"Zq7!x.\x01\xffAB";
        let mut checked = 0;
        for prefix_len in 0..7 {
            for suffix_len in 0..7 {
                for (alphabet, pad) in [
                    (BASE64_STANDARD, true),
                    (BASE64_STANDARD, false),
                    (BASE64_URL_SAFE, true),
                    (BASE64_URL_SAFE, false),
                ] {
                    let mut data = filler[..prefix_len].to_vec();
                    data.extend_from_slice(secret.as_bytes());
                    data.extend_from_slice(&filler[..suffix_len]);
                    let encoded = base64(&data, alphabet, pad);
                    let first = prefix_len * 8 / 6;
                    let last = ((prefix_len + secret.len()) * 8 - 1) / 6;
                    let characters: Vec<char> = encoded.chars().collect();
                    let lead: String = characters[..first].iter().collect();
                    let mut tail: String = characters[last + 1..].iter().collect();
                    if tail.chars().all(|character| character == '=') {
                        tail.clear();
                    }
                    let text = format!("before {encoded} after");
                    let expected = format!("before {lead}***{tail} after");
                    assert_eq!(
                        secrets.redact(&text),
                        expected,
                        "prefix {prefix_len}, suffix {suffix_len}, pad {pad}, input {text}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 7 * 7 * 4);
    }
}
