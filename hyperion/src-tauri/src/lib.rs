// Hyperion — bOS Configurator desktop core.
//
// Phase 0: this Rust layer reimplements the four read-only Flask endpoints from
// bos_copilot.py as Tauri commands, computed from the parsed `bos_map.json`
// (produced by bos_explore.py). All UI/grading logic lives in the webview
// (src/main.ts); this layer only loads data and exposes:
//   app_state, get_tree, get_node, list_playbooks, get_playbook, parse_bos
//
// Strictly read-only with respect to the bOS system — never writes to it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::State;

/// In-memory model of one loaded project workspace.
struct Store {
    workspace: PathBuf,
    config_name: String,
    nodes: Vec<Value>,
    by_path: HashMap<String, Value>,
    norm_index: HashMap<String, String>, // norm(path) -> canonical path
    ref_index: HashMap<String, Vec<Value>>, // target path -> [{by, property, kind}]
    children_index: HashMap<String, Vec<Value>>, // parent path -> [{name, path, type}]
    tree: Value,
}

fn norm(s: &str) -> String {
    s.replace('\\', "/").to_lowercase()
}

/// Mirror of Python `str(path or name)`.
fn node_path(n: &Value) -> String {
    let p = n.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if !p.is_empty() {
        return p.to_string();
    }
    n.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn name_lower(v: &Value) -> String {
    v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_lowercase()
}

// ---- nested tree (port of bos_copilot.build_tree) ----
#[derive(Default)]
struct TNode {
    name: String,
    path: String,
    node_type: Option<String>,
    children: BTreeMap<String, TNode>,
}

fn to_list(n: &TNode) -> Value {
    let mut kids: Vec<Value> = n.children.values().map(to_list).collect();
    kids.sort_by(|a, b| name_lower(a).cmp(&name_lower(b)));
    json!({
        "name": n.name,
        "path": n.path,
        "type": n.node_type,
        "children": kids,
    })
}

fn build_tree(nodes: &[Value]) -> Value {
    let mut root = TNode {
        name: "(root)".into(),
        path: String::new(),
        node_type: None,
        children: BTreeMap::new(),
    };
    for n in nodes {
        let path = node_path(n);
        let ntype = n.get("type").and_then(|v| v.as_str()).map(|s| s.to_string());
        let parts: Vec<&str> = path.split('\\').filter(|p| !p.is_empty()).collect();
        let mut cur = &mut root;
        let mut acc: Vec<&str> = Vec::new();
        for part in parts {
            acc.push(part);
            let full = acc.join("\\");
            cur = cur.children.entry(part.to_string()).or_insert_with(|| TNode {
                name: part.to_string(),
                path: full,
                node_type: None,
                children: BTreeMap::new(),
            });
        }
        cur.node_type = ntype;
    }
    to_list(&root)
}

fn addref(idx: &mut HashMap<String, Vec<Value>>, target: Option<&str>, by: &str, prop: &Value, kind: &str) {
    if let Some(t) = target {
        if !t.is_empty() {
            idx.entry(t.to_string()).or_default().push(json!({
                "by": by, "property": prop.clone(), "kind": kind,
            }));
        }
    }
}

fn find_first_bos(dir: &PathBuf) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.to_lowercase().ends_with(".bos"))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Build the whole store from `<workspace>/bos_map.json`.
fn build_store(workspace: PathBuf) -> Store {
    let map_path = workspace.join("bos_map.json");
    let nodes: Vec<Value> = std::fs::read_to_string(&map_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let mut by_path: HashMap<String, Value> = HashMap::new();
    let mut norm_index: HashMap<String, String> = HashMap::new();
    let mut ref_index: HashMap<String, Vec<Value>> = HashMap::new();
    let mut children_index: HashMap<String, Vec<Value>> = HashMap::new();

    for n in &nodes {
        let p = node_path(n);
        by_path.insert(p.clone(), n.clone());
        norm_index.insert(norm(&p), p.clone());

        if let Some(arr) = n.get("inputs").and_then(|v| v.as_array()) {
            for r in arr {
                addref(&mut ref_index, r.get("object").and_then(|v| v.as_str()), &p,
                       r.get("property").unwrap_or(&Value::Null), "reads");
            }
        }
        if let Some(o) = n.get("output") {
            if !o.is_null() {
                addref(&mut ref_index, o.get("object").and_then(|v| v.as_str()), &p,
                       o.get("property").unwrap_or(&Value::Null), "outputs to");
            }
        }
        if let Some(arr) = n.get("writes").and_then(|v| v.as_array()) {
            for r in arr {
                addref(&mut ref_index, r.get("object").and_then(|v| v.as_str()), &p,
                       r.get("property").unwrap_or(&Value::Null), "writes");
            }
        }
        if let Some((parent, leaf)) = p.rsplit_once('\\') {
            children_index.entry(parent.to_string()).or_default().push(json!({
                "name": leaf,
                "path": p,
                "type": n.get("type").cloned().unwrap_or(Value::Null),
            }));
        }
    }

    let tree = build_tree(&nodes);
    let config_name = find_first_bos(&workspace).unwrap_or_else(|| "(no .bos)".into());

    Store { workspace, config_name, nodes, by_path, norm_index, ref_index, children_index, tree }
}

fn default_workspace() -> PathBuf {
    if let Ok(w) = std::env::var("HYPERION_WORKSPACE") {
        return PathBuf::from(w);
    }
    // dev default: hyperion/src-tauri -> hyperion -> bos-copilot (where bos_map.json lives)
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

// ----------------------------- Tauri commands -----------------------------

#[tauri::command]
fn app_state(state: State<'_, Mutex<Store>>) -> Value {
    let s = state.lock().unwrap();
    json!({ "config": s.config_name, "count": s.nodes.len(),
            "workspace": s.workspace.to_string_lossy() })
}

#[tauri::command]
fn get_tree(state: State<'_, Mutex<Store>>) -> Value {
    state.lock().unwrap().tree.clone()
}

#[tauri::command]
fn get_node(path: String, state: State<'_, Mutex<Store>>) -> Result<Value, String> {
    let s = state.lock().unwrap();
    let key_bs = path.replace('/', "\\");
    let n = s
        .by_path
        .get(&key_bs)
        .or_else(|| s.norm_index.get(&norm(&path)).and_then(|p| s.by_path.get(p)));
    let n = match n {
        Some(n) => n,
        None => return Err(format!("node not found: {path}")),
    };

    let key = node_path(n);
    let mut out = n.clone();

    // dedupe writes by (object, property), preserving order
    if let Some(arr) = n.get("writes").and_then(|v| v.as_array()) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut uniq: Vec<Value> = Vec::new();
        for r in arr {
            let sig = format!(
                "{}|{}",
                r.get("object").map(|v| v.to_string()).unwrap_or_default(),
                r.get("property").map(|v| v.to_string()).unwrap_or_default()
            );
            if seen.insert(sig) {
                uniq.push(r.clone());
            }
        }
        out["writes"] = Value::Array(uniq);
    }

    let mut consists = s.children_index.get(&key).cloned().unwrap_or_default();
    consists.sort_by(|a, b| name_lower(a).cmp(&name_lower(b)));
    out["consists_of"] = Value::Array(consists);
    out["referenced_by"] = Value::Array(s.ref_index.get(&key).cloned().unwrap_or_default());
    Ok(out)
}

#[tauri::command]
fn list_playbooks(state: State<'_, Mutex<Store>>) -> Vec<Value> {
    let dir = state.lock().unwrap().workspace.join("playbooks");
    let mut out: Vec<Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|f| f.ends_with(".json") && !f.starts_with('_'))
            .collect();
        files.sort();
        for f in files {
            match std::fs::read_to_string(dir.join(&f)).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
                Some(pb) => {
                    let feature = pb.get("feature").and_then(|v| v.as_str()).unwrap_or(&f).to_string();
                    let steps = pb.get("steps").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                    out.push(json!({ "file": f, "feature": feature, "steps": steps }));
                }
                None => out.push(json!({ "file": f, "feature": "(invalid json)", "steps": 0 })),
            }
        }
    }
    out
}

#[tauri::command]
fn get_playbook(name: String, state: State<'_, Mutex<Store>>) -> Result<Value, String> {
    if name.contains('/') || name.contains('\\') || !name.ends_with(".json") {
        return Err("invalid playbook name".into());
    }
    let path = state.lock().unwrap().workspace.join("playbooks").join(&name);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{e}"))
}

/// Re-run the Python parser (bos_explore.py) on a .bos file, refreshing
/// `<workspace>/bos_map.json`, then rebuild the in-memory store.
#[tauri::command]
fn parse_bos(path: String, state: State<'_, Mutex<Store>>) -> Result<Value, String> {
    let workspace = state.lock().unwrap().workspace.clone();
    let script = workspace.join("bos_explore.py");
    let out_json = workspace.join("bos_map.json");
    let status = std::process::Command::new("python")
        .arg(&script)
        .arg(&path)
        .arg("--json")
        .arg(&out_json)
        .current_dir(&workspace)
        .status()
        .map_err(|e| format!("failed to launch python: {e}"))?;
    if !status.success() {
        return Err(format!("bos_explore.py exited with {status}"));
    }
    let mut s = state.lock().unwrap();
    *s = build_store(workspace);
    if let Some(fname) = PathBuf::from(&path).file_name() {
        s.config_name = fname.to_string_lossy().to_string();
    }
    Ok(json!({ "config": s.config_name, "count": s.nodes.len() }))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let store = build_store(default_workspace());
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Mutex::new(store))
        .invoke_handler(tauri::generate_handler![
            app_state,
            get_tree,
            get_node,
            list_playbooks,
            get_playbook,
            parse_bos
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
