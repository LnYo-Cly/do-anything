//! Deterministic 7-tier locator synthesis for `ref`-targeted interactive steps.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::cdp::{Deadline, RemoteResult};
use super::{
    BrowserSession, DialogContext, ElementTarget, Locator, ResolvePoll, page_call,
    page_call_function_remote, page_call_function_with_object,
};
use crate::core::registry::StepError;

/// Result of synthesizing a deterministic locator from a live `ref` element.
#[derive(Debug, Clone)]
pub struct SynthesizedLocator {
    pub locator: Value,
    pub fragile: bool,
    pub object_id: String,
}

const NORMALIZE_INTERACTIVE_BODY: &str = r#"function () {
  var roles = {
    button: 1, link: 1, textbox: 1, searchbox: 1, checkbox: 1, radio: 1,
    switch: 1, combobox: 1, listbox: 1, option: 1, menuitem: 1,
    menuitemcheckbox: 1, menuitemradio: 1, tab: 1, slider: 1, spinbutton: 1, treeitem: 1
  };
  var isInteractive = function (node) {
    if (!node || node.nodeType !== 1) return false;
    var tag = node.tagName ? node.tagName.toLowerCase() : '';
    if (tag === 'button' || tag === 'select' || tag === 'textarea' || tag === 'summary' || tag === 'option') return true;
    if (tag === 'a' && node.hasAttribute('href')) return true;
    if (tag === 'input' && String(node.type || 'text').toLowerCase() !== 'hidden') return true;
    if (node.isContentEditable) return true;
    var r = (node.getAttribute('role') || '').trim().toLowerCase();
    return Boolean(r && roles[r]);
  };
  var cur = this;
  if (cur && cur.nodeType !== 1) {
    cur = cur.parentElement || cur.parentNode;
  }
  while (cur && cur.nodeType === 1) {
    if (isInteractive(cur)) return cur;
    cur = cur.parentElement;
  }
  return (this && this.nodeType === 1) ? this : (this && this.parentElement) || this;
}"#;

const INSPECT_NODE_TEMPLATE: &str = r#"var testidAttr = __DA_TESTID_ATTR__;
  var normText = function (s) {
    return String(s === null || s === undefined ? '' : s).replace(/\s+/g, ' ').trim();
  };
  var labelTextFor = function (node) {
    if (node.labels && node.labels.length > 0) {
      var parts = [];
      for (var i = 0; i < node.labels.length; i++) {
        var t = normText(node.labels[i].innerText || node.labels[i].textContent);
        if (t) parts.push(t);
      }
      if (parts.length > 0) return parts.join(' ');
    }
    var wrap = node.closest ? node.closest('label') : null;
    if (wrap) return normText(wrap.innerText || wrap.textContent);
    return '';
  };
  var inferredRole = function (node, tagName) {
    var explicit = (node.getAttribute('role') || '').trim().toLowerCase();
    if (explicit) return explicit;
    if (tagName === 'textarea') return 'textbox';
    if (tagName === 'select') return 'combobox';
    if (tagName === 'input') {
      var ty = (node.type || 'text').toLowerCase();
      if (ty === 'checkbox') return 'checkbox';
      if (ty === 'radio') return 'radio';
      if (ty === 'search') return 'searchbox';
      if (ty === 'number') return 'spinbutton';
      if (ty === 'range') return 'slider';
      if (ty === 'submit' || ty === 'button' || ty === 'reset') return 'button';
      return 'textbox';
    }
    return '';
  };
  var nthOfTypeSegment = function (node) {
    var tagName = node.tagName ? node.tagName.toLowerCase() : '*';
    var parent = node.parentElement;
    if (!parent) return tagName;
    var siblings = parent.children;
    var index = 0;
    var same = 0;
    for (var i = 0; i < siblings.length; i++) {
      if (siblings[i].tagName === node.tagName) {
        same++;
        if (siblings[i] === node) index = same;
      }
    }
    return same > 1 ? (tagName + ':nth-of-type(' + index + ')') : tagName;
  };
  var pathBetween = function (root, target) {
    var segs = [];
    var cur = target;
    while (cur && cur !== root && cur.nodeType === 1) {
      segs.unshift(nthOfTypeSegment(cur));
      cur = cur.parentElement;
    }
    return segs.join(' > ');
  };

  var tag = this.tagName ? this.tagName.toLowerCase() : 'div';
  var classes = this.classList ? Array.prototype.slice.call(this.classList) : [];
  var isForm = tag === 'input' || tag === 'textarea' || tag === 'select';
  var inner = isForm ? '' : normText(this.innerText);
  if (inner.length > 120) inner = '';

  var ancestors = [];
  var cur = this.parentElement;
  var depth = 0;
  while (cur && cur.nodeType === 1 && depth < 6) {
    var aTag = cur.tagName ? cur.tagName.toLowerCase() : '';
    if (aTag === 'html') break;
    ancestors.push({
      tag: aTag,
      testid: (cur.getAttribute(testidAttr) || '').trim(),
      id: (cur.id || '').trim(),
      role: (cur.getAttribute('role') || '').trim().toLowerCase(),
      aria_label: (cur.getAttribute('aria-label') || '').trim(),
      rel_path: pathBetween(cur, this)
    });
    if (aTag === 'body') break;
    cur = cur.parentElement;
    depth++;
  }

  var root = document.body || document.documentElement;
  var structural = this === root ? (root.tagName ? root.tagName.toLowerCase() : 'body') : ((root.tagName ? root.tagName.toLowerCase() : 'body') + ' > ' + pathBetween(root, this));

  return {
    tag: tag,
    testid: (this.getAttribute(testidAttr) || '').trim(),
    id: (this.id || '').trim(),
    classes: classes,
    name_attr: (this.getAttribute('name') || '').trim(),
    type_attr: (this.getAttribute('type') || '').trim(),
    aria_label: (this.getAttribute('aria-label') || '').trim(),
    placeholder: (this.getAttribute('placeholder') || '').trim(),
    label_text: labelTextFor(this),
    inferred_role: inferredRole(this, tag),
    inner_text: inner,
    ancestors: ancestors,
    structural_path: structural
  };"#;

#[derive(Debug, Deserialize)]
struct AncestorDomInfo {
    tag: String,
    testid: String,
    id: String,
    role: String,
    aria_label: String,
    rel_path: String,
}

#[derive(Debug, Deserialize)]
struct NodeDomInfo {
    tag: String,
    testid: String,
    id: String,
    classes: Vec<String>,
    name_attr: String,
    type_attr: String,
    aria_label: String,
    placeholder: String,
    label_text: String,
    inferred_role: String,
    inner_text: String,
    ancestors: Vec<AncestorDomInfo>,
    structural_path: String,
}

struct Candidate {
    locator_json: Value,
    target: ElementTarget,
    fragile: bool,
}

/// Synthesizes and live-verifies a deterministic locator for `object_id`.
pub fn synthesize_for_object(
    session: &mut BrowserSession,
    object_id: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<SynthesizedLocator, StepError> {
    let scope = session.scope();
    let norm_remote = {
        let connection = session.connection_mut()?;
        page_call_function_remote(
            connection,
            &scope,
            object_id,
            NORMALIZE_INTERACTIVE_BODY,
            deadline,
            dialog,
        )
        .map_err(|failure| failure.environment("could not inspect the target element"))?
    };
    let norm_id = match norm_remote {
        RemoteResult::Object(id) => id,
        RemoteResult::Value(_) => object_id.to_string(),
    };

    let (ax_role, ax_name) = query_ax_info(session, &norm_id, deadline, dialog)?;

    let inspect_script = INSPECT_NODE_TEMPLATE.replace(
        "__DA_TESTID_ATTR__",
        &json_string(&session.testid_attribute),
    );
    let info_val = session.call_function_on(
        "target element",
        &norm_id,
        &inspect_script,
        "inspect for locator synthesis",
        deadline,
        dialog,
    )?;
    let info: NodeDomInfo = serde_json::from_value(info_val).map_err(|error| {
        StepError::Environment(format!(
            "the browser returned an unexpected inspection payload: {error}"
        ))
    })?;

    let candidates = build_candidates(&info, &ax_role, &ax_name);
    let verify_budget = Duration::from_millis(800);

    for candidate in candidates {
        let check_deadline = deadline.capped(verify_budget);
        let Ok(ResolvePoll::Found(found_id)) =
            session.resolve_target_once(&candidate.target, check_deadline, dialog)
        else {
            continue;
        };
        let same = {
            let scope = session.scope();
            let connection = session.connection_mut()?;
            page_call_function_with_object(
                connection,
                &scope,
                &norm_id,
                &found_id,
                "function (other) { return this === other; }",
                check_deadline,
                dialog,
            )
        };
        if matches!(same, Ok(Value::Bool(true))) {
            return Ok(SynthesizedLocator {
                locator: candidate.locator_json,
                fragile: candidate.fragile,
                object_id: norm_id,
            });
        }
    }

    Ok(SynthesizedLocator {
        locator: json!({ "css": info.structural_path }),
        fragile: true,
        object_id: norm_id,
    })
}

fn query_ax_info(
    session: &mut BrowserSession,
    object_id: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<(String, String), StepError> {
    let scope = session.scope();
    let described = {
        let connection = session.connection_mut()?;
        page_call(
            connection,
            &scope,
            "DOM.describeNode",
            json!({ "objectId": object_id }),
            deadline,
            dialog,
        )
        .map_err(|failure| failure.environment("could not describe the target DOM node"))?
    };
    let Some(backend_id) = described
        .pointer("/node/backendNodeId")
        .and_then(Value::as_i64)
    else {
        return Ok((String::new(), String::new()));
    };

    let ax_reply = {
        let connection = session.connection_mut()?;
        page_call(
            connection,
            &scope,
            "Accessibility.getPartialAXTree",
            json!({ "backendNodeId": backend_id, "fetchRelatives": false }),
            deadline,
            dialog,
        )
    };
    let Ok(reply) = ax_reply else {
        return Ok((String::new(), String::new()));
    };

    let nodes = reply
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for node in &nodes {
        if node.get("ignored").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if node.get("backendDOMNodeId").and_then(Value::as_i64) == Some(backend_id) {
            let role = node
                .pointer("/role/value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            let name = node
                .pointer("/name/value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            return Ok((role, name));
        }
    }

    Ok((String::new(), String::new()))
}

fn build_candidates(info: &NodeDomInfo, ax_role: &str, ax_name: &str) -> Vec<Candidate> {
    let mut out = Vec::new();

    // Tier 1: testid
    if !info.testid.is_empty() {
        push_candidate(
            &mut out,
            json!({ "testid": info.testid }),
            Locator::TestId(info.testid.clone()),
            None,
            false,
        );
    }

    // Tier 2: role + name
    if is_semantic_role(ax_role) && !ax_name.is_empty() {
        push_candidate(
            &mut out,
            json!({ "role": ax_role, "name": ax_name }),
            Locator::Role {
                role: ax_role.to_string(),
                name: Some(ax_name.to_string()),
            },
            None,
            false,
        );
    }

    // Tier 3: form control label or placeholder expressed as role + name
    if !info.inferred_role.is_empty() {
        let mut form_names = Vec::new();
        if !info.label_text.is_empty() {
            form_names.push(info.label_text.clone());
        }
        if !info.placeholder.is_empty() && info.placeholder != info.label_text {
            form_names.push(info.placeholder.clone());
        }
        sort_digits_last(&mut form_names, |s| s);
        for candidate_name in form_names {
            if candidate_name == ax_name && info.inferred_role == ax_role {
                continue;
            }
            push_candidate(
                &mut out,
                json!({ "role": info.inferred_role, "name": candidate_name }),
                Locator::Role {
                    role: info.inferred_role.clone(),
                    name: Some(candidate_name),
                },
                None,
                false,
            );
        }
    }

    // Tier 4: exact visible text
    if !info.inner_text.is_empty() {
        push_candidate(
            &mut out,
            json!({ "text": info.inner_text }),
            Locator::Text(info.inner_text.clone()),
            None,
            false,
        );
    }

    // Tier 5: stable id
    if !info.id.is_empty() && !is_generated_token(&info.id) {
        let css = css_id_selector(&info.id);
        push_candidate(
            &mut out,
            json!({ "css": css }),
            Locator::Css(css),
            None,
            false,
        );
    }

    // Tier 6: css scoped inside a stable-identified ancestor using `within`
    for anc in &info.ancestors {
        let Some((anc_json, anc_target)) = stable_ancestor_locator(anc) else {
            continue;
        };
        let mut inner_selectors = build_inner_css_candidates(info, anc);
        sort_digits_last(&mut inner_selectors, |s| s);
        for inner_css in inner_selectors {
            out.push(Candidate {
                locator_json: json!({
                    "css": inner_css,
                    "within": anc_json,
                }),
                target: ElementTarget {
                    locator: Locator::Css(inner_css),
                    within: Some(Box::new(anc_target.clone())),
                    nth: None,
                },
                fragile: false,
            });
        }
    }

    // Tier 7: structural CSS path fallback (marked fragile)
    if !info.structural_path.is_empty() {
        push_candidate(
            &mut out,
            json!({ "css": info.structural_path }),
            Locator::Css(info.structural_path.clone()),
            None,
            true,
        );
    }

    out
}

fn push_candidate(
    out: &mut Vec<Candidate>,
    locator_json: Value,
    locator: Locator,
    within: Option<Box<ElementTarget>>,
    fragile: bool,
) {
    out.push(Candidate {
        locator_json,
        target: ElementTarget {
            locator,
            within,
            nth: None,
        },
        fragile,
    });
}

fn stable_ancestor_locator(anc: &AncestorDomInfo) -> Option<(Value, ElementTarget)> {
    if !anc.testid.is_empty() {
        return Some((
            json!({ "testid": anc.testid }),
            ElementTarget {
                locator: Locator::TestId(anc.testid.clone()),
                within: None,
                nth: None,
            },
        ));
    }
    if !anc.id.is_empty() && !is_generated_token(&anc.id) {
        let css = css_id_selector(&anc.id);
        return Some((
            json!({ "css": css }),
            ElementTarget {
                locator: Locator::Css(css),
                within: None,
                nth: None,
            },
        ));
    }
    if !anc.aria_label.is_empty() {
        let role = if !anc.role.is_empty() {
            anc.role.as_str()
        } else {
            match anc.tag.as_str() {
                "section" => "region",
                "form" => "form",
                "nav" => "navigation",
                "dialog" => "dialog",
                "main" => "main",
                "aside" => "complementary",
                _ => "",
            }
        };
        if !role.is_empty() {
            return Some((
                json!({ "role": role, "name": anc.aria_label }),
                ElementTarget {
                    locator: Locator::Role {
                        role: role.to_string(),
                        name: Some(anc.aria_label.clone()),
                    },
                    within: None,
                    nth: None,
                },
            ));
        }
    }
    None
}

fn build_inner_css_candidates(info: &NodeDomInfo, anc: &AncestorDomInfo) -> Vec<String> {
    let mut selectors = Vec::new();
    if !info.name_attr.is_empty() {
        selectors.push(format!(
            "{}[name={}]",
            info.tag,
            css_attr_value(&info.name_attr)
        ));
    }
    if !info.aria_label.is_empty() {
        selectors.push(format!(
            "{}[aria-label={}]",
            info.tag,
            css_attr_value(&info.aria_label)
        ));
    }
    if !info.type_attr.is_empty() && info.tag == "input" {
        selectors.push(format!("input[type={}]", css_attr_value(&info.type_attr)));
    }
    for cls in &info.classes {
        if !cls.is_empty() && !is_generated_token(cls) && is_simple_ident(cls) {
            selectors.push(format!("{}.{}", info.tag, cls));
        }
    }
    selectors.push(info.tag.clone());
    if !anc.rel_path.is_empty() && anc.rel_path != info.tag {
        selectors.push(anc.rel_path.clone());
    }
    selectors
}

fn is_semantic_role(role: &str) -> bool {
    !role.is_empty()
        && !matches!(
            role,
            "generic" | "none" | "RootWebArea" | "StaticText" | "InlineTextBox" | "LineBreak"
        )
}

/// Filters out machine-generated IDs and CSS-in-JS / CSS-module class hashes
/// (`:r1:`, `css-1dbjc4n`, `sc-bdVaJa`, `Button__x9K2a`, pure hex hashes, etc.).
pub(crate) fn is_generated_token(token: &str) -> bool {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return true;
    }
    // React useId: :r1:, :R0a:, etc.
    if trimmed.starts_with(':') && trimmed.ends_with(':') && trimmed.len() >= 3 {
        return true;
    }
    // Emotion / styled-components: css-<hash>
    if let Some(rest) = trimmed.strip_prefix("css-")
        && rest.len() >= 5
        && rest.chars().all(|c| c.is_ascii_alphanumeric())
        && (rest.chars().any(|c| c.is_ascii_digit()) || rest.len() >= 6)
    {
        return true;
    }
    // Styled-components: sc-<hash>
    if let Some(rest) = trimmed.strip_prefix("sc-")
        && rest.len() >= 5
        && rest.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return true;
    }
    // CSS Modules hash suffix: Foo_bar__x9K2a or foo_1a2b3c
    if let Some((_, suffix)) = trimmed.split_once("__")
        && suffix.len() >= 5
        && suffix.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return true;
    }
    // Pure hexadecimal hash >= 6 chars (e.g. a1b2c3d4)
    if trimmed.len() >= 6 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return true;
    }
    // Single opaque mixed-case+digit alphanumeric token >= 8 chars
    if trimmed.len() >= 8
        && trimmed.chars().all(|c| c.is_ascii_alphanumeric())
        && trimmed.chars().any(|c| c.is_ascii_digit())
        && trimmed.chars().any(|c| c.is_ascii_uppercase())
        && trimmed.chars().any(|c| c.is_ascii_lowercase())
    {
        return true;
    }
    false
}

fn sort_digits_last<T>(items: &mut [T], key: impl Fn(&T) -> &str) {
    items.sort_by_key(|item| key(item).chars().any(|c| c.is_ascii_digit()));
}

fn is_simple_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_' || first == '-')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn css_id_selector(id: &str) -> String {
    if is_simple_ident(id) && !id.starts_with('-') {
        format!("#{id}")
    } else {
        format!("[id={}]", css_attr_value(id))
    }
}

fn css_attr_value(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn json_string(value: &str) -> String {
    Value::String(value.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_filtered_and_stable_names_kept() {
        for bad in [
            ":r1:",
            ":R2a:",
            "css-1dbjc4n",
            "css-9f8a7b",
            "css-abc123",
            "sc-bdVaJa",
            "Btn_root__x9K2a",
            "a1b2c3d4",
        ] {
            assert!(
                is_generated_token(bad),
                "`{bad}` must be detected as generated"
            );
        }
        for good in [
            "submit-btn",
            "action-btn",
            "col-2",
            "email",
            "save_primary",
            "nav-link",
        ] {
            assert!(!is_generated_token(good), "`{good}` must be kept as stable");
        }
    }

    #[test]
    fn digit_containing_candidates_sort_after_non_digit_candidates() {
        let mut items = vec!["col-2", "action-btn", "step-1", "primary"];
        sort_digits_last(&mut items, |s| s);
        assert_eq!(items, vec!["action-btn", "primary", "col-2", "step-1"]);
    }
}
