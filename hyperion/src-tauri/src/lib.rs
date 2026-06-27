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
mod entra;
mod ingest;
mod projects;
mod roster;
mod vault;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use tauri::State;

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
fn context_add_file(path: String, projects: State<'_, Mutex<Projects>>) -> Result<Value, String> {
    let db = active_project_db(&projects)?;
    let p = Path::new(&path);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "could not read the file name".to_string())?;
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
    projects::context_add(&db, name, &bytes)
}

/// Delete an ingested context file (and its chunks) by id from the active project.
#[tauri::command]
fn context_delete(id: i64, projects: State<'_, Mutex<Projects>>) -> Result<bool, String> {
    let db = active_project_db(&projects)?;
    projects::context_delete(&db, id)
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
        .manage(Mutex::new(Auth::default()))
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
            memory_list,
            memory_set,
            memory_delete,
            agent_roster,
            agent_instincts_get,
            agent_instincts_set,
            agent_instincts_history,
            agent_instincts_revert,
            context_list,
            context_add_file,
            context_delete,
            vault_status,
            vault_unlock,
            vault_lock,
            vault_list_secrets,
            vault_set_secret,
            vault_delete_secret,
            vault_reveal_secret,
            scan_secret,
            entra_status,
            entra_sign_in,
            entra_sign_out,
            agent_status,
            agent_ask
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
