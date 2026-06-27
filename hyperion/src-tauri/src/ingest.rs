// Hyperion — context-file ingestion + chunking (Phase 3, M1).
//
// Integrators drop reference material into a project — a Belimo actuator datasheet,
// a Milesight gateway CSV export, a wiring spec — and the co-pilot should answer
// from it. This module turns an uploaded file into retrievable text: detect the
// kind, extract UTF-8 text (text/markdown/CSV/log/JSON/… for now; PDF & Word land
// in a later unit), and split it into overlapping chunks small enough that a few
// can be retrieved into the prompt without blowing the budget.
//
// Ingested content is UNTRUSTED — a datasheet could carry injected instructions —
// so the caller fences + entity-encodes retrieved chunks exactly like the `.bos`
// grounding. Strictly local; nothing here writes to bOS or the vault.

use std::collections::HashSet;
use std::path::Path;

/// Largest single file we will ingest (bytes). Generous for a datasheet/CSV, but
/// capped so one upload can't bloat the project DB or dominate retrieval.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Text-extractable file kinds, by lowercased extension. Binary/office formats
/// (pdf, docx, xlsx, images) are deferred to a later unit and rejected for now.
const TEXT_EXTS: [&str; 13] = [
    "txt", "md", "markdown", "csv", "tsv", "log", "json", "yaml", "yml", "xml", "ini", "cfg",
    "conf",
];

/// Chunking parameters (characters). Chunks target ~CHUNK_TARGET chars and prefer to
/// break on a line boundary for coherence; a small overlap keeps a fact that
/// straddles a boundary findable from either side.
const CHUNK_TARGET: usize = 1500;
const CHUNK_OVERLAP: usize = 150;

/// Classify a filename by extension into a short kind tag (its lowercased extension,
/// or "bin" when there is none).
pub fn detect_kind(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "bin".to_string())
}

/// Is this a kind we can extract text from today?
pub fn is_text_kind(kind: &str) -> bool {
    TEXT_EXTS.contains(&kind)
}

/// Extract UTF-8 text from an uploaded file's bytes. Supports text-like kinds only;
/// rejects empty, oversized, and unsupported (binary/office) files with a clear
/// message. Lossy decoding tolerates the odd stray byte in a CSV/log; CR is dropped
/// so chunking sees uniform `\n` line endings.
pub fn extract_text(name: &str, bytes: &[u8]) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("file is empty".into());
    }
    if bytes.len() > MAX_FILE_BYTES {
        return Err(format!(
            "file is too large (max {} MB)",
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    let kind = detect_kind(name);
    if !is_text_kind(&kind) {
        return Err(format!(
            "'{kind}' files aren't supported yet — add a text/markdown/CSV/JSON file (PDF & Word land in a later unit)"
        ));
    }
    let text = String::from_utf8_lossy(bytes).replace('\r', "");
    if text.trim().is_empty() {
        return Err("no readable text in this file".into());
    }
    Ok(text)
}

/// Split text into overlapping chunks. Works in `char`s (correct Unicode boundaries):
/// take ~CHUNK_TARGET chars, prefer to end on a newline within the last overlap
/// window, emit the trimmed piece, then step forward leaving a CHUNK_OVERLAP tail.
/// Always makes forward progress, so it terminates on any input.
pub fn chunk(text: &str) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n <= CHUNK_TARGET {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < n {
        let mut end = (start + CHUNK_TARGET).min(n);
        if end < n {
            if let Some(br) = find_line_break(&chars, start, end) {
                end = br;
            }
        }
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim().to_string();
        if !piece.is_empty() {
            chunks.push(piece);
        }
        if end >= n {
            break;
        }
        // Step forward, leaving an overlap tail, but always advance at least one char.
        start = end.saturating_sub(CHUNK_OVERLAP).max(start + 1);
    }
    chunks
}

/// Find a newline boundary to break on, scanning back from `end` up to the overlap
/// window (so a chunk ends at a line, not mid-word). Returns the index *after* the
/// newline, or `None` if there is none in the window.
fn find_line_break(chars: &[char], start: usize, end: usize) -> Option<usize> {
    let floor = end.saturating_sub(CHUNK_OVERLAP).max(start + 1);
    (floor..end)
        .rev()
        .find(|&i| chars[i] == '\n')
        .map(|i| i + 1)
}

/// Lowercased alphanumeric word tokens of length >= 3 — short tokens ("of", "to",
/// "a") are dropped so retrieval keys off meaningful terms.
pub fn keywords(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(|t| t.to_string())
        .collect()
}

/// Score a chunk against query terms: how many distinct query terms appear in it.
/// A simple, dependency-free keyword overlap — the embedding/vector ranker is a
/// later unit (the vector-store choice is still open).
pub fn score(query_terms: &HashSet<String>, chunk_text: &str) -> usize {
    if query_terms.is_empty() {
        return 0;
    }
    let lower = chunk_text.to_lowercase();
    query_terms
        .iter()
        .filter(|t| lower.contains(t.as_str()))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_and_text_kind() {
        assert_eq!(detect_kind("Belimo-LR24A.csv"), "csv");
        assert_eq!(detect_kind("notes.MD"), "md");
        assert_eq!(detect_kind("noext"), "bin");
        assert!(is_text_kind("csv"));
        assert!(is_text_kind("json"));
        assert!(!is_text_kind("pdf"));
        assert!(!is_text_kind("bin"));
    }

    #[test]
    fn extract_text_validates_kind_size_and_emptiness() {
        assert!(extract_text("a.txt", b"").is_err()); // empty
        assert!(extract_text("a.pdf", b"%PDF-1.7 ...").is_err()); // unsupported kind
        let big = vec![b'x'; MAX_FILE_BYTES + 1];
        assert!(extract_text("a.txt", &big).is_err()); // oversized
                                                       // Whitespace-only text is rejected as having no readable text.
        assert!(extract_text("a.txt", b"   \n\t  ").is_err());
        // A real text file extracts, with CRs stripped.
        let t = extract_text("a.csv", b"slave,addr\r\npump,3\r\n").unwrap();
        assert!(!t.contains('\r'));
        assert!(t.contains("pump,3"));
    }

    #[test]
    fn chunk_splits_long_text_with_progress_and_overlap() {
        // Short text -> a single chunk.
        assert_eq!(chunk("one short line"), vec!["one short line".to_string()]);
        assert!(chunk("   ").is_empty());

        // Long text -> multiple chunks, each within a sane bound, covering the input.
        let line = "The Belimo actuator is wired to Modbus slave 7.\n";
        let big = line.repeat(120); // ~5.7k chars
        let chunks = chunk(&big);
        assert!(
            chunks.len() >= 3,
            "expected several chunks, got {}",
            chunks.len()
        );
        for c in &chunks {
            assert!(c.chars().count() <= CHUNK_TARGET + CHUNK_OVERLAP + 4);
            assert!(!c.is_empty());
        }
        // The salient fact appears in the chunks.
        assert!(chunks.iter().any(|c| c.contains("Modbus slave 7")));
    }

    #[test]
    fn chunk_handles_one_very_long_line() {
        // A single line longer than the target must still be split (no infinite loop).
        let one_line = "a".repeat(CHUNK_TARGET * 3 + 17);
        let chunks = chunk(&one_line);
        assert!(chunks.len() >= 3);
        let joined: String = chunks.concat();
        assert!(joined.len() >= one_line.len()); // overlap means >=, never loses content
    }

    #[test]
    fn keywords_and_score_rank_by_overlap() {
        let q = keywords("What Modbus slave is the Belimo actuator on?");
        assert!(q.contains("modbus"));
        assert!(q.contains("belimo"));
        assert!(!q.contains("is")); // short token dropped
        let hit = score(&q, "The Belimo actuator sits on Modbus slave 7.");
        let miss = score(&q, "The lobby scene fades over four seconds.");
        assert!(hit > miss);
        assert_eq!(score(&HashSet::new(), "anything"), 0);
    }
}
