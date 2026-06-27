// Hyperion — deterministic security scan + enterprise-readiness gate (M6,
// Milestone #9, Requirements #32 / #37 / #45).
//
// Two pure pieces live here, both fully unit-testable with synthetic input and
// touching no filesystem and no network:
//
//   * `scan_source` flags security risks in a batch of in-memory
//     `(path, contents)` source pairs — hardcoded secrets (reusing the vault's
//     own credential heuristics so the two stay in lockstep), `unsafe` Rust
//     blocks, risky web APIs (`eval(`, raw-HTML sinks), and unresolved risk
//     markers left in comments. Returns one `Finding` per hit.
//
//   * `enterprise_gate` evaluates a handful of boolean/count enterprise-readiness
//     criteria (encrypted vault, SSO, CI, tests, zero plaintext secrets) into a
//     pass/fail `GateResult` with a per-item explanation the UI can render.
//
// The two Tauri commands in `lib.rs` (`security_scan` / `enterprise_gate_check`)
// are the only places that read the project's own sources off disk and observe
// the running app's state; the logic here stays pure. Strictly read-only with
// respect to bOS, like the rest of this layer.

use serde::Serialize;

use crate::vault;

/// One flagged security risk. `path` is the source file (a repo-relative display
/// path), `line` is 1-based, `kind` is the stable rule id, `severity` ranks it
/// ("high" | "medium" | "low"), and `message` explains the risk and the fix.
/// Serializes to a flat JSON object the renderer can list directly, matching the
/// shape used by `standard::Finding`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    pub kind: &'static str,
    pub severity: &'static str,
    pub message: String,
}

impl Finding {
    fn new(
        path: &str,
        line: usize,
        kind: &'static str,
        severity: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Finding {
            path: path.to_string(),
            line,
            kind,
            severity,
            message: message.into(),
        }
    }
}

/// The stable `kind` for a high-confidence hardcoded secret. The enterprise gate
/// counts findings with this kind as blocking; the looser `hardcoded-credential`
/// (a `name = value` heuristic) is informational and never gates a release.
pub const HARDCODED_SECRET_KIND: &str = "hardcoded-secret";

/// Which language a file is scanned as, decided purely by extension. `Other`
/// files are skipped entirely (the scan never invents rules for formats it
/// doesn't understand). Mirrors `standard::Lang`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Ts,
    Other,
}

impl Lang {
    fn from_path(path: &str) -> Self {
        let lower = path.to_lowercase();
        if lower.ends_with(".rs") {
            Lang::Rust
        } else if lower.ends_with(".ts") {
            Lang::Ts
        } else {
            Lang::Other
        }
    }
}

/// Scan a batch of source files for security risks and return every finding in a
/// deterministic order (files in input order; within a file, by ascending line
/// then by the order the rules are checked). Pure: the input is the full file
/// contents, so there is no filesystem or network access here.
///
/// Rules, each chosen to be low-noise:
///   * Any code line — a hardcoded secret, detected by reusing the vault's
///     `scan_for_secrets` structural heuristics plus the bare vendor-prefix key
///     check (`sk-…`, `ghp_…`).
///   * Rust code line — an `unsafe` block/fn/impl.
///   * TypeScript code line — `eval(...)` and raw-HTML sinks (`.innerHTML`,
///     `dangerouslySetInnerHTML`, `document.write(`).
///   * Any comment line — an unresolved risk marker (FIXME / XXX / HACK /
///     INSECURE) that should be cleared before an enterprise release.
///
/// Two exemptions keep the self-scan honest and quiet:
///   * Comment lines are exempt from the *pattern* rules (secrets / unsafe /
///     web sinks), so prose that merely discusses a credential — like this file —
///     is never flagged; the risk-marker rule, which only fires *on* comments, is
///     applied first.
///   * In Rust, everything after a real `#[cfg(test)]` attribute is treated as
///     test code and skipped: synthetic fixtures (fake keys, deliberate `unsafe`)
///     are not real risks. This mirrors `standard::audit`.
pub fn scan_source(files: &[(String, String)]) -> Vec<Finding> {
    let mut out = Vec::new();
    for (path, contents) in files {
        match Lang::from_path(path) {
            Lang::Other => continue,
            lang => scan_file(path, contents, lang, &mut out),
        }
    }
    dedup_findings(out)
}

/// Collapse findings that repeat the same `(path, line, kind)` to a single hit,
/// preserving first-seen order. A single line can match two rules of the same
/// kind — e.g. `Authorization: "Bearer sk-…"` trips both the structural secret
/// heuristic and the bare vendor-prefix check — and without this the enterprise
/// gate would count one secret twice and inflate the blocking total. `(path,
/// line, kind)` (not just `(line, kind)`) keeps identical line numbers in
/// different files independent.
fn dedup_findings(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen = std::collections::HashSet::new();
    findings
        .into_iter()
        .filter(|f| seen.insert((f.path.clone(), f.line, f.kind)))
        .collect()
}

fn scan_file(path: &str, contents: &str, lang: Lang, out: &mut Vec<Finding>) {
    let mut in_test = false;

    for (idx, line) in contents.lines().enumerate() {
        let line_no = idx + 1;

        // Only a real attribute flips us into the test region — a comment that
        // merely mentions the attribute must not suppress later findings.
        if lang == Lang::Rust && line.contains("#[cfg(test)]") && !is_comment_line(line) {
            in_test = true;
        }
        if in_test {
            continue;
        }

        // Risk markers live *in* comments, so check them before the comment skip.
        scan_risk_markers(path, line_no, line, out);

        if is_comment_line(line) {
            continue;
        }

        // Secrets can hide in any language's string literals.
        scan_secrets_line(path, line_no, line, out);

        match lang {
            Lang::Rust => scan_rust_line(path, line_no, line, out),
            Lang::Ts => scan_ts_line(path, line_no, line, out),
            Lang::Other => {}
        }
    }
}

/// Hardcoded-secret rule: reuse the vault's credential heuristics so the scanner
/// and the write-time guardrail flag the exact same shapes. The structural scan
/// (`scan_for_secrets`) yields already-*masked* details, so a finding never
/// echoes the raw secret; the vendor-prefix check (`token_looks_like_secret`)
/// adds bare keys like `sk-or-…`/`ghp_…` that carry no surrounding marker.
fn scan_secrets_line(path: &str, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    for f in vault::scan_for_secrets(line) {
        let kind = f.get("kind").and_then(|k| k.as_str()).unwrap_or("secret");
        let detail = f.get("detail").and_then(|d| d.as_str()).unwrap_or("");
        // The high-confidence vault kinds are blocking; `credential_assignment`
        // is a looser `name = value` heuristic, surfaced as informational.
        let (id, severity) = if vault::HIGH_CONFIDENCE_SECRET_KINDS.contains(&kind) {
            (HARDCODED_SECRET_KIND, "high")
        } else {
            ("hardcoded-credential", "medium")
        };
        out.push(Finding::new(
            path,
            line_no,
            id,
            severity,
            format!(
                "Possible hardcoded {kind} in source ({detail}). Move it into the encrypted vault."
            ),
        ));
    }
    // Bare vendor-prefixed API keys not caught by the structural markers above.
    for token in line.split_whitespace() {
        if vault::token_looks_like_secret(token) {
            out.push(Finding::new(
                path,
                line_no,
                HARDCODED_SECRET_KIND,
                "high",
                format!(
                    "Possible hardcoded API key in source ({}). Move it into the encrypted vault.",
                    vault::mask(token)
                ),
            ));
        }
    }
}

fn scan_rust_line(path: &str, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    // Match the `unsafe` keyword as a standalone word so identifiers like
    // `unsafe_code` (e.g. in a `#![forbid(unsafe_code)]` attribute) never trip it.
    if contains_word(line, "unsafe") {
        out.push(Finding::new(
            path,
            line_no,
            "unsafe-block",
            "medium",
            "`unsafe` code needs a `// SAFETY:` justification and extra review — \
             prefer a safe abstraction where possible.",
        ));
    }
}

fn scan_ts_line(path: &str, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    if line.contains("eval(") {
        out.push(Finding::new(
            path,
            line_no,
            "risky-eval",
            "high",
            "Avoid `eval(...)` — it executes arbitrary code. Use a structured \
             parser (e.g. `JSON.parse`) instead.",
        ));
    }
    if line.contains(".innerHTML")
        || line.contains("dangerouslySetInnerHTML")
        || line.contains("document.write(")
    {
        out.push(Finding::new(
            path,
            line_no,
            "raw-html-sink",
            "medium",
            "Assigning untrusted HTML enables XSS — set `textContent` or sanitize \
             the input before inserting it.",
        ));
    }
}

/// Unresolved risk markers that should be cleared before an enterprise release.
/// Matched only on comment lines (where developers leave them); the array's own
/// definition is code, so the scanner never flags this very line.
const RISK_MARKERS: [&str; 4] = ["FIXME", "XXX", "HACK", "INSECURE"];

fn scan_risk_markers(path: &str, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    if !is_comment_line(line) {
        return;
    }
    if let Some(marker) = RISK_MARKERS.iter().find(|m| line.contains(**m)) {
        out.push(Finding::new(
            path,
            line_no,
            "risk-marker",
            "low",
            format!(
                "`{marker}` marker flags unresolved or risky code — resolve it before \
                 an enterprise release."
            ),
        ));
    }
}

/// True if `word` (ASCII) appears in `line` bounded by non-identifier characters
/// on both sides, so `unsafe` matches in `unsafe {` / `unsafe fn` but not inside
/// `unsafe_code`. Byte indexing is safe here: `word` is ASCII, and a UTF-8
/// continuation byte (`>= 0x80`) is correctly treated as a word boundary.
fn contains_word(line: &str, word: &str) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(pos) = line[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
        if from >= line.len() {
            break;
        }
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// True if the line, ignoring indentation, is (or starts as) a comment — used to
/// exempt prose from the pattern rules. Covers `//` line comments and `/*` / `*`
/// block-comment lines, which is all Hyperion's sources use. Mirrors
/// `standard::is_comment_line`.
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
}

/// Count the high-confidence hardcoded secrets in a scan result — the blocking
/// input the enterprise gate consumes. The looser `hardcoded-credential` kind is
/// intentionally excluded so a `name = value` false positive never fails a gate.
pub fn plaintext_secret_count(findings: &[Finding]) -> usize {
    findings
        .iter()
        .filter(|f| f.kind == HARDCODED_SECRET_KIND)
        .count()
}

// ----------------------------- enterprise gate -----------------------------

/// Simple, observable inputs to the enterprise-readiness gate. Each is a boolean
/// the app already knows or a count from the source scan — keeping the gate logic
/// pure and trivially testable.
#[derive(Debug, Clone)]
pub struct EnterpriseInputs {
    /// An encrypted (AES-256-GCM) vault has been provisioned.
    pub vault_encrypted: bool,
    /// Access is gated behind Microsoft Entra single sign-on.
    pub sso_enabled: bool,
    /// A continuous-integration workflow is present in the repo.
    pub ci_present: bool,
    /// The codebase ships automated (`#[cfg(test)]`) tests.
    pub tests_present: bool,
    /// High-confidence hardcoded secrets found in source (0 to pass).
    pub plaintext_secret_findings: usize,
}

/// One evaluated criterion: its display `name`, whether it passed (`ok`), and a
/// human-readable `detail` explaining the verdict or the remediation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateItem {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl GateItem {
    fn build(name: &str, ok: bool, pass_detail: &str, fail_detail: String) -> Self {
        GateItem {
            name: name.to_string(),
            ok,
            detail: if ok {
                pass_detail.to_string()
            } else {
                fail_detail
            },
        }
    }
}

/// The gate verdict: `passed` is true only when every criterion is `ok`, with the
/// full per-item breakdown for the UI. Serializes to
/// `{ "passed": bool, "items": [{ "name", "ok", "detail" }, ...] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateResult {
    pub passed: bool,
    pub items: Vec<GateItem>,
}

/// Evaluate enterprise-readiness criteria into a pass/fail result. Pure over the
/// inputs: the gate passes only when all criteria hold. Each item carries a
/// concrete detail so the UI can show exactly what to fix.
pub fn enterprise_gate(criteria: &EnterpriseInputs) -> GateResult {
    let items = vec![
        GateItem::build(
            "Encrypted secrets vault",
            criteria.vault_encrypted,
            "Secrets are sealed at rest with AES-256-GCM under an OS-keychain key.",
            "Provision the encrypted vault before storing any credentials.".to_string(),
        ),
        GateItem::build(
            "Single sign-on (Microsoft Entra)",
            criteria.sso_enabled,
            "Access is gated behind a Microsoft Entra sign-in.",
            "Sign in with Microsoft Entra to enable SSO-gated access.".to_string(),
        ),
        GateItem::build(
            "Continuous integration",
            criteria.ci_present,
            "A CI workflow runs the build, lints, and tests.",
            "Add a CI workflow that runs `cargo fmt`, `clippy`, and the tests.".to_string(),
        ),
        GateItem::build(
            "Automated tests",
            criteria.tests_present,
            "The codebase ships `#[cfg(test)]` coverage.",
            "Add automated tests before an enterprise release.".to_string(),
        ),
        GateItem::build(
            "No plaintext secrets in source",
            criteria.plaintext_secret_findings == 0,
            "The source scan found no hardcoded credentials.",
            format!(
                "Remove the {} hardcoded secret(s) flagged by the source scan.",
                criteria.plaintext_secret_findings
            ),
        ),
    ];
    let passed = items.iter().all(|i| i.ok);
    GateResult { passed, items }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|x| x.kind).collect()
    }

    fn has(f: &[Finding], kind: &str) -> bool {
        f.iter().any(|x| x.kind == kind)
    }

    #[test]
    fn flags_hardcoded_secrets_in_any_language() {
        // A bare vendor-prefixed key in a Rust string literal.
        let rs = "fn run() {\n    let key = \"sk-or-v1-abc123def456ghi789jkl012mno345\";\n}\n";
        let f = scan_source(&[("src-tauri/src/lib.rs".into(), rs.into())]);
        assert!(has(&f, "hardcoded-secret"), "got: {:?}", kinds(&f));
        let hit = f.iter().find(|x| x.kind == "hardcoded-secret").unwrap();
        assert_eq!(hit.line, 2);
        assert_eq!(hit.severity, "high");
        // The finding must never echo the raw key.
        assert!(
            !hit.message.contains("abc123def456"),
            "secret leaked: {}",
            hit.message
        );

        // A structural Authorization/Bearer token in TypeScript.
        let ts = "const h = {\n  Authorization: \"Bearer abcdef0123456789xyz\",\n};\n";
        let g = scan_source(&[("src/api.ts".into(), ts.into())]);
        assert!(has(&g, "hardcoded-secret"), "got: {:?}", kinds(&g));
    }

    #[test]
    fn deduplicates_a_doubly_matching_secret_line() {
        // `Authorization: "Bearer sk-or-…"` trips both the structural secret
        // heuristic and the bare vendor-prefix check on the same line. The line
        // must yield exactly one `hardcoded-secret`, so the gate counts it once.
        let rs = "fn run() {\n    let h = \"Authorization: Bearer sk-or-v1-abc123def456ghi789jkl012\";\n}\n";
        let f = scan_source(&[("src-tauri/src/lib.rs".into(), rs.into())]);
        let secrets: Vec<&Finding> = f
            .iter()
            .filter(|x| x.kind == HARDCODED_SECRET_KIND && x.line == 2)
            .collect();
        assert_eq!(
            secrets.len(),
            1,
            "doubly-matching line must collapse to one finding, got: {secrets:?}"
        );
        assert_eq!(plaintext_secret_count(&f), 1);
    }

    #[test]
    fn flags_credential_assignment_as_informational() {
        let rs = "fn run() {\n    let password = \"hunter2longvalue\";\n}\n";
        let f = scan_source(&[("src-tauri/src/lib.rs".into(), rs.into())]);
        let hit = f.iter().find(|x| x.kind == "hardcoded-credential").unwrap();
        assert_eq!(hit.severity, "medium");
        // Looser heuristic must not count toward the blocking secret total.
        assert_eq!(plaintext_secret_count(&f), 0);
    }

    #[test]
    fn flags_unsafe_block_in_rust_but_not_unsafe_code_identifier() {
        let bad = "fn run() {\n    unsafe { ptr.write(0); }\n}\n";
        let f = scan_source(&[("src-tauri/src/lib.rs".into(), bad.into())]);
        assert!(has(&f, "unsafe-block"), "got: {:?}", kinds(&f));
        assert_eq!(f.iter().find(|x| x.kind == "unsafe-block").unwrap().line, 2);

        // The `unsafe_code` identifier (a forbid attribute) is a different word.
        let ok = "#![forbid(unsafe_code)]\nfn run() {}\n";
        let g = scan_source(&[("src-tauri/src/lib.rs".into(), ok.into())]);
        assert!(!has(&g, "unsafe-block"), "got: {:?}", kinds(&g));
    }

    #[test]
    fn flags_risky_ts_apis() {
        let ts = "function f(s) {\n  eval(s);\n  el.innerHTML = s;\n}\n";
        let f = scan_source(&[("src/main.ts".into(), ts.into())]);
        assert!(has(&f, "risky-eval"), "got: {:?}", kinds(&f));
        assert!(has(&f, "raw-html-sink"), "got: {:?}", kinds(&f));
        assert_eq!(
            f.iter().find(|x| x.kind == "risky-eval").unwrap().severity,
            "high"
        );

        // Those rules are TypeScript-only — the same text as Rust is untouched.
        let g = scan_source(&[("src-tauri/src/lib.rs".into(), ts.into())]);
        assert!(!has(&g, "risky-eval"), "got: {:?}", kinds(&g));
        assert!(!has(&g, "raw-html-sink"), "got: {:?}", kinds(&g));
    }

    #[test]
    fn flags_risk_markers_only_in_comments() {
        let src =
            "fn run() {\n    // FIXME: validate this input before shipping\n    let x = 1;\n}\n";
        let f = scan_source(&[("src-tauri/src/lib.rs".into(), src.into())]);
        assert!(has(&f, "risk-marker"), "got: {:?}", kinds(&f));
        assert_eq!(f.iter().find(|x| x.kind == "risk-marker").unwrap().line, 2);

        // The same word inside a string literal (not a comment) is not a marker.
        let code = "fn run() {\n    let label = \"FIXME button\";\n}\n";
        let g = scan_source(&[("src-tauri/src/lib.rs".into(), code.into())]);
        assert!(!has(&g, "risk-marker"), "got: {:?}", kinds(&g));
    }

    #[test]
    fn skips_test_regions_and_comments_and_unknown_files() {
        // A real secret-shaped fixture inside `#[cfg(test)]` is not a real risk.
        let rs = "fn run() {}\n\n#[cfg(test)]\nmod tests {\n    const K: &str = \"sk-or-v1-abc123def456ghi789jkl012\";\n    // unsafe { } in a test doc\n}\n";
        let f = scan_source(&[("src-tauri/src/lib.rs".into(), rs.into())]);
        assert!(f.is_empty(), "test region must be skipped, got: {f:?}");

        // A secret discussed in a comment (non-test) is exempt from pattern rules.
        let commented = "// example: Bearer abcdef0123456789xyz lives in a comment\nfn run() {}\n";
        let g = scan_source(&[("src-tauri/src/lib.rs".into(), commented.into())]);
        assert!(!has(&g, "hardcoded-secret"), "got: {:?}", kinds(&g));

        // Non-source files are ignored entirely.
        let md = "# Notes\neval( unsafe sk-or-v1-abcdefghijklmnop )\n";
        let h = scan_source(&[("README.md".into(), md.into())]);
        assert!(h.is_empty(), "non-source files must be ignored, got: {h:?}");
    }

    #[test]
    fn scan_is_deterministic_and_serializes_to_flat_json() {
        let rs = "fn run() {\n    unsafe { go(); }\n}\n";
        let files = vec![("src-tauri/src/lib.rs".to_string(), rs.to_string())];
        let a = scan_source(&files);
        let b = scan_source(&files);
        assert_eq!(a, b, "scan must be deterministic");

        let v = serde_json::to_value(&a[0]).unwrap();
        for key in ["path", "line", "kind", "severity", "message"] {
            assert!(v.get(key).is_some(), "missing {key} in {v}");
        }
    }

    fn ready() -> EnterpriseInputs {
        EnterpriseInputs {
            vault_encrypted: true,
            sso_enabled: true,
            ci_present: true,
            tests_present: true,
            plaintext_secret_findings: 0,
        }
    }

    #[test]
    fn gate_passes_when_all_criteria_hold() {
        let r = enterprise_gate(&ready());
        assert!(r.passed, "items: {:?}", r.items);
        assert_eq!(r.items.len(), 5);
        assert!(r.items.iter().all(|i| i.ok));
    }

    #[test]
    fn gate_fails_when_any_criterion_is_unmet() {
        let mut inputs = ready();
        inputs.sso_enabled = false;
        let r = enterprise_gate(&inputs);
        assert!(!r.passed);
        let sso = r
            .items
            .iter()
            .find(|i| i.name.contains("Single sign-on"))
            .unwrap();
        assert!(!sso.ok);
        assert!(sso.detail.to_lowercase().contains("entra"));
    }

    #[test]
    fn gate_reports_plaintext_secret_count_in_detail() {
        let mut inputs = ready();
        inputs.plaintext_secret_findings = 3;
        let r = enterprise_gate(&inputs);
        assert!(!r.passed);
        let item = r
            .items
            .iter()
            .find(|i| i.name.contains("plaintext secrets"))
            .unwrap();
        assert!(!item.ok);
        assert!(item.detail.contains('3'), "detail: {}", item.detail);
    }

    #[test]
    fn gate_serializes_to_expected_shape() {
        let v = serde_json::to_value(enterprise_gate(&ready())).unwrap();
        assert_eq!(v.get("passed").and_then(|p| p.as_bool()), Some(true));
        let items = v.get("items").and_then(|i| i.as_array()).unwrap();
        for item in items {
            for key in ["name", "ok", "detail"] {
                assert!(item.get(key).is_some(), "missing {key} in {item}");
            }
        }
    }
}
