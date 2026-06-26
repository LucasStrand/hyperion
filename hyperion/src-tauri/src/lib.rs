// Hyperion — bOS Configurator desktop core.
//
// Phase 0: this Rust layer reimplements the four read-only Flask endpoints from
// bos_copilot.py as Tauri commands, computed from the parsed `bos_map.json`
// (produced by bos_explore.py). All UI/grading logic lives in the webview
// (src/main.ts); this layer only loads data and exposes:
//   app_state, get_tree, get_node, list_playbooks, get_playbook, parse_bos
//
// Strictly read-only with respect to the bOS system — never writes to it.

mod projects;
mod vault;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::State;

use projects::Projects;
use vault::Vault;

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
    n.get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn name_lower(v: &Value) -> String {
    v.get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_lowercase()
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
    kids.sort_by_key(name_lower);
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
        let ntype = n
            .get("type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let parts: Vec<&str> = path.split('\\').filter(|p| !p.is_empty()).collect();
        let mut cur = &mut root;
        let mut acc: Vec<&str> = Vec::new();
        for part in parts {
            acc.push(part);
            let full = acc.join("\\");
            cur = cur
                .children
                .entry(part.to_string())
                .or_insert_with(|| TNode {
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

fn addref(
    idx: &mut HashMap<String, Vec<Value>>,
    target: Option<&str>,
    by: &str,
    prop: &Value,
    kind: &str,
) {
    if let Some(t) = target {
        if !t.is_empty() {
            idx.entry(t.to_string()).or_default().push(json!({
                "by": by, "property": prop.clone(), "kind": kind,
            }));
        }
    }
}

fn find_first_bos(dir: &Path) -> Option<String> {
    let rd = std::fs::read_dir(dir).ok()?;
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.to_lowercase().ends_with(".bos"))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// Build the whole store from `<workspace>/bos_map.json` (dev/no-project mode).
fn build_store(workspace: PathBuf) -> Store {
    let map_path = workspace.join("bos_map.json");
    let nodes: Vec<Value> = std::fs::read_to_string(&map_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let config_name = find_first_bos(&workspace).unwrap_or_else(|| "(no .bos)".into());
    build_store_from_nodes(workspace, config_name, nodes)
}

/// Build the in-memory render store directly from a parsed node array. Used by
/// `build_store` (file-backed) and by the project store (snapshot-backed).
fn build_store_from_nodes(workspace: PathBuf, config_name: String, nodes: Vec<Value>) -> Store {
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
                addref(
                    &mut ref_index,
                    r.get("object").and_then(|v| v.as_str()),
                    &p,
                    r.get("property").unwrap_or(&Value::Null),
                    "reads",
                );
            }
        }
        if let Some(o) = n.get("output") {
            if !o.is_null() {
                addref(
                    &mut ref_index,
                    o.get("object").and_then(|v| v.as_str()),
                    &p,
                    o.get("property").unwrap_or(&Value::Null),
                    "outputs to",
                );
            }
        }
        if let Some(arr) = n.get("writes").and_then(|v| v.as_array()) {
            for r in arr {
                addref(
                    &mut ref_index,
                    r.get("object").and_then(|v| v.as_str()),
                    &p,
                    r.get("property").unwrap_or(&Value::Null),
                    "writes",
                );
            }
        }
        if let Some((parent, leaf)) = p.rsplit_once('\\') {
            children_index
                .entry(parent.to_string())
                .or_default()
                .push(json!({
                    "name": leaf,
                    "path": p,
                    "type": n.get("type").cloned().unwrap_or(Value::Null),
                }));
        }
    }

    let tree = build_tree(&nodes);

    Store {
        workspace,
        config_name,
        nodes,
        by_path,
        norm_index,
        ref_index,
        children_index,
        tree,
    }
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
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    json!({ "config": s.config_name, "count": s.nodes.len(),
            "workspace": s.workspace.to_string_lossy() })
}

#[tauri::command]
fn get_tree(state: State<'_, Mutex<Store>>) -> Value {
    state.lock().unwrap_or_else(|e| e.into_inner()).tree.clone()
}

#[tauri::command]
fn get_node(path: String, state: State<'_, Mutex<Store>>) -> Result<Value, String> {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let key_bs = path.replace('/', "\\");
    let n = s.by_path.get(&key_bs).or_else(|| {
        s.norm_index
            .get(&norm(&path))
            .and_then(|p| s.by_path.get(p))
    });
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
    consists.sort_by_key(name_lower);
    out["consists_of"] = Value::Array(consists);
    out["referenced_by"] = Value::Array(s.ref_index.get(&key).cloned().unwrap_or_default());
    Ok(out)
}

#[tauri::command]
fn list_playbooks(state: State<'_, Mutex<Store>>) -> Vec<Value> {
    let dir = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .workspace
        .join("playbooks");
    let mut out: Vec<Value> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|f| f.ends_with(".json") && !f.starts_with('_'))
            .collect();
        files.sort();
        for f in files {
            match std::fs::read_to_string(dir.join(&f))
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            {
                Some(pb) => {
                    let feature = pb
                        .get("feature")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&f)
                        .to_string();
                    let steps = pb
                        .get("steps")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
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
    let path = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .workspace
        .join("playbooks")
        .join(&name);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{e}"))
}

/// Run bos_explore.py on a .bos file, writing `out_json`, and return its nodes.
fn run_parser(workspace: &Path, bos_path: &str, out_json: &Path) -> Result<Vec<Value>, String> {
    let script = workspace.join("bos_explore.py");
    let status = std::process::Command::new("python")
        .arg(&script)
        .arg(bos_path)
        .arg("--json")
        .arg(out_json)
        .current_dir(workspace)
        .status()
        .map_err(|e| format!("failed to launch python: {e}"))?;
    if !status.success() {
        return Err(format!("bos_explore.py exited with {status}"));
    }
    let text = std::fs::read_to_string(out_json).map_err(|e| format!("read parser output: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("parse parser output: {e}"))
}

fn file_name_of(path: &str) -> String {
    PathBuf::from(path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "(no .bos)".into())
}

/// Re-run the Python parser (bos_explore.py) on a .bos file, refreshing
/// `<workspace>/bos_map.json`, then rebuild the in-memory store. Dev/no-project
/// path; project mode uses `import_bos`.
///
/// Note: the store lock is released during the parse, so a concurrent
/// `open_project` could interleave. Harmless in this single-operator desktop
/// app (the UI never fires both at once); revisit if parsing moves off the UI.
#[tauri::command]
fn parse_bos(path: String, state: State<'_, Mutex<Store>>) -> Result<Value, String> {
    let workspace = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .workspace
        .clone();
    let out_json = workspace.join("bos_map.json");
    let nodes = run_parser(&workspace, &path, &out_json)?;
    let config_name = file_name_of(&path);
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    *s = build_store_from_nodes(workspace, config_name, nodes);
    Ok(json!({ "config": s.config_name, "count": s.nodes.len() }))
}

// ----------------------------- project commands -----------------------------

/// One JSON summary of the active project (or null) plus its snapshots.
fn project_view(p: &Projects) -> Value {
    match &p.active {
        None => json!({ "active": Value::Null, "root": p.root.to_string_lossy() }),
        Some(ap) => {
            let snaps = projects::snapshots(&ap.db).unwrap_or_default();
            json!({
                "active": { "id": ap.id, "name": ap.name, "path": ap.dir.to_string_lossy() },
                "snapshots": snaps,
                "root": p.root.to_string_lossy(),
            })
        }
    }
}

#[tauri::command]
fn list_projects(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let p = projects.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(&p.root).map_err(|e| format!("create projects root: {e}"))?;
    Ok(projects::list(&p.root))
}

#[tauri::command]
fn create_project(name: String, projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let p = projects.lock().unwrap_or_else(|e| e.into_inner());
    std::fs::create_dir_all(&p.root).map_err(|e| format!("create projects root: {e}"))?;
    projects::create(&p.root, &name)
}

#[tauri::command]
fn current_project(projects: State<'_, Mutex<Projects>>) -> Value {
    project_view(&projects.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Open a project by id, loading its active snapshot into the render store.
#[tauri::command]
fn open_project(
    id: String,
    projects: State<'_, Mutex<Projects>>,
    store: State<'_, Mutex<Store>>,
) -> Result<Value, String> {
    let mut p = projects.lock().unwrap_or_else(|e| e.into_inner());
    let ap = projects::open(&p.root, &id)?;
    // Load the active snapshot (if any) into the renderer.
    let workspace = store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .workspace
        .clone();
    if let Some((fname, nodes)) = projects::active_snapshot(&ap.db)? {
        let arr = nodes.as_array().cloned().unwrap_or_default();
        *store.lock().unwrap_or_else(|e| e.into_inner()) =
            build_store_from_nodes(workspace, fname, arr);
    }
    p.active = Some(ap);
    Ok(project_view(&p))
}

/// Parse a `.bos` into the active project as a new snapshot and render it.
#[tauri::command]
fn import_bos(
    path: String,
    label: Option<String>,
    projects: State<'_, Mutex<Projects>>,
    store: State<'_, Mutex<Store>>,
) -> Result<Value, String> {
    // Take only the paths we need, then release the projects lock before the
    // (slow, blocking) Python parse so other project commands aren't starved.
    let (db, out_json) = {
        let p = projects.lock().unwrap_or_else(|e| e.into_inner());
        let ap = p
            .active
            .as_ref()
            .ok_or_else(|| "no project is open — create or open one first".to_string())?;
        (ap.db.clone(), ap.dir.join("last_parse.json"))
    };
    let workspace = store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .workspace
        .clone();
    let nodes = run_parser(&workspace, &path, &out_json)?;
    let fname = file_name_of(&path);
    let label = label.unwrap_or_else(|| fname.clone());
    let nodes_val = Value::Array(nodes.clone());
    let id = projects::add_snapshot(&db, &label, Some(&fname), &nodes_val)?;
    *store.lock().unwrap_or_else(|e| e.into_inner()) =
        build_store_from_nodes(workspace, fname, nodes);
    let p = projects.lock().unwrap_or_else(|e| e.into_inner());
    Ok(json!({ "snapshot_id": id, "view": project_view(&p) }))
}

// ----------------------------- vault commands -----------------------------

/// Vault status (exists / unlocked / secret count) — never returns values.
#[tauri::command]
fn vault_status(vault: State<'_, Mutex<Vault>>) -> Value {
    vault.lock().unwrap_or_else(|e| e.into_inner()).status()
}

/// Unlock the vault using the OS-keychain DEK (Phase 1 step 2 gates this behind
/// Entra SSO). Idempotent; creates an empty vault on first unlock.
#[tauri::command]
fn vault_unlock(vault: State<'_, Mutex<Vault>>) -> Result<Value, String> {
    let mut v = vault.lock().unwrap_or_else(|e| e.into_inner());
    v.unlock()?;
    Ok(v.status())
}

/// Lock the vault, zeroizing the in-memory DEK.
#[tauri::command]
fn vault_lock(vault: State<'_, Mutex<Vault>>) -> Value {
    let mut v = vault.lock().unwrap_or_else(|e| e.into_inner());
    v.lock();
    v.status()
}

/// Sorted secret names (never values).
#[tauri::command]
fn vault_list_secrets(vault: State<'_, Mutex<Vault>>) -> Result<Vec<String>, String> {
    vault.lock().unwrap_or_else(|e| e.into_inner()).names()
}

/// Insert or replace a secret. The plaintext value never leaves this process
/// except via `vault_reveal_secret`.
#[tauri::command]
fn vault_set_secret(
    name: String,
    value: String,
    vault: State<'_, Mutex<Vault>>,
) -> Result<(), String> {
    vault
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set(&name, &value)
}

/// Remove a secret. Returns whether it existed.
#[tauri::command]
fn vault_delete_secret(name: String, vault: State<'_, Mutex<Vault>>) -> Result<bool, String> {
    vault
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .delete(&name)
}

/// Reveal a single secret's raw value (requires unlocked). Use sparingly.
#[tauri::command]
fn vault_reveal_secret(name: String, vault: State<'_, Mutex<Vault>>) -> Result<String, String> {
    vault
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reveal(&name)
}

/// Plaintext-secret guardrail: scan text for credentials that belong in the
/// vault. Returns masked findings (the scan never echoes a raw secret).
#[tauri::command]
fn scan_secret(text: String) -> Vec<Value> {
    vault::scan_for_secrets(&text)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let workspace = default_workspace();
    let store = build_store(workspace.clone());
    let projects_root = projects::default_root(&workspace);
    let vault = Vault::new(vault::default_path(&projects_root));
    let projects = Projects::new(projects_root);
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Mutex::new(store))
        .manage(Mutex::new(projects))
        .manage(Mutex::new(vault))
        .invoke_handler(tauri::generate_handler![
            app_state,
            get_tree,
            get_node,
            list_playbooks,
            get_playbook,
            parse_bos,
            list_projects,
            create_project,
            current_project,
            open_project,
            import_bos,
            vault_status,
            vault_unlock,
            vault_lock,
            vault_list_secrets,
            vault_set_secret,
            vault_delete_secret,
            vault_reveal_secret,
            scan_secret
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
