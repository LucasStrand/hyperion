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

use crate::embed;
use crate::ingest;
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
         -- One row per file name. context_file is first populated in M1, so no real
         -- DB can hold a duplicate name to violate this; it makes the name-dedup in
         -- context_add a DB-enforced invariant (mirrors memory_slug_uq) rather than a
         -- purely application-level one.
         CREATE UNIQUE INDEX IF NOT EXISTS context_file_name_uq ON context_file(name);
         -- Extracted, chunked text of an ingested context file (M1). One row per
         -- chunk, ordered by `ord`; deleted with its file. The FK declares the
         -- ownership (ON DELETE CASCADE) so the relationship is explicit in the
         -- schema; context_add/context_delete still cascade manually in a transaction
         -- (foreign_keys enforcement is a per-connection PRAGMA, left off for now).
         -- Created here (IF NOT EXISTS) so older project DBs self-heal on open.
         CREATE TABLE IF NOT EXISTS context_chunk (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             file_id INTEGER NOT NULL REFERENCES context_file(id) ON DELETE CASCADE,
             ord INTEGER NOT NULL, text TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS context_chunk_file ON context_chunk(file_id);
         -- Optional embedding vector for a chunk (M2 RAG upgrade). One row per
         -- chunk (chunk_id PRIMARY KEY), storing the source `model` + `dim` so a
         -- later model/dimension change is detectable and stale-dimension rows can
         -- be ignored (cosine returns 0.0 on a dim mismatch). `vec` is a
         -- little-endian f32 blob. Additive + IF NOT EXISTS, so older project DBs
         -- self-heal on open() with zero data loss; embeddings stay best-effort,
         -- and an empty table simply means retrieval uses the keyword ranker.
         -- foreign_keys enforcement is off (see context_chunk), so context_add /
         -- context_delete cascade these rows MANUALLY, before their chunks.
         CREATE TABLE IF NOT EXISTS context_embedding (
             chunk_id INTEGER PRIMARY KEY REFERENCES context_chunk(id) ON DELETE CASCADE,
             model TEXT NOT NULL, dim INTEGER NOT NULL, vec BLOB NOT NULL
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
         -- Versioned, append-only per-agent instinct overrides (M5). One row per
         -- (agent_id, version); the built-in role instincts are the version-0
         -- baseline (in-binary, not stored), and each operator save appends a new
         -- version. A revert copies an old body forward as a new version, so
         -- history is never destroyed. Created here (IF NOT EXISTS) so older
         -- project DBs self-heal on open; empty until an agent is customized.
         CREATE TABLE IF NOT EXISTS agent_instincts (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             agent_id TEXT NOT NULL, version INTEGER NOT NULL,
             body TEXT NOT NULL, updated_at TEXT NOT NULL
         );
         CREATE UNIQUE INDEX IF NOT EXISTS agent_instincts_ver_uq
             ON agent_instincts(agent_id, version);
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
    // here would leak into context. The shared `body_has_plaintext_secret` guard
    // rejects the high-confidence structural shapes (PEM key, AWS key, bearer token)
    // *and* a bare vendor-prefixed key (e.g. the app's own `sk-or-…`), while the
    // looser `credential_assignment` heuristic — which false-positives on notes like
    // "token bucket: ..." — is intentionally excluded. Secrets belong in the vault.
    if vault::body_has_plaintext_secret(body) {
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

// ----------------------------- context files (M1) -----------------------------
//
// Ingested reference material (a datasheet, a Milesight CSV export) lives in
// `context_file` (the original extracted text) + `context_chunk` (its retrievable
// pieces). On every ask the agent retrieves the few chunks most relevant to the
// question and the caller fences them as UNTRUSTED data — file content can carry
// injected instructions, so it is treated exactly like the `.bos` grounding.
// Strictly local; never written back to bOS.

/// Ingest a file's bytes into the active project: extract text, chunk it, and store
/// the file + chunks. Re-adding the same `name` replaces the previous copy (and its
/// chunks). Returns `{id, name, kind, chunks}`. Validation (kind/size/empty) lives
/// in `ingest::extract_text`. All writes happen in one transaction.
pub fn context_add(db: &Path, name: &str, bytes: &[u8]) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("file has no name".into());
    }
    let kind = ingest::detect_kind(name);
    let text = ingest::extract_text(name, bytes)?;
    // Plaintext-secret guardrail (mirrors memory_set and roster instinct writes):
    // ingested text is stored in the *unencrypted* project DB and later spliced into
    // the agent prompt — and on the cloud runtime, sent to OpenRouter. A config file
    // (.yaml/.json/.ini/.conf) carrying an embedded credential would otherwise leak
    // silently. The shared `body_has_plaintext_secret` guard rejects the high-
    // confidence shapes (PEM key, AWS key, bearer token, bare vendor `sk-or-…` keys).
    // Secrets belong in the encrypted vault, never in a context file.
    if vault::body_has_plaintext_secret(&text) {
        return Err(
            "this file appears to contain a plaintext secret — store credentials in the encrypted vault, not in a context file".into(),
        );
    }
    let chunks = ingest::chunk(&text);
    if chunks.is_empty() {
        return Err("no readable text to index".into());
    }
    let mut conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
    // Replace any prior file with this name (and its chunks) so re-uploads refresh.
    {
        let stale: Vec<i64> = {
            let mut stmt = tx
                .prepare("SELECT id FROM context_file WHERE name = ?1")
                .map_err(|e| format!("{e}"))?;
            let ids = stmt
                .query_map(rusqlite::params![name], |r| r.get::<_, i64>(0))
                .map_err(|e| format!("{e}"))?;
            let mut v = Vec::new();
            for id in ids {
                v.push(id.map_err(|e| format!("{e}"))?);
            }
            v
        };
        for id in stale {
            // foreign_keys is off, so cascade manually: embeddings first (they FK
            // the chunks), then the chunks, then the file row.
            tx.execute(
                "DELETE FROM context_embedding WHERE chunk_id IN
                     (SELECT id FROM context_chunk WHERE file_id = ?1)",
                rusqlite::params![id],
            )
            .map_err(|e| format!("{e}"))?;
            tx.execute(
                "DELETE FROM context_chunk WHERE file_id = ?1",
                rusqlite::params![id],
            )
            .map_err(|e| format!("{e}"))?;
            tx.execute(
                "DELETE FROM context_file WHERE id = ?1",
                rusqlite::params![id],
            )
            .map_err(|e| format!("{e}"))?;
        }
    }
    tx.execute(
        "INSERT INTO context_file(name, kind, added_at, content)
         VALUES (?1, ?2, datetime('now'), ?3)",
        rusqlite::params![name, kind, text.as_bytes()],
    )
    .map_err(|e| format!("insert context_file: {e}"))?;
    let file_id = tx.last_insert_rowid();
    {
        let mut stmt = tx
            .prepare("INSERT INTO context_chunk(file_id, ord, text) VALUES (?1, ?2, ?3)")
            .map_err(|e| format!("{e}"))?;
        for (i, c) in chunks.iter().enumerate() {
            stmt.execute(rusqlite::params![file_id, i as i64, c])
                .map_err(|e| format!("insert chunk: {e}"))?;
        }
    }
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    // Best-effort embeddings (M2). Done AFTER commit so a network/API failure can
    // never roll back a successful ingest; on any error we simply leave the
    // context_embedding table without rows for this file and retrieval falls back
    // to the keyword ranker. The chunk-insert loop above does not capture rowids,
    // so re-query them in `ord` order to align with the embedding vectors.
    embed_file_chunks(&mut conn, file_id);
    Ok(json!({
        "id": file_id,
        "name": name,
        "kind": kind,
        "chunks": chunks.len(),
    }))
}

/// Best-effort: embed the chunks of `file_id` and store the vectors in
/// `context_embedding`. Any failure (no API key, network error, malformed
/// response) is swallowed — embeddings are optional and retrieval falls back to
/// keyword scoring. Never propagates an error to the ingest path.
fn embed_file_chunks(conn: &mut Connection, file_id: i64) {
    // Re-query (id, text) in ord order; the insert loop didn't capture rowids.
    let rows: Vec<(i64, String)> = {
        let mut stmt = match conn
            .prepare("SELECT id, text FROM context_chunk WHERE file_id = ?1 ORDER BY ord")
        {
            Ok(s) => s,
            Err(_) => return,
        };
        let mapped = match stmt.query_map(rusqlite::params![file_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(m) => m,
            Err(_) => return,
        };
        let mut v = Vec::new();
        for row in mapped {
            match row {
                Ok(t) => v.push(t),
                Err(_) => return,
            }
        }
        v
    };
    if rows.is_empty() {
        return;
    }
    let texts: Vec<&str> = rows.iter().map(|(_, t)| t.as_str()).collect();
    let (model, vectors) = match embed::embed(&texts) {
        Ok(out) => out,
        Err(_) => return, // not configured / network error -> keyword fallback
    };
    if vectors.len() != rows.len() {
        return;
    }
    // Store vectors in their own short transaction.
    let tx = match conn.transaction() {
        Ok(t) => t,
        Err(_) => return,
    };
    {
        let mut stmt = match tx.prepare(
            "INSERT OR REPLACE INTO context_embedding(chunk_id, model, dim, vec)
             VALUES (?1, ?2, ?3, ?4)",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        for ((chunk_id, _), vec) in rows.iter().zip(vectors.iter()) {
            let blob = embed::vec_to_blob(vec);
            if stmt
                .execute(rusqlite::params![chunk_id, model, vec.len() as i64, blob])
                .is_err()
            {
                return; // tx dropped (rolled back) on early return
            }
        }
    }
    let _ = tx.commit();
}

/// All ingested context files (id, name, kind, added_at, byte size, chunk count),
/// newest first. Never returns the file content itself.
pub fn context_list(db: &Path) -> Result<Vec<Value>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT cf.id, cf.name, cf.kind, cf.added_at, LENGTH(cf.content),
                    (SELECT COUNT(*) FROM context_chunk WHERE file_id = cf.id)
             FROM context_file cf ORDER BY cf.added_at DESC, cf.id DESC",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "name": r.get::<_, String>(1)?,
                "kind": r.get::<_, Option<String>>(2)?,
                "added_at": r.get::<_, String>(3)?,
                "bytes": r.get::<_, i64>(4)?,
                "chunks": r.get::<_, i64>(5)?,
            }))
        })
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

/// Delete an ingested context file (and its chunks) by id. Returns whether it existed.
pub fn context_delete(db: &Path, id: i64) -> Result<bool, String> {
    let mut conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
    // foreign_keys is off, so cascade manually: embeddings first (they FK the
    // chunks), then the chunks, then the file row.
    tx.execute(
        "DELETE FROM context_embedding WHERE chunk_id IN
             (SELECT id FROM context_chunk WHERE file_id = ?1)",
        rusqlite::params![id],
    )
    .map_err(|e| format!("{e}"))?;
    tx.execute(
        "DELETE FROM context_chunk WHERE file_id = ?1",
        rusqlite::params![id],
    )
    .map_err(|e| format!("{e}"))?;
    let n = tx
        .execute(
            "DELETE FROM context_file WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| format!("{e}"))?;
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(n > 0)
}

/// Retrieve up to `k` chunks most relevant to `query`, as `(file_name, chunk_text)`.
/// When the project has stored embeddings AND the query can be embedded, chunks are
/// ranked by cosine similarity (highest first); otherwise this falls back to the
/// dependency-free keyword-overlap ranker. Reads every chunk — fine at project scale
/// (a handful of files) and avoids an FTS dependency for now. The signature/return
/// type is intentionally stable: callers (lib.rs build_context_block) are untouched,
/// and the chosen chunks are still UNTRUSTED data, fenced by the caller.
pub fn context_retrieve(db: &Path, query: &str, k: usize) -> Result<Vec<(String, String)>, String> {
    // Embedding branch (best-effort): only when the project actually has vectors
    // AND the query embeds. Any failure falls through to the keyword path below.
    if let Some(hits) = context_retrieve_embedding(db, query, k) {
        return Ok(hits);
    }

    let terms = ingest::keywords(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT cf.name, cc.text FROM context_chunk cc
             JOIN context_file cf ON cf.id = cc.file_id",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| format!("{e}"))?;
    let mut scored: Vec<(usize, String, String)> = Vec::new();
    for row in rows {
        let (name, text) = row.map_err(|e| format!("{e}"))?;
        let s = ingest::score(&terms, &text);
        if s > 0 {
            scored.push((s, name, text));
        }
    }
    // Highest score first; stable sort keeps DB order for equal scores.
    scored.sort_by_key(|t| std::cmp::Reverse(t.0));
    Ok(scored
        .into_iter()
        .take(k)
        .map(|(_, name, text)| (name, text))
        .collect())
}

/// Embedding-based retrieval. Returns `Some(top-k)` ranked by cosine similarity
/// when the project has stored vectors AND the query embeds successfully; returns
/// `None` (so the caller falls back to keyword scoring) when there are no
/// embeddings, the embedding client is unconfigured/unreachable, or anything else
/// goes wrong. Never errors — embeddings are strictly optional.
fn context_retrieve_embedding(db: &Path, query: &str, k: usize) -> Option<Vec<(String, String)>> {
    let conn = Connection::open(db).ok()?;
    // Cheap gate: skip the network round-trip entirely if nothing is embedded.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM context_embedding", [], |r| r.get(0))
        .ok()?;
    if count == 0 {
        return None;
    }
    // Embed the query (single text). Any Err -> fall back to keyword.
    let (_model, mut qvecs) = embed::embed(&[query]).ok()?;
    let qvec = qvecs.pop()?;
    if qvec.is_empty() {
        return None;
    }
    rank_chunks_by_cosine(&conn, &qvec, k)
}

/// Pure ranker: score every stored embedding against `qvec` by cosine and return
/// `Some(top-k)` `(file_name, chunk_text)`. Rows whose blob can't be decoded or
/// whose dimension mismatches the query are skipped (so a stale-model row after an
/// env change is ignored rather than mis-ranked). Returns `None` if nothing scored
/// (caller falls back to keyword) or on any DB error. No network — separated from
/// the query-embedding step above so it is unit-testable with synthetic vectors.
fn rank_chunks_by_cosine(
    conn: &Connection,
    qvec: &[f32],
    k: usize,
) -> Option<Vec<(String, String)>> {
    let mut stmt = conn
        .prepare(
            "SELECT cf.name, cc.text, ce.vec FROM context_embedding ce
             JOIN context_chunk cc ON cc.id = ce.chunk_id
             JOIN context_file cf ON cf.id = cc.file_id",
        )
        .ok()?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })
        .ok()?;
    let mut scored: Vec<(f32, String, String)> = Vec::new();
    for row in rows {
        // Skip only the offending row on a read error — never discard every
        // result (which would silently fall back to keyword for the whole query).
        let Ok((name, text, blob)) = row else {
            continue;
        };
        let v = match embed::blob_to_vec(&blob) {
            Some(v) if v.len() == qvec.len() => v,
            _ => continue,
        };
        let s = embed::cosine(qvec, &v);
        if s > 0.0 {
            scored.push((s, name, text));
        }
    }
    if scored.is_empty() {
        // Embeddings exist but nothing scored (e.g. all dim-mismatched after a
        // model change). Fall back to keyword scoring rather than return empty.
        return None;
    }
    // Highest cosine first; total_cmp is panic-free on any float (incl. NaN).
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    Some(
        scored
            .into_iter()
            .take(k)
            .map(|(_, name, text)| (name, text))
            .collect(),
    )
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

    #[test]
    fn context_add_list_retrieve_delete_roundtrip() {
        let (root, db) = fresh_db("ctx_rt");

        // Ingest a datasheet-like CSV; it should produce at least one chunk.
        let body = b"device,bus,slave\nBelimo LR24A actuator,Modbus,7\nlobby scene,KNX,1.1\n";
        let added = context_add(&db, "belimo.csv", body).unwrap();
        assert_eq!(added["name"], "belimo.csv");
        assert_eq!(added["kind"], "csv");
        assert!(added["chunks"].as_i64().unwrap() >= 1);

        // It shows up in the list with a byte size and chunk count, no content.
        let list = context_list(&db).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "belimo.csv");
        assert!(list[0].get("content").is_none());
        assert!(list[0]["chunks"].as_i64().unwrap() >= 1);

        // Retrieval finds the chunk mentioning the queried terms.
        let hits =
            context_retrieve(&db, "what modbus slave is the belimo actuator on?", 4).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, "belimo.csv");
        assert!(hits[0].1.contains("Belimo"));
        // A query with no shared keywords retrieves nothing.
        assert!(context_retrieve(&db, "zzz qqq", 4).unwrap().is_empty());

        // Re-adding the same name replaces (no duplicate row).
        context_add(&db, "belimo.csv", b"device,bus\nupdated,now\n").unwrap();
        assert_eq!(context_list(&db).unwrap().len(), 1);

        // Unsupported kind is rejected.
        assert!(context_add(&db, "scan.pdf", b"%PDF-1.7").is_err());

        // Delete removes the file and its chunks; second delete is a no-op.
        let id = context_list(&db).unwrap()[0]["id"].as_i64().unwrap();
        assert!(context_delete(&db, id).unwrap());
        assert!(!context_delete(&db, id).unwrap());
        assert!(context_list(&db).unwrap().is_empty());
        assert!(context_retrieve(&db, "belimo", 4).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Insert a context_file + one chunk per `texts` entry, returning the chunk ids
    /// in order. Bypasses ingest/embeddings so a test can wire synthetic vectors
    /// directly (no network).
    fn seed_chunks(db: &Path, file_name: &str, texts: &[&str]) -> Vec<i64> {
        let conn = Connection::open(db).unwrap();
        conn.execute(
            "INSERT INTO context_file(name, kind, added_at, content)
             VALUES (?1, 'csv', datetime('now'), ?2)",
            rusqlite::params![file_name, file_name.as_bytes()],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();
        let mut ids = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            conn.execute(
                "INSERT INTO context_chunk(file_id, ord, text) VALUES (?1, ?2, ?3)",
                rusqlite::params![file_id, i as i64, t],
            )
            .unwrap();
            ids.push(conn.last_insert_rowid());
        }
        ids
    }

    fn insert_embedding(db: &Path, chunk_id: i64, vec: &[f32]) {
        let conn = Connection::open(db).unwrap();
        conn.execute(
            "INSERT INTO context_embedding(chunk_id, model, dim, vec) VALUES (?1, 'test', ?2, ?3)",
            rusqlite::params![chunk_id, vec.len() as i64, embed::vec_to_blob(vec)],
        )
        .unwrap();
    }

    #[test]
    fn context_retrieve_falls_back_to_keyword_when_no_embeddings() {
        let (root, db) = fresh_db("ctx_kw_fallback");
        // No embeddings stored -> the cheap COUNT gate returns None and the keyword
        // ranker runs. (CI has no embedding key, so this is the real default path.)
        let _ = seed_chunks(
            &db,
            "devices.csv",
            &[
                "Belimo LR24A actuator on Modbus slave 7",
                "lobby KNX scene 1.1",
            ],
        );
        let hits = context_retrieve(&db, "which modbus slave is the belimo actuator", 4).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].1.contains("Belimo"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rank_chunks_by_cosine_orders_by_similarity() {
        let (root, db) = fresh_db("ctx_cosine_rank");
        // Three chunks with hand-picked orthogonal-ish vectors (no network).
        let ids = seed_chunks(
            &db,
            "doc.txt",
            &["alpha chunk", "beta chunk", "gamma chunk"],
        );
        insert_embedding(&db, ids[0], &[1.0, 0.0, 0.0]);
        insert_embedding(&db, ids[1], &[0.0, 1.0, 0.0]);
        insert_embedding(&db, ids[2], &[0.9, 0.1, 0.0]); // close to alpha

        let conn = Connection::open(&db).unwrap();
        // Query vector points along alpha; alpha should rank first, then gamma
        // (0.9,0.1), then beta (orthogonal, cosine 0 -> dropped by the >0 filter).
        let qvec = [1.0f32, 0.0, 0.0];
        let hits = rank_chunks_by_cosine(&conn, &qvec, 4).unwrap();
        assert_eq!(hits[0].1, "alpha chunk");
        assert_eq!(hits[1].1, "gamma chunk");
        // beta is orthogonal (cosine 0) and filtered out.
        assert_eq!(hits.len(), 2);

        // k caps the result count.
        assert_eq!(rank_chunks_by_cosine(&conn, &qvec, 1).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rank_chunks_by_cosine_skips_dim_mismatch() {
        let (root, db) = fresh_db("ctx_cosine_dim");
        let ids = seed_chunks(&db, "doc.txt", &["good chunk", "stale chunk"]);
        insert_embedding(&db, ids[0], &[1.0, 0.0, 0.0]); // matches query dim 3
        insert_embedding(&db, ids[1], &[1.0, 0.0]); // stale 2-dim row -> skipped

        let conn = Connection::open(&db).unwrap();
        let qvec = [1.0f32, 0.0, 0.0];
        let hits = rank_chunks_by_cosine(&conn, &qvec, 4).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "good chunk");

        // If EVERY row mismatches the query dim, nothing scores -> None (caller
        // would then fall back to keyword).
        let q4 = [1.0f32, 0.0, 0.0, 0.0];
        assert!(rank_chunks_by_cosine(&conn, &q4, 4).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn context_delete_removes_embeddings() {
        let (root, db) = fresh_db("ctx_del_emb");
        let ids = seed_chunks(&db, "doc.txt", &["a", "b"]);
        insert_embedding(&db, ids[0], &[1.0, 0.0]);
        insert_embedding(&db, ids[1], &[0.0, 1.0]);
        let conn = Connection::open(&db).unwrap();
        let file_id: i64 = conn
            .query_row(
                "SELECT id FROM context_file WHERE name = 'doc.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(conn);
        assert!(context_delete(&db, file_id).unwrap());
        // No orphan embedding rows remain.
        let conn = Connection::open(&db).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM context_embedding", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn context_add_rejects_plaintext_secret() {
        let (root, db) = fresh_db("ctx_secret");
        // A config file carrying a bare OpenRouter key must be refused *before* it can
        // land in the unencrypted project DB or be spliced into the model prompt.
        let secret = b"{\n  \"openrouter_key\": \"sk-or-v1-abc123def456ghi789jkl012mno345\"\n}\n";
        let err = context_add(&db, "creds.json", secret).unwrap_err();
        assert!(err.contains("plaintext secret"), "got: {err}");
        // Nothing was stored.
        assert!(context_list(&db).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }
}
