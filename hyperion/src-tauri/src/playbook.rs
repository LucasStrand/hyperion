// Hyperion — playbook schema normalization (guided-walkthrough extension).
//
// Playbooks are authored as plain JSON (see `playbooks/_SCHEMA.json`) and loaded
// verbatim as a `serde_json::Value`; all grading/rendering lives in the webview.
// This module adds ONE additive, backward-compatible concern: the optional
// per-step `ui` guided-walkthrough action that tells the operator which real
// ComfortClick app to act in, where, what to do, and what to look for.
//
// It is purely INSTRUCTIONAL — Hyperion never drives the apps. Normalization is
// non-destructive: a playbook with no `ui` keys round-trips unchanged, so every
// pre-existing playbook keeps parsing and rendering exactly as before.

use serde_json::{Map, Value};

/// The three real ComfortClick bOS apps a guided step can target.
pub const APPS: [&str; 3] = ["Configurator", "Service", "Client"];

/// Canonicalize an app name (case-insensitive) to its display form, or `None`
/// if it is not one of the three real bOS apps.
pub fn canon_app(s: &str) -> Option<&'static str> {
    let t = s.trim();
    APPS.into_iter().find(|app| app.eq_ignore_ascii_case(t))
}

/// Trim a JSON string field, returning `None` for missing/non-string/empty.
fn trimmed(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Validate + normalize a single `ui` guided-walkthrough action. A valid entry
/// MUST name a recognized `app` and a non-empty `action` (what to click/set);
/// `location` and `verify` are optional. Returns `None` for anything invalid so
/// the renderer never sees a malformed action.
fn normalize_ui_entry(e: &Value) -> Option<Value> {
    let obj = e.as_object()?;
    let app = canon_app(obj.get("app").and_then(Value::as_str).unwrap_or(""))?;
    let action = trimmed(obj.get("action"))?;
    let mut out = Map::new();
    out.insert("app".into(), Value::String(app.to_string()));
    out.insert("action".into(), Value::String(action));
    if let Some(loc) = trimmed(obj.get("location")) {
        out.insert("location".into(), Value::String(loc));
    }
    if let Some(ver) = trimmed(obj.get("verify")) {
        out.insert("verify".into(), Value::String(ver));
    }
    Some(Value::Object(out))
}

/// Normalize a step's `ui` value (a single object OR an array of them) into a
/// clean array of validated guided-walkthrough actions. Invalid entries are
/// dropped; if nothing valid remains, the `ui` key is removed entirely.
fn normalize_step_ui(step: &mut Map<String, Value>) {
    let Some(ui) = step.remove("ui") else { return };
    let entries = match ui {
        Value::Array(a) => a,
        obj @ Value::Object(_) => vec![obj],
        // A `ui` of the wrong shape (string/number/bool/null) is simply dropped.
        _ => return,
    };
    let normed: Vec<Value> = entries.iter().filter_map(normalize_ui_entry).collect();
    if !normed.is_empty() {
        step.insert("ui".into(), Value::Array(normed));
    }
}

/// Normalize an entire playbook value in place. Non-objects and playbooks
/// without a `steps` array are returned untouched. Only the optional per-step
/// `ui` field is rewritten; every other field (feature, summary, action, target,
/// detail, check, diff, highlight, legacy verify, …) is left exactly as authored.
pub fn normalize(mut pb: Value) -> Value {
    if let Some(steps) = pb.get_mut("steps").and_then(Value::as_array_mut) {
        for step in steps.iter_mut() {
            if let Some(obj) = step.as_object_mut() {
                normalize_step_ui(obj);
            }
        }
    }
    pb
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canon_app_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(canon_app("configurator"), Some("Configurator"));
        assert_eq!(canon_app("  SERVICE "), Some("Service"));
        assert_eq!(canon_app("Client"), Some("Client"));
        assert_eq!(canon_app("notepad"), None);
        assert_eq!(canon_app(""), None);
        // Every canonical form round-trips through APPS.
        for a in APPS {
            assert_eq!(canon_app(a), Some(a));
        }
    }

    #[test]
    fn playbook_without_ui_is_unchanged() {
        let pb = json!({
            "feature": "X",
            "steps": [
                {"n": 1, "title": "t", "action": "navigate", "target": "A\\B", "verify": "legacy"}
            ]
        });
        assert_eq!(normalize(pb.clone()), pb);
    }

    #[test]
    fn non_object_and_stepless_playbooks_pass_through() {
        assert_eq!(normalize(json!("hi")), json!("hi"));
        assert_eq!(normalize(json!({"feature": "X"})), json!({"feature": "X"}));
    }

    #[test]
    fn single_ui_object_is_normalized_to_array_with_canonical_app() {
        let pb = json!({
            "feature": "X",
            "steps": [{
                "n": 1, "title": "t",
                "ui": {"app": "configurator", "location": "Tasks > Foo",
                       "action": "Add > Program", "verify": "It appears"}
            }]
        });
        let out = normalize(pb);
        let ui = &out["steps"][0]["ui"];
        assert!(ui.is_array());
        assert_eq!(ui[0]["app"], json!("Configurator"));
        assert_eq!(ui[0]["location"], json!("Tasks > Foo"));
        assert_eq!(ui[0]["action"], json!("Add > Program"));
        assert_eq!(ui[0]["verify"], json!("It appears"));
    }

    #[test]
    fn array_ui_keeps_valid_drops_invalid_entries() {
        let pb = json!({
            "feature": "X",
            "steps": [{
                "n": 1, "title": "t",
                "ui": [
                    {"app": "Service", "action": "Restart the service"},
                    {"app": "Excel", "action": "open"},          // bad app -> dropped
                    {"app": "Client", "action": "   "},          // empty action -> dropped
                    {"app": "Client", "action": "Tap the tile", "location": "Home"}
                ]
            }]
        });
        let out = normalize(pb);
        let ui = out["steps"][0]["ui"].as_array().unwrap();
        assert_eq!(ui.len(), 2);
        assert_eq!(ui[0]["app"], json!("Service"));
        // Optional fields absent when not provided.
        assert!(ui[0].get("location").is_none());
        assert!(ui[0].get("verify").is_none());
        assert_eq!(ui[1]["app"], json!("Client"));
        assert_eq!(ui[1]["location"], json!("Home"));
    }

    #[test]
    fn ui_with_no_valid_entries_is_removed() {
        let pb = json!({
            "feature": "X",
            "steps": [{"n": 1, "title": "t", "ui": {"app": "nope", "action": "x"}}]
        });
        let out = normalize(pb);
        assert!(out["steps"][0].get("ui").is_none());
    }

    #[test]
    fn malformed_ui_shape_is_dropped_without_panic() {
        let pb = json!({
            "feature": "X",
            "steps": [{"n": 1, "title": "t", "ui": "just a string"}]
        });
        let out = normalize(pb);
        assert!(out["steps"][0].get("ui").is_none());
    }
}
