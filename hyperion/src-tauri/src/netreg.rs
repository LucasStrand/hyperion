// Hyperion — network address + login registry (Phase 1, M6, Requirement #14).
//
// A vault-backed registry of the building network's devices and logins. Each entry
// (`net_entry` row) records a human label, a network address, an optional username
// and notes, and an OPAQUE `secret_cipher` blob. This module is the pure CRUD layer
// over that table — it operates on a `&Path` project DB exactly like the `context_*`
// / `memory_*` functions in `projects.rs`, and it is deliberately *vault-agnostic*:
//
//   * `secret_cipher` is an already-sealed blob handed in by the caller. netreg never
//     encrypts, decrypts, or even interprets it — it just stores it and hands it back.
//     The `net_*` commands in `lib.rs` are responsible for sealing the plaintext secret
//     with the unlocked vault before `add`, and for unsealing it after `get`.
//   * The *clear* columns (label / address / username / notes) are secret-scanned on
//     write with the shared `vault::body_has_plaintext_secret` guard, so a credential
//     pasted into one of them is refused — a secret may only ever reach the DB through
//     the encrypted `secret_cipher` blob, never in the clear.
//
// Strictly local: the project DB is per-machine and git-ignored. No bOS write-back.

use std::path::Path;

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::vault;

/// Upper bound on a clear text field (label / address / username / notes), in bytes.
/// Generous for a descriptive label or a multi-line note, but capped so a single
/// entry can't bloat the project DB. Enforced at write time in `add`.
const MAX_FIELD_LEN: usize = 4096;

/// One fully-resolved registry row, including the opaque `secret_cipher` blob. Used
/// by `get` so a command can unseal the secret; `list` never exposes the blob.
pub struct NetEntry {
    pub id: i64,
    pub label: String,
    pub address: String,
    pub username: Option<String>,
    /// The sealed secret blob, or `None` when the entry has no stored secret. Opaque
    /// to this module — the caller (lib.rs) unseals it with the vault.
    pub secret_cipher: Option<Vec<u8>>,
    pub notes: Option<String>,
    pub updated_at: String,
}

/// Validate the *clear* fields of an entry: a non-empty label and address (trimmed),
/// each field within `MAX_FIELD_LEN`, and none of label/address/username/notes looking
/// like a plaintext secret. Shared by `add`; pulled out so the rule lives in one place.
fn validate_clear_fields(
    label: &str,
    address: &str,
    username: Option<&str>,
    notes: Option<&str>,
) -> Result<(), String> {
    if label.is_empty() {
        return Err("network entry needs a label".into());
    }
    if address.is_empty() {
        return Err("network entry needs an address".into());
    }
    // Plaintext-secret guardrail (mirrors memory_set / wiki_save / context_add): the
    // clear columns are stored unencrypted, so a real credential pasted into any of
    // them would leak. The secret belongs in the sealed `secret_cipher` blob, never
    // here. Scan every clear field with the shared high-confidence guard.
    for (name, value) in [
        ("label", Some(label)),
        ("address", Some(address)),
        ("username", username),
        ("notes", notes),
    ] {
        if let Some(v) = value {
            if v.len() > MAX_FIELD_LEN {
                return Err(format!("network entry {name} is too long (max 4 KB)"));
            }
            if vault::body_has_plaintext_secret(v) {
                return Err(format!(
                    "the {name} field looks like it contains a plaintext secret — store the secret in the encrypted vault, not in the clear"
                ));
            }
        }
    }
    Ok(())
}

/// Normalize an optional clear field: trim it and treat an all-whitespace value as
/// absent, so the UI sending `Some("")` is stored as SQL NULL (not an empty string).
fn norm_opt(v: Option<&str>) -> Option<String> {
    v.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Insert a new registry entry. `label`/`address` are trimmed and required; `username`
/// and `notes` are trimmed and optional; `secret_cipher` is the already-sealed secret
/// blob (or `None`). Validates the clear fields and stamps `updated_at = datetime('now')`.
/// Returns the new row id.
pub fn add(
    db: &Path,
    label: &str,
    address: &str,
    username: Option<&str>,
    secret_cipher: Option<&[u8]>,
    notes: Option<&str>,
) -> Result<i64, String> {
    let label = label.trim();
    let address = address.trim();
    let username = norm_opt(username);
    let notes = norm_opt(notes);
    validate_clear_fields(label, address, username.as_deref(), notes.as_deref())?;
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    conn.execute(
        "INSERT INTO net_entry(label, address, username, secret_cipher, notes, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
        rusqlite::params![label, address, username, secret_cipher, notes],
    )
    .map_err(|e| format!("insert net_entry: {e}"))?;
    Ok(conn.last_insert_rowid())
}

/// All registry entries as JSON (id, label, address, username, has_secret, notes,
/// updated_at), ordered by label then id for a stable picker. The `secret_cipher`
/// blob is deliberately omitted — `has_secret` reports only whether one is stored, so
/// the list view can never leak (even sealed) secret material. Use `get` to unseal one.
pub fn list(db: &Path) -> Result<Vec<Value>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT id, label, address, username, notes, updated_at,
                    (secret_cipher IS NOT NULL)
             FROM net_entry ORDER BY label, id",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "label": r.get::<_, String>(1)?,
                "address": r.get::<_, String>(2)?,
                "username": r.get::<_, Option<String>>(3)?,
                "notes": r.get::<_, Option<String>>(4)?,
                "updated_at": r.get::<_, String>(5)?,
                "has_secret": r.get::<_, i64>(6)? != 0,
            }))
        })
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

/// Fetch one entry by id, including its opaque `secret_cipher` blob, or `None` when no
/// such row exists. The caller unseals the blob with the vault to reveal the secret.
pub fn get(db: &Path, id: i64) -> Result<Option<NetEntry>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    conn.query_row(
        "SELECT id, label, address, username, secret_cipher, notes, updated_at
         FROM net_entry WHERE id = ?1",
        rusqlite::params![id],
        |r| {
            Ok(NetEntry {
                id: r.get::<_, i64>(0)?,
                label: r.get::<_, String>(1)?,
                address: r.get::<_, String>(2)?,
                username: r.get::<_, Option<String>>(3)?,
                secret_cipher: r.get::<_, Option<Vec<u8>>>(4)?,
                notes: r.get::<_, Option<String>>(5)?,
                updated_at: r.get::<_, String>(6)?,
            })
        },
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        other => Err(format!("read net_entry {id}: {other}")),
    })
}

/// Delete an entry by id. Returns whether a row was removed (idempotent on a re-call).
pub fn delete(db: &Path, id: i64) -> Result<bool, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let n = conn
        .execute("DELETE FROM net_entry WHERE id = ?1", rusqlite::params![id])
        .map_err(|e| format!("delete net_entry: {e}"))?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Create a project under a fresh temp root and return `(root, project.db path)`.
    /// `projects::create` runs `init_db`, so the `net_entry` table exists. The synthetic
    /// `secret_cipher` bytes used below are NOT real ciphertext — netreg treats the blob
    /// as opaque, so the tests need neither the vault nor the OS keychain.
    fn fresh_db(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hyperion_netreg_test_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let summary = crate::projects::create(&root, "Net Test").unwrap();
        let id = summary.get("id").unwrap().as_str().unwrap().to_string();
        let db = root.join(&id).join("project.db");
        (root, db)
    }

    #[test]
    fn add_list_get_delete_roundtrip() {
        let (root, db) = fresh_db("rt");

        // Empty registry: list is empty and get on a missing id is None.
        assert!(list(&db).unwrap().is_empty());
        assert!(get(&db, 1).unwrap().is_none());

        // Add an entry WITH a (synthetic) sealed secret blob.
        let cipher = b"\x00\x01sealed-bytes\xff";
        let id = add(
            &db,
            "  Main PLC  ",
            "  192.168.1.50  ",
            Some("admin"),
            Some(cipher),
            Some("  ground floor riser  "),
        )
        .unwrap();
        assert!(id > 0);

        // List exposes metadata + has_secret, but NEVER the secret_cipher blob, and
        // trims the clear fields.
        let rows = list(&db).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["label"], "Main PLC");
        assert_eq!(rows[0]["address"], "192.168.1.50");
        assert_eq!(rows[0]["username"], "admin");
        assert_eq!(rows[0]["notes"], "ground floor riser");
        assert_eq!(rows[0]["has_secret"], true);
        assert!(rows[0].get("secret_cipher").is_none());

        // Get returns the full entry including the opaque blob, byte-for-byte.
        let entry = get(&db, id).unwrap().unwrap();
        assert_eq!(entry.label, "Main PLC");
        assert_eq!(entry.address, "192.168.1.50");
        assert_eq!(entry.username.as_deref(), Some("admin"));
        assert_eq!(entry.notes.as_deref(), Some("ground floor riser"));
        assert_eq!(entry.secret_cipher.as_deref(), Some(&cipher[..]));

        // Delete removes it; a second delete is a no-op.
        assert!(delete(&db, id).unwrap());
        assert!(!delete(&db, id).unwrap());
        assert!(list(&db).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn add_without_secret_reports_no_secret() {
        let (root, db) = fresh_db("nosecret");

        // An entry with no secret stores NULL; has_secret is false and get yields None.
        let id = add(&db, "Switch", "10.0.0.2", None, None, None).unwrap();
        let rows = list(&db).unwrap();
        assert_eq!(rows[0]["has_secret"], false);
        assert_eq!(rows[0]["username"], Value::Null);
        assert_eq!(rows[0]["notes"], Value::Null);
        assert!(get(&db, id).unwrap().unwrap().secret_cipher.is_none());

        // An all-whitespace username/notes is normalized to absent (SQL NULL).
        let id2 = add(&db, "Router", "10.0.0.1", Some("   "), None, Some("  ")).unwrap();
        let e2 = get(&db, id2).unwrap().unwrap();
        assert!(e2.username.is_none());
        assert!(e2.notes.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn add_rejects_empty_label_or_address() {
        let (root, db) = fresh_db("empty");

        assert!(add(&db, "   ", "10.0.0.1", None, None, None).is_err());
        assert!(add(&db, "Label", "   ", None, None, None).is_err());
        // Nothing was stored on a rejected write.
        assert!(list(&db).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn add_rejects_plaintext_secret_in_clear_fields() {
        let (root, db) = fresh_db("secret");

        // A bare vendor-prefixed API key in any clear field is refused — secrets may
        // only reach the DB via the sealed secret_cipher blob.
        let key = "sk-or-v1-abc123def456ghi789jkl012mno345";
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";

        assert!(add(&db, key, "10.0.0.1", None, None, None).is_err());
        assert!(add(&db, "Label", key, None, None, None).is_err());
        assert!(add(&db, "Label", "10.0.0.1", Some(key), None, None).is_err());
        assert!(add(&db, "Label", "10.0.0.1", None, None, Some(pem)).is_err());
        // None of the rejected writes persisted.
        assert!(list(&db).unwrap().is_empty());

        // A clean entry with an ordinary note still goes through.
        assert!(add(
            &db,
            "Main pump",
            "modbus://3",
            Some("operator"),
            None,
            Some("Main pump is Modbus slave 3.")
        )
        .is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }
}
