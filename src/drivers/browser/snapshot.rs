//! Accessibility-tree snapshots and stable `ref` tracking for `da session`.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::cdp::{Connection, Deadline};
use super::{
    DialogContext, PageScope, page_call, page_call_function, page_evaluate, step_error_text,
};
use crate::core::registry::StepError;

/// Default maximum tree depth returned by a snapshot.
pub const DEFAULT_MAX_DEPTH: usize = 12;

/// Default maximum node count returned by a snapshot before truncation.
pub const DEFAULT_MAX_NODES: usize = 150;

/// One element reference recorded in the latest snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefEntry {
    pub backend_node_id: i64,
    pub loader_id: String,
}

/// Element references from the most recent snapshot of a browser session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RefStore {
    #[serde(default)]
    pub refs: BTreeMap<String, RefEntry>,
}

/// Returns the current main frame's `loaderId` to detect page navigations.
pub fn current_loader_id(
    connection: &mut Connection,
    scope: &PageScope,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<String, StepError> {
    let reply = page_call(
        connection,
        scope,
        "Page.getFrameTree",
        json!({}),
        deadline,
        dialog,
    )
    .map_err(|failure| failure.environment("could not inspect the current frame"))?;
    Ok(reply
        .pointer("/frameTree/frame/loaderId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

/// Resolves a snapshot `ref` to a live DOM `objectId` and `backendDOMNodeId`,
/// failing immediately if the ref is unknown, from a prior document, or detached.
pub fn resolve_ref_object_id(
    connection: &mut Connection,
    scope: &PageScope,
    ref_store: &RefStore,
    ref_id: &str,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<(String, i64), StepError> {
    let entry = ref_store.refs.get(ref_id).ok_or_else(|| {
        StepError::Execution(format!(
            "ref `{ref_id}` is not in the latest snapshot; run `da session snapshot` to get current element refs"
        ))
    })?;

    let loader_id = current_loader_id(connection, scope, deadline, dialog)?;
    if loader_id != entry.loader_id {
        return Err(StepError::Execution(format!(
            "ref `{ref_id}` is stale because the page changed; run `da session snapshot` to get current element refs"
        )));
    }

    let resolved = page_call(
        connection,
        scope,
        "DOM.resolveNode",
        json!({ "backendNodeId": entry.backend_node_id }),
        deadline,
        dialog,
    )
    .map_err(|failure| match failure.error() {
        super::cdp::CdpError::Method { .. } => StepError::Execution(format!(
            "ref `{ref_id}` is stale because the element is no longer on the page; run `da session snapshot` to get current element refs"
        )),
        _ => failure.environment(&format!("could not resolve ref `{ref_id}`")),
    })?;

    let object_id = resolved
        .pointer("/object/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            StepError::Execution(format!(
                "ref `{ref_id}` is stale because the element is no longer on the page; run `da session snapshot` to get current element refs"
            ))
        })?
        .to_string();

    let connected = page_call_function(
        connection,
        scope,
        &object_id,
        "function () { return Boolean(this && this.isConnected); }",
        deadline,
        dialog,
    )
    .map_err(|failure| failure.environment(&format!("could not verify ref `{ref_id}`")))?;

    if connected != Value::Bool(true) {
        return Err(StepError::Execution(format!(
            "ref `{ref_id}` is stale because the element was removed from the page; run `da session snapshot` to get current element refs"
        )));
    }

    Ok((object_id, entry.backend_node_id))
}

/// Captures a compact accessibility-tree snapshot and refreshes `RefStore`.
#[allow(
    clippy::too_many_arguments,
    reason = "explicit snapshot options avoid wrapping parameters in a single-use struct"
)]
pub fn capture_snapshot(
    connection: &mut Connection,
    scope: &PageScope,
    existing_refs: &RefStore,
    scope_ref: Option<&str>,
    max_depth: Option<usize>,
    max_nodes: Option<usize>,
    deadline: Deadline,
    dialog: &mut DialogContext,
) -> Result<(Value, RefStore), String> {
    let max_depth = max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    let max_nodes = max_nodes.unwrap_or(DEFAULT_MAX_NODES);

    let target_backend_id = match scope_ref {
        Some(ref_id) => {
            let (_obj, backend_id) =
                resolve_ref_object_id(connection, scope, existing_refs, ref_id, deadline, dialog)
                    .map_err(step_error_text)?;
            Some(backend_id)
        }
        None => None,
    };

    let url = page_evaluate(connection, scope, "location.href", deadline, dialog)
        .map_err(|failure| failure.describe("could not read the current URL"))?
        .as_str()
        .unwrap_or_default()
        .to_string();
    let title = page_evaluate(connection, scope, "document.title", deadline, dialog)
        .map_err(|failure| failure.describe("could not read the document title"))?
        .as_str()
        .unwrap_or_default()
        .to_string();
    let loader_id =
        current_loader_id(connection, scope, deadline, dialog).map_err(step_error_text)?;

    let ax_reply = page_call(
        connection,
        scope,
        "Accessibility.getFullAXTree",
        json!({}),
        deadline,
        dialog,
    )
    .map_err(|failure| failure.describe("could not read the accessibility tree"))?;

    let raw_nodes = ax_reply
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "the browser did not return accessibility nodes".to_string())?;

    let mut by_id: HashMap<&str, &Value> = HashMap::with_capacity(raw_nodes.len());
    for node in raw_nodes {
        if let Some(id) = node.get("nodeId").and_then(Value::as_str) {
            by_id.insert(id, node);
        }
    }

    let root_id = match target_backend_id {
        Some(backend_id) => raw_nodes
            .iter()
            .find(|node| {
                node.get("backendDOMNodeId").and_then(Value::as_i64) == Some(backend_id)
                    && node.get("ignored").and_then(Value::as_bool) != Some(true)
            })
            .or_else(|| {
                raw_nodes
                    .iter()
                    .find(|node| node.get("backendDOMNodeId").and_then(Value::as_i64) == Some(backend_id))
            })
            .and_then(|node| node.get("nodeId").and_then(Value::as_str))
            .ok_or_else(|| {
                format!(
                    "ref `{}` is no longer in the accessibility tree; run `da session snapshot` without `--ref`",
                    scope_ref.unwrap_or("")
                )
            })?,
        None => raw_nodes
            .iter()
            .find(|node| node.get("parentId").is_none())
            .or_else(|| raw_nodes.first())
            .and_then(|node| node.get("nodeId").and_then(Value::as_str))
            .ok_or_else(|| "the accessibility tree is empty".to_string())?,
    };

    let mut builder = TreeBuilder {
        by_id: &by_id,
        loader_id: &loader_id,
        max_depth,
        max_nodes,
        unwrap_root_web_area: scope_ref.is_none(),
        emitted: 0,
        next_ref: 1,
        truncated: false,
        refs: RefStore::default(),
    };

    let tree = builder.collect_node(root_id, 0, "", "");

    let snapshot = json!({
        "url": url,
        "title": title,
        "truncated": builder.truncated,
        "node_count": builder.emitted,
        "max_depth": max_depth,
        "max_nodes": max_nodes,
        "tree": tree,
    });

    Ok((snapshot, builder.refs))
}

struct TreeBuilder<'a> {
    by_id: &'a HashMap<&'a str, &'a Value>,
    loader_id: &'a str,
    max_depth: usize,
    max_nodes: usize,
    unwrap_root_web_area: bool,
    emitted: usize,
    next_ref: usize,
    truncated: bool,
    refs: RefStore,
}

impl<'a> TreeBuilder<'a> {
    fn collect_node(
        &mut self,
        node_id: &str,
        depth: usize,
        parent_role: &str,
        parent_name: &str,
    ) -> Vec<Value> {
        let Some(&node) = self.by_id.get(node_id) else {
            return Vec::new();
        };

        let ignored = node.get("ignored").and_then(Value::as_bool) == Some(true);
        let role = node
            .pointer("/role/value")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if matches!(role, "InlineTextBox" | "LineBreak") {
            return Vec::new();
        }

        let name = node
            .pointer("/name/value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();

        let child_ids: Vec<&str> = node
            .get("childIds")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        if ignored {
            return self.collect_children(&child_ids, depth, parent_role, parent_name);
        }

        if role == "StaticText"
            && !parent_name.is_empty()
            && name == parent_name
            && matches!(
                parent_role,
                "button"
                    | "link"
                    | "heading"
                    | "menuitem"
                    | "menuitemcheckbox"
                    | "menuitemradio"
                    | "tab"
                    | "checkbox"
                    | "radio"
                    | "option"
                    | "switch"
                    | "label"
            )
        {
            return Vec::new();
        }

        if self.unwrap_root_web_area && role == "RootWebArea" {
            return self.collect_children(&child_ids, depth, role, name);
        }

        let value_prop = node.pointer("/value/value").cloned();
        let states = extract_states(node);

        if matches!(role, "generic" | "none")
            && name.is_empty()
            && value_prop.is_none()
            && states.is_empty()
        {
            return self.collect_children(&child_ids, depth, parent_role, parent_name);
        }

        if self.emitted >= self.max_nodes || depth > self.max_depth {
            self.truncated = true;
            return Vec::new();
        }

        self.emitted += 1;
        let mut obj = Map::new();

        if let Some(backend_id) = node.get("backendDOMNodeId").and_then(Value::as_i64) {
            let ref_id = format!("e{}", self.next_ref);
            self.next_ref += 1;
            self.refs.refs.insert(
                ref_id.clone(),
                RefEntry {
                    backend_node_id: backend_id,
                    loader_id: self.loader_id.to_string(),
                },
            );
            obj.insert("ref".to_string(), Value::String(ref_id));
        }

        obj.insert("role".to_string(), Value::String(role.to_string()));
        if !name.is_empty() {
            obj.insert("name".to_string(), Value::String(name.to_string()));
        }
        if let Some(val) = value_prop {
            obj.insert("value".to_string(), val);
        }
        for (k, v) in states {
            obj.insert(k, v);
        }

        let children = self.collect_children(&child_ids, depth + 1, role, name);
        if !children.is_empty() {
            obj.insert("children".to_string(), Value::Array(children));
        }

        vec![Value::Object(obj)]
    }

    fn collect_children(
        &mut self,
        child_ids: &[&str],
        depth: usize,
        parent_role: &str,
        parent_name: &str,
    ) -> Vec<Value> {
        let mut out = Vec::new();
        for child_id in child_ids {
            if self.emitted >= self.max_nodes {
                self.truncated = true;
                break;
            }
            out.extend(self.collect_node(child_id, depth, parent_role, parent_name));
        }
        out
    }
}

fn extract_states(node: &Value) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let Some(props) = node.get("properties").and_then(Value::as_array) else {
        return out;
    };
    for prop in props {
        let Some(prop_name) = prop.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(val) = prop.pointer("/value/value") else {
            continue;
        };
        match prop_name {
            "disabled" | "selected" if val == &Value::Bool(true) => {
                out.push((prop_name.to_string(), Value::Bool(true)));
            }
            "expanded" if val.is_boolean() => {
                out.push((prop_name.to_string(), val.clone()));
            }
            "checked" => {
                out.push((prop_name.to_string(), val.clone()));
            }
            _ => {}
        }
    }
    out
}
