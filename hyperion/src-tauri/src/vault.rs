// Hyperion — encrypted secrets vault (Phase 1, M6).
//
// The vault is a single AES-256-GCM blob on disk (`vault.bin`):
//
//     vault.bin = nonce(12 bytes) || ciphertext
//     plaintext = JSON object { "<secret-name>": "<secret-value>", ... }
//
// The 32-byte data-encryption key (DEK) never touches disk in the clear: it
// lives in the OS keychain (Windows Credential Manager via the `keyring` crate,
// service `hyperion-vault`, account `dek`, stored base64). The DEK is loaded
// into memory only while the vault is *unlocked*, and zeroized on lock.
//
// Threat model / boundaries:
//   * At rest the blob is useless without the keychain DEK.
//   * The keychain entry is protected by the OS user login.
//   * Phase 1 step 2 (Entra SSO) gates `unlock()` behind a successful Microsoft
//     Entra sign-in for defense-in-depth; until then the keychain is the gate.
//   * Strictly local — the vault is git-ignored and never synced to bOS.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aes_gcm::{aead::Aead, Aes256Gcm, Key, KeyInit, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rand::RngCore;
use serde_json::{json, Value};
use zeroize::{Zeroize, Zeroizing};

const KEYCHAIN_SERVICE: &str = "hyperion-vault";
const KEYCHAIN_ACCOUNT: &str = "dek";
const NONCE_LEN: usize = 12;
const DEK_LEN: usize = 32;

/// Runtime vault state. The DEK is present only while unlocked.
pub struct Vault {
    path: PathBuf,
    dek: Option<[u8; DEK_LEN]>,
}

impl Drop for Vault {
    fn drop(&mut self) {
        if let Some(dek) = self.dek.as_mut() {
            dek.zeroize();
        }
    }
}

/// Vault location: `HYPERION_VAULT` env, else `<projects_root>/vault.bin`.
pub fn default_path(projects_root: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("HYPERION_VAULT") {
        return PathBuf::from(p);
    }
    projects_root.join("vault.bin")
}

impl Vault {
    pub fn new(path: PathBuf) -> Self {
        Vault { path, dek: None }
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    pub fn is_unlocked(&self) -> bool {
        self.dek.is_some()
    }

    /// JSON status for the UI: never leaks secret values.
    pub fn status(&self) -> Value {
        let count = if self.dek.is_some() {
            self.load().map(|m| m.len() as i64).unwrap_or(-1)
        } else {
            -1
        };
        json!({
            "exists": self.exists(),
            "unlocked": self.is_unlocked(),
            // -1 = unknown (locked or unreadable); >=0 = number of secrets held.
            "count": count,
        })
    }

    /// Load (or create) the DEK from the OS keychain and verify it against the
    /// existing blob. Creates an empty vault on first unlock.
    pub fn unlock(&mut self) -> Result<(), String> {
        let dek = dek_from_keychain()?;
        // Sample existence once: a delete racing the two checks must not turn a
        // verify-existing into a create-empty that overwrites real secrets.
        let is_new = !self.path.exists();
        if !is_new {
            // Verify the keychain DEK actually decrypts this blob.
            decrypt_file(&self.path, &dek).map_err(|_| {
                "vault key mismatch — keychain DEK does not match vault.bin".to_string()
            })?;
        }
        self.dek = Some(dek);
        if is_new {
            self.save(&BTreeMap::new())?;
        }
        Ok(())
    }

    /// Drop the in-memory DEK (zeroized).
    pub fn lock(&mut self) {
        if let Some(mut dek) = self.dek.take() {
            dek.zeroize();
        }
    }

    fn dek(&self) -> Result<&[u8; DEK_LEN], String> {
        self.dek
            .as_ref()
            .ok_or_else(|| "vault is locked — unlock it first".to_string())
    }

    // NOTE (Phase 1 follow-up): the returned map holds plaintext secret values
    // as plain `String`s; their heap buffers are freed but not zeroized when the
    // map drops. Wrapping values in `Zeroizing`/`SecretString` is a tracked
    // hardening item — the at-rest blob and the DEK are already protected.
    fn load(&self) -> Result<BTreeMap<String, String>, String> {
        let dek = self.dek()?;
        if !self.path.exists() {
            return Ok(BTreeMap::new());
        }
        let plain = decrypt_file(&self.path, dek)?;
        serde_json::from_slice(&plain).map_err(|e| format!("vault is corrupt: {e}"))
    }

    fn save(&self, map: &BTreeMap<String, String>) -> Result<(), String> {
        let dek = self.dek()?;
        let plain = serde_json::to_vec(map).map_err(|e| format!("{e}"))?;
        let blob = encrypt(&plain, dek)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create vault dir: {e}"))?;
        }
        std::fs::write(&self.path, blob).map_err(|e| format!("write vault: {e}"))
    }

    /// Sorted secret names (never values).
    pub fn names(&self) -> Result<Vec<String>, String> {
        Ok(self.load()?.into_keys().collect())
    }

    /// Insert or replace a secret. Refuses empty names.
    pub fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("secret name cannot be empty".into());
        }
        let mut map = self.load()?;
        map.insert(name.to_string(), value.to_string());
        self.save(&map)
    }

    /// Remove a secret. Returns whether it existed.
    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let mut map = self.load()?;
        let existed = map.remove(name).is_some();
        if existed {
            self.save(&map)?;
        }
        Ok(existed)
    }

    /// Reveal a secret's raw value (requires unlocked). Use sparingly.
    pub fn reveal(&self, name: &str) -> Result<String, String> {
        self.load()?
            .remove(name)
            .ok_or_else(|| format!("no such secret: {name}"))
    }
}

// ----------------------------- crypto core -----------------------------

fn encrypt(plain: &[u8], dek: &[u8; DEK_LEN]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(dek));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(nonce, plain)
        .map_err(|_| "encryption failed".to_string())?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn decrypt_bytes(blob: &[u8], dek: &[u8; DEK_LEN]) -> Result<Vec<u8>, String> {
    if blob.len() < NONCE_LEN {
        return Err("vault file is truncated".into());
    }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(dek));
    cipher
        .decrypt(Nonce::from_slice(nonce_bytes), ct)
        .map_err(|_| "decryption failed (wrong key or tampered blob)".to_string())
}

fn decrypt_file(path: &Path, dek: &[u8; DEK_LEN]) -> Result<Vec<u8>, String> {
    let blob = std::fs::read(path).map_err(|e| format!("read vault: {e}"))?;
    decrypt_bytes(&blob, dek)
}

/// Fetch the DEK from the OS keychain, generating and storing one on first use.
fn dek_from_keychain() -> Result<[u8; DEK_LEN], String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("open keychain: {e}"))?;
    match entry.get_password() {
        Ok(b64) => {
            // Both the base64 string and the decoded bytes are key material;
            // wrap them so they are zeroized when this arm returns.
            let b64 = Zeroizing::new(b64);
            let bytes: Zeroizing<Vec<u8>> = Zeroizing::new(
                B64.decode(b64.trim())
                    .map_err(|e| format!("keychain DEK is not valid base64: {e}"))?,
            );
            if bytes.len() != DEK_LEN {
                return Err(format!("keychain DEK has wrong length: {}", bytes.len()));
            }
            let mut dek = [0u8; DEK_LEN];
            dek.copy_from_slice(&bytes);
            Ok(dek)
        }
        Err(keyring::Error::NoEntry) => {
            let mut dek = [0u8; DEK_LEN];
            rand::rngs::OsRng.fill_bytes(&mut dek);
            let encoded = Zeroizing::new(B64.encode(dek));
            entry
                .set_password(&encoded)
                .map_err(|e| format!("store keychain DEK: {e}"))?;
            Ok(dek)
        }
        Err(e) => Err(format!("read keychain: {e}")),
    }
}

// ----------------------- plaintext-secret guardrail -----------------------

/// Mask a value for display: keep the first/last 2 chars, hide the middle.
pub fn mask(value: &str) -> String {
    let n = value.chars().count();
    if n <= 4 {
        return "*".repeat(n.max(1));
    }
    let chars: Vec<char> = value.chars().collect();
    let head: String = chars[..2].iter().collect();
    let tail: String = chars[n - 2..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(n - 4))
}

/// Heuristic scan for credentials that should live in the vault, not in
/// plaintext context (snapshots, notes, wiki). Returns one finding per hit with
/// a *masked* sample so the warning itself never echoes the secret.
pub fn scan_for_secrets(text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let lower = text.to_lowercase();

    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        out.push(json!({ "kind": "private_key", "detail": "PEM private key block" }));
    }
    if let Some(idx) = text.find("AKIA") {
        // "AKIA" is ASCII, so `idx` is a valid char boundary in `text`.
        let sample: String = text[idx..].chars().take(20).collect();
        let alnum_tail = sample.chars().skip(4).all(|c| c.is_ascii_alphanumeric());
        if sample.chars().count() >= 16 && alnum_tail {
            out.push(json!({ "kind": "aws_access_key", "detail": mask(&sample) }));
        }
    }
    for marker in ["bearer ", "authorization: "] {
        // Index into `lower` (where `idx` is valid): `to_lowercase()` can shift
        // byte offsets, so the same index into `text` may not be a char
        // boundary. Casing is irrelevant for a masked sample.
        if let Some(idx) = lower.find(marker) {
            let token: String = lower[idx + marker.len()..]
                .chars()
                .take_while(|c| !c.is_whitespace())
                .collect();
            if token.chars().count() >= 12 {
                out.push(json!({ "kind": "bearer_token", "detail": mask(&token) }));
            }
        }
    }
    // key=value / key: value pairs naming a credential with a non-trivial value.
    for line in text.lines() {
        let l = line.to_lowercase();
        let names = [
            "password", "passwd", "pwd", "secret", "api_key", "apikey", "token",
        ];
        if names.iter().any(|k| l.contains(k)) {
            if let Some(val) = line.split(['=', ':']).nth(1) {
                let v = val.trim().trim_matches(['"', '\'']);
                if v.chars().count() >= 6 && !v.eq_ignore_ascii_case("null") {
                    out.push(json!({
                        "kind": "credential_assignment",
                        "detail": mask(v),
                    }));
                }
            }
        }
    }
    out
}

/// The high-confidence secret *kinds* from `scan_for_secrets` that a write path
/// (project memory, agent instincts) must reject outright. The looser
/// `credential_assignment` heuristic is intentionally excluded — it false-positives
/// on ordinary notes like "token bucket: …" — so write guards key off this set only.
pub const HIGH_CONFIDENCE_SECRET_KINDS: [&str; 3] =
    ["private_key", "aws_access_key", "bearer_token"];

/// Does a single whitespace token look like a bare API key by a *known vendor
/// prefix* (OpenAI/OpenRouter/Anthropic/GitHub/Slack)? This catches a key that is
/// pasted on its own — notably the app's own `sk-or-…` OpenRouter keys — without a
/// surrounding `Bearer`/`Authorization:` marker that `scan_for_secrets` keys off.
/// Deliberately prefix-only: the looser opaque-entropy heuristic (used for redacting
/// log output) would reject UUIDs and long device IDs an integrator legitimately
/// records, so it is *not* used as a write-rejection rule.
pub fn token_looks_like_secret(token: &str) -> bool {
    // Strip surrounding quotes, brackets, and Markdown emphasis/code punctuation so a
    // key pasted as `sk-or-…`, **sk-…**, or "sk-…", is still unwrapped and matched.
    let t = token.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\''
                | ','
                | ';'
                | ':'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '`'
                | '*'
                | '_'
                | '~'
                | '|'
                | '!'
                | '#'
        )
    });
    if t.len() < 12 {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    const PREFIXES: [&str; 6] = ["sk-", "sk-or-", "sk-ant-", "ghp_", "github_pat_", "xoxb-"];
    PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// True if `body` contains a plaintext credential that must never be stored where it
/// will be spliced into a model prompt (project memory, agent instincts). Combines
/// the structural scan (PEM key / AWS key / `Bearer`-marked token) with the
/// vendor-prefix token check above, so a bare `sk-or-…` key is caught either way.
/// Single source of truth shared by `projects::memory_set` and `roster::validate_body`.
pub fn body_has_plaintext_secret(body: &str) -> bool {
    let structural = scan_for_secrets(body).iter().any(|f| {
        f.get("kind")
            .and_then(|k| k.as_str())
            .is_some_and(|k| HIGH_CONFIDENCE_SECRET_KINDS.contains(&k))
    });
    structural || body.split_whitespace().any(token_looks_like_secret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_secret_catches_bare_vendor_keys_but_not_uuids() {
        // The app's own OpenRouter key shape — bare, no Bearer marker — is caught.
        assert!(body_has_plaintext_secret(
            "use this key sk-or-v1-abc123def456ghi789jkl012mno345"
        ));
        assert!(body_has_plaintext_secret(
            "token ghp_ABCDEFghijkl0123456789MNOPqrstuvWX"
        ));
        // A key wrapped in Markdown backticks/bold is still unwrapped and caught.
        assert!(body_has_plaintext_secret(
            "the key is `sk-or-v1-abc123def456ghi789jkl012`"
        ));
        assert!(body_has_plaintext_secret(
            "**sk-ant-api03-abc123def456ghi789jkl012mno**"
        ));
        // A PEM block is still caught structurally.
        assert!(body_has_plaintext_secret(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----"
        ));
        // Ordinary operator notes — including a UUID/device id — are NOT flagged.
        assert!(!body_has_plaintext_secret("Main pump is Modbus slave 3."));
        assert!(!body_has_plaintext_secret(
            "Device id 550e8400-e29b-41d4-a716-446655440000 sits in zone 1.1"
        ));
    }

    #[test]
    fn aes_gcm_round_trip() {
        let dek = [7u8; DEK_LEN];
        let blob = encrypt(b"hello vault", &dek).unwrap();
        // nonce is prepended; blob must be longer than the plaintext + tag.
        assert!(blob.len() > NONCE_LEN);
        let back = decrypt_bytes(&blob, &dek).unwrap();
        assert_eq!(back, b"hello vault");
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let blob = encrypt(b"secret", &[1u8; DEK_LEN]).unwrap();
        assert!(decrypt_bytes(&blob, &[2u8; DEK_LEN]).is_err());
    }

    #[test]
    fn each_save_uses_a_fresh_nonce() {
        let dek = [9u8; DEK_LEN];
        let a = encrypt(b"x", &dek).unwrap();
        let b = encrypt(b"x", &dek).unwrap();
        assert_ne!(a[..NONCE_LEN], b[..NONCE_LEN], "nonce must not repeat");
    }

    #[test]
    fn mask_edges() {
        assert_eq!(mask(""), "*");
        assert_eq!(mask("a"), "*");
        assert_eq!(mask("abcd"), "****");
        assert_eq!(mask("abcdef"), "ab**ef");
        // multibyte: counts chars, not bytes, and never panics.
        assert_eq!(mask("héllo!"), "hé**o!");
    }

    #[test]
    fn scan_detects_common_secrets() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMII...\n-----END RSA PRIVATE KEY-----";
        assert!(scan_for_secrets(pem)
            .iter()
            .any(|f| f["kind"] == "private_key"));

        let assign = "password = hunter2longvalue";
        assert!(scan_for_secrets(assign)
            .iter()
            .any(|f| f["kind"] == "credential_assignment"));

        let bearer = "Authorization: Bearer abcdef0123456789";
        assert!(scan_for_secrets(bearer)
            .iter()
            .any(|f| f["kind"] == "bearer_token"));
    }

    #[test]
    fn scan_does_not_panic_on_unicode_near_markers() {
        // U+0130 changes byte length under to_lowercase(); must not panic.
        let s = "İİİ authorization: Bearer abcdefghijklmnop İİİ";
        let _ = scan_for_secrets(s);
        let s2 = "şifre password: gizliDeğer123";
        let _ = scan_for_secrets(s2);
    }

    #[test]
    fn scan_ignores_clean_text() {
        assert!(scan_for_secrets("just some ordinary config notes").is_empty());
    }
}
