// Hyperion — context-driven MCP/skill recommender (M9, Milestone #13).
//
// A deterministic, fully local mapper that looks at what the operator currently has
// loaded — whether a `.bos` configuration is open, the kinds of ingested context
// files (pdf/docx/csv/…), and the pending question — and suggests which MCP servers
// and ECC skills are most useful right now. It is the offline counterpart to the
// context suggester (`suggest.rs`): that one tells the operator what *grounding* is
// missing; this one tells them what *tools* to reach for.
//
// `recommend` is pure: same input → same output, no network, no I/O, no clock. The
// `recommend_tools` command in `lib.rs` gathers the inputs from live state and calls
// it. Output is a small ordered, de-duplicated list of `ToolRec { kind, name, reason }`
// values that serialize straight to JSON for the webview. Read-only toward bOS.

use serde::{Deserialize, Serialize};

use crate::ingest;

/// What the recommender knows about the current context. `has_bos` is whether a
/// configuration is loaded; `context_file_kinds` are the lowercased extensions of
/// the active project's ingested files (e.g. `["pdf", "csv"]`); `query` is the
/// operator's pending question (may be empty). Deserializable so the webview can
/// also drive it directly, though `recommend_tools` builds it server-side.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ToolingInput {
    #[serde(default)]
    pub has_bos: bool,
    #[serde(default)]
    pub context_file_kinds: Vec<String>,
    #[serde(default)]
    pub query: String,
}

/// One recommendation. `kind` is `"mcp"` (a server) or `"skill"` (an ECC skill),
/// `name` is its identifier, and `reason` is the human-readable "why now". Serializes
/// to a flat JSON object the renderer can list directly (mirrors `suggest::Suggestion`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolRec {
    pub kind: &'static str,
    pub name: &'static str,
    pub reason: String,
}

// ---- trigger vocabularies (lowercased word tokens, matched via ingest::keywords) ----

/// Building-automation / IoT vocabulary — the ComfortClick/bOS domain.
const IOT_TERMS: &[&str] = &[
    "modbus",
    "knx",
    "bacnet",
    "building",
    "automation",
    "hvac",
    "sensor",
    "actuator",
    "relay",
    "dimmer",
    "thermostat",
    "comfortclick",
    "lighting",
    "scene",
    "plc",
    "mqtt",
    "zigbee",
    "heating",
    "cooling",
    "setpoint",
    "gateway",
];

/// Code / software-engineering vocabulary — routes to a review skill.
const CODE_TERMS: &[&str] = &[
    "code",
    "rust",
    "cargo",
    "clippy",
    "function",
    "refactor",
    "compile",
    "compiler",
    "borrow",
    "async",
    "trait",
    "struct",
    "lint",
    "typescript",
    "javascript",
    "review",
    "panic",
    "stacktrace",
];

/// Secret / authentication vocabulary — routes to the security-review skill.
const SECURITY_TERMS: &[&str] = &[
    "secret",
    "secrets",
    "password",
    "credential",
    "credentials",
    "vault",
    "token",
    "auth",
    "authentication",
    "encrypt",
    "encryption",
    "vulnerability",
    "security",
    "certificate",
    "tls",
];

/// "I need something off the live web" vocabulary — routes to the Firecrawl MCP.
const WEB_TERMS: &[&str] = &[
    "web",
    "search",
    "online",
    "internet",
    "datasheet",
    "manual",
    "vendor",
    "documentation",
    "lookup",
    "website",
    "url",
];

/// Browser-driving vocabulary — routes to the Chrome DevTools MCP.
const BROWSER_TERMS: &[&str] = &[
    "browser",
    "screenshot",
    "devtools",
    "webpage",
    "lighthouse",
    "render",
];

/// Document-format kinds whose extraction benefits from a document skill.
const DOC_KINDS: &[&str] = &["pdf", "docx"];

/// Structured-data kinds (tabular / serialized) that a parsing skill handles well.
const DATA_KINDS: &[&str] = &["csv", "tsv", "json", "xml", "yaml", "yml"];

/// Push a recommendation unless one with the same `(kind, name)` is already present,
/// so overlapping signals (e.g. a loaded `.bos` *and* an IoT question) yield one entry.
fn push_unique(out: &mut Vec<ToolRec>, kind: &'static str, name: &'static str, reason: String) {
    if !out.iter().any(|r| r.kind == kind && r.name == name) {
        out.push(ToolRec { kind, name, reason });
    }
}

/// Map the current context to an ordered, de-duplicated list of tool recommendations.
/// Pure and deterministic: it tokenizes `query` exactly like the retriever
/// (`ingest::keywords`) and matches the loaded-file kinds, never touching the network
/// or the filesystem. Ordering is by domain priority (IoT → documents → data → code →
/// security → web → browser) so the most context-defining suggestion leads.
pub fn recommend(input: &ToolingInput) -> Vec<ToolRec> {
    let terms = ingest::keywords(&input.query);
    let has_term = |opts: &[&str]| opts.iter().any(|t| terms.contains(*t));

    // Normalize the file kinds once (trim + lowercase) for membership tests.
    let kinds: Vec<String> = input
        .context_file_kinds
        .iter()
        .map(|k| k.trim().to_ascii_lowercase())
        .collect();
    let present = |opts: &[&'static str]| -> Vec<&'static str> {
        opts.iter()
            .copied()
            .filter(|k| kinds.iter().any(|x| x == k))
            .collect()
    };

    let mut out: Vec<ToolRec> = Vec::new();

    // 1. ComfortClick / IoT — the primary domain. A loaded `.bos` is the strongest
    // signal; failing that, an IoT-flavoured question still routes here.
    if input.has_bos {
        push_unique(
            &mut out,
            "skill",
            "comfortclick-bos",
            "A bOS configuration is loaded — lean on the ComfortClick / IoT building-automation skill for object, logic and Modbus/KNX guidance.".into(),
        );
    } else if has_term(IOT_TERMS) {
        push_unique(
            &mut out,
            "skill",
            "comfortclick-bos",
            "The question is about building automation (Modbus/KNX/HVAC) — use the ComfortClick / IoT skill.".into(),
        );
    }

    // 2. Document processing — PDFs / Word docs in the context store extract more
    // reliably (tables, structured specs) through a dedicated document skill.
    let docs = present(DOC_KINDS);
    if !docs.is_empty() {
        push_unique(
            &mut out,
            "skill",
            "nutrient-document-processing",
            format!(
                "{} context file(s) are loaded — a document-processing skill pulls tables and structured specs more reliably than plain text.",
                docs.join("/").to_ascii_uppercase()
            ),
        );
    }

    // 3. Structured data — CSV/JSON/XML/… parse deterministically with a structured-text
    // skill rather than free-form LLM reading.
    let data = present(DATA_KINDS);
    if !data.is_empty() {
        push_unique(
            &mut out,
            "skill",
            "regex-vs-llm-structured-text",
            format!(
                "Structured data files ({}) are loaded — use a structured-text skill to parse them deterministically.",
                data.join("/")
            ),
        );
    }

    // 4. Code review — a software question wants the review skill.
    if has_term(CODE_TERMS) {
        push_unique(
            &mut out,
            "skill",
            "rust-review",
            "This looks like a code question — reach for the code-review / rust-review skill."
                .into(),
        );
    }

    // 5. Security review — anything about secrets, auth or credentials.
    if has_term(SECURITY_TERMS) {
        push_unique(
            &mut out,
            "skill",
            "security-review",
            "The question touches secrets, auth or credentials — run it through the security-review skill.".into(),
        );
    }

    // 6. Web fetch — needs live pages / vendor docs off the internet.
    if has_term(WEB_TERMS) {
        push_unique(
            &mut out,
            "mcp",
            "firecrawl",
            "You're after live web pages or vendor docs — the Firecrawl MCP server can search and fetch them.".into(),
        );
    }

    // 7. Browser driving — screenshots, DOM, performance audits.
    if has_term(BROWSER_TERMS) {
        push_unique(
            &mut out,
            "mcp",
            "chrome-devtools",
            "This involves a browser/page — the Chrome DevTools MCP server can drive it and capture screenshots.".into(),
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an input from the three signals; `query` is split into the same tokens
    /// the retriever uses, so tests read like the real call sites.
    fn input(has_bos: bool, kinds: &[&str], query: &str) -> ToolingInput {
        ToolingInput {
            has_bos,
            context_file_kinds: kinds.iter().map(|s| s.to_string()).collect(),
            query: query.to_string(),
        }
    }

    fn names(recs: &[ToolRec]) -> Vec<&str> {
        recs.iter().map(|r| r.name).collect()
    }

    fn has(recs: &[ToolRec], kind: &str, name: &str) -> bool {
        recs.iter().any(|r| r.kind == kind && r.name == name)
    }

    #[test]
    fn empty_context_yields_no_recommendations() {
        let recs = recommend(&input(false, &[], ""));
        assert!(recs.is_empty(), "got: {:?}", names(&recs));
    }

    #[test]
    fn loaded_bos_recommends_the_iot_skill() {
        let recs = recommend(&input(true, &[], ""));
        assert!(has(&recs, "skill", "comfortclick-bos"), "got: {recs:?}");
    }

    #[test]
    fn iot_query_recommends_the_iot_skill_without_a_bos() {
        let recs = recommend(&input(false, &[], "how do I poll a Modbus slave for HVAC?"));
        assert!(has(&recs, "skill", "comfortclick-bos"), "got: {recs:?}");
    }

    #[test]
    fn pdf_context_recommends_a_document_skill_and_mentions_the_kind() {
        let recs = recommend(&input(false, &["pdf"], ""));
        assert!(
            has(&recs, "skill", "nutrient-document-processing"),
            "got: {recs:?}"
        );
        let rec = recs
            .iter()
            .find(|r| r.name == "nutrient-document-processing")
            .unwrap();
        assert!(rec.reason.contains("PDF"), "reason: {}", rec.reason);
    }

    #[test]
    fn structured_data_context_recommends_a_parsing_skill() {
        let recs = recommend(&input(false, &["csv"], ""));
        assert!(
            has(&recs, "skill", "regex-vs-llm-structured-text"),
            "got: {recs:?}"
        );
    }

    #[test]
    fn code_question_recommends_a_review_skill() {
        let recs = recommend(&input(
            false,
            &[],
            "this Rust function won't compile — borrow error",
        ));
        assert!(has(&recs, "skill", "rust-review"), "got: {recs:?}");
    }

    #[test]
    fn security_question_recommends_the_security_skill() {
        let recs = recommend(&input(
            false,
            &[],
            "where should I store this API token / credential?",
        ));
        assert!(has(&recs, "skill", "security-review"), "got: {recs:?}");
    }

    #[test]
    fn web_question_recommends_the_firecrawl_mcp() {
        let recs = recommend(&input(
            false,
            &[],
            "search the web for the Belimo datasheet",
        ));
        assert!(has(&recs, "mcp", "firecrawl"), "got: {recs:?}");
    }

    #[test]
    fn browser_question_recommends_the_devtools_mcp() {
        let recs = recommend(&input(
            false,
            &[],
            "take a screenshot of the page in the browser",
        ));
        assert!(has(&recs, "mcp", "chrome-devtools"), "got: {recs:?}");
    }

    #[test]
    fn overlapping_signals_do_not_duplicate_a_recommendation() {
        // A loaded .bos AND an IoT question both point at the same skill -> one entry.
        let recs = recommend(&input(true, &[], "configure the KNX lighting scene"));
        let count = recs.iter().filter(|r| r.name == "comfortclick-bos").count();
        assert_eq!(count, 1, "got: {recs:?}");
    }

    #[test]
    fn recommendations_are_ordered_by_domain_priority() {
        // IoT (bos) should lead a code-flavoured question, and the skill precedes the MCP.
        let recs = recommend(&input(
            true,
            &["pdf"],
            "review this Rust code and search the web",
        ));
        let order = names(&recs);
        let pos = |n: &str| order.iter().position(|x| *x == n).unwrap();
        assert!(
            pos("comfortclick-bos") < pos("rust-review"),
            "got: {order:?}"
        );
        assert!(pos("rust-review") < pos("firecrawl"), "got: {order:?}");
    }

    #[test]
    fn recommendation_serializes_to_a_flat_json_object() {
        let recs = recommend(&input(true, &[], ""));
        let v = serde_json::to_value(&recs[0]).unwrap();
        assert!(v.get("kind").is_some() && v.get("name").is_some() && v.get("reason").is_some());
    }
}
