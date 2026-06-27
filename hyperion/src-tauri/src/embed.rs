// Hyperion — embedding client + vector helpers (Phase 3, M2).
//
// An OPTIONAL embedding layer that upgrades context retrieval from keyword
// overlap to cosine similarity. Everything here is best-effort: if no embedding
// API key is configured (the default in CI and offline operation), `embed`
// returns an `Err` and every caller falls back to the keyword ranker. No part of
// this module touches the vault or Tauri state — it is env-only by design, so the
// pure-`projects.rs` layer can call it without a State handle.
//
// Security: the embedding API key is read from the environment and never logged
// or echoed. Any HTTP error/response text surfaced to the UI is first scrubbed
// through `agent::redact_secrets`. Strictly read-only toward bOS.

use std::io::Read;
use std::time::Duration;

use serde_json::{json, Value};

use crate::agent::{redact_secrets, tail_chars};

/// Hard ceiling on a single embedding round-trip. Shorter than the chat ASK
/// timeout: embeddings are an optional enrichment, so on any stall we want to
/// fall back to the keyword ranker fast rather than block the whole ask.
const EMBED_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on the embedding HTTP response body (mirrors agent::MAX_CAPTURE):
/// a runaway or malicious response cannot exhaust memory before the timeout.
const MAX_CAPTURE: u64 = 2 * 1024 * 1024;

/// Default OpenAI-compatible base; overridable via `HYPERION_EMBED_BASE_URL`.
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
/// Default embedding model; overridable via `HYPERION_EMBED_MODEL`.
const DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Upper bound on a single returned vector's dimension. Well above any current
/// embedding model (text-embedding-3-large is 3072); rejects a hostile endpoint
/// that returns absurdly long vectors to bloat the SQLite store / query memory.
const MAX_DIM: usize = 8192;

/// Largest batch of inputs sent in one `POST /embeddings`. OpenAI-compatible APIs
/// cap inputs-per-request (OpenAI: 2048); callers that embed a whole file must
/// split into batches no larger than this so a big file doesn't get a blanket
/// HTTP 400 and silently fall back to keyword scoring.
pub const MAX_BATCH: usize = 256;

// ----------------------------- vector (de)serialization -----------------------------

/// Pack a vector of `f32` into a little-endian byte blob for storage in SQLite.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode a little-endian `f32` blob back into a vector. Returns `None` if the
/// byte length is not a multiple of 4 (a corrupt or foreign-format row), so a bad
/// row is simply skipped rather than panicking the ranker.
pub fn blob_to_vec(b: &[u8]) -> Option<Vec<f32>> {
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 4);
    for chunk in b.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Some(out)
}

/// Cosine similarity of two vectors. Returns `0.0` on a dimension mismatch or if
/// either vector has zero norm — so a stale-dimension or empty row simply ranks
/// last instead of crashing or producing a NaN. Inputs containing NaN/Inf would
/// yield NaN, but `parse_embeddings` only ever stores finite floats (JSON has no
/// NaN/Inf literal) and the caller's `> 0.0` filter discards any NaN score anyway.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// ----------------------------- the embedding client -----------------------------

/// Embed a batch of texts via an OpenAI-compatible `POST {base}/embeddings`.
/// Returns `(model, vectors)` where `vectors[i]` is the embedding of `texts[i]`
/// (order preserved). Config is read from the environment:
///   - `HYPERION_EMBED_API_KEY`  (required; absent/empty -> Err, callers fall back)
///   - `HYPERION_EMBED_BASE_URL` (default `https://api.openai.com/v1`)
///   - `HYPERION_EMBED_MODEL`    (default `text-embedding-3-small`)
///
/// Every error is a recoverable signal: callers treat any `Err` as "fall back to
/// the keyword ranker", so offline CI (no key, no network) stays green. The API
/// key is never logged; error/response text is scrubbed before being surfaced.
///
/// EGRESS NOTE: when configured, the verbatim `texts` (ingested chunk content at
/// index time, and the operator's question at query time) are sent to the
/// configured endpoint. This is the only path in the app that ships context off
/// the machine; it is opt-in (off unless `HYPERION_EMBED_API_KEY` is set) and
/// documented in the wiki (`context.html`).
pub fn embed<S: AsRef<str>>(texts: &[S]) -> Result<(String, Vec<Vec<f32>>), String> {
    if texts.is_empty() {
        return Ok((String::new(), Vec::new()));
    }
    if texts.len() > MAX_BATCH {
        return Err(format!(
            "embeddings: batch of {} exceeds MAX_BATCH ({MAX_BATCH}); split before calling",
            texts.len()
        ));
    }
    let key = std::env::var("HYPERION_EMBED_API_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        return Err("embeddings not configured".into());
    }
    let base = std::env::var("HYPERION_EMBED_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
    let base = base.trim_end_matches('/');
    // Only http(s) — refuse file://, data://, or a typo'd scheme that would carry
    // the Authorization header (and the context body) to an unintended target.
    if !base.starts_with("https://") && !base.starts_with("http://") {
        return Err("embeddings: HYPERION_EMBED_BASE_URL must start with http:// or https://".into());
    }
    let model = std::env::var("HYPERION_EMBED_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let url = format!("{base}/embeddings");

    // Serialize with serde_json (already a dependency) rather than ureq's `json`
    // feature, mirroring agent::ask_openrouter.
    let inputs: Vec<&str> = texts.iter().map(|t| t.as_ref()).collect();
    let body = serde_json::to_string(&json!({
        "model": model,
        "input": inputs,
    }))
    .map_err(|e| format!("embeddings: failed to encode request: {e}"))?;

    let resp = ureq::post(&url)
        .timeout(EMBED_TIMEOUT)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_string(&body);

    match resp {
        Ok(r) => {
            let text = read_body_capped(r);
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| format!("embeddings: malformed JSON response: {e}"))?;
            let vectors = parse_embeddings(&v, texts.len())?;
            // Prefer the model the API echoes back; fall back to what we requested.
            let used = v
                .get("model")
                .and_then(|m| m.as_str())
                .unwrap_or(&model)
                .to_string();
            Ok((used, vectors))
        }
        // Surface the API's own error text (redacted, never the key) so the user can act.
        Err(ureq::Error::Status(code, r)) => {
            let detail = read_body_capped(r);
            Err(format!(
                "embeddings HTTP {code}: {}",
                tail_chars(&redact_secrets(&detail), 400)
            ))
        }
        Err(e) => Err(format!(
            "embeddings request failed: {}",
            redact_secrets(&e.to_string())
        )),
    }
}

/// Pull the ordered `data[i].embedding` arrays from an OpenAI-compatible response,
/// honoring each entry's `index` so vectors line up with the input order. Errors
/// if any vector is missing, so a partial response falls back rather than mis-rank.
fn parse_embeddings(v: &Value, n: usize) -> Result<Vec<Vec<f32>>, String> {
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "embeddings: response contained no data array".to_string())?;
    if data.len() != n {
        return Err(format!(
            "embeddings: expected {n} vectors, got {}",
            data.len()
        ));
    }
    let mut out: Vec<Option<Vec<f32>>> = vec![None; n];
    for (pos, entry) in data.iter().enumerate() {
        // Use the explicit index if present and in range, else positional order.
        let idx = entry
            .get("index")
            .and_then(|i| i.as_u64())
            .map(|i| i as usize)
            .filter(|i| *i < n)
            .unwrap_or(pos);
        let arr = entry
            .get("embedding")
            .and_then(|e| e.as_array())
            .ok_or_else(|| "embeddings: an entry had no embedding array".to_string())?;
        if arr.len() > MAX_DIM {
            return Err(format!(
                "embeddings: vector dimension {} exceeds cap {MAX_DIM}",
                arr.len()
            ));
        }
        let vec: Vec<f32> = arr
            .iter()
            .filter_map(|x| x.as_f64().map(|f| f as f32))
            .collect();
        if vec.len() != arr.len() || vec.is_empty() {
            return Err("embeddings: an embedding vector was empty or non-numeric".into());
        }
        // A well-formed response maps each slot exactly once. If two entries
        // resolve to the same slot (duplicate/out-of-range `index`), surface it
        // loudly rather than silently overwriting — the caller falls back to
        // keyword scoring instead of mis-ranking on a malformed response.
        if out[idx].replace(vec).is_some() {
            return Err(format!("embeddings: response mapped two vectors to index {idx}"));
        }
    }
    out.into_iter()
        .collect::<Option<Vec<Vec<f32>>>>()
        .ok_or_else(|| "embeddings: response was missing one or more vectors".into())
}

/// Read an HTTP body under a hard cap so a large or malicious response cannot
/// exhaust memory (mirrors agent::read_body_capped).
fn read_body_capped(r: ureq::Response) -> String {
    let mut buf = Vec::new();
    let _ = r.into_reader().take(MAX_CAPTURE).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serializes any test that mutates process-global env vars, so they can't
    // race each other under the parallel test harness.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn vec_blob_roundtrip() {
        let v = vec![0.0f32, 1.5, -2.25, 1234.5, f32::MIN_POSITIVE];
        let blob = vec_to_blob(&v);
        assert_eq!(blob.len(), v.len() * 4);
        let back = blob_to_vec(&blob).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn blob_to_vec_rejects_misaligned() {
        // A length that is not a multiple of 4 is a corrupt/foreign row.
        assert!(blob_to_vec(&[0u8, 1, 2]).is_none());
        assert!(blob_to_vec(&[0u8; 5]).is_none());
        // Empty blob is a valid zero-length vector.
        assert_eq!(blob_to_vec(&[]), Some(Vec::new()));
    }

    #[test]
    fn cosine_correctness_and_edges() {
        // Identical direction -> 1.0.
        let a = vec![1.0f32, 2.0, 3.0];
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
        // Orthogonal -> 0.0.
        let x = vec![1.0f32, 0.0];
        let y = vec![0.0f32, 1.0];
        assert!(cosine(&x, &y).abs() < 1e-6);
        // Opposite -> -1.0.
        let n = vec![-1.0f32, -2.0, -3.0];
        assert!((cosine(&a, &n) + 1.0).abs() < 1e-6);
        // Scale-invariant.
        let a2 = vec![2.0f32, 4.0, 6.0];
        assert!((cosine(&a, &a2) - 1.0).abs() < 1e-6);
        // Dimension mismatch -> 0.0 (no panic).
        assert_eq!(cosine(&a, &x), 0.0);
        // Zero-norm -> 0.0 (no NaN).
        let z = vec![0.0f32, 0.0, 0.0];
        assert_eq!(cosine(&a, &z), 0.0);
        // Empty -> 0.0.
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn embed_without_key_is_recoverable_err() {
        // With no API key configured the client must return Err (never panic/hang)
        // so callers fall back to the keyword ranker. Guard the env so this is
        // deterministic regardless of the host environment.
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("HYPERION_EMBED_API_KEY").ok();
        std::env::remove_var("HYPERION_EMBED_API_KEY");
        let err = embed(&["hello".to_string()]).unwrap_err();
        assert!(err.contains("not configured"), "got: {err}");
        if let Some(p) = prev {
            std::env::set_var("HYPERION_EMBED_API_KEY", p);
        }
    }

    #[test]
    fn parse_embeddings_orders_by_index() {
        let v = json!({
            "model": "test-model",
            "data": [
                { "index": 1, "embedding": [3.0, 4.0] },
                { "index": 0, "embedding": [1.0, 2.0] },
            ]
        });
        let out = parse_embeddings(&v, 2).unwrap();
        assert_eq!(out, vec![vec![1.0f32, 2.0], vec![3.0f32, 4.0]]);
    }

    #[test]
    fn parse_embeddings_rejects_wrong_count() {
        let v = json!({ "data": [ { "index": 0, "embedding": [1.0] } ] });
        assert!(parse_embeddings(&v, 2).is_err());
    }

    #[test]
    fn parse_embeddings_rejects_duplicate_index() {
        // Two entries mapping to the same slot must error (not silently overwrite).
        let v = json!({
            "data": [
                { "index": 0, "embedding": [1.0, 2.0] },
                { "index": 0, "embedding": [3.0, 4.0] },
            ]
        });
        let err = parse_embeddings(&v, 2).unwrap_err();
        assert!(err.contains("two vectors to index"), "got: {err}");
    }

    #[test]
    fn parse_embeddings_rejects_oversized_dim() {
        // A vector larger than MAX_DIM is rejected before it can bloat storage.
        let big: Vec<f32> = vec![0.1; MAX_DIM + 1];
        let v = json!({ "data": [ { "index": 0, "embedding": big } ] });
        let err = parse_embeddings(&v, 1).unwrap_err();
        assert!(err.contains("exceeds cap"), "got: {err}");
    }

    #[test]
    fn embed_rejects_oversized_batch() {
        // The batch guard fires before the API-key check, so this needs no env.
        let texts: Vec<String> = (0..=MAX_BATCH).map(|i| i.to_string()).collect();
        let err = embed(&texts).unwrap_err();
        assert!(err.contains("exceeds MAX_BATCH"), "got: {err}");
    }

    #[test]
    fn embed_rejects_non_http_scheme() {
        // A configured key + a non-http(s) base URL must fail before any network
        // call (so the Authorization header / context body never leaves over a
        // foreign scheme). Guard the process-global env for determinism.
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev_key = std::env::var("HYPERION_EMBED_API_KEY").ok();
        let prev_base = std::env::var("HYPERION_EMBED_BASE_URL").ok();
        std::env::set_var("HYPERION_EMBED_API_KEY", "test-key");
        std::env::set_var("HYPERION_EMBED_BASE_URL", "ftp://evil.example/v1");
        let err = embed(&["hello".to_string()]).unwrap_err();
        assert!(err.contains("must start with http"), "got: {err}");
        // Restore.
        match prev_key {
            Some(k) => std::env::set_var("HYPERION_EMBED_API_KEY", k),
            None => std::env::remove_var("HYPERION_EMBED_API_KEY"),
        }
        match prev_base {
            Some(b) => std::env::set_var("HYPERION_EMBED_BASE_URL", b),
            None => std::env::remove_var("HYPERION_EMBED_BASE_URL"),
        }
    }
}
