// Hyperion — context-file ingestion + chunking (Phase 3, M1).
//
// Integrators drop reference material into a project — a Belimo actuator datasheet,
// a Milesight gateway CSV export, a wiring spec — and the co-pilot should answer
// from it. This module turns an uploaded file into retrievable text: detect the
// kind, extract text (UTF-8 for text/markdown/CSV/log/JSON/…; the PDF content stream
// for `.pdf`; the WordprocessingML body for `.docx`), and split it into overlapping
// chunks small enough that a few can be retrieved into the prompt without blowing the
// budget.
//
// PDF and DOCX are UNTRUSTED BINARY input parsed by third-party crates: PDF extraction
// runs inside `catch_unwind` (a hostile/malformed file yields an error, never a crash),
// and DOCX decompression is bounded (zip-bomb guard). Extracted content is untrusted
// like any other context — a datasheet could carry injected instructions — so the caller
// fences + entity-encodes retrieved chunks exactly like the `.bos` grounding. Strictly
// local; nothing here writes to bOS or the vault.

use std::collections::HashSet;
use std::path::Path;

/// Largest single file we will ingest (bytes). Generous for a datasheet/CSV, but
/// capped so one upload can't bloat the project DB or dominate retrieval.
pub const MAX_FILE_BYTES: usize = 4 * 1024 * 1024;

/// File kinds we decode as UTF-8 text directly, by lowercased extension. Binary document
/// formats are handled separately (`pdf`, `docx`); everything else (xlsx, images, …) is
/// still rejected.
const TEXT_EXTS: [&str; 13] = [
    "txt", "md", "markdown", "csv", "tsv", "log", "json", "yaml", "yml", "xml", "ini", "cfg",
    "conf",
];

/// Largest amount of decompressed XML we will read out of a `.docx`, as a guard against a
/// zip bomb (a tiny archive that inflates to gigabytes). Generous for a real document.
const MAX_DOCX_XML_BYTES: u64 = 64 * 1024 * 1024;

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

/// Extract text from an uploaded file's bytes. Handles UTF-8 text kinds, PDF, and Word
/// `.docx`; rejects empty, oversized, and unsupported kinds with a clear message. Lossy
/// decoding tolerates the odd stray byte; CR is dropped so chunking sees uniform `\n`
/// line endings regardless of source format.
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
    let raw = if is_text_kind(&kind) {
        String::from_utf8_lossy(bytes).into_owned()
    } else if kind == "pdf" {
        extract_pdf(bytes)?
    } else if kind == "docx" {
        extract_docx(bytes)?
    } else {
        return Err(format!(
            "'{kind}' files aren't supported — add a text/markdown/CSV/JSON file, a PDF, or a Word .docx"
        ));
    };
    let text = raw.replace('\r', "");
    if text.trim().is_empty() {
        return Err("no readable text in this file".into());
    }
    Ok(text)
}

/// Extract text from a PDF's bytes. The file comes from an integrator's machine and is
/// fully untrusted; the underlying parser can panic on a malformed or hostile PDF, so the
/// call runs inside `catch_unwind` — a bad file yields a clean error, never a process crash.
/// Known limitation: there is no parse *timeout*, so a pathological PDF could run long and
/// briefly hang the UI; the 4 MiB input cap bounds it, and a wall-clock guard is a future unit.
fn extract_pdf(bytes: &[u8]) -> Result<String, String> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    match catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    })) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(format!("could not read this PDF: {e}")),
        Err(_) => Err("could not read this PDF (it may be malformed or encrypted)".into()),
    }
}

/// Extract text from a Word `.docx`. A `.docx` is a ZIP whose `word/document.xml` holds the
/// body; we pull the run text and turn paragraph/break/tab markers into whitespace.
/// Decompression is bounded (MAX_DOCX_XML_BYTES) so a crafted archive can't exhaust memory.
fn extract_docx(bytes: &[u8]) -> Result<String, String> {
    use std::io::{Cursor, Read};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    // The zip reader and its DEFLATE decoder can panic on a crafted compressed stream,
    // so contain the whole parse exactly like extract_pdf — a hostile .docx becomes an
    // error, never a process crash.
    catch_unwind(AssertUnwindSafe(|| -> Result<String, String> {
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| format!("not a valid .docx (expected a ZIP): {e}"))?;
        // A real .docx holds a handful of parts; a five-figure entry count is a crafted
        // archive, not a document. Refuse it before doing any per-entry work.
        if zip.len() > 4096 {
            return Err("this .docx has an implausible number of entries".into());
        }
        let doc = zip.by_name("word/document.xml").map_err(|_| {
            "this .docx has no word/document.xml — is it really a Word file?".to_string()
        })?;
        let mut raw = Vec::new();
        doc.take(MAX_DOCX_XML_BYTES)
            .read_to_end(&mut raw)
            .map_err(|e| format!("read .docx body: {e}"))?;
        // take() caps the read at the limit; hitting it means the body was truncated, so
        // fail loudly rather than silently indexing a partial document.
        if raw.len() as u64 >= MAX_DOCX_XML_BYTES {
            return Err(format!(
                "this .docx body exceeds {} MB — too large to extract",
                MAX_DOCX_XML_BYTES / (1024 * 1024)
            ));
        }
        Ok(docx_xml_to_text(&String::from_utf8_lossy(&raw)))
    }))
    .unwrap_or_else(|_| Err("could not read this .docx (it may be malformed)".into()))
}

/// Convert WordprocessingML body XML to plain text: concatenate `<w:t>` run text, emit a
/// newline at each paragraph end and line break, and a tab for `<w:tab>`. quick-xml emits
/// entity references (`&amp;`, `&#160;`) as their own events, so they are resolved here too.
/// Lenient on malformed XML — stops at the first parse error rather than failing the whole import.
fn docx_xml_to_text(xml: &str) -> String {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    let mut reader = Reader::from_reader(xml.as_bytes());
    let mut out = String::new();
    let mut in_text = false;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"t" => in_text = true,
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"tab" => out.push('\t'),
                b"br" | b"cr" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Text(e)) if in_text => {
                if let Ok(decoded) = e.decode() {
                    out.push_str(&decoded);
                }
            }
            // Entity references inside a run: resolve numeric char refs and the five
            // predefined XML entities; ignore anything else (DOCX bodies use no others).
            Ok(Event::GeneralRef(e)) if in_text => match e.resolve_char_ref() {
                Ok(Some(c)) => out.push(c),
                _ => match &*e {
                    b"amp" => out.push('&'),
                    b"lt" => out.push('<'),
                    b"gt" => out.push('>'),
                    b"apos" => out.push('\''),
                    b"quot" => out.push('"'),
                    _ => {}
                },
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
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

/// Common English function words that pass the length filter but carry no topical
/// signal. Because `score` matches by substring across every chunk, leaving these in
/// lets an ordinary English question ("how do I configure the network?") score chunks
/// on "how"/"the"/"are" and crowd out the genuinely relevant ones. Short domain terms
/// (PID, fan, bus) are deliberately NOT here. Kept small and lowercase.
const STOP_WORDS: [&str; 38] = [
    "the", "and", "for", "are", "was", "were", "been", "being", "that", "this", "these", "those",
    "with", "from", "into", "onto", "its", "their", "them", "they", "you", "your", "our", "but",
    "not", "all", "any", "can", "how", "what", "where", "when", "which", "who", "why", "has",
    "have", "had",
];

/// Lowercased alphanumeric word tokens of length >= 3, minus common English stop-words
/// — so retrieval keys off meaningful terms, not the connective tissue of a sentence.
pub fn keywords(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3 && !STOP_WORDS.contains(t))
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
        assert!(extract_text("a.xlsx", b"PK\x03\x04 binary").is_err()); // unsupported kind
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
        // For this whitespace-free input no per-chunk trim removes content, so the
        // overlap makes the concatenation at least as long as the original.
        let joined: String = chunks.concat();
        assert!(joined.len() >= one_line.len());
    }

    #[test]
    fn keywords_and_score_rank_by_overlap() {
        let q = keywords("What Modbus slave is the Belimo actuator on?");
        assert!(q.contains("modbus"));
        assert!(q.contains("belimo"));
        assert!(!q.contains("is")); // short token dropped
        assert!(!q.contains("what")); // stop-word dropped
        assert!(!q.contains("the")); // stop-word dropped
        let hit = score(&q, "The Belimo actuator sits on Modbus slave 7.");
        let miss = score(&q, "The lobby scene fades over four seconds.");
        assert!(hit > miss);
        assert_eq!(score(&HashSet::new(), "anything"), 0);
    }

    /// Build a minimal `.docx` (a ZIP holding `word/document.xml`) for the extractor test.
    fn make_docx(body_xml: &str) -> Vec<u8> {
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("word/document.xml", opts).unwrap();
        zw.write_all(body_xml.as_bytes()).unwrap();
        zw.finish().unwrap().into_inner()
    }

    #[test]
    fn extract_docx_pulls_run_text_across_paragraphs() {
        // Two runs in one paragraph concatenate; <w:tab/> becomes a tab; the `&amp;`
        // entity unescapes to `&`; a second paragraph is a separate line.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>
<w:p><w:r><w:t>Belimo actuator on</w:t></w:r><w:r><w:t xml:space="preserve"> Modbus slave 7</w:t></w:r><w:r><w:tab/><w:t>A &amp; B</w:t></w:r></w:p>
<w:p><w:r><w:t>Wired to the LR24A.</w:t></w:r></w:p>
</w:body></w:document>"#;
        let text = extract_text("notes.docx", &make_docx(xml)).unwrap();
        assert!(
            text.contains("Belimo actuator on Modbus slave 7"),
            "runs should join within a paragraph; got: {text:?}"
        );
        assert!(
            text.contains('\t'),
            "<w:tab/> should emit a tab; got: {text:?}"
        );
        assert!(
            text.contains("A & B"),
            "&amp; should unescape; got: {text:?}"
        );
        assert!(text.contains("Wired to the LR24A."));
        // Distinct paragraphs end up on distinct lines (chunking/retrieval rely on this).
        assert!(text.contains('\n'));
    }

    #[test]
    fn extract_docx_rejects_non_zip() {
        // A `.docx` that isn't a ZIP is a clear error, not a panic or silent empty doc.
        let err = extract_text("bad.docx", b"this is plainly not a zip archive").unwrap_err();
        assert!(err.to_lowercase().contains("docx") || err.to_lowercase().contains("zip"));
    }

    #[test]
    fn extract_docx_without_document_xml_errors() {
        // A ZIP that is missing word/document.xml (e.g. an .xlsx renamed .docx) is rejected
        // with a clear message rather than yielding empty text.
        use std::io::Write;
        let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("word/styles.xml", opts).unwrap();
        zw.write_all(b"<styles/>").unwrap();
        let bytes = zw.finish().unwrap().into_inner();
        let err = extract_text("nodoc.docx", &bytes).unwrap_err();
        assert!(err.contains("document.xml"), "got: {err}");
    }

    #[test]
    fn extract_pdf_rejects_garbage_without_panicking() {
        // A malformed/hostile PDF must surface an error (via catch_unwind), never crash.
        let err = extract_text("bad.pdf", b"%PDF-1.7\nnot really a pdf body\n%%EOF").unwrap_err();
        assert!(err.to_lowercase().contains("pdf"), "got: {err}");
    }
}
