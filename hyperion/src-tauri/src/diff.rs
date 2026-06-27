// Hyperion — semantic `.bos` snapshot diff (M8).
//
// A snapshot's `map_json` is a parsed `.bos` config: a JSON array of node
// objects, each keyed by a stable `path` (the bOS object path, e.g.
// `Server\Tasks\Lighting`). This module computes a *semantic* diff between two
// such trees — which nodes were added, which were removed, and, for nodes
// present in both, which individual fields changed (with their before/after
// values).
//
// `diff_nodes` is intentionally PURE over two `serde_json::Value` trees: it
// touches no DB, file, or network, so it is fully unit-testable with synthetic
// node JSON. The Tauri `snapshot_diff` command (in lib.rs) is the only place
// that loads the two snapshots' `map_json` and feeds them in. Read-only toward
// bOS — this only compares already-parsed configs, never writes one back.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

/// One added or removed node: its stable `path` and its `type` (if known).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NodeRef {
    pub path: String,
    #[serde(rename = "type")]
    pub node_type: Option<String>,
}

/// One field-level change on a node that exists in both snapshots. `field` is the
/// object key that differs (empty only for a non-object node compared whole);
/// `before`/`after` are the raw JSON values (with `null` when the field is absent
/// on one side, i.e. a field that was added or dropped).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldChange {
    pub path: String,
    pub field: String,
    pub before: Value,
    pub after: Value,
}

/// The structured diff between two snapshots, serializable to JSON as
/// `{ added: [...], removed: [...], changed: [...] }`. Entries are deterministically
/// ordered (nodes by path, fields by key) so the same pair always diffs identically.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct DiffResult {
    pub added: Vec<NodeRef>,
    pub removed: Vec<NodeRef>,
    pub changed: Vec<FieldChange>,
}

/// Stable key for a node: its `path`, falling back to `name` (mirrors the
/// renderer's `node_path`). Nodes with neither are unkeyable and skipped.
fn node_key(n: &Value) -> Option<String> {
    let path = n.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if !path.is_empty() {
        return Some(path.to_string());
    }
    let name = n.get("name").and_then(|v| v.as_str()).unwrap_or("");
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// A node's `type` field as an owned string, if present.
fn node_type(n: &Value) -> Option<String> {
    n.get("type").and_then(|v| v.as_str()).map(str::to_string)
}

/// Index a snapshot tree (a JSON array of node objects) by stable key. A
/// `BTreeMap` keeps the output deterministically path-ordered; a duplicate path
/// keeps the last occurrence (matching the renderer's `by_path` insert order).
fn index(tree: &Value) -> BTreeMap<String, &Value> {
    let mut map = BTreeMap::new();
    if let Some(arr) = tree.as_array() {
        for n in arr {
            if let Some(k) = node_key(n) {
                map.insert(k, n);
            }
        }
    }
    map
}

/// Append every field-level change between two versions of the same node to `out`.
/// Compares the union of object keys (sorted, for deterministic output); a key
/// missing on one side reads as `null`, so an added/dropped field is reported as
/// a change to/from `null`. A non-object node is compared as a whole value under
/// an empty `field`.
fn field_changes(path: &str, before: &Value, after: &Value, out: &mut Vec<FieldChange>) {
    let mut keys: BTreeSet<&str> = BTreeSet::new();
    if let Some(o) = before.as_object() {
        keys.extend(o.keys().map(String::as_str));
    }
    if let Some(o) = after.as_object() {
        keys.extend(o.keys().map(String::as_str));
    }
    if keys.is_empty() {
        // Neither side is a JSON object (degenerate snapshot data) — compare whole.
        if before != after {
            out.push(FieldChange {
                path: path.to_string(),
                field: String::new(),
                before: before.clone(),
                after: after.clone(),
            });
        }
        return;
    }
    for key in keys {
        // A key absent on one side resolves to `null`, so an added/dropped field
        // reads as a change to/from `null` (and a field that is `null` on one side
        // and absent on the other is correctly treated as unchanged).
        let b = before.get(key).cloned().unwrap_or(Value::Null);
        let a = after.get(key).cloned().unwrap_or(Value::Null);
        if b != a {
            out.push(FieldChange {
                path: path.to_string(),
                field: key.to_string(),
                before: b,
                after: a,
            });
        }
    }
}

/// Semantic diff between two snapshot node trees. `from` is the older snapshot,
/// `to` the newer; both are the parsed `map_json` (a JSON array of node objects).
/// Returns the added (in `to` only), removed (in `from` only), and field-level
/// changed (in both, differing) nodes. Pure: no DB, file, or network.
pub fn diff_nodes(from: &Value, to: &Value) -> DiffResult {
    let from_idx = index(from);
    let to_idx = index(to);

    let mut added = Vec::new();
    for (key, node) in &to_idx {
        if !from_idx.contains_key(key) {
            added.push(NodeRef {
                path: key.clone(),
                node_type: node_type(node),
            });
        }
    }

    let mut removed = Vec::new();
    for (key, node) in &from_idx {
        if !to_idx.contains_key(key) {
            removed.push(NodeRef {
                path: key.clone(),
                node_type: node_type(node),
            });
        }
    }

    let mut changed = Vec::new();
    for (key, before) in &from_idx {
        if let Some(after) = to_idx.get(key) {
            field_changes(key, before, after, &mut changed);
        }
    }

    DiffResult {
        added,
        removed,
        changed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A minimal snapshot: a JSON array of node objects.
    fn tree(nodes: Value) -> Value {
        nodes
    }

    #[test]
    fn detects_added_node() {
        let from = tree(json!([
            { "path": "Server\\Tasks\\A", "type": "Task" },
        ]));
        let to = tree(json!([
            { "path": "Server\\Tasks\\A", "type": "Task" },
            { "path": "Server\\Tasks\\B", "type": "Logic" },
        ]));
        let d = diff_nodes(&from, &to);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].path, "Server\\Tasks\\B");
        assert_eq!(d.added[0].node_type.as_deref(), Some("Logic"));
        assert!(d.removed.is_empty());
        assert!(d.changed.is_empty());
    }

    #[test]
    fn detects_removed_node() {
        let from = tree(json!([
            { "path": "Server\\Tasks\\A", "type": "Task" },
            { "path": "Server\\Tasks\\B", "type": "Logic" },
        ]));
        let to = tree(json!([
            { "path": "Server\\Tasks\\A", "type": "Task" },
        ]));
        let d = diff_nodes(&from, &to);
        assert!(d.added.is_empty());
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0].path, "Server\\Tasks\\B");
        assert_eq!(d.removed[0].node_type.as_deref(), Some("Logic"));
        assert!(d.changed.is_empty());
    }

    #[test]
    fn detects_field_change() {
        let from = tree(json!([
            { "path": "Server\\Tasks\\A", "type": "Task", "enabled": true },
        ]));
        let to = tree(json!([
            { "path": "Server\\Tasks\\A", "type": "Task", "enabled": false },
        ]));
        let d = diff_nodes(&from, &to);
        assert!(d.added.is_empty());
        assert!(d.removed.is_empty());
        assert_eq!(d.changed.len(), 1);
        let c = &d.changed[0];
        assert_eq!(c.path, "Server\\Tasks\\A");
        assert_eq!(c.field, "enabled");
        assert_eq!(c.before, json!(true));
        assert_eq!(c.after, json!(false));
    }

    #[test]
    fn added_and_dropped_fields_read_as_null() {
        // `note` is added (null -> "hi"); `legacy` is dropped ("old" -> null).
        let from = tree(json!([
            { "path": "N", "legacy": "old" },
        ]));
        let to = tree(json!([
            { "path": "N", "note": "hi" },
        ]));
        let d = diff_nodes(&from, &to);
        // Fields are emitted in sorted key order: legacy, note.
        assert_eq!(d.changed.len(), 2);
        assert_eq!(d.changed[0].field, "legacy");
        assert_eq!(d.changed[0].before, json!("old"));
        assert_eq!(d.changed[0].after, Value::Null);
        assert_eq!(d.changed[1].field, "note");
        assert_eq!(d.changed[1].before, Value::Null);
        assert_eq!(d.changed[1].after, json!("hi"));
    }

    #[test]
    fn nested_structural_field_change() {
        // A changed input wiring is a single field change on `inputs`.
        let from = tree(json!([
            { "path": "Logic\\Sum", "type": "Logic",
              "inputs": [{ "object": "Sensor\\Temp", "property": "Value" }] },
        ]));
        let to = tree(json!([
            { "path": "Logic\\Sum", "type": "Logic",
              "inputs": [{ "object": "Sensor\\Humidity", "property": "Value" }] },
        ]));
        let d = diff_nodes(&from, &to);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].field, "inputs");
        assert_eq!(
            d.changed[0].before,
            json!([{ "object": "Sensor\\Temp", "property": "Value" }])
        );
        assert_eq!(
            d.changed[0].after,
            json!([{ "object": "Sensor\\Humidity", "property": "Value" }])
        );
    }

    #[test]
    fn identical_trees_have_empty_diff() {
        let t = tree(json!([
            { "path": "A", "type": "Task" },
            { "path": "B", "type": "Logic" },
        ]));
        let d = diff_nodes(&t, &t);
        assert_eq!(d, DiffResult::default());
    }

    #[test]
    fn falls_back_to_name_when_no_path() {
        // A node keyed by `name` (no `path`) is matched across snapshots.
        let from = tree(json!([{ "name": "Orphan", "type": "X" }]));
        let to = tree(json!([{ "name": "Orphan", "type": "Y" }]));
        let d = diff_nodes(&from, &to);
        assert!(d.added.is_empty() && d.removed.is_empty());
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].field, "type");
    }

    #[test]
    fn combined_add_remove_change_serializes() {
        let from = tree(json!([
            { "path": "Keep", "type": "Task", "v": 1 },
            { "path": "Gone", "type": "Old" },
        ]));
        let to = tree(json!([
            { "path": "Keep", "type": "Task", "v": 2 },
            { "path": "New", "type": "Fresh" },
        ]));
        let d = diff_nodes(&from, &to);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].path, "New");
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0].path, "Gone");
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].path, "Keep");
        assert_eq!(d.changed[0].field, "v");

        // Serializes to the documented JSON shape with `type` (not `node_type`).
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["added"][0]["path"], "New");
        assert_eq!(v["added"][0]["type"], "Fresh");
        assert_eq!(v["changed"][0]["field"], "v");
        assert_eq!(v["changed"][0]["before"], json!(1));
        assert_eq!(v["changed"][0]["after"], json!(2));
    }

    #[test]
    fn empty_and_nonarray_inputs_are_safe() {
        // Non-array snapshot values index to nothing — no panic, empty diff.
        let d = diff_nodes(&json!({}), &json!(null));
        assert_eq!(d, DiffResult::default());
        // Empty arrays likewise.
        let d = diff_nodes(&json!([]), &json!([]));
        assert_eq!(d, DiffResult::default());
    }
}
