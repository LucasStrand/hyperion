// Hyperion — per-project store (Phase 0).
//
// A *Project* is a folder under the projects root, holding a SQLite database
// (`project.db`). Phase 0 uses the `meta` and `snapshot` tables: snapshots are
// ordered, parsed `.bos` configs (the renderer reads the active snapshot's
// `map_json`). The remaining tables (context_file / memory / wiki_page /
// timeline) are created now but populated by later phases (M3-M8) so the
// schema is forward-compatible and a project opened today keeps working.
//
// Strictly local: these DBs are per-machine and git-ignored. No bOS write-back.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::vault;

/// Bump when the schema changes in a non-additive way.
pub const SCHEMA_VERSION: &str = "1";

/// Runtime state: where projects live and which one is open.
pub struct Projects {
    pub root: PathBuf,
    pub active: Option<ActiveProject>,
}

/// The currently open project (metadata only; connections are opened per call).
pub struct ActiveProject {
    pub id: String, // folder slug, also the stable identifier
    pub name: String,
    pub dir: PathBuf,
    pub db: PathBuf,
}

impl Projects {
    pub fn new(root: PathBuf) -> Self {
        Projects { root, active: None }
    }
}

/// Projects root: `HYPERION_PROJECTS` env, else `<workspace>/hyperion-projects`.
pub fn default_root(workspace: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("HYPERION_PROJECTS") {
        return PathBuf::from(p);
    }
    workspace.join("hyperion-projects")
}

/// Filesystem-safe identifier derived from a display name.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        "project".into()
    } else {
        s
    }
}

/// Create all tables (idempotent) and stamp metadata for a fresh DB.
fn init_db(conn: &Connection, name: &str) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS meta (
             key   TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS snapshot (
             id           INTEGER PRIMARY KEY AUTOINCREMENT,
             label        TEXT NOT NULL,
             bos_filename TEXT,
             created_at   TEXT NOT NULL,
             node_count   INTEGER NOT NULL,
             map_json     TEXT NOT NULL
         );
         -- Forward-compatible tables for later phases (created, not yet used):
         CREATE TABLE IF NOT EXISTS context_file (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             name TEXT NOT NULL, kind TEXT, added_at TEXT NOT NULL, content BLOB
         );
         CREATE TABLE IF NOT EXISTS memory (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             mtype TEXT NOT NULL, slug TEXT NOT NULL, body TEXT NOT NULL, updated_at TEXT NOT NULL
         );
         -- One row per slug. Added here (IF NOT EXISTS) so it also lands on DBs that
         -- created the forward-compat `memory` table before this index existed; the
         -- table was never populated before M5, so there can be no duplicate slugs
         -- to violate it. Enables the atomic ON CONFLICT(slug) upsert in memory_set.
         CREATE UNIQUE INDEX IF NOT EXISTS memory_slug_uq ON memory(slug);
         CREATE TABLE IF NOT EXISTS wiki_page (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             slug TEXT UNIQUE NOT NULL, title TEXT NOT NULL, html TEXT NOT NULL, updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS timeline (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             kind TEXT NOT NULL, summary TEXT NOT NULL, detail TEXT, created_at TEXT NOT NULL
         );",
    )?;
    // Stamp the version only on a fresh DB — never downgrade a future schema
    // when an older binary self-heals the forward-compat tables on open().
    if get_meta(conn, "schema_version")?.is_none() {
        set_meta(conn, "schema_version", SCHEMA_VERSION)?;
    }
    set_meta(conn, "name", name)?;
    // Only set created_at once.
    if get_meta(conn, "created_at")?.is_none() {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES ('created_at', datetime('now'))",
            [],
        )?;
    }
    Ok(())
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        rusqlite::params![key],
        |r| r.get::<_, String>(0),
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(other),
    })
}

/// Open (creating if needed) the DB for a project directory.
fn open_db(dir: &Path) -> Result<Connection, String> {
    Connection::open(dir.join("project.db")).map_err(|e| format!("open project db: {e}"))
}

/// Summarize one project folder as JSON (for the project picker).
fn summarize(dir: &Path) -> Option<Value> {
    let db = dir.join("project.db");
    if !db.exists() {
        return None;
    }
    let conn = Connection::open(&db).ok()?;
    let id = dir.file_name()?.to_string_lossy().to_string();
    let name = get_meta(&conn, "name")
        .ok()
        .flatten()
        .unwrap_or_else(|| id.clone());
    let created_at = get_meta(&conn, "created_at")
        .ok()
        .flatten()
        .unwrap_or_default();
    let snapshots: i64 = conn
        .query_row("SELECT COUNT(*) FROM snapshot", [], |r| r.get(0))
        .unwrap_or(0);
    Some(json!({
        "id": id,
        "name": name,
        "created_at": created_at,
        "snapshots": snapshots,
        "path": dir.to_string_lossy(),
    }))
}

/// All projects under the root, sorted by folder name.
pub fn list(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        let mut dirs: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for d in dirs {
            if let Some(v) = summarize(&d) {
                out.push(v);
            }
        }
    }
    out
}

/// Create a new project folder + DB. Returns its summary. Errors if it exists.
pub fn create(root: &Path, name: &str) -> Result<Value, String> {
    let slug = slugify(name);
    let dir = root.join(&slug);
    if dir.exists() {
        return Err(format!("a project named '{slug}' already exists"));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("create project dir: {e}"))?;
    let conn = open_db(&dir)?;
    init_db(&conn, name).map_err(|e| format!("init schema: {e}"))?;
    summarize(&dir).ok_or_else(|| "project created but could not be read back".into())
}

/// Reject any id that could escape the projects root (absolute path, separator,
/// or `..`). Valid ids are exactly what `slugify` produces: a single segment.
fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && !PathBuf::from(id).is_absolute()
        && !id.contains('/')
        && !id.contains('\\')
        && id != ".."
        && id != "."
}

/// Resolve a project id to an ActiveProject, ensuring its DB schema exists.
pub fn open(root: &Path, id: &str) -> Result<ActiveProject, String> {
    if !is_safe_id(id) {
        return Err(format!("invalid project id: {id}"));
    }
    let dir = root.join(id);
    if !dir.join("project.db").exists() {
        return Err(format!("no such project: {id}"));
    }
    let conn = open_db(&dir)?;
    let name = get_meta(&conn, "name")
        .ok()
        .flatten()
        .unwrap_or_else(|| id.to_string());
    // Self-heal: make sure forward-compat tables exist on older project DBs.
    init_db(&conn, &name).map_err(|e| format!("init schema: {e}"))?;
    Ok(ActiveProject {
        id: id.to_string(),
        name,
        dir: dir.clone(),
        db: dir.join("project.db"),
    })
}

/// Ordered snapshot list (metadata only, no map_json) for a project.
pub fn snapshots(db: &Path) -> Result<Vec<Value>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT id, label, bos_filename, created_at, node_count FROM snapshot ORDER BY id")
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "label": r.get::<_, String>(1)?,
                "bos_filename": r.get::<_, Option<String>>(2)?,
                "created_at": r.get::<_, String>(3)?,
                "node_count": r.get::<_, i64>(4)?,
            }))
        })
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

/// Insert a parsed snapshot, mark it active, and return its id.
pub fn add_snapshot(
    db: &Path,
    label: &str,
    bos_filename: Option<&str>,
    nodes: &Value,
) -> Result<i64, String> {
    let count = nodes.as_array().map(|a| a.len() as i64).unwrap_or(0);
    let map_json = serde_json::to_string(nodes).map_err(|e| format!("{e}"))?;
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    conn.execute(
        "INSERT INTO snapshot(label, bos_filename, created_at, node_count, map_json)
         VALUES (?1, ?2, datetime('now'), ?3, ?4)",
        rusqlite::params![label, bos_filename, count, map_json],
    )
    .map_err(|e| format!("insert snapshot: {e}"))?;
    let id = conn.last_insert_rowid();
    set_meta(&conn, "active_snapshot_id", &id.to_string()).map_err(|e| format!("{e}"))?;
    Ok(id)
}

/// Load the active snapshot's `(bos_filename, nodes)`, or None if none yet.
pub fn active_snapshot(db: &Path) -> Result<Option<(String, Value)>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let active = get_meta(&conn, "active_snapshot_id").map_err(|e| format!("{e}"))?;
    // Fall back to the most recent snapshot if no pointer is set.
    let row: Option<(String, String)> = match active {
        Some(idstr) => conn
            .query_row(
                "SELECT COALESCE(bos_filename, label), map_json FROM snapshot WHERE id = ?1",
                rusqlite::params![idstr.parse::<i64>().unwrap_or(-1)],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok(),
        None => conn
            .query_row(
                "SELECT COALESCE(bos_filename, label), map_json FROM snapshot ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .ok(),
    };
    match row {
        Some((fname, mj)) => {
            let nodes: Value = serde_json::from_str(&mj).map_err(|e| format!("{e}"))?;
            Ok(Some((fname, nodes)))
        }
        None => Ok(None),
    }
}

// ----------------------------- per-project memory (M5) -----------------------------
//
// Persistent, operator-authored notes loaded into the agent's grounding so it
// remembers facts across sessions (e.g. "the main pump is Modbus slave 3"). They
// live in the forward-compatible `memory` table (id, mtype, slug, body,
// updated_at). A note is keyed by `slug` and *upserted* (one row per slug). Notes
// are typed so the UI and the prompt can group them; an unknown type is rejected.
//
// Strictly local and read-only toward bOS. Secrets never belong here — they live
// only in the encrypted vault — and memory is never written into a vault prompt.

/// The four allowed memory categories. `project` = facts about this install,
/// `feature` = how a built feature works, `reference` = external/datasheet facts,
/// `security` = security-relevant reminders (never the secret itself).
pub const MEMORY_TYPES: [&str; 4] = ["project", "feature", "reference", "security"];

/// Is `mtype` one of the allowed categories?
pub fn valid_mtype(mtype: &str) -> bool {
    MEMORY_TYPES.contains(&mtype)
}

/// All memory notes for a project as JSON (id, mtype, slug, body, updated_at),
/// ordered deterministically by type then slug for a stable UI.
pub fn memory_list(db: &Path) -> Result<Vec<Value>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT id, mtype, slug, body, updated_at FROM memory ORDER BY mtype, slug, id")
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "mtype": r.get::<_, String>(1)?,
                "slug": r.get::<_, String>(2)?,
                "body": r.get::<_, String>(3)?,
                "updated_at": r.get::<_, String>(4)?,
            }))
        })
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

/// Upper bound on a stored note body (bytes). Generous for a rich operator note,
/// but capped so a single note can't dominate the agent's prompt budget or bloat
/// the DB. Enforced at write time in `memory_set`.
const MAX_BODY_LEN: usize = 8192;

/// Insert or replace a memory note, keyed by its (slugified) `slug`. Validates
/// `mtype`, requires a non-empty body within `MAX_BODY_LEN`, rejects bodies that
/// look like a plaintext secret, and stamps `updated_at = datetime('now')`.
/// Returns the row id. Atomic: a single `INSERT … ON CONFLICT(slug) DO UPDATE …
/// RETURNING id` against the `memory_slug_uq` unique index, so two concurrent
/// writers can't race between a separate update probe and an insert.
pub fn memory_set(db: &Path, mtype: &str, slug: &str, body: &str) -> Result<i64, String> {
    if !valid_mtype(mtype) {
        return Err(format!(
            "invalid memory type '{mtype}' (expected one of project|feature|reference|security)"
        ));
    }
    let slug = slugify(slug);
    let body = body.trim();
    if body.is_empty() {
        return Err("memory note body cannot be empty".into());
    }
    if body.len() > MAX_BODY_LEN {
        return Err("memory note body is too long (max 8 KB)".into());
    }
    // Plaintext-secret guardrail (mirrors the `scan_secret` command for snapshots):
    // a note is spliced verbatim into the agent prompt, so a real credential pasted
    // here would leak into context. Reject only the high-confidence shapes (PEM key,
    // AWS key, bearer token); the looser `credential_assignment` heuristic
    // false-positives on ordinary notes like "token bucket: ..." and would block
    // legitimate writes. Secrets belong in the encrypted vault, never in memory.
    const HIGH_CONFIDENCE: [&str; 3] = ["private_key", "aws_access_key", "bearer_token"];
    let has_secret = vault::scan_for_secrets(body).iter().any(|f| {
        f.get("kind")
            .and_then(|k| k.as_str())
            .is_some_and(|k| HIGH_CONFIDENCE.contains(&k))
    });
    if has_secret {
        return Err(
            "this note looks like it contains a plaintext secret — store it in the encrypted vault, not in project memory".into(),
        );
    }
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    conn.query_row(
        "INSERT INTO memory(mtype, slug, body, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(slug) DO UPDATE SET
             mtype = excluded.mtype,
             body = excluded.body,
             updated_at = excluded.updated_at
         RETURNING id",
        rusqlite::params![mtype, slug, body],
        |r| r.get::<_, i64>(0),
    )
    .map_err(|e| format!("upsert memory: {e}"))
}

/// Delete a memory note by id. Returns whether a row was removed.
pub fn memory_delete(db: &Path, id: i64) -> Result<bool, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let n = conn
        .execute("DELETE FROM memory WHERE id = ?1", rusqlite::params![id])
        .map_err(|e| format!("delete memory: {e}"))?;
    Ok(n > 0)
}

/// Compact `(mtype, body)` pairs for the agent grounding, ordered like
/// `memory_list`. The caller (lib.rs) labels and token-bounds them; this layer
/// stays free of prompt formatting.
pub fn memory_load_for_prompt(db: &Path) -> Result<Vec<(String, String)>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare("SELECT mtype, body FROM memory ORDER BY mtype, slug, id")
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, isolated projects root for one test (cleaned up by the caller).
    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hyperion_projects_test_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Create a project under a fresh root and return `(root, project.db path)`.
    fn fresh_db(tag: &str) -> (PathBuf, PathBuf) {
        let root = temp_root(tag);
        let summary = create(&root, "Test Project").unwrap();
        let id = summary.get("id").unwrap().as_str().unwrap().to_string();
        let db = root.join(&id).join("project.db");
        (root, db)
    }

    #[test]
    fn memory_roundtrip_set_list_delete() {
        let (root, db) = fresh_db("mem_rt");

        // Set a note; slug is normalized via slugify ("Main Pump" -> "main-pump").
        let id = memory_set(&db, "project", "Main Pump", "Main pump is Modbus slave 3.").unwrap();
        assert!(id > 0);
        let list = memory_list(&db).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["mtype"], "project");
        assert_eq!(list[0]["slug"], "main-pump");
        assert_eq!(list[0]["body"], "Main pump is Modbus slave 3.");

        // Upsert by slug: same slug updates the existing row (type + body), no dup.
        let id2 = memory_set(&db, "feature", "main-pump", "Updated note.").unwrap();
        assert_eq!(id, id2);
        let list = memory_list(&db).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["mtype"], "feature");
        assert_eq!(list[0]["body"], "Updated note.");

        // Prompt view returns the (mtype, body) pair.
        let loaded = memory_load_for_prompt(&db).unwrap();
        assert_eq!(
            loaded,
            vec![("feature".to_string(), "Updated note.".to_string())]
        );

        // Validation: bad type and empty body are rejected.
        assert!(memory_set(&db, "bogus", "x", "y").is_err());
        assert!(memory_set(&db, "project", "z", "   ").is_err());

        // Delete is idempotent on the second call.
        assert!(memory_delete(&db, id).unwrap());
        assert!(!memory_delete(&db, id).unwrap());
        assert!(memory_list(&db).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_set_rejects_too_long_body() {
        let (root, db) = fresh_db("mem_toolong");

        // One byte over the cap is rejected with a clear message.
        let huge = "x".repeat(MAX_BODY_LEN + 1);
        let err = memory_set(&db, "project", "huge", &huge).unwrap_err();
        assert!(err.contains("too long"), "got: {err}");
        assert!(memory_list(&db).unwrap().is_empty());

        // A body exactly at the cap is accepted.
        let at_cap = "y".repeat(MAX_BODY_LEN);
        assert!(memory_set(&db, "project", "atcap", &at_cap).is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn memory_set_rejects_plaintext_secret() {
        let (root, db) = fresh_db("mem_secret");

        // A pasted PEM private key is a high-confidence secret and must be refused.
        let body = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc123\n-----END RSA PRIVATE KEY-----";
        let err = memory_set(&db, "security", "leaked-key", body).unwrap_err();
        assert!(
            err.contains("secret") || err.contains("vault"),
            "got: {err}"
        );
        assert!(memory_list(&db).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn valid_mtype_accepts_only_known_categories() {
        for t in MEMORY_TYPES {
            assert!(valid_mtype(t));
        }
        assert!(!valid_mtype("notes"));
        assert!(!valid_mtype(""));
        assert!(!valid_mtype("Project")); // case-sensitive
    }
}
