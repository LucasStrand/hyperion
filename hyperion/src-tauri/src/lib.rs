// Hyperion — bOS Configurator desktop core.
//
// Phase 0: this Rust layer reimplements the four read-only Flask endpoints from
// bos_copilot.py as Tauri commands, computed from the parsed `bos_map.json`
// (produced by bos_explore.py). All UI/grading logic lives in the webview
// (src/main.ts); this layer only loads data and exposes:
//   app_state, get_tree, get_node, list_playbooks, get_playbook, parse_bos
//
// Strictly read-only with respect to the bOS system — never writes to it.

mod agent;
mod artifacts;
mod bosparse;
mod collab;
mod crawler;
mod diff;
mod embed;
mod entra;
mod export;
mod ingest;
mod milesight;
mod netreg;
mod playbook;
mod projects;
mod roster;
mod security;
mod standard;
mod suggest;
mod tooling;
mod vault;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::{Manager, State};

use entra::Auth;
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

/// Resolve a node by path, accepting `/` or `\` separators and case-insensitive
/// matches. Shared by the `get_node` command and the agent grounding builder.
fn lookup_node<'a>(s: &'a Store, path: &str) -> Option<&'a Value> {
    let key_bs = path.replace('/', "\\");
    s.by_path
        .get(&key_bs)
        .or_else(|| s.norm_index.get(&norm(path)).and_then(|p| s.by_path.get(p)))
}

#[tauri::command]
fn get_node(path: String, state: State<'_, Mutex<Store>>) -> Result<Value, String> {
    let s = state.lock().unwrap_or_else(|e| e.into_inner());
    let n = match lookup_node(&s, &path) {
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
    let value: Value = serde_json::from_str(&text).map_err(|e| format!("{e}"))?;
    // Additive: validate/normalize the optional per-step `ui` guided-walkthrough
    // actions. Playbooks without `ui` round-trip unchanged.
    Ok(playbook::normalize(value))
}

/// Locate the bundled `bos_explore.py` (used only by the optional Python
/// fallback). In a packaged installer the script ships in the Tauri resource
/// dir; in `cargo tauri dev` it sits next to the workspace. Prefer the resource
/// copy, fall back to `<workspace>/bos_explore.py`.
fn resolve_parser_script(app: &tauri::AppHandle, workspace: &Path) -> PathBuf {
    if let Ok(res) = app
        .path()
        .resolve("bos_explore.py", tauri::path::BaseDirectory::Resource)
    {
        if res.exists() {
            return res;
        }
    }
    workspace.join("bos_explore.py")
}

/// Find a usable Python 3 interpreter on PATH (`python`, then `python3`, then the
/// Windows `py` launcher). Returns the command name, or a clear message the UI
/// surfaces verbatim. Only reached on the rare Python fallback path now that the
/// pure-Rust parser (`bosparse`) is the default.
fn detect_python() -> Result<String, String> {
    for cand in ["python", "python3", "py"] {
        let ok = std::process::Command::new(cand)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(cand.to_string());
        }
    }
    Err(
        "the optional Python fallback parser needs Python 3 on PATH plus \
         `pip install nrbf`, but no interpreter was found. The pure-Rust parser \
         normally handles .bos files; this fallback only runs if it errors."
            .to_string(),
    )
}

/// Parse a `.bos` file into the node array, writing `out_json`, and return its
/// nodes.
///
/// The pure-Rust parser (`bosparse`) is the default — the packaged installer no
/// longer requires Python + `pip install nrbf`. Python is now OPTIONAL: kept only
/// as a safety-net fallback, used automatically if the Rust parser errors on some
/// unforeseen record shape, so no user can regress.
fn run_parser(
    app: &tauri::AppHandle,
    workspace: &Path,
    bos_path: &str,
    out_json: &Path,
) -> Result<Vec<Value>, String> {
    match parse_bos_rust(bos_path, out_json) {
        Ok(nodes) => Ok(nodes),
        Err(rust_err) => match run_parser_python(app, workspace, bos_path, out_json) {
            Ok(nodes) => Ok(nodes),
            Err(py_err) => Err(format!(
                "pure-Rust parser failed ({rust_err}); optional Python fallback \
                 also failed ({py_err})"
            )),
        },
    }
}

/// Pure-Rust `.bos` parse (default). Reads the file, parses the MS-NRBF graph,
/// reconstructs the node tree, and persists the same JSON `out_json` the Python
/// reference wrote (so dev-mode `bos_map.json` keeps working).
fn parse_bos_rust(bos_path: &str, out_json: &Path) -> Result<Vec<Value>, String> {
    // Cap the read so an oversized/hostile .bos can't be slurped whole before the
    // parser (which works on the full byte buffer) ever runs. Mirrors the
    // metadata pre-check on the other ingest paths (`milesight_import`,
    // `context_add_file`). Compare in u64 — `len()` is u64.
    const MAX_BOS_BYTES: u64 = 256 * 1024 * 1024;
    let meta = std::fs::metadata(bos_path).map_err(|e| format!("read .bos file: {e}"))?;
    if meta.len() > MAX_BOS_BYTES {
        return Err(format!(
            "the .bos file is too large (max {} MB)",
            MAX_BOS_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(bos_path).map_err(|e| format!("read .bos file: {e}"))?;
    let nodes = bosparse::parse_bos_file(&bytes)?;
    let text =
        serde_json::to_string_pretty(&nodes).map_err(|e| format!("serialize parsed nodes: {e}"))?;
    std::fs::write(out_json, text).map_err(|e| format!("write parser output: {e}"))?;
    Ok(nodes)
}

/// Optional fallback: shell out to bos_explore.py (requires Python + `nrbf`).
/// Only reached if the pure-Rust parser above returns an error. Resolves the
/// bundled script from the resource dir and detects the interpreter so the
/// fallback still works inside a packaged installer.
fn run_parser_python(
    app: &tauri::AppHandle,
    workspace: &Path,
    bos_path: &str,
    out_json: &Path,
) -> Result<Vec<Value>, String> {
    let python = detect_python()?;
    let script = resolve_parser_script(app, workspace);
    if !script.exists() {
        return Err(format!(
            "bundled parser not found at {} — the install may be corrupt",
            script.display()
        ));
    }
    let status = std::process::Command::new(&python)
        .arg(&script)
        .arg(bos_path)
        .arg("--json")
        .arg(out_json)
        .current_dir(workspace)
        .status()
        .map_err(|e| format!("failed to launch {python}: {e}"))?;
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
fn parse_bos(
    app: tauri::AppHandle,
    path: String,
    state: State<'_, Mutex<Store>>,
) -> Result<Value, String> {
    let workspace = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .workspace
        .clone();
    let out_json = workspace.join("bos_map.json");
    let nodes = run_parser(&app, &workspace, &path, &out_json)?;
    let config_name = file_name_of(&path);
    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
    *s = build_store_from_nodes(workspace, config_name, nodes);
    Ok(json!({ "config": s.config_name, "count": s.nodes.len() }))
}

// ----------------------------- milesight import (M1) -----------------------------
//
// Parse an uploaded Milesight LoRaWAN gateway configuration export into a
// normalized IoT topology (gateway + LoRa settings + attached devices). The
// webview passes an absolute path picked via the OS dialog; this command reads
// the file off disk (size-capped like `context_add_file`), parses it as JSON, and
// runs the pure `milesight::parse_gateway`. No project is required and nothing is
// written — strictly local and read-only toward bOS.
//
// NOTE: the parser assumes the common Milesight UG-series JSON export shape (see
// `milesight.rs`); validate against a real export before relying on the mapping.
#[tauri::command]
fn milesight_import(path: String) -> Result<Value, String> {
    let p = Path::new(&path);
    // Bound the read so a huge/binary file can't be slurped before validation
    // (mirrors `context_add_file`). Compare in u64 — `len()` is u64.
    let meta = std::fs::metadata(p).map_err(|e| format!("read file: {e}"))?;
    if meta.len() > ingest::MAX_FILE_BYTES as u64 {
        return Err(format!(
            "file is too large (max {} MB)",
            ingest::MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    let text = std::fs::read_to_string(p).map_err(|e| format!("read file: {e}"))?;
    let json: Value = serde_json::from_str(&text).map_err(|e| format!("parse JSON: {e}"))?;
    let topology = milesight::parse_gateway(&json);
    serde_json::to_value(&topology).map_err(|e| format!("serialize topology: {e}"))
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
    app: tauri::AppHandle,
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
    let nodes = run_parser(&app, &workspace, &path, &out_json)?;
    let fname = file_name_of(&path);
    let label = label.unwrap_or_else(|| fname.clone());
    let nodes_val = Value::Array(nodes.clone());
    let id = projects::add_snapshot(&db, &label, Some(&fname), &nodes_val)?;
    *store.lock().unwrap_or_else(|e| e.into_inner()) =
        build_store_from_nodes(workspace, fname, nodes);
    let p = projects.lock().unwrap_or_else(|e| e.into_inner());
    Ok(json!({ "snapshot_id": id, "view": project_view(&p) }))
}

/// Load a snapshot's parsed node tree (`map_json`) by id from a project db. The
/// snapshot rows are written by `projects::add_snapshot`; this is the read-by-id
/// counterpart the diff needs (the public `active_snapshot` only loads the active
/// pointer). Read-only — opens the db, reads one row, parses the stored JSON.
fn load_snapshot_nodes(db: &Path, id: i64) -> Result<Value, String> {
    let conn = rusqlite::Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let map_json: String = conn
        .query_row(
            "SELECT map_json FROM snapshot WHERE id = ?1",
            rusqlite::params![id],
            |r| r.get(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => format!("no such snapshot: {id}"),
            other => format!("read snapshot {id}: {other}"),
        })?;
    serde_json::from_str(&map_json).map_err(|e| format!("parse snapshot {id}: {e}"))
}

/// Semantic diff between two of the active project's snapshots: which nodes were
/// added, removed, or had individual fields changed between `from_id` (older) and
/// `to_id` (newer). Loads each snapshot's parsed node tree by id and runs the pure
/// `diff::diff_nodes`. Returns `{ added, removed, changed }` JSON. Read-only.
#[tauri::command]
fn snapshot_diff(
    from_id: i64,
    to_id: i64,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let from = load_snapshot_nodes(&db, from_id)?;
    let to = load_snapshot_nodes(&db, to_id)?;
    let result = diff::diff_nodes(&from, &to);
    serde_json::to_value(result).map_err(|e| format!("serialize diff: {e}"))
}

// ----------------------------- memory commands -----------------------------

/// Path to the active project's db, or a clear error if none is open — mirrors
/// the `import_bos` "open a project first" contract so the UI message is uniform.
fn active_project_db(projects: &State<'_, Mutex<Projects>>) -> Result<PathBuf, String> {
    let p = projects.lock().unwrap_or_else(|e| e.into_inner());
    let ap = p
        .active
        .as_ref()
        .ok_or_else(|| "no project is open — create or open one first".to_string())?;
    Ok(ap.db.clone())
}

/// List the active project's persistent memory notes (id, mtype, slug, body, updated_at).
#[tauri::command]
fn memory_list(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    projects::memory_list(&db)
}

/// Insert or replace a memory note (upsert by slug) in the active project. The
/// `mtype` must be one of project|feature|reference|security. Returns its id.
#[tauri::command]
fn memory_set(
    mtype: String,
    slug: String,
    body: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let id = projects::memory_set(&db, &mtype, &slug, &body)?;
    Ok(json!({ "id": id }))
}

/// Delete a memory note by id from the active project. Returns whether it existed.
#[tauri::command]
fn memory_delete(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    projects::memory_delete(&db, id)
}

// ----------------------------- wiki commands (M4) -----------------------------
//
// Operator-editable wiki pages (projects.rs `wiki_page`). Per-project, so all
// three require an open project (same "open a project first" contract as memory).
// `wiki_save` runs the shared plaintext-secret guard. Strictly local toward bOS.

/// List the active project's wiki pages (slug, title, updated_at, bytes — no HTML).
#[tauri::command]
fn wiki_list(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    projects::wiki_list(&db)
}

/// Fetch one wiki page by slug (slug, title, html, updated_at), or null if absent.
#[tauri::command]
fn wiki_get(slug: String, projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    projects::wiki_get(&db, &slug)
}

/// Insert or replace a wiki page (upsert by slug) in the active project. Validated
/// and secret-scanned in `projects::wiki_save`. Returns its row id.
#[tauri::command]
fn wiki_save(
    slug: String,
    title: String,
    html: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let id = projects::wiki_save(&db, &slug, &title, &html)?;
    Ok(json!({ "id": id }))
}

/// Dev-default location of the hand-authored wiki vault: `hyperion/docs/wiki`,
/// resolved relative to this crate (`hyperion/src-tauri`). Overridable via the
/// `HYPERION_WIKI_DIR` env var for packaged builds / tests.
fn wiki_docs_dir() -> PathBuf {
    if let Ok(d) = std::env::var("HYPERION_WIKI_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("docs").join("wiki"))
        .unwrap_or_else(|| PathBuf::from("docs/wiki"))
}

/// Static-site export of the wiki vault (M4). Gathers `hyperion/docs/wiki/*.html`
/// and writes a self-contained folder to `dest`: every page verbatim plus a
/// generated `index.html` gallery linking them. Returns `{written, index_path}`.
/// Read-only toward bOS; the only writes are into the operator-chosen `dest`.
#[tauri::command]
fn wiki_export(dest: String) -> Result<Value, String> {
    let dest = dest.trim();
    if dest.is_empty() {
        return Err("choose a destination folder".into());
    }
    let pages = export::gather_wiki_pages(&wiki_docs_dir())?;
    if pages.is_empty() {
        return Err("no wiki pages found to export".into());
    }
    let summary = export::export_site(&pages, Path::new(dest))?;
    Ok(json!({ "written": summary.written, "index_path": summary.index_path }))
}

// ------------------------- artifact templates (V2, Track 4) -------------------------
//
// The bundled HTML-effectiveness artifact library (artifacts.rs): pickable, themeable
// starting points the operator inserts into a wiki page. Both commands are static —
// the catalog is compiled into the binary via `include_str!`, so neither needs an
// open project or any state. Read-only toward bOS. Saving the inserted HTML still
// runs through `wiki_save`'s secret scan and length check.

/// The artifact-template catalog: `{key, label, description}` for each bundled
/// pattern, in gallery order. The HTML body is fetched separately (see
/// `artifact_template_get`) so the listing stays small.
#[tauri::command]
fn artifact_templates_list() -> Result<Vec<Value>, String> {
    artifacts::catalog()
        .iter()
        .map(|t| serde_json::to_value(t).map_err(|e| format!("serialize artifact template: {e}")))
        .collect()
}

/// The full, themeable HTML body of one artifact template, by key, for the operator
/// to drop into the wiki editor. Errors cleanly when the key is unknown.
#[tauri::command]
fn artifact_template_get(key: String) -> Result<Value, String> {
    let html = artifacts::get(&key)?;
    Ok(json!({ "key": key, "html": html }))
}

/// The canonical html-effectiveness guide the bundled templates are distilled from;
/// `artifact_guide_refresh` re-derives the per-technique "use when…" guidance from this
/// live source.
const ARTIFACT_GUIDE_URL: &str = "https://thariqs.github.io/html-effectiveness/";

/// Refresh the artifact-template *guidance* from its live source — additive and
/// optional. Firecrawls the html-effectiveness guide (reusing the existing
/// `crawler::fetch` Firecrawl/GET path, never re-implementing HTTP), strips it to text
/// via `crawler::extract_text`, derives a per-technique "use when…" note
/// (`artifacts::derive_guide_notes`), and caches the distilled notes as project
/// knowledge via the crawler's `crawl_store` (keyed by the guide URL, secret-scanned
/// there) so the guidance is searchable in-app next to the templates. The embedded
/// template HTML is never touched.
///
/// Graceful no-op: when no `HYPERION_FIRECRAWL_API_KEY` is configured, or the fetch
/// fails, it returns `{refreshed:false, reason}` rather than erroring — the feature is
/// optional and the app never requires the key. Requires an open project (it stores
/// per-project knowledge), same contract as the other `crawl_*` commands.
#[tauri::command]
async fn artifact_guide_refresh(projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    // Optional feature: the refresh needs a Firecrawl key. Absent -> clear no-op, not an
    // error, so the app never requires the key (checked before requiring a project so the
    // message is always reachable).
    if !crawler::firecrawl_configured() {
        return Ok(json!({
            "refreshed": false,
            "reason": "set HYPERION_FIRECRAWL_API_KEY to refresh the artifact guide from source (this feature is optional)",
        }));
    }
    let db = active_project_db(&projects)?;
    // The network fetch + HTML strip run on the blocking pool (mirrors `crawl_add`) so a
    // slow/unreachable host can't stall the async event loop.
    tauri::async_runtime::spawn_blocking(move || {
        let html = match crawler::fetch(ARTIFACT_GUIDE_URL) {
            Ok(h) => h,
            // A failed fetch is non-fatal: report the (already redacted) reason.
            Err(e) => return Ok(json!({ "refreshed": false, "reason": e })),
        };
        let (_title, text) = crawler::extract_text(&html);
        let notes = artifacts::derive_guide_notes(&text);
        if notes.is_empty() {
            return Ok(json!({
                "refreshed": false,
                "reason": "the guide page had no recognizable per-technique guidance",
            }));
        }
        let body = artifacts::format_guide_knowledge(&notes);
        let id = projects::crawl_store(
            &db,
            ARTIFACT_GUIDE_URL,
            Some("HTML-effectiveness artifact guide"),
            &body,
            Some("artifact-guide"),
        )?;
        let techniques: Vec<&str> = notes.iter().map(|n| n.label).collect();
        Ok(json!({
            "refreshed": true,
            "id": id,
            "count": notes.len(),
            "techniques": techniques,
        }))
    })
    .await
    .map_err(|e| format!("artifact guide refresh task failed: {e}"))?
}

// ----------------------------- roster commands (M5) -----------------------------
//
// The agent roster + versioned instincts (roster.rs). Listing the roster works
// without a project (the built-in agents are static); instinct customization is
// per-project, so the editor commands require an open project (same contract as
// memory). All of this is local and read-only toward bOS.

/// The full agent roster. When a project is open, each agent is annotated with
/// whether its instincts have been customized and the active version.
#[tauri::command]
fn agent_roster(projects: State<'_, Mutex<Projects>>) -> Vec<Value> {
    let db = active_project_db(&projects).ok();
    roster::roster_json(db.as_deref())
}

/// Detail view of one agent's instincts (resolved body, built-in baseline, version,
/// provenance) for the editor. Requires an open project.
#[tauri::command]
fn agent_instincts_get(
    agent_id: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    roster::instincts_detail(&db, &agent_id)
}

/// Save a new instinct version for an agent (append-only). Validated + secret-scanned.
/// Returns the new version number. Requires an open project.
#[tauri::command]
fn agent_instincts_set(
    agent_id: String,
    body: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let version = roster::instincts_set(&db, &agent_id, &body)?;
    Ok(json!({ "version": version }))
}

/// Full instinct version history for an agent (newest first). Requires an open project.
#[tauri::command]
fn agent_instincts_history(
    agent_id: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    roster::instincts_history(&db, &agent_id)
}

/// Revert an agent's instincts to an earlier version (copies it forward as a new
/// version; `version = 0` restores the built-in baseline). Requires an open project.
#[tauri::command]
fn agent_instincts_revert(
    agent_id: String,
    version: i64,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let new_version = roster::instincts_revert(&db, &agent_id, version)?;
    Ok(json!({ "version": new_version }))
}

// ----------------------------- context files (M1) -----------------------------
//
// Ingested reference material the agent retrieves from on every ask (ingest.rs +
// projects.rs). Per-project, so all three require an open project. Reading the file
// off disk happens here (the webview passes a path picked via the OS dialog); the
// content stays local and is only ever spliced into the prompt as fenced, encoded,
// untrusted data.

/// List the active project's ingested context files (metadata only, never content).
#[tauri::command]
fn context_list(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    projects::context_list(&db)
}

/// Ingest a file (by absolute path) into the active project: read it, extract text,
/// chunk, and store. Returns `{id, name, kind, chunks}`. Rejects unsupported kinds.
#[tauri::command]
async fn context_add_file(
    path: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let p = Path::new(&path);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "could not read the file name".to_string())?
        .to_string();
    // Bound the read so a huge/binary file can't be slurped before validation.
    let meta = std::fs::metadata(p).map_err(|e| format!("read file: {e}"))?;
    // Compare in u64 — `meta.len()` is u64, so casting it down to usize would
    // truncate (and pass the guard) for a >4 GiB file on a 32-bit build.
    if meta.len() > ingest::MAX_FILE_BYTES as u64 {
        return Err(format!(
            "file is too large (max {} MB)",
            ingest::MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(p).map_err(|e| format!("read file: {e}"))?;
    // context_add chunks, secret-scans, and (when an embedding key is configured)
    // makes a blocking network round-trip to embed the chunks. Run it on the
    // blocking pool so a slow/unreachable embedding endpoint can't stall the
    // async event loop. Mirrors the spawn_blocking used by agent_ask retrieval.
    tauri::async_runtime::spawn_blocking(move || projects::context_add(&db, &name, &bytes))
        .await
        .map_err(|e| format!("ingest task failed: {e}"))?
}

/// Delete an ingested context file (and its chunks) by id from the active project.
#[tauri::command]
fn context_delete(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    projects::context_delete(&db, id)
}

/// Proactively suggest what context the active project is missing (no `.bos`
/// snapshot, no/few context files, or query terms documented nowhere). Read-only
/// and deterministic; returns one flat JSON object per suggestion. `query` is the
/// operator's pending question, used to flag undocumented salient terms.
#[tauri::command]
fn context_suggest(
    query: Option<String>,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    suggest::suggest(&db, query.as_deref())?
        .iter()
        .map(|s| serde_json::to_value(s).map_err(|e| format!("serialize suggestion: {e}")))
        .collect()
}

// ----------------------------- knowledge crawler (M7) -----------------------------
//
// A best-effort sidecar (crawler.rs + projects.rs `crawl_*`) that fetches official
// docs / forum pages, caches the stripped text as project knowledge, and proposes
// deterministic "eureka" improvements from terms the crawled corpus surfaces that
// the project's loaded context doesn't yet mention. Per-project, so every command
// requires an open project. The network fetch is OFF unless `HYPERION_CRAWL_ENABLED`
// is set (so offline/CI never reaches out), and cached text is secret-scanned by
// `crawl_store`. Strictly read-only toward bOS.

/// Collect the active project's "context terms": the salient keyword tokens already
/// present in its loaded grounding — memory note bodies, the active `.bos` snapshot's
/// node names/paths, and ingested context-file names. The eureka heuristic treats a
/// crawled term absent from this set as a novel "you should look at X". Read-only and
/// dependency-free (reuses `ingest::keywords`); any per-source read error is skipped
/// so a partial project still yields useful terms.
fn active_context_terms(db: &Path) -> Vec<String> {
    let mut terms: HashSet<String> = HashSet::new();
    if let Ok(notes) = projects::memory_load_for_prompt(db) {
        for (_mtype, body) in notes {
            terms.extend(ingest::keywords(&body));
        }
    }
    if let Ok(Some((_fname, nodes))) = projects::active_snapshot(db) {
        if let Some(arr) = nodes.as_array() {
            for n in arr {
                if let Some(name) = n.get("name").and_then(|v| v.as_str()) {
                    terms.extend(ingest::keywords(name));
                }
                if let Some(path) = n.get("path").and_then(|v| v.as_str()) {
                    terms.extend(ingest::keywords(path));
                }
            }
        }
    }
    if let Ok(files) = projects::context_list(db) {
        for f in files {
            if let Some(name) = f.get("name").and_then(|v| v.as_str()) {
                terms.extend(ingest::keywords(name));
            }
        }
    }
    terms.into_iter().collect()
}

/// Crawl a URL into the active project: fetch the page, extract `(title, text)`, and
/// cache it (best-effort, upsert by url). Returns `{id, url, title, bytes}`. The
/// network fetch + HTML parse run on the blocking pool (mirrors `context_add_file`)
/// so a slow/unreachable host can't stall the async event loop. Disabled (Err) unless
/// `HYPERION_CRAWL_ENABLED` is set truthy.
#[tauri::command]
async fn crawl_add(url: String, projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    tauri::async_runtime::spawn_blocking(move || {
        let html = crawler::fetch(&url)?;
        let (title, text) = crawler::extract_text(&html);
        if text.trim().is_empty() {
            return Err("fetched page had no readable text".to_string());
        }
        let title_opt = if title.trim().is_empty() {
            None
        } else {
            Some(title.as_str())
        };
        let id = projects::crawl_store(&db, &url, title_opt, &text, Some("web"))?;
        Ok(json!({ "id": id, "url": url, "title": title, "bytes": text.len() }))
    })
    .await
    .map_err(|e| format!("crawl task failed: {e}"))?
}

/// List the active project's cached crawl docs (metadata only, never body text).
#[tauri::command]
fn crawl_list(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    projects::crawl_list(&db)
}

/// Fetch one cached crawl doc by id (full record incl. text), or null if absent.
#[tauri::command]
fn crawl_get(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    projects::crawl_get(&db, id)
}

/// Delete a cached crawl doc by id from the active project. Returns whether it existed.
#[tauri::command]
fn crawl_delete(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    projects::crawl_delete(&db, id)
}

/// Run the deterministic eureka heuristic over the active project's cached crawl docs
/// versus its loaded context terms. Returns one flat JSON object per suggestion
/// (term, weight, source, message), highest-weighted first — bOS pillar terms
/// (Configurator/Service/Client) are weighted ahead of incidental vocabulary.
#[tauri::command]
fn crawl_eureka(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    active_eureka_suggestions(&db)?
        .iter()
        .map(|s| serde_json::to_value(s).map_err(|e| format!("serialize eureka: {e}")))
        .collect()
}

/// Compute the active project's current eureka `Suggestion`s: cached crawl docs vs.
/// the loaded context terms. The single place the gather-and-rank step lives, shared
/// by `crawl_eureka` (which serializes the suggestions) and `crawl_eureka_propose_pr`
/// (which formats them into a PR), so neither command duplicates the logic.
fn active_eureka_suggestions(db: &Path) -> Result<Vec<crawler::Suggestion>, String> {
    let docs = projects::crawl_load_for_eureka(db)?;
    let terms = active_context_terms(db);
    Ok(crawler::eureka(&docs, &terms))
}

/// Close the knowledge loop: compute the active project's eureka findings and, when any
/// are novel, draft them into a human-approvable in-app pull request via
/// `collab::pr_create` (a human `narrative` + machine-readable `ai_docs`). Returns
/// `{created: true, pr_id, title, count}` on success. When nothing is novel it returns
/// `{created: false, reason}` WITHOUT opening an empty PR. `pr_create` secret-scans
/// every field, so a finding that somehow embedded a secret is rejected there exactly
/// as any other PR would be. Read-only toward bOS.
#[tauri::command]
fn crawl_eureka_propose_pr(projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let suggestions = active_eureka_suggestions(&db)?;
    let Some(draft) = crawler::format_proposal(&suggestions) else {
        return Ok(json!({
            "created": false,
            "reason": "nothing novel to propose — the crawled docs add no terms beyond your loaded project context",
        }));
    };
    let pr_id = collab::pr_create(
        &db,
        &draft.title,
        Some(&draft.narrative),
        Some(&draft.ai_docs),
    )?;
    Ok(json!({
        "created": true,
        "pr_id": pr_id,
        "title": draft.title,
        "count": draft.count,
    }))
}

// --- multi-source crawl: curated source registry + tiered sweep (M7 extension) ---
//
// The single-page `crawl_add` above is upgraded into a multi-source, tiered system.
// The operator curates a per-project REGISTRY of official ComfortClick/IoT docs +
// forum URLs (`crawl_source_*`), then a `crawl_sweep` fetches every enabled source in
// one shot. The sweep tiers the work HONESTLY (see `projects::crawl_store_deduped` /
// `sources_enabled` for the in-depth split):
//   * CHEAP pass ("dumber crawler"): for each enabled source, fetch + strip + store —
//     the exact single-page logic, looped, deduped by url/content so a re-run is safe.
//   * SMART pass ("smarter agent"): the deterministic eureka heuristic distills the
//     refreshed corpus against the loaded project context. This is a heuristic
//     distiller, NOT an external LLM — nothing here spawns a model from Rust.
//
// RECURRING sweeps: Hyperion is a desktop app and CANNOT schedule itself (there is no
// in-process cron — that would be a fake). To run a sweep on a schedule, drive this
// command from OUTSIDE the app with Claude Code's `CronCreate`/`loop` against a small
// script that opens the project and invokes `crawl_sweep`. See the "Recurring sweeps"
// note in README.md / docs/wiki/plan.html for the exact recipe.

/// Add (or update) a curated crawl source for the active project. `kind` is `docs` or
/// `forum`; `label` is optional. Re-adding the same url upserts in place. Returns the
/// source `{id}` shape via `source_list` semantics (just the new id here).
#[tauri::command]
fn crawl_source_add(
    url: String,
    label: Option<String>,
    kind: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<i64, String> {
    let db = active_project_db(&projects)?;
    projects::source_add(&db, &url, label.as_deref(), &kind)
}

/// List the active project's curated crawl sources (id, url, label, kind, enabled, added_at).
#[tauri::command]
fn crawl_source_list(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    projects::source_list(&db)
}

/// Enable or disable one curated source by id (parks it without deleting). Returns
/// whether a row was updated.
#[tauri::command]
fn crawl_source_set_enabled(
    id: i64,
    enabled: bool,
    projects: State<'_, Mutex<Projects>>,
) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    projects::source_set_enabled(&db, id, enabled)
}

/// Remove one curated source by id (already-cached pages stay in `crawl_doc`). Returns
/// whether a row existed.
#[tauri::command]
fn crawl_source_remove(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    projects::source_remove(&db, id)
}

/// Sweep every ENABLED curated source into the active project's cached knowledge, then
/// (optionally) distill it.
///
/// CHEAP pass (always): for each enabled source, run the same best-effort fetch+strip
/// path as `crawl_add` and cache the result via `crawl_store_deduped` — deduping by
/// url AND content so re-running the sweep is safe (unchanged sources are no-ops). Each
/// source's `kind` (docs|forum) is carried into the cached doc's `source` tag, so forum
/// pages remain attributable and are swept identically to docs. Per-source failures are
/// collected, never fatal — a slow/unreachable host doesn't abort the rest.
///
/// SMART pass (`smart = true`): after the cache is refreshed, run the deterministic
/// eureka heuristic over the whole crawled corpus vs the loaded context and report how
/// many novel findings it surfaced (the operator then opens `crawl_eureka_propose_pr`).
/// This is a heuristic distiller, not an external agent — no model is invoked here.
///
/// The network fetch + HTML parse run on the blocking pool (mirrors `crawl_add`) so a
/// slow host can't stall the async event loop. Disabled (each fetch Errs) unless
/// `HYPERION_CRAWL_ENABLED` is set truthy.
#[tauri::command]
async fn crawl_sweep(smart: bool, projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    tauri::async_runtime::spawn_blocking(move || {
        // ---- CHEAP pass ("dumber crawler"): fetch + strip + store, deduped. ----
        let sources = projects::sources_enabled(&db)?;
        let mut created = 0u32;
        let mut updated = 0u32;
        let mut unchanged = 0u32;
        let mut failed = 0u32;
        let mut errors: Vec<Value> = Vec::new();
        for (url, kind) in &sources {
            match crawler::fetch(url) {
                Ok(html) => {
                    let (title, text) = crawler::extract_text(&html);
                    if text.trim().is_empty() {
                        failed += 1;
                        errors.push(json!({ "url": url, "error": "page had no readable text" }));
                        continue;
                    }
                    let title_opt = if title.trim().is_empty() {
                        None
                    } else {
                        Some(title.as_str())
                    };
                    match projects::crawl_store_deduped(&db, url, title_opt, &text, Some(kind)) {
                        Ok((_, projects::CrawlOutcome::Created)) => created += 1,
                        Ok((_, projects::CrawlOutcome::Updated)) => updated += 1,
                        Ok((_, projects::CrawlOutcome::Unchanged)) => unchanged += 1,
                        Err(e) => {
                            failed += 1;
                            errors.push(json!({ "url": url, "error": e }));
                        }
                    }
                }
                Err(e) => {
                    failed += 1;
                    errors.push(json!({ "url": url, "error": e }));
                }
            }
        }
        // ---- SMART pass ("smarter agent"): eureka distill vs loaded context. ----
        // Heuristic, deterministic, offline — NOT an external LLM agent.
        let eureka_findings = if smart {
            active_eureka_suggestions(&db)?.len()
        } else {
            0
        };
        Ok(json!({
            "sources": sources.len(),
            "created": created,
            "updated": updated,
            "unchanged": unchanged,
            "failed": failed,
            "errors": errors,
            "smart": smart,
            "eureka_findings": eureka_findings,
        }))
    })
    .await
    .map_err(|e| format!("sweep task failed: {e}"))?
}

// ----------------------------- collab: PRs + timeline (M8) -----------------------------
//
// In-app pull requests (human narrative + AI docs), their comment/argue threads,
// and the project timeline (collab.rs). Per-project, so every command requires an
// open project (same "open a project first" contract as memory). Operator/agent
// text is secret-scanned in `collab` before it lands in the unencrypted project DB.
// Strictly local and read-only toward bOS.

/// List the active project's pull requests (id, title, status, created_at, comment
/// count — no bodies). Newest first.
#[tauri::command]
fn pr_list(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    collab::pr_list(&db)
}

/// Open a new pull request in the active project. `narrative` (human) and `ai_docs`
/// (agent) are optional and secret-scanned. Returns its row id.
#[tauri::command]
fn pr_create(
    title: String,
    narrative: Option<String>,
    ai_docs: Option<String>,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let id = collab::pr_create(&db, &title, narrative.as_deref(), ai_docs.as_deref())?;
    Ok(json!({ "id": id }))
}

/// Fetch one PR by id (full record + its comment thread), or null if absent.
#[tauri::command]
fn pr_get(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    collab::pr_get(&db, id)
}

/// Append a comment to a PR's thread. Body is secret-scanned. Returns its id.
#[tauri::command]
fn pr_comment_add(
    pr_id: i64,
    author: String,
    body: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let id = collab::pr_comment_add(&db, pr_id, &author, &body)?;
    Ok(json!({ "id": id }))
}

/// Set a PR's lifecycle status (open|merged|closed). Returns whether it existed.
#[tauri::command]
fn pr_set_status(
    id: i64,
    status: String,
    projects: State<'_, Mutex<Projects>>,
) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    collab::pr_set_status(&db, id, &status)
}

/// Delete a PR and its comment thread from the active project. Returns whether it existed.
#[tauri::command]
fn pr_delete(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    collab::pr_delete(&db, id)
}

/// The active project's timeline (id, kind, summary, detail, created_at), newest
/// first. Optional `limit` caps the number of returned events.
#[tauri::command]
fn timeline_list(
    limit: Option<i64>,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    collab::timeline_list(&db, limit)
}

/// Append an event to the active project's timeline. `summary`/`detail` are
/// secret-scanned. Returns the new row id.
#[tauri::command]
fn timeline_add(
    kind: String,
    summary: String,
    detail: Option<String>,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let id = collab::timeline_add(&db, &kind, &summary, detail.as_deref())?;
    Ok(json!({ "id": id }))
}

// ----------------------------- tool auto-select (M9) -----------------------------
//
// A deterministic recommender (tooling.rs) that maps the loaded context — whether a
// `.bos` is open, the kinds of ingested context files, and the pending question — to
// suggested MCP servers + ECC skills. Pure mapping; this command only gathers the
// inputs from live state. Offline and read-only toward bOS; no project is required
// (an open project just adds the context-file-kind signal).

/// Recommend MCP servers + ECC skills for the current context. `query` is the
/// operator's pending question (optional). Reads whether a `.bos` is loaded from the
/// render store and the ingested file kinds from the active project (if any), then
/// runs the pure `tooling::recommend`. Returns one flat JSON object per recommendation.
#[tauri::command]
fn recommend_tools(
    query: Option<String>,
    store: State<'_, Mutex<Store>>,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Vec<Value>, String> {
    let has_bos = !store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .nodes
        .is_empty();
    // The file-kind signal comes from the active project's ingested context, if a
    // project is open. No project simply means no file signal — the recommender still
    // works on the `.bos` + question alone, so this never requires a project.
    let context_file_kinds: Vec<String> = match active_project_db(&projects).ok() {
        Some(db) => projects::context_list(&db)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| {
                f.get("kind")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect(),
        None => Vec::new(),
    };
    let input = tooling::ToolingInput {
        has_bos,
        context_file_kinds,
        query: query.unwrap_or_default(),
    };
    tooling::recommend(&input)
        .iter()
        .map(|r| serde_json::to_value(r).map_err(|e| format!("serialize tool rec: {e}")))
        .collect()
}

// ----------------------------- code standard (M3) -----------------------------
//
// The recommended project code standard (standard.rs) plus a deterministic audit
// that flags deviations in Hyperion's own sources. Both are fully local and
// read-only toward bOS; no project needs to be open (this inspects the app's own
// source tree, not project data).

/// The canonical code standard as a structured summary (title, prose, and the
/// list of audit rules with their fixes). Static and side-effect-free; the prose
/// version lives in `docs/wiki/code-standard.html`.
#[tauri::command]
fn code_standard() -> Value {
    standard::standard_summary()
}

/// Audit Hyperion's own sources against the code standard. Reads every
/// `src-tauri/src/*.rs` and `src/*.ts` off disk (relative to the crate root) and
/// runs the pure `standard::audit`, returning one flat JSON object per deviation.
#[tauri::command]
fn code_audit() -> Result<Vec<Value>, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let files = standard::collect_project_sources(&manifest_dir)?;
    standard::audit(&files)
        .iter()
        .map(|f| serde_json::to_value(f).map_err(|e| format!("serialize finding: {e}")))
        .collect()
}

// ----------------------------- security commands (M6) -----------------------------

/// Scan Hyperion's own sources for security risks (hardcoded secrets, `unsafe`
/// blocks, risky web APIs, unresolved risk markers). Reads every
/// `src-tauri/src/*.rs` and `src/*.ts` off disk (relative to the crate root,
/// reusing the standard-audit collector) and runs the pure `security::scan_source`,
/// returning one flat JSON object per finding. Read-only with respect to bOS.
#[tauri::command]
fn security_scan() -> Result<Vec<Value>, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let files = standard::collect_project_sources(&manifest_dir)?;
    security::scan_source(&files)
        .iter()
        .map(|f| serde_json::to_value(f).map_err(|e| format!("serialize finding: {e}")))
        .collect()
}

/// True if a CI workflow is present in the repo: any file under
/// `<workspace>/.github/workflows`. Deterministic disk check, no network.
fn ci_workflow_present(workspace: &Path) -> bool {
    let dir = workspace.join(".github").join("workflows");
    std::fs::read_dir(&dir)
        .map(|mut entries| entries.any(|e| e.is_ok()))
        .unwrap_or(false)
}

/// Evaluate the enterprise-readiness gate from the running app's known state:
/// the encrypted vault exists, access is Entra-gated (currently signed in), a CI
/// workflow is present, the crate ships tests, and the source scan finds no
/// plaintext secrets. Returns the pass/fail `GateResult` with per-item detail.
#[tauri::command]
fn enterprise_gate_check(
    auth: State<'_, Mutex<Auth>>,
    vault: State<'_, Mutex<Vault>>,
) -> Result<Value, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace = default_workspace();

    let files = standard::collect_project_sources(&manifest_dir)?;
    let plaintext_secret_findings =
        security::plaintext_secret_count(&security::scan_source(&files));
    let tests_present = files.iter().any(|(_, c)| c.contains("#[cfg(test)]"));

    // Lock order matches the rest of the app: auth before vault.
    let sso_enabled = auth
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_authenticated();
    let vault_encrypted = vault.lock().unwrap_or_else(|e| e.into_inner()).exists();

    let result = security::enterprise_gate(&security::EnterpriseInputs {
        vault_encrypted,
        sso_enabled,
        ci_present: ci_workflow_present(&workspace),
        tests_present,
        plaintext_secret_findings,
    });
    serde_json::to_value(result).map_err(|e| format!("serialize gate result: {e}"))
}

// ----------------------------- vault commands -----------------------------

/// Vault status (exists / unlocked / secret count) — never returns values.
#[tauri::command]
fn vault_status(vault: State<'_, Mutex<Vault>>) -> Value {
    vault.lock().unwrap_or_else(|e| e.into_inner()).status()
}

/// Unlock the vault using the OS-keychain DEK. Gated behind a Microsoft Entra
/// sign-in (defense-in-depth). Idempotent; creates an empty vault on first use.
#[tauri::command]
fn vault_unlock(
    auth: State<'_, Mutex<Auth>>,
    vault: State<'_, Mutex<Vault>>,
) -> Result<Value, String> {
    // Hold the auth guard across the vault unlock (lock order: auth before
    // vault, consistent everywhere) so a concurrent sign-out can't slip in
    // between the check and the unlock.
    let a = auth.lock().unwrap_or_else(|e| e.into_inner());
    if !a.is_authenticated() {
        return Err("sign in with Microsoft Entra before unlocking the vault".into());
    }
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

// ----------------------------- network registry commands (M6) -----------------------------
//
// A vault-backed registry of the building network's addresses + logins (netreg.rs +
// the `net_entry` table). Per-project, so every command requires an open project (same
// "open a project first" contract as memory/wiki/context). The per-entry secret is
// *sealed by the vault*: `net_add` stores the plaintext secret in the encrypted vault
// under a fresh per-entry key and persists only that key in the row's opaque
// `secret_cipher` blob; `net_get` reveals it back. The clear fields are secret-scanned
// in `netreg::add`. A locked vault yields a clear error rather than storing plaintext.

/// A fresh, collision-resistant vault key under which one network entry's secret is
/// sealed. Only this *key* (not the secret) lands in `net_entry.secret_cipher`; the
/// secret itself is encrypted at rest by the vault under this name. Minted randomly so
/// it can be created *before* the row is inserted, avoiding a second write to set it.
fn fresh_net_secret_key() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("netreg/{hex}")
}

/// List the active project's network entries (id, label, address, username, notes,
/// updated_at, has_secret — never the sealed secret blob or its plaintext).
#[tauri::command]
fn net_list(projects: State<'_, Mutex<Projects>>) -> Result<Vec<Value>, String> {
    let db = active_project_db(&projects)?;
    netreg::list(&db)
}

/// Add a network entry to the active project. When `secret` is non-empty the vault must
/// be unlocked: the secret is sealed into the vault under a fresh key and only that key
/// is stored in the row. Clear fields are validated + secret-scanned in `netreg::add`.
#[tauri::command]
fn net_add(
    label: String,
    address: String,
    username: Option<String>,
    secret: Option<String>,
    notes: Option<String>,
    projects: State<'_, Mutex<Projects>>,
    vault: State<'_, Mutex<Vault>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    // Seal the secret (if any) into the vault FIRST, under a fresh key, so only the key
    // — never the plaintext — is handed to netreg. Gated on an unlocked vault so a
    // locked vault returns a clear error instead of silently dropping the secret.
    let sealed_key: Option<String> =
        match secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(sec) => {
                let v = vault.lock().unwrap_or_else(|e| e.into_inner());
                if !v.is_unlocked() {
                    return Err(
                        "vault is locked — unlock it before storing a network login secret".into(),
                    );
                }
                let key = fresh_net_secret_key();
                v.set(&key, sec)?;
                Some(key)
            }
            None => None,
        };
    let cipher = sealed_key.as_ref().map(|k| k.as_bytes());
    match netreg::add(
        &db,
        &label,
        &address,
        username.as_deref(),
        cipher,
        notes.as_deref(),
    ) {
        Ok(id) => Ok(json!({ "id": id })),
        Err(e) => {
            // The row was rejected (e.g. empty label, or a secret-shaped clear field).
            // Roll back the just-sealed secret so the vault keeps no orphan entry.
            if let Some(key) = &sealed_key {
                let v = vault.lock().unwrap_or_else(|e| e.into_inner());
                let _ = v.delete(key);
            }
            Err(e)
        }
    }
}

/// Reveal one network entry by id, unsealing its secret from the vault. Requires the
/// vault to be unlocked when the entry has a stored secret. Returns the entry metadata
/// plus the plaintext `secret` (or null when none is stored).
#[tauri::command]
fn net_get(
    id: i64,
    projects: State<'_, Mutex<Projects>>,
    vault: State<'_, Mutex<Vault>>,
) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let entry = netreg::get(&db, id)?.ok_or_else(|| format!("no such network entry: {id}"))?;
    // Unseal the secret from the vault (requires unlocked) when one is stored.
    let secret = match &entry.secret_cipher {
        Some(blob) => {
            let key = std::str::from_utf8(blob)
                .map_err(|_| "stored secret reference is corrupt".to_string())?;
            let v = vault.lock().unwrap_or_else(|e| e.into_inner());
            if !v.is_unlocked() {
                return Err("vault is locked — unlock it to reveal this login's secret".into());
            }
            Some(v.reveal(key)?)
        }
        None => None,
    };
    Ok(json!({
        "id": entry.id,
        "label": entry.label,
        "address": entry.address,
        "username": entry.username,
        "notes": entry.notes,
        "updated_at": entry.updated_at,
        "secret": secret,
    }))
}

/// Delete a network entry by id from the active project. Returns whether it existed.
/// Best-effort drops the entry's sealed secret from the vault first (when one exists
/// and the vault is unlocked); the row is removed regardless, so a locked vault never
/// blocks deleting an entry.
#[tauri::command]
fn net_delete(
    id: i64,
    projects: State<'_, Mutex<Projects>>,
    vault: State<'_, Mutex<Vault>>,
) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    if let Ok(Some(entry)) = netreg::get(&db, id) {
        if let Some(blob) = &entry.secret_cipher {
            if let Ok(key) = std::str::from_utf8(blob) {
                let v = vault.lock().unwrap_or_else(|e| e.into_inner());
                if v.is_unlocked() {
                    let _ = v.delete(key);
                }
            }
        }
    }
    netreg::delete(&db, id)
}

// ----------------------------- Entra SSO commands -----------------------------

/// Auth status: `{ authenticated, identity }`.
#[tauri::command]
fn entra_status(auth: State<'_, Mutex<Auth>>) -> Value {
    auth.lock().unwrap_or_else(|e| e.into_inner()).status()
}

/// Run the interactive Microsoft Entra sign-in (opens the system browser).
/// Blocking; the auth lock is released during the browser round-trip so other
/// commands are not starved.
#[tauri::command]
fn entra_sign_in(auth: State<'_, Mutex<Auth>>) -> Result<Value, String> {
    // Perform the (slow, blocking) browser flow WITHOUT holding the lock.
    let mut runner = Auth::default();
    runner.sign_in()?;
    let status = runner.status();
    // Commit the result under the lock only briefly.
    *auth.lock().unwrap_or_else(|e| e.into_inner()) = runner;
    Ok(status)
}

/// Sign out of Entra and lock the vault (signing out must drop secret access).
#[tauri::command]
fn entra_sign_out(auth: State<'_, Mutex<Auth>>, vault: State<'_, Mutex<Vault>>) -> Value {
    let mut a = auth.lock().unwrap_or_else(|e| e.into_inner());
    a.sign_out();
    vault.lock().unwrap_or_else(|e| e.into_inner()).lock();
    a.status()
}

// ----------------------------- agent commands -----------------------------

/// Snapshot of which runtimes can serve a request right now.
struct RuntimeAvailability {
    claude: bool,
    codex: bool,
    openrouter_key: bool,
}

/// Is an OpenRouter key reachable — from the environment, or from the vault if
/// it is already unlocked (never forces an unlock)?
fn openrouter_key_present(vault: &State<'_, Mutex<Vault>>) -> bool {
    if std::env::var("OPENROUTER_API_KEY")
        .map(|k| !k.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    let v = vault.lock().unwrap_or_else(|e| e.into_inner());
    // Check the stored *value*, not just the name: `Vault::set` rejects empty
    // names but not empty values, so a blank `openrouter_api_key` would otherwise
    // report available and then fail in `resolve_openrouter_key` during an ask.
    v.is_unlocked()
        && v.reveal("openrouter_api_key")
            .map(|val| !val.trim().is_empty())
            .unwrap_or(false)
}

/// Resolve the actual OpenRouter key: env first, else the unlocked vault. Always
/// trimmed — a trailing newline (common from `export KEY=$(cat …)` or a paste)
/// would otherwise produce `Bearer …\n` and an opaque 401.
fn resolve_openrouter_key(vault: &State<'_, Mutex<Vault>>) -> Option<String> {
    if let Ok(k) = std::env::var("OPENROUTER_API_KEY") {
        let k = k.trim();
        if !k.is_empty() {
            return Some(k.to_string());
        }
    }
    let v = vault.lock().unwrap_or_else(|e| e.into_inner());
    if v.is_unlocked() {
        if let Ok(val) = v.reveal("openrouter_api_key") {
            let val = val.trim();
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn availability(vault: &State<'_, Mutex<Vault>>) -> RuntimeAvailability {
    RuntimeAvailability {
        claude: agent::claude_path().is_some(),
        codex: agent::codex_path().is_some(),
        openrouter_key: openrouter_key_present(vault),
    }
}

/// Pick the active runtime: an explicit, *available* override
/// (`HYPERION_AGENT_RUNTIME`) wins; otherwise the first available in the
/// preference order Claude Code → Codex → OpenRouter (local first).
fn active_runtime(a: &RuntimeAvailability) -> Option<agent::Runtime> {
    let avail = |r: agent::Runtime| match r {
        agent::Runtime::ClaudeCode => a.claude,
        agent::Runtime::Codex => a.codex,
        agent::Runtime::OpenRouter => a.openrouter_key,
    };
    if let Ok(ov) = std::env::var("HYPERION_AGENT_RUNTIME") {
        if let Some(r) = agent::Runtime::parse(&ov) {
            if avail(r) {
                return Some(r);
            }
        }
    }
    [
        agent::Runtime::ClaudeCode,
        agent::Runtime::Codex,
        agent::Runtime::OpenRouter,
    ]
    .into_iter()
    .find(|&r| avail(r))
}

/// Agent runtime status for the UI: which backends are present and which is active.
#[tauri::command]
fn agent_status(vault: State<'_, Mutex<Vault>>) -> Value {
    let a = availability(&vault);
    let active = active_runtime(&a);
    json!({
        "runtimes": [
            { "kind": "claude",     "label": "Claude Code", "available": a.claude },
            { "kind": "codex",      "label": "Codex",       "available": a.codex },
            { "kind": "openrouter", "label": "OpenRouter",  "available": a.openrouter_key },
        ],
        "active": active.map(|r| r.label()),
        "any": active.is_some(),
    })
}

/// Build a compact, token-bounded grounding block from the loaded `.bos` for the
/// agent's system prompt. Phase 2 grounds on the live tree (+ the focused node);
/// Phase 3 will prepend retrieved context chunks here. Never reads the vault.
fn build_grounding(s: &Store, focus: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Loaded .bos config: {} ({} objects).\n",
        s.config_name,
        s.nodes.len()
    ));
    if s.nodes.is_empty() {
        out.push_str(
            "No configuration is loaded yet — tell the user to open a project and import a .bos.\n",
        );
        return out;
    }

    // Top-level sections (depth-1 children of root) with child counts.
    if let Some(kids) = s.tree.get("children").and_then(|v| v.as_array()) {
        out.push_str("Top-level sections (name × direct children):\n");
        for (i, k) in kids.iter().enumerate() {
            if i >= 40 {
                out.push_str(&format!("  …and {} more.\n", kids.len() - 40));
                break;
            }
            let name = k.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let n = k
                .get("children")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            out.push_str(&format!("  - {name} ({n})\n"));
        }
    }

    // Distinct object types present, with frequency.
    let mut types: BTreeMap<String, usize> = BTreeMap::new();
    for n in &s.nodes {
        if let Some(t) = n.get("type").and_then(|v| v.as_str()) {
            *types.entry(t.to_string()).or_default() += 1;
        }
    }
    if !types.is_empty() {
        let parts: Vec<String> = types.iter().map(|(t, c)| format!("{t}×{c}")).collect();
        let shown = parts.len().min(30);
        out.push_str("Object types present: ");
        out.push_str(&parts[..shown].join(", "));
        if parts.len() > shown {
            out.push_str(&format!(", …{} more", parts.len() - shown));
        }
        out.push('\n');
    }

    // The node the user is currently looking at, if any.
    if let Some(fp) = focus.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(n) = lookup_node(s, fp) {
            out.push_str("\nCurrently selected node (the user is looking at this):\n");
            out.push_str(&node_brief(s, n));
        }
    }

    // Hard cap so a huge config can't blow up the prompt.
    const MAX: usize = 6000;
    if out.chars().count() > MAX {
        out = out.chars().take(MAX).collect::<String>() + "\n…(grounding truncated)\n";
    }
    out
}

/// One-node summary for grounding: path, type, write targets, fan-in/out counts.
fn node_brief(s: &Store, n: &Value) -> String {
    let key = node_path(n);
    let mut b = String::new();
    b.push_str(&format!("  path: {key}\n"));
    if let Some(t) = n.get("type").and_then(|v| v.as_str()) {
        b.push_str(&format!("  type: {t}\n"));
    }
    if let Some(w) = n.get("writes").and_then(|v| v.as_array()) {
        let targets: Vec<String> = w
            .iter()
            .filter_map(|r| r.get("object").and_then(|v| v.as_str()))
            .take(10)
            .map(|x| x.to_string())
            .collect();
        if !targets.is_empty() {
            b.push_str(&format!("  writes to: {}\n", targets.join(", ")));
        }
    }
    let refs = s.ref_index.get(&key).map(|v| v.len()).unwrap_or(0);
    let kids = s.children_index.get(&key).map(|v| v.len()).unwrap_or(0);
    b.push_str(&format!("  referenced by: {refs}; consists of: {kids}\n"));
    b
}

/// Build a labeled, token-bounded block of the active project's memory notes for
/// the agent's system prompt. These are *operator-authored* (entered in the UI),
/// so unlike the `.bos`-derived grounding they are background facts about the
/// project rather than untrusted data — but they are still kept small so a long
/// note can't crowd out the question. Returns None when there are no notes. Never
/// touches the vault.
fn build_memory_block(notes: &[(String, String)]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    const MAX: usize = 4000;
    // Escape each note's fence delimiters and flatten its newlines exactly as
    // `safe_grounding` (below) hardens the .bos data. A memory note is operator
    // text, but it is still spliced verbatim into the system prompt, so a body
    // containing `</bos-data>` must not be able to close the untrusted-data fence
    // below, and a multi-line body must not be able to start its own top-level
    // `# ` header line. Escaping `& < >` neutralizes the fence, and rewriting each
    // `\n` to a continuation indent keeps every body on indented continuation
    // lines — so the block is structurally inert regardless of its content.
    let safe = |t: &str| {
        t.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\n', "\n  ")
    };
    let mut out = String::from(
        "Background facts about this project the operator saved (persist across sessions):\n",
    );
    for (mtype, body) in notes {
        let line = format!("- [{}] {}\n", safe(mtype), safe(body.trim()));
        if out.chars().count() + line.chars().count() > MAX {
            out.push_str("…(memory truncated)\n");
            break;
        }
        out.push_str(&line);
    }
    Some(out)
}

/// Build the retrieved-context block from `(file_name, chunk_text)` hits. Each chunk
/// is UNTRUSTED file content, so its fence delimiters are entity-encoded (so a chunk
/// containing `</context-files>` or `</bos-data>` cannot close the surrounding fence)
/// and the whole block is char-capped. The caller wraps the result in a
/// `<context-files>` fence inside the untrusted region. Returns the encoded body.
fn build_context_block(hits: &[(String, String)]) -> String {
    const MAX: usize = 6000;
    // Entity-encode the fence delimiters AND flatten newlines to a continuation
    // indent, exactly as build_memory_block hardens operator text: encoding `& < >`
    // neutralizes the fence, and rewriting each `\n` keeps a multi-line chunk from
    // starting its own top-level `# ` heading line that could mimic the prompt
    // skeleton. The block is then structurally inert regardless of file content.
    let safe = |t: &str| {
        t.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\n', "\n  ")
    };
    let mut out = String::new();
    for (name, text) in hits {
        let entry = format!("--- from {} ---\n{}\n", safe(name), safe(text.trim()));
        if out.chars().count() + entry.chars().count() > MAX {
            out.push_str("…(retrieved context truncated)\n");
            break;
        }
        out.push_str(&entry);
    }
    out
}

/// Ask the active agent runtime a grounded question. Holds no lock across the
/// (slow, blocking) model call: grounding is built and any key resolved up
/// front, then the locks are released before `agent::ask`.
#[tauri::command]
async fn agent_ask(
    question: String,
    focus_path: Option<String>,
    agent_id: Option<String>,
    store: State<'_, Mutex<Store>>,
    vault: State<'_, Mutex<Vault>>,
    projects: State<'_, Mutex<Projects>>,
) -> Result<Value, String> {
    let question = question.trim().to_string();
    if question.is_empty() {
        return Err("ask a question first".into());
    }
    if question.chars().count() > 8000 {
        return Err("question is too long (max 8000 characters)".into());
    }

    // All state access happens up front (no MutexGuard is held across an await).
    let avail = availability(&vault);
    let runtime = active_runtime(&avail).ok_or(
        "no agent runtime available — install Claude Code or Codex, or set OPENROUTER_API_KEY",
    )?;

    // Build grounding, then drop the store lock before the model call. The
    // grounding is .bos-derived (untrusted) data, fenced so the model treats it
    // as data, not instructions (paired with the INSTINCTS note).
    let (grounding, focus_type) = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        let g = build_grounding(&s, focus_path.as_deref());
        // The focused node's `type`, if any, nudges deterministic routing.
        let ftype = focus_path
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .and_then(|p| lookup_node(&s, p))
            .and_then(|n| n.get("type").and_then(|v| v.as_str()))
            .map(|t| t.to_string());
        (g, ftype)
    };
    // Escape the fence delimiters in the .bos-derived grounding so a hostile node
    // name/path containing `</bos-data>` cannot close the untrusted-data section
    // and inject higher-priority-looking prompt text. (The user's turn is fenced
    // separately by a random sentinel in `compose_prompt`, but the bos-data block
    // must not be escapable either.)
    let safe_grounding = grounding
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    // Load the active project's persistent memory (if a project is open). These
    // are operator-authored notes — background facts, not instructions — so they
    // sit in their own labeled section ahead of the untrusted <bos-data> fence,
    // escaped (in build_memory_block) so a note can't break out of it. A missing/
    // empty memory table simply yields no section (an ask works without a project).
    // Resolve the active project DB once (if any) — used for both the chosen
    // agent's (possibly customized) instincts and the project memory.
    let db = {
        let p = projects.lock().unwrap_or_else(|e| e.into_inner());
        p.active.as_ref().map(|ap| ap.db.clone())
    };

    // Choose the agent: an explicit, non-empty `agent_id` selects a specialist
    // directly; otherwise the Coordinator deterministically routes (offline) on
    // the question text + the focused node's type. An unknown id is a hard error
    // rather than a silent fallback, so a UI bug can't quietly mis-route.
    let explicit = agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let agent = match explicit {
        Some(id) => roster::get(id).ok_or_else(|| format!("unknown agent: {id}"))?,
        None => roster::route(&question, focus_type.as_deref()),
    };
    let routed = explicit.is_none();

    // Suggested handoffs per the agent's share protocol. Computed now (it borrows
    // `question`) and owned as JSON so `question` can be moved into the blocking
    // call below. The static roster references outlive the await.
    let handoffs: Vec<Value> = roster::suggest_handoffs(agent, &question)
        .iter()
        .map(|h| json!({ "id": h.id, "name": h.name }))
        .collect();

    // The agent's role instincts — the customized version if this project has one,
    // else the built-in baseline. These are trusted operator/built-in instructions,
    // so they sit alongside INSTINCTS, ahead of the untrusted <bos-data> fence.
    let role_block = match &db {
        Some(db) => roster::role_block(db, agent)
            .map_err(|e| format!("could not load this agent's instincts: {e}"))?,
        None => roster::role_block_builtin(agent),
    };

    // Project memory (operator-authored facts), if a project is open.
    let memory_block = match &db {
        Some(db) => projects::memory_load_for_prompt(db)
            .ok()
            .and_then(|notes| build_memory_block(&notes)),
        None => None,
    };

    // Retrieve the most relevant chunks of the project's ingested context files for
    // this question. This is UNTRUSTED file content (a datasheet could carry injected
    // instructions), so it is entity-encoded and fenced exactly like the .bos data.
    let context_block = match &db {
        Some(db) => {
            // context_retrieve scans every chunk row and scores it in Rust — keep that
            // blocking DB work off the async executor so a large context store can't
            // stall the Tauri event loop. (JoinError or a retrieval error → no context;
            // the ask still proceeds on the .bos grounding alone.)
            let db = db.clone();
            let q = question.clone();
            tauri::async_runtime::spawn_blocking(move || projects::context_retrieve(&db, &q, 4))
                .await
                .ok()
                .and_then(|r| r.ok())
                .filter(|hits| !hits.is_empty())
                .map(|hits| build_context_block(&hits))
        }
        None => None,
    };

    let mut system = agent::INSTINCTS.to_string();
    system.push_str("\n\n");
    system.push_str(&role_block);
    if let Some(mem) = &memory_block {
        system.push_str("\n\n# Project memory (operator-authored background facts)\n");
        system.push_str(mem);
    }
    system.push_str(&format!(
        "\n\n# Loaded system context (untrusted data)\n<bos-data>\n{safe_grounding}\n</bos-data>"
    ));
    if let Some(ctx) = &context_block {
        system.push_str(&format!(
            "\n\n# Retrieved from the project's context files (untrusted data)\n<context-files>\n{ctx}\n</context-files>"
        ));
    }

    // Resolve the key (briefly locking the vault) only for the cloud path.
    let key = if runtime == agent::Runtime::OpenRouter {
        resolve_openrouter_key(&vault)
    } else {
        None
    };
    let model = std::env::var("HYPERION_OPENROUTER_MODEL")
        .unwrap_or_else(|_| agent::DEFAULT_OPENROUTER_MODEL.to_string());

    // Run the blocking round-trip (subprocess or HTTP, up to 180s) off the UI
    // thread so a slow/hung runtime never freezes the Tauri event loop.
    let answer = tauri::async_runtime::spawn_blocking(move || {
        agent::ask(runtime, &system, &question, key.as_deref(), &model)
    })
    .await
    .map_err(|e| format!("agent task failed: {e}"))??;

    Ok(json!({
        "runtime": runtime.label(),
        "answer": answer,
        "agent": { "id": agent.id, "name": agent.name, "role": agent.role },
        "routed": routed,
        "handoffs": handoffs,
    }))
}

/// Parse one `KEY=VALUE` line from the optional startup env file. Returns `None` for
/// blanks and `#` comments. The first `=` splits key/value; surrounding whitespace and
/// one matching pair of single/double quotes are trimmed from the value. Pure and
/// unit-tested; the file I/O lives in `load_env_file`.
fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let k = k.trim();
    if k.is_empty() {
        return None;
    }
    let v = v.trim();
    let v = if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        &v[1..v.len() - 1]
    } else {
        v
    };
    Some((k.to_string(), v.to_string()))
}

/// Load `%APPDATA%\com.hyperion.iot\hyperion.env` into the process environment at
/// startup so the INSTALLED desktop app can pick up optional API keys / feature flags
/// (`OPENROUTER_API_KEY`, `HYPERION_FIRECRAWL_API_KEY`, `HYPERION_CRAWL_ENABLED`,
/// `HYPERION_EMBED_*`, `HYPERION_ENTRA_*`) without those secrets ever living in the
/// repo. A tiny dependency-free `KEY=VALUE` reader — NOT a full dotenv. A var already
/// present in the real process environment is NEVER overwritten (real env wins). The
/// file sits in the per-user app-config dir (outside the repo, git-ignored); the
/// encrypted vault remains the primary store for sensitive project data.
fn load_env_file() {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return;
    };
    let path = std::path::Path::new(&appdata)
        .join("com.hyperion.iot")
        .join("hyperion.env");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in contents.lines() {
        if let Some((k, v)) = parse_env_line(line) {
            if std::env::var_os(&k).is_none() {
                std::env::set_var(&k, &v);
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    load_env_file();
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
        .manage(Mutex::new(Auth::default()))
        .invoke_handler(tauri::generate_handler![
            app_state,
            get_tree,
            get_node,
            list_playbooks,
            get_playbook,
            parse_bos,
            milesight_import,
            list_projects,
            create_project,
            current_project,
            open_project,
            import_bos,
            snapshot_diff,
            memory_list,
            memory_set,
            memory_delete,
            wiki_list,
            wiki_get,
            wiki_save,
            wiki_export,
            artifact_templates_list,
            artifact_template_get,
            artifact_guide_refresh,
            agent_roster,
            agent_instincts_get,
            agent_instincts_set,
            agent_instincts_history,
            agent_instincts_revert,
            context_list,
            context_add_file,
            context_delete,
            context_suggest,
            crawl_add,
            crawl_list,
            crawl_get,
            crawl_delete,
            crawl_eureka,
            crawl_eureka_propose_pr,
            crawl_source_add,
            crawl_source_list,
            crawl_source_set_enabled,
            crawl_source_remove,
            crawl_sweep,
            pr_list,
            pr_create,
            pr_get,
            pr_comment_add,
            pr_set_status,
            pr_delete,
            timeline_list,
            timeline_add,
            recommend_tools,
            code_standard,
            code_audit,
            security_scan,
            enterprise_gate_check,
            vault_status,
            vault_unlock,
            vault_lock,
            vault_list_secrets,
            vault_set_secret,
            vault_delete_secret,
            vault_reveal_secret,
            scan_secret,
            net_list,
            net_add,
            net_get,
            net_delete,
            entra_status,
            entra_sign_in,
            entra_sign_out,
            agent_status,
            agent_ask
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod env_loader_tests {
    use super::parse_env_line;

    #[test]
    fn parses_pairs_skips_blanks_and_comments_and_strips_quotes() {
        assert_eq!(parse_env_line(""), None);
        assert_eq!(parse_env_line("   "), None);
        assert_eq!(parse_env_line("# a comment"), None);
        assert_eq!(parse_env_line("=novalue"), None);
        assert_eq!(
            parse_env_line("OPENROUTER_API_KEY=sk-or-abc"),
            Some(("OPENROUTER_API_KEY".into(), "sk-or-abc".into()))
        );
        // Surrounding whitespace trimmed; first `=` wins (value may contain `=`).
        assert_eq!(
            parse_env_line("  HYPERION_CRAWL_ENABLED = 1 "),
            Some(("HYPERION_CRAWL_ENABLED".into(), "1".into()))
        );
        assert_eq!(
            parse_env_line("K=a=b=c"),
            Some(("K".into(), "a=b=c".into()))
        );
        // One matching pair of quotes is stripped; mismatched quotes are left intact.
        assert_eq!(
            parse_env_line(r#"K="quoted value""#),
            Some(("K".into(), "quoted value".into()))
        );
        assert_eq!(
            parse_env_line("K='quoted'"),
            Some(("K".into(), "quoted".into()))
        );
        assert_eq!(
            parse_env_line(r#"K="mismatch'"#),
            Some(("K".into(), r#""mismatch'"#.into()))
        );
    }
}
