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

use crate::agent::Runtime;
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
/// `name` is its identifier, and `reason` is the human-readable "why now". `invoke`
/// is a concrete, copy-pasteable "how to run it" hint for the human operator or the
/// external agent runtime (a slash-command for a skill, or a server · tool pair for
/// an MCP). The recommendation itself is advisory; the honest execution bridge lives in
/// [`plan_invocation`], which can really launch ONE case — an ECC skill under a detected
/// Claude Code runtime (`claude -p "/skill"`). Every other pairing (a skill under
/// Codex/OpenRouter, or any MCP tool, which is callable only inside a configured agent
/// session) stays copy-and-run-yourself guidance. Serializes to a flat JSON object the
/// renderer lists directly (mirrors `suggest::Suggestion`); the ordered list returned by
/// [`recommend`] doubles as a step-by-step tool plan.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolRec {
    pub kind: &'static str,
    pub name: &'static str,
    pub reason: String,
    pub invoke: &'static str,
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
/// `invoke` carries the concrete "how to run it" hint (see [`ToolRec::invoke`]).
fn push_unique(
    out: &mut Vec<ToolRec>,
    kind: &'static str,
    name: &'static str,
    reason: String,
    invoke: &'static str,
) {
    if !out.iter().any(|r| r.kind == kind && r.name == name) {
        out.push(ToolRec {
            kind,
            name,
            reason,
            invoke,
        });
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
            "/comfortclick-bos",
        );
    } else if has_term(IOT_TERMS) {
        push_unique(
            &mut out,
            "skill",
            "comfortclick-bos",
            "The question is about building automation (Modbus/KNX/HVAC) — use the ComfortClick / IoT skill.".into(),
            "/comfortclick-bos",
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
            "/nutrient-document-processing",
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
            "/regex-vs-llm-structured-text",
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
            "/rust-review",
        );
    }

    // 5. Security review — anything about secrets, auth or credentials.
    if has_term(SECURITY_TERMS) {
        push_unique(
            &mut out,
            "skill",
            "security-review",
            "The question touches secrets, auth or credentials — run it through the security-review skill.".into(),
            "/security-review",
        );
    }

    // 6. Web fetch — needs live pages / vendor docs off the internet.
    if has_term(WEB_TERMS) {
        push_unique(
            &mut out,
            "mcp",
            "firecrawl",
            "You're after live web pages or vendor docs — the Firecrawl MCP server can search and fetch them.".into(),
            "firecrawl MCP · firecrawl_search / firecrawl_scrape",
        );
    }

    // 7. Browser driving — screenshots, DOM, performance audits.
    if has_term(BROWSER_TERMS) {
        push_unique(
            &mut out,
            "mcp",
            "chrome-devtools",
            "This involves a browser/page — the Chrome DevTools MCP server can drive it and capture screenshots.".into(),
            "chrome-devtools MCP · navigate_page / take_screenshot",
        );
    }

    out
}

// ----------------------------- invocation (M9 → execution bridge) -----------------------------
//
// The recommender above is advisory: it names a skill/MCP and a copy-pasteable `invoke`
// hint. This block takes the SMALLEST honest step toward execution. Hyperion already
// shells out to a local agent runtime for grounded Q&A (`agent::ask` → `claude -p` /
// `codex exec`); we reuse exactly that mechanism to actually launch a recommended ECC
// *skill* — but only under the one runtime that understands ECC slash-commands (Claude
// Code). Everything else (a skill under Codex/OpenRouter, or any MCP tool) stays
// advisory and is returned as copy-ready guidance, never silently "run".
//
// Security: the executed argument vector is built ONLY from `invoke_hint`'s compile-time
// catalog below — the caller passes the (kind,name) IDENTIFIERS of a recommendation, and
// the runnable command is re-derived here, so no operator/webview free-text ever reaches
// the process arguments. There is no shell (see `agent::run_invocation`).

/// Canonical, compile-time "how to run it" string for a known tool `(kind, name)`. The
/// single source of truth for the executable/advisory command, shared with `recommend`
/// (whose inline `invoke` values must stay equal to these — enforced by a test). Returns
/// `None` for any pair the recommender cannot emit, so an unknown request is a hard error
/// rather than an attempt to run an arbitrary string.
fn invoke_hint(kind: &str, name: &str) -> Option<&'static str> {
    match (kind, name) {
        ("skill", "comfortclick-bos") => Some("/comfortclick-bos"),
        ("skill", "nutrient-document-processing") => Some("/nutrient-document-processing"),
        ("skill", "regex-vs-llm-structured-text") => Some("/regex-vs-llm-structured-text"),
        ("skill", "rust-review") => Some("/rust-review"),
        ("skill", "security-review") => Some("/security-review"),
        ("mcp", "firecrawl") => Some("firecrawl MCP · firecrawl_search / firecrawl_scrape"),
        ("mcp", "chrome-devtools") => Some("chrome-devtools MCP · navigate_page / take_screenshot"),
        _ => None,
    }
}

/// A concrete plan for acting on one recommendation under the detected runtime. Serializes
/// to a flat JSON object for the webview. `executable` is the load-bearing flag: it is
/// `true` ONLY when Hyperion can really run this here (an ECC skill under Claude Code);
/// otherwise `display`/`note` carry copy-ready guidance and `args` is empty.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InvocationPlan {
    pub kind: String,
    pub name: String,
    /// Label of the runtime the plan was built for (e.g. "Claude Code").
    pub runtime: &'static str,
    /// True only when `args` is a real, runnable command for a detected runtime.
    pub executable: bool,
    /// The runtime binary NAME to resolve+run when executable (e.g. "claude"); empty otherwise.
    pub program: &'static str,
    /// Constant argv TAIL (the program path is prepended by the caller). Empty unless executable.
    pub args: Vec<String>,
    /// Copy-ready terminal command (executable case) or the advisory hint (otherwise).
    pub display: String,
    /// Plain-language statement of what this does — real execution vs. advisory-only.
    pub note: String,
}

/// Build the [`InvocationPlan`] for a recommended tool under `runtime`. Pure: same
/// `(kind, name, runtime)` → same plan, no I/O. Real execution is possible only for an
/// ECC skill (a `/slash` command) under [`Runtime::ClaudeCode`], the sole backend that
/// understands ECC slash-commands; every other pairing yields an advisory plan. Errors
/// only when `(kind, name)` is not a tool the recommender can emit.
pub fn plan_invocation(kind: &str, name: &str, runtime: Runtime) -> Result<InvocationPlan, String> {
    let invoke = invoke_hint(kind, name).ok_or_else(|| format!("unknown tool: {kind}/{name}"))?;
    let base = |executable: bool,
                program: &'static str,
                args: Vec<String>,
                display: String,
                note: String| {
        InvocationPlan {
            kind: kind.to_string(),
            name: name.to_string(),
            runtime: runtime.label(),
            executable,
            program,
            args,
            display,
            note,
        }
    };

    // An ECC skill is a slash-command; that is the only thing we can actually launch.
    let is_skill_slash = kind == "skill" && invoke.starts_with('/');
    if is_skill_slash && runtime == Runtime::ClaudeCode {
        // `claude -p "/skill"`: a one-shot, headless (print-mode) Claude Code session.
        // `invoke` is a static catalog string, so passing it as a positional argument
        // cannot smuggle flags or shell syntax.
        return Ok(base(
            true,
            "claude",
            vec!["-p".to_string(), invoke.to_string()],
            format!("claude -p \"{invoke}\""),
            "Real execution: Hyperion launches the skill as a one-shot headless Claude Code session (claude -p) and returns its output.".to_string(),
        ));
    }
    if is_skill_slash {
        // A skill under Codex/OpenRouter: those runtimes have no ECC skills, so Hyperion
        // cannot run it — surface the copy-ready Claude Code command as guidance.
        return Ok(base(
            false,
            "",
            Vec::new(),
            format!("claude -p \"{invoke}\""),
            format!(
                "Advisory only: ECC skills run inside Claude Code, but the active runtime is {}. Hyperion will not run this — paste the command above into a Claude Code session.",
                runtime.label()
            ),
        ));
    }
    // MCP (or any non-slash hint): an MCP tool is callable only by an agent inside a
    // session that already has that server configured. Hyperion shows the call, never runs it.
    Ok(base(
        false,
        "",
        Vec::new(),
        invoke.to_string(),
        "Advisory only: MCP tools are invoked by the agent inside a session that has this server configured. Hyperion surfaces the call but does not execute it.".to_string(),
    ))
}

/// What `lib::run_tool` should do, decided purely from the plan, whether the operator
/// confirmed execution, and whether the runtime binary is actually present on PATH. Kept
/// as a pure function so every branch — including the *not-installed* path — is unit-
/// testable without spawning a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// No execution requested: return the plan for the UI to copy/run.
    PlanOnly,
    /// Execution requested but the plan is advisory: return guidance, run nothing.
    AdvisoryBlocked,
    /// Execution requested for a runnable plan, but the runtime binary is not on PATH.
    MissingBinary,
    /// Cleared to actually run the command.
    Execute,
}

/// Pure gate for the tool runner. `execute` is the operator's explicit confirmation;
/// `binary_present` is whether the plan's runtime binary was just found on PATH.
pub fn run_gate(plan: &InvocationPlan, execute: bool, binary_present: bool) -> RunOutcome {
    if !execute {
        RunOutcome::PlanOnly
    } else if !plan.executable {
        RunOutcome::AdvisoryBlocked
    } else if !binary_present {
        RunOutcome::MissingBinary
    } else {
        RunOutcome::Execute
    }
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
        assert!(
            v.get("kind").is_some()
                && v.get("name").is_some()
                && v.get("reason").is_some()
                && v.get("invoke").is_some()
        );
    }

    #[test]
    fn every_recommendation_carries_an_actionable_invoke_hint() {
        // A broad context that fires several branches; every emitted rec must carry a
        // concrete, non-empty "how to run it" hint so the ordered list reads as a plan.
        let recs = recommend(&input(
            true,
            &["pdf", "csv"],
            "review this Rust code, check the API token, and search the web for the datasheet in the browser",
        ));
        assert!(recs.len() >= 5, "expected several recs, got: {recs:?}");
        for r in &recs {
            assert!(!r.invoke.trim().is_empty(), "blank invoke for {:?}", r.name);
        }
        // Skills surface as a slash-command; MCP servers name the server.
        let bos = recs.iter().find(|r| r.name == "comfortclick-bos").unwrap();
        assert_eq!(bos.invoke, "/comfortclick-bos");
        let fc = recs.iter().find(|r| r.name == "firecrawl").unwrap();
        assert!(fc.invoke.contains("firecrawl"), "got: {}", fc.invoke);
    }

    // ---- invocation bridge ----

    #[test]
    fn invoke_hint_is_the_single_source_of_truth_for_every_recommendation() {
        // A broad context that fires every branch; each emitted rec's inline `invoke`
        // must equal the canonical catalog string used to build runnable commands, so
        // the two can never drift.
        let recs = recommend(&input(
            true,
            &["pdf", "csv"],
            "review this Rust code, check the API token, and search the web for the datasheet in the browser",
        ));
        assert!(recs.len() >= 5, "expected several recs, got: {recs:?}");
        for r in &recs {
            assert_eq!(
                invoke_hint(r.kind, r.name),
                Some(r.invoke),
                "catalog drift for {}/{}",
                r.kind,
                r.name
            );
        }
    }

    #[test]
    fn skill_under_claude_code_is_executable_with_a_static_argv() {
        let plan = plan_invocation("skill", "comfortclick-bos", Runtime::ClaudeCode).unwrap();
        assert!(plan.executable);
        assert_eq!(plan.program, "claude");
        // The argv is exactly the constant flag + the static slash command — no caller text.
        assert_eq!(
            plan.args,
            vec!["-p".to_string(), "/comfortclick-bos".to_string()]
        );
        assert!(plan.display.contains("/comfortclick-bos"));
        assert!(plan.note.to_lowercase().contains("real execution"));
    }

    #[test]
    fn skill_under_non_claude_runtimes_is_advisory_only() {
        for rt in [Runtime::Codex, Runtime::OpenRouter] {
            let plan = plan_invocation("skill", "rust-review", rt).unwrap();
            assert!(!plan.executable, "{:?} should not execute a skill", rt);
            assert!(plan.args.is_empty());
            assert_eq!(plan.program, "");
            assert!(plan.note.to_lowercase().contains("advisory"));
        }
    }

    #[test]
    fn mcp_tool_is_advisory_under_every_runtime() {
        for rt in [Runtime::ClaudeCode, Runtime::Codex, Runtime::OpenRouter] {
            let plan = plan_invocation("mcp", "firecrawl", rt).unwrap();
            assert!(!plan.executable, "MCP must never be executed ({rt:?})");
            assert!(plan.args.is_empty());
            assert!(plan.display.contains("firecrawl"));
            assert!(plan.note.to_lowercase().contains("advisory"));
        }
    }

    #[test]
    fn unknown_tool_is_a_hard_error() {
        assert!(plan_invocation("skill", "no-such-skill", Runtime::ClaudeCode).is_err());
        assert!(plan_invocation("mcp", "no-such-mcp", Runtime::ClaudeCode).is_err());
    }

    #[test]
    fn run_gate_covers_planonly_advisory_missing_binary_and_execute() {
        let exec_plan = plan_invocation("skill", "security-review", Runtime::ClaudeCode).unwrap();
        let advisory_plan = plan_invocation("mcp", "chrome-devtools", Runtime::ClaudeCode).unwrap();

        // No confirmation → just return the plan, regardless of executability.
        assert_eq!(run_gate(&exec_plan, false, true), RunOutcome::PlanOnly);
        // Confirmed, but the plan is advisory → blocked, never run.
        assert_eq!(
            run_gate(&advisory_plan, true, true),
            RunOutcome::AdvisoryBlocked
        );
        // Confirmed runnable plan, but the binary is NOT installed → missing-binary path.
        assert_eq!(run_gate(&exec_plan, true, false), RunOutcome::MissingBinary);
        // Confirmed, runnable, binary present → cleared to execute.
        assert_eq!(run_gate(&exec_plan, true, true), RunOutcome::Execute);
    }
}
