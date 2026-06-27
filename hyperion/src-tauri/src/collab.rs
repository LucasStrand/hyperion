// Hyperion — in-app pull requests, comment threads + project timeline (M8,
// Requirements #29/#30/#31).
//
// A *pull request* in Hyperion is not a git PR — it is an in-app proposal that
// pairs a human-authored `narrative` (why a change should land) with the
// AI-generated `ai_docs` (what the agent documented about it). Operators argue it
// out in a `pr_comment` thread, then the PR is `merged` or `closed`. Every notable
// project event (a snapshot import, a merge, …) is also appended to the shared
// `timeline` so a project has a single chronological story.
//
// This module is the pure CRUD/logic layer over the `pr`, `pr_comment`, and
// `timeline` tables. It operates on a `&Path` project DB exactly like the
// `context_*` / `memory_*` functions in `projects.rs`, and like them it runs the
// shared `vault::body_has_plaintext_secret` guard on every operator/agent-authored
// text field before it lands in the *unencrypted* project DB — a narrative,
// AI doc, comment, or timeline entry must never be able to smuggle a credential
// into the store. Secrets belong only in the encrypted vault.
//
// foreign_keys enforcement is a per-connection PRAGMA left off project-wide (see
// `context_chunk` in projects.rs), so the `pr` -> `pr_comment` cascade is done
// MANUALLY here, in a transaction, on PR delete.
//
// Strictly local: the project DB is per-machine and git-ignored. No bOS write-back.

use std::path::Path;

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::vault;

/// The three allowed PR lifecycle states. `open` = under discussion (the default),
/// `merged` = accepted, `closed` = rejected/withdrawn. Validated in `pr_set_status`.
pub const PR_STATUSES: [&str; 3] = ["open", "merged", "closed"];

/// Is `status` one of the allowed PR lifecycle states?
pub fn valid_status(status: &str) -> bool {
    PR_STATUSES.contains(&status)
}

/// Upper bound on a PR title (bytes). Generous for a descriptive one-liner but
/// capped so a title can't bloat the list view. Enforced in `pr_create`.
const MAX_TITLE_LEN: usize = 512;

/// Upper bound on a long-form text body (bytes): a PR `narrative`, its `ai_docs`,
/// or a single comment. Generous for a rich write-up but capped so one row can't
/// dominate the project DB. Enforced on every write.
const MAX_TEXT_LEN: usize = 64 * 1024;

/// Upper bound on a comment author label / timeline kind (bytes).
const MAX_LABEL_LEN: usize = 256;

/// Trim a body and reject it if it looks like a plaintext secret. Returns the
/// trimmed text on success. Mirrors the guard in `memory_set` / `context_add`:
/// these fields are persisted unencrypted and later surfaced (and on the cloud
/// runtime can be sent to the model), so a real credential must be refused here.
fn check_secret<'a>(field: &str, text: &'a str) -> Result<&'a str, String> {
    let text = text.trim();
    if vault::body_has_plaintext_secret(text) {
        return Err(format!(
            "this {field} looks like it contains a plaintext secret — store it in the encrypted vault, not in a pull request"
        ));
    }
    Ok(text)
}

// ----------------------------- pull requests -----------------------------

/// Open a new pull request. Requires a non-empty `title` within `MAX_TITLE_LEN`;
/// `narrative` and `ai_docs` are optional long-form text (each within
/// `MAX_TEXT_LEN`) and are secret-scanned. A blank/whitespace-only optional field
/// is stored as SQL NULL. Status starts at `open`. Returns the new row id.
pub fn pr_create(
    db: &Path,
    title: &str,
    narrative: Option<&str>,
    ai_docs: Option<&str>,
) -> Result<i64, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("pull request needs a title".into());
    }
    if title.len() > MAX_TITLE_LEN {
        return Err("pull request title is too long (max 512 bytes)".into());
    }
    if vault::body_has_plaintext_secret(title) {
        return Err("this title looks like it contains a plaintext secret — store it in the encrypted vault, not in a pull request".into());
    }
    let narrative = normalize_optional("narrative", narrative)?;
    let ai_docs = normalize_optional("AI doc", ai_docs)?;
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    conn.execute(
        "INSERT INTO pr(title, narrative, ai_docs, status, created_at)
         VALUES (?1, ?2, ?3, 'open', datetime('now'))",
        rusqlite::params![title, narrative, ai_docs],
    )
    .map_err(|e| format!("insert pr: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Validate, secret-scan, and normalize an optional long-form field: a `None` or a
/// blank/whitespace-only value becomes SQL NULL (`None`); otherwise the trimmed,
/// length-checked, secret-scanned text is returned as `Some`.
fn normalize_optional(field: &str, text: Option<&str>) -> Result<Option<String>, String> {
    match text {
        None => Ok(None),
        Some(raw) => {
            let trimmed = check_secret(field, raw)?;
            if trimmed.is_empty() {
                return Ok(None);
            }
            if trimmed.len() > MAX_TEXT_LEN {
                return Err(format!("pull request {field} is too long (max 64 KB)"));
            }
            Ok(Some(trimmed.to_string()))
        }
    }
}

/// All pull requests for a project as JSON (id, title, status, created_at, and the
/// comment count), newest first. Bodies (narrative / ai_docs) are omitted here —
/// fetched per PR via `pr_get` — so the list stays lean.
pub fn pr_list(db: &Path) -> Result<Vec<Value>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT p.id, p.title, p.status, p.created_at,
                    (SELECT COUNT(*) FROM pr_comment WHERE pr_id = p.id)
             FROM pr p ORDER BY p.id DESC",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "title": r.get::<_, String>(1)?,
                "status": r.get::<_, String>(2)?,
                "created_at": r.get::<_, String>(3)?,
                "comments": r.get::<_, i64>(4)?,
            }))
        })
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

/// Fetch one PR by id as JSON — the full record (id, title, narrative, ai_docs,
/// status, created_at) plus its `comments` thread (id, author, body, created_at)
/// in chronological order — or `Value::Null` when no such PR exists.
pub fn pr_get(db: &Path, id: i64) -> Result<Value, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let pr = conn
        .query_row(
            "SELECT id, title, narrative, ai_docs, status, created_at FROM pr WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "title": r.get::<_, String>(1)?,
                    "narrative": r.get::<_, Option<String>>(2)?,
                    "ai_docs": r.get::<_, Option<String>>(3)?,
                    "status": r.get::<_, String>(4)?,
                    "created_at": r.get::<_, String>(5)?,
                }))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(format!("read pr: {other}")),
        })?;
    let mut pr = match pr {
        Some(v) => v,
        None => return Ok(Value::Null),
    };
    pr["comments"] = Value::Array(pr_comments(&conn, id)?);
    Ok(pr)
}

/// The full comment thread for `pr_id`, in chronological (id-ascending) order.
fn pr_comments(conn: &Connection, pr_id: i64) -> Result<Vec<Value>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id, author, body, created_at FROM pr_comment
             WHERE pr_id = ?1 ORDER BY id",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![pr_id], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "author": r.get::<_, String>(1)?,
                "body": r.get::<_, String>(2)?,
                "created_at": r.get::<_, String>(3)?,
            }))
        })
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

/// Append a comment to a PR's thread. Requires the PR to exist, a non-empty
/// `author` (within `MAX_LABEL_LEN`) and a non-empty `body` (within `MAX_TEXT_LEN`,
/// secret-scanned). Returns the new comment id.
pub fn pr_comment_add(db: &Path, pr_id: i64, author: &str, body: &str) -> Result<i64, String> {
    let author = author.trim();
    if author.is_empty() {
        return Err("comment needs an author".into());
    }
    if author.len() > MAX_LABEL_LEN {
        return Err("comment author is too long (max 256 bytes)".into());
    }
    let body = check_secret("comment", body)?;
    if body.is_empty() {
        return Err("comment body cannot be empty".into());
    }
    if body.len() > MAX_TEXT_LEN {
        return Err("comment body is too long (max 64 KB)".into());
    }
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    // The FK is not enforced (foreign_keys is off project-wide), so guard the
    // parent's existence explicitly rather than silently orphaning a comment.
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM pr WHERE id = ?1",
            rusqlite::params![pr_id],
            |_| Ok(()),
        )
        .map(|_| true)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(false),
            other => Err(format!("{other}")),
        })?;
    if !exists {
        return Err(format!("no such pull request: {pr_id}"));
    }
    conn.execute(
        "INSERT INTO pr_comment(pr_id, author, body, created_at)
         VALUES (?1, ?2, ?3, datetime('now'))",
        rusqlite::params![pr_id, author, body],
    )
    .map_err(|e| format!("insert comment: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// Set a PR's lifecycle status (`open` | `merged` | `closed`). Validates the
/// target status and returns whether the PR existed (so a no-op on a missing id is
/// reported, not silently swallowed).
pub fn pr_set_status(db: &Path, id: i64, status: &str) -> Result<bool, String> {
    if !valid_status(status) {
        return Err(format!(
            "invalid pull request status '{status}' (expected one of open|merged|closed)"
        ));
    }
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let n = conn
        .execute(
            "UPDATE pr SET status = ?1 WHERE id = ?2",
            rusqlite::params![status, id],
        )
        .map_err(|e| format!("update pr status: {e}"))?;
    Ok(n > 0)
}

/// Delete a PR and its entire comment thread. foreign_keys is off project-wide, so
/// the `pr_comment` rows are cascaded MANUALLY in a transaction (comments first,
/// then the PR row). Returns whether the PR existed.
pub fn pr_delete(db: &Path, id: i64) -> Result<bool, String> {
    let mut conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let tx = conn.transaction().map_err(|e| format!("begin tx: {e}"))?;
    tx.execute(
        "DELETE FROM pr_comment WHERE pr_id = ?1",
        rusqlite::params![id],
    )
    .map_err(|e| format!("delete comments: {e}"))?;
    let n = tx
        .execute("DELETE FROM pr WHERE id = ?1", rusqlite::params![id])
        .map_err(|e| format!("delete pr: {e}"))?;
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(n > 0)
}

// ----------------------------- project timeline -----------------------------

/// Append an event to the project timeline. Requires a non-empty `kind` (within
/// `MAX_LABEL_LEN`) and `summary` (within `MAX_TEXT_LEN`); `detail` is optional
/// long-form text. Both `summary` and `detail` are secret-scanned, since timeline
/// entries persist unencrypted and are surfaced in the UI. Returns the new row id.
pub fn timeline_add(
    db: &Path,
    kind: &str,
    summary: &str,
    detail: Option<&str>,
) -> Result<i64, String> {
    let kind = kind.trim();
    if kind.is_empty() {
        return Err("timeline event needs a kind".into());
    }
    if kind.len() > MAX_LABEL_LEN {
        return Err("timeline kind is too long (max 256 bytes)".into());
    }
    let summary = check_secret("summary", summary)?;
    if summary.is_empty() {
        return Err("timeline event needs a summary".into());
    }
    if summary.len() > MAX_TEXT_LEN {
        return Err("timeline summary is too long (max 64 KB)".into());
    }
    let detail = normalize_optional("detail", detail)?;
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    conn.execute(
        "INSERT INTO timeline(kind, summary, detail, created_at)
         VALUES (?1, ?2, ?3, datetime('now'))",
        rusqlite::params![kind, summary, detail],
    )
    .map_err(|e| format!("insert timeline event: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// The project timeline as JSON (id, kind, summary, detail, created_at), newest
/// first. An optional `limit` caps the number of returned events (most recent);
/// `None` returns the whole timeline.
pub fn timeline_list(db: &Path, limit: Option<i64>) -> Result<Vec<Value>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    // A negative LIMIT means "no limit" in SQLite, so map None -> -1.
    let lim = limit.filter(|n| *n >= 0).unwrap_or(-1);
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, summary, detail, created_at FROM timeline
             ORDER BY id DESC LIMIT ?1",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![lim], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "kind": r.get::<_, String>(1)?,
                "summary": r.get::<_, String>(2)?,
                "detail": r.get::<_, Option<String>>(3)?,
                "created_at": r.get::<_, String>(4)?,
            }))
        })
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
    use std::path::PathBuf;

    /// A fresh, isolated projects root for one test (cleaned up by the caller).
    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hyperion_collab_test_{}_{}",
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
        let summary = crate::projects::create(&root, "Test Project").unwrap();
        let id = summary.get("id").unwrap().as_str().unwrap().to_string();
        let db = root.join(&id).join("project.db");
        (root, db)
    }

    #[test]
    fn pr_create_list_get_roundtrip() {
        let (root, db) = fresh_db("pr_rt");

        // No PRs yet: list is empty and get on a missing id is JSON null.
        assert!(pr_list(&db).unwrap().is_empty());
        assert_eq!(pr_get(&db, 1).unwrap(), Value::Null);

        // Open a PR with a narrative + AI docs.
        let id = pr_create(
            &db,
            "  Add lobby scene  ",
            Some("We want a one-touch lobby scene."),
            Some("Agent: wires KNX 1.1 to the scene actor."),
        )
        .unwrap();
        assert!(id > 0);

        // List shows it (title trimmed, status open, zero comments), no bodies.
        let list = pr_list(&db).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["title"], "Add lobby scene");
        assert_eq!(list[0]["status"], "open");
        assert_eq!(list[0]["comments"], 0);
        assert!(list[0].get("narrative").is_none());

        // Get returns the full record with an empty comment thread.
        let pr = pr_get(&db, id).unwrap();
        assert_eq!(pr["title"], "Add lobby scene");
        assert_eq!(pr["narrative"], "We want a one-touch lobby scene.");
        assert_eq!(pr["ai_docs"], "Agent: wires KNX 1.1 to the scene actor.");
        assert_eq!(pr["status"], "open");
        assert_eq!(pr["comments"], json!([]));

        // A whitespace-only optional field is stored as NULL.
        let id2 = pr_create(&db, "Empty bodies", Some("   "), None).unwrap();
        let pr2 = pr_get(&db, id2).unwrap();
        assert_eq!(pr2["narrative"], Value::Null);
        assert_eq!(pr2["ai_docs"], Value::Null);

        // Newest first ordering.
        let list = pr_list(&db).unwrap();
        assert_eq!(list[0]["id"], id2);
        assert_eq!(list[1]["id"], id);

        // An empty title is rejected.
        assert!(pr_create(&db, "   ", None, None).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pr_comment_thread_add_and_retrieve() {
        let (root, db) = fresh_db("pr_comments");
        let id = pr_create(&db, "Tune AHU schedule", None, None).unwrap();

        // Adding to a missing PR is rejected (FK enforcement is off, so we guard it).
        let err = pr_comment_add(&db, 9999, "alice", "hi").unwrap_err();
        assert!(err.contains("no such pull request"), "got: {err}");

        // Append a two-message argue thread.
        let c1 = pr_comment_add(&db, id, "alice", "Should we precool at 6am?").unwrap();
        let c2 = pr_comment_add(&db, id, "bob", "Yes — outdoor temp peaks by 9.").unwrap();
        assert!(c2 > c1);

        // Thread is returned in chronological order, embedded in pr_get.
        let pr = pr_get(&db, id).unwrap();
        let comments = pr["comments"].as_array().unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0]["author"], "alice");
        assert_eq!(comments[0]["body"], "Should we precool at 6am?");
        assert_eq!(comments[1]["author"], "bob");

        // The list view reflects the comment count.
        assert_eq!(pr_list(&db).unwrap()[0]["comments"], 2);

        // Empty author / empty body are rejected.
        assert!(pr_comment_add(&db, id, "  ", "hi").is_err());
        assert!(pr_comment_add(&db, id, "alice", "   ").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pr_status_transitions_and_delete_cascade() {
        let (root, db) = fresh_db("pr_status");
        let id = pr_create(&db, "Replace pump driver", None, None).unwrap();
        pr_comment_add(&db, id, "alice", "LGTM").unwrap();

        // Default status is open; valid transitions stick.
        assert_eq!(pr_get(&db, id).unwrap()["status"], "open");
        assert!(pr_set_status(&db, id, "merged").unwrap());
        assert_eq!(pr_get(&db, id).unwrap()["status"], "merged");
        assert!(pr_set_status(&db, id, "closed").unwrap());
        assert_eq!(pr_get(&db, id).unwrap()["status"], "closed");

        // An unknown status is rejected; a status on a missing PR is reported false.
        assert!(pr_set_status(&db, id, "bogus").is_err());
        assert!(!pr_set_status(&db, 9999, "open").unwrap());

        // Delete cascades the comment thread (foreign_keys is off, cascaded manually).
        assert!(pr_delete(&db, id).unwrap());
        assert!(!pr_delete(&db, id).unwrap()); // idempotent
        assert_eq!(pr_get(&db, id).unwrap(), Value::Null);
        let conn = Connection::open(&db).unwrap();
        let orphans: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pr_comment WHERE pr_id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn valid_status_accepts_only_known_states() {
        for s in PR_STATUSES {
            assert!(valid_status(s));
        }
        assert!(!valid_status("draft"));
        assert!(!valid_status(""));
        assert!(!valid_status("Open")); // case-sensitive
    }

    #[test]
    fn timeline_add_and_list() {
        let (root, db) = fresh_db("timeline_rt");

        // Empty to start.
        assert!(timeline_list(&db, None).unwrap().is_empty());

        let t1 = timeline_add(&db, "snapshot", "Imported home.bos", Some("412 nodes")).unwrap();
        let t2 = timeline_add(&db, "pr", "Merged 'Add lobby scene'", None).unwrap();
        assert!(t2 > t1);

        // Newest first; detail round-trips (and NULL stays null).
        let list = timeline_list(&db, None).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["kind"], "pr");
        assert_eq!(list[0]["summary"], "Merged 'Add lobby scene'");
        assert_eq!(list[0]["detail"], Value::Null);
        assert_eq!(list[1]["kind"], "snapshot");
        assert_eq!(list[1]["detail"], "412 nodes");

        // limit caps to the most recent N.
        let one = timeline_list(&db, Some(1)).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0]["id"], t2);

        // Empty kind / empty summary are rejected.
        assert!(timeline_add(&db, "  ", "x", None).is_err());
        assert!(timeline_add(&db, "k", "   ", None).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_plaintext_secret_in_bodies() {
        let (root, db) = fresh_db("collab_secret");
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc123\n-----END RSA PRIVATE KEY-----";

        // A secret in the narrative, AI docs, a comment, or a timeline entry is
        // refused before it can land in the unencrypted project DB.
        let err = pr_create(&db, "Leak", Some(pem), None).unwrap_err();
        assert!(
            err.contains("secret") || err.contains("vault"),
            "got: {err}"
        );
        assert!(pr_create(&db, "Leak", None, Some(pem)).is_err());
        assert!(pr_list(&db).unwrap().is_empty());

        let id = pr_create(&db, "Clean PR", None, None).unwrap();
        let err = pr_comment_add(&db, id, "alice", pem).unwrap_err();
        assert!(
            err.contains("secret") || err.contains("vault"),
            "got: {err}"
        );
        assert!(pr_get(&db, id).unwrap()["comments"]
            .as_array()
            .unwrap()
            .is_empty());

        assert!(timeline_add(&db, "note", pem, None).is_err());
        assert!(timeline_add(&db, "note", "ok", Some(pem)).is_err());
        assert!(timeline_list(&db, None).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }
}
