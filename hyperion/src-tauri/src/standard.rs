// Hyperion — canonical code standard + deterministic audit (M3, Milestone #5).
//
// Two things live here. First, the project's *recommended* code standard, as a
// small machine-readable summary (`standard_summary`) that mirrors the prose in
// `docs/wiki/code-standard.html` — both derive from Hyperion's actual, observed
// conventions (Rust edition 2021, `cargo fmt`, `clippy -D warnings`, no
// `unwrap()`/`expect`/`panic!` in non-test code, errors as `Result<_, String>` at
// the command layer, a `#[cfg(test)]` module per file; vanilla TS with no stray
// `console.log`, spaces not tabs, a trailing newline).
//
// Second, a deterministic *audit* (`audit`) that flags deviations from that
// standard with a suggested fix. `audit` is intentionally PURE over in-memory
// `(path, contents)` pairs — it touches no filesystem and no network, so it is
// fully unit-testable with synthetic source text. The `code_audit` Tauri command
// (in lib.rs) is the only place that reads the project's own sources off disk and
// hands them to `audit`. Strictly read-only with respect to bOS, like the rest of
// this layer.

use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

/// One flagged deviation from the code standard. `path` is the source file (a
/// repo-relative display path), `line` is 1-based, `rule` is the stable rule id,
/// `message` explains the deviation, `severity` ranks it ("high" | "medium" |
/// "low"), and `suggested_fix` is a short, concrete remediation (a fix
/// instruction or before→after snippet) the UI can show next to the flaw.
/// Serializes to a flat JSON object the renderer can list directly, matching the
/// shape used by `suggest::Suggestion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub path: String,
    pub line: usize,
    pub rule: &'static str,
    pub message: String,
    pub severity: &'static str,
    pub suggested_fix: String,
}

impl Finding {
    fn new(
        path: &str,
        line: usize,
        rule: &'static str,
        severity: &'static str,
        message: impl Into<String>,
        suggested_fix: impl Into<String>,
    ) -> Self {
        Finding {
            path: path.to_string(),
            line,
            rule,
            severity,
            message: message.into(),
            suggested_fix: suggested_fix.into(),
        }
    }
}

/// Which language a file is audited as, decided purely by extension. `Other`
/// files are skipped entirely (the audit never invents rules for formats it
/// doesn't understand).
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

/// Audit a batch of source files against the canonical standard and return every
/// deviation, in a deterministic order (files in input order; within a file, by
/// ascending line then by the order the rules are checked). Pure: the input is
/// the full file contents, so there is no filesystem or network access here.
///
/// Heuristic, text-based rules (no full parse), each chosen to be low-noise:
/// * Rust, outside a `#[cfg(test)]` region — `.unwrap()`, `.expect(`, and the
///   panicking macros (`panic!`/`todo!`/`unimplemented!`/`unreachable!`).
/// * TypeScript — a stray `console.log(`.
/// * Any audited file — tab characters in leading indentation, and a missing
///   final newline.
///
/// Lines whose first non-whitespace content is a comment (`//`, `/*`, `*`) are
/// exempt from the *pattern* rules, so documentation that merely mentions
/// `unwrap()` (like this very file) is never flagged.
pub fn audit(files: &[(String, String)]) -> Vec<Finding> {
    let mut out = Vec::new();
    for (path, contents) in files {
        match Lang::from_path(path) {
            Lang::Other => continue,
            lang => audit_file(path, contents, lang, &mut out),
        }
    }
    out
}

fn audit_file(path: &str, contents: &str, lang: Lang, out: &mut Vec<Finding>) {
    // Once we enter a `#[cfg(test)]` region the rest of the file is treated as
    // test code, where `unwrap()`/`expect`/`panic!` are allowed. This matches
    // Hyperion's layout: every module keeps its tests in a single trailing
    // `#[cfg(test)] mod tests` block.
    let mut in_test = false;

    for (idx, line) in contents.lines().enumerate() {
        let line_no = idx + 1;

        // Only a real attribute flips us into the test region — a comment that
        // merely mentions `#[cfg(test)]` must not suppress later findings.
        if lang == Lang::Rust && line.contains("#[cfg(test)]") && !is_comment_line(line) {
            in_test = true;
        }

        // Tab-indentation rule applies to every audited line regardless of
        // comment status — formatting is formatting.
        if leading_has_tab(line) {
            out.push(Finding::new(
                path,
                line_no,
                "tab-indentation",
                "low",
                "Indent with spaces, not tabs (run `cargo fmt` / format the file).",
                "Replace the leading tab(s) with spaces (`cargo fmt`, or set the editor to insert spaces).",
            ));
        }

        if is_comment_line(line) {
            continue;
        }

        match lang {
            Lang::Rust if !in_test => audit_rust_line(path, line_no, line, out),
            Lang::Ts => audit_ts_line(path, line_no, line, out),
            _ => {}
        }
    }

    // Missing trailing newline: a non-empty file should end in exactly one `\n`.
    // An empty file is fine (nothing to terminate).
    if !contents.is_empty() && !contents.ends_with('\n') {
        let last = contents.lines().count().max(1);
        out.push(Finding::new(
            path,
            last,
            "missing-trailing-newline",
            "low",
            "File should end with a single trailing newline.",
            "Add a single newline (`\\n`) at the end of the file.",
        ));
    }
}

fn audit_rust_line(path: &str, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    if line.contains(".unwrap()") {
        out.push(Finding::new(
            path,
            line_no,
            "no-unwrap",
            "medium",
            "Avoid `.unwrap()` in non-test code — propagate with `?` or map to a \
             `Result<_, String>` at the command layer.",
            "Replace `value.unwrap()` with `value?` (when in a `Result` fn) or \
             `value.unwrap_or(default)` / `value.ok_or(\"…\")?`.",
        ));
    }
    if line.contains(".expect(") {
        out.push(Finding::new(
            path,
            line_no,
            "no-expect",
            "medium",
            "Avoid `.expect(...)` in non-test code — return a descriptive \
             `Result<_, String>` instead of panicking.",
            "Replace `value.expect(\"msg\")` with `value.map_err(|e| \
             format!(\"msg: {e}\"))?` or `value.ok_or(\"msg\")?`.",
        ));
    }
    if let Some(mac) = PANIC_MACROS.iter().find(|m| line.contains(*m)) {
        out.push(Finding::new(
            path,
            line_no,
            "no-panic",
            "high",
            format!(
                "Avoid `{mac}` in non-test code — surface the condition as an error \
                 (`Result<_, String>`) so the app never crashes."
            ),
            format!("Replace `{mac}` with an early `return Err(\"…\".to_string())` (or `?`)."),
        ));
    }
}

fn audit_ts_line(path: &str, line_no: usize, line: &str, out: &mut Vec<Finding>) {
    if line.contains("console.log(") {
        out.push(Finding::new(
            path,
            line_no,
            "no-console",
            "low",
            "Remove the stray `console.log(...)` before shipping.",
            "Delete the `console.log(...)` call (or gate it behind a debug flag).",
        ));
    }
}

/// The panicking macros flagged outside test code. Each is matched with its `!`
/// so a function named e.g. `todo` could never trip the rule.
const PANIC_MACROS: [&str; 4] = ["panic!", "todo!", "unimplemented!", "unreachable!"];

/// True if the line's leading indentation (the whitespace before the first
/// non-whitespace char) contains a tab.
fn leading_has_tab(line: &str) -> bool {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .any(|c| c == '\t')
}

/// True if the line, ignoring indentation, is (or starts as) a comment — used to
/// exempt prose from the pattern rules. Covers `//` line comments and `/*` / `*`
/// block-comment lines, which is all Hyperion's sources use.
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
}

/// The canonical standard as a structured, machine-readable summary: the prose
/// lives in `docs/wiki/code-standard.html`, but the `code_standard` command
/// returns this so the webview can render the rules and link each audit `Finding`
/// back to the rule that produced it (by `rule` id). Derived from the real
/// codebase, not invented.
pub fn standard_summary() -> Value {
    json!({
        "title": "Hyperion code standard",
        "summary": "Rust edition 2021, formatted with `cargo fmt` and clean under \
                    `clippy -D warnings`; no `unwrap()`/`expect`/`panic!` in non-test \
                    code; errors surface as `Result<_, String>` at the command layer; \
                    one `#[cfg(test)]` module per file. TypeScript is vanilla and \
                    dependency-light: spaces not tabs, no stray `console.log`, a \
                    trailing newline.",
        "rules": [
            {
                "rule": "no-unwrap",
                "applies_to": "rust",
                "severity": "medium",
                "title": "No `.unwrap()` in non-test code",
                "fix": "Propagate with `?` or map to a `Result<_, String>` at the command layer."
            },
            {
                "rule": "no-expect",
                "applies_to": "rust",
                "severity": "medium",
                "title": "No `.expect(...)` in non-test code",
                "fix": "Return a descriptive `Result<_, String>` instead of panicking."
            },
            {
                "rule": "no-panic",
                "applies_to": "rust",
                "severity": "high",
                "title": "No `panic!`/`todo!`/`unimplemented!`/`unreachable!` in non-test code",
                "fix": "Surface the condition as an error so the app never crashes."
            },
            {
                "rule": "no-console",
                "applies_to": "ts",
                "severity": "low",
                "title": "No stray `console.log(...)`",
                "fix": "Remove debug logging before shipping."
            },
            {
                "rule": "tab-indentation",
                "applies_to": "any",
                "severity": "low",
                "title": "Indent with spaces, not tabs",
                "fix": "Run `cargo fmt` / reformat to spaces."
            },
            {
                "rule": "missing-trailing-newline",
                "applies_to": "any",
                "severity": "low",
                "title": "Files end with a trailing newline",
                "fix": "Add a single newline at end of file."
            }
        ]
    })
}

/// Read the project's own auditable sources off disk: every `*.rs` under
/// `<manifest_dir>/src` and every `*.ts` under `<manifest_dir>/../src`, returning
/// `(display_path, contents)` pairs sorted by display path for a deterministic
/// audit. This is the only filesystem-touching piece — `audit` itself stays pure.
/// `manifest_dir` is the crate root (`CARGO_MANIFEST_DIR`, i.e. `src-tauri`).
pub fn collect_project_sources(manifest_dir: &Path) -> Result<Vec<(String, String)>, String> {
    let mut files = Vec::new();
    read_dir_ext(&manifest_dir.join("src"), "rs", "src-tauri/src", &mut files)?;
    // hyperion/src holds the webview TypeScript (one level up from the crate).
    if let Some(app_root) = manifest_dir.parent() {
        read_dir_ext(&app_root.join("src"), "ts", "src", &mut files)?;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// Append `(display_path, contents)` for every file in `dir` with extension
/// `ext`. A missing directory is not an error (the audit just sees fewer files);
/// an unreadable file surfaces a `Result::Err`.
fn read_dir_ext(
    dir: &Path,
    ext: &str,
    display_prefix: &str,
    out: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry.map_err(|e| format!("read dir {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let contents =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        out.push((format!("{display_prefix}/{name}"), contents));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|x| x.rule).collect()
    }

    fn has(f: &[Finding], rule: &str) -> bool {
        f.iter().any(|x| x.rule == rule)
    }

    #[test]
    fn flags_unwrap_expect_and_panic_in_non_test_rust() {
        let src = "fn run() {\n    let a = thing.unwrap();\n    let b = other.expect(\"x\");\n    panic!(\"boom\");\n}\n";
        let f = audit(&[("src-tauri/src/lib.rs".into(), src.into())]);
        assert!(has(&f, "no-unwrap"), "got: {:?}", rules(&f));
        assert!(has(&f, "no-expect"), "got: {:?}", rules(&f));
        assert!(has(&f, "no-panic"), "got: {:?}", rules(&f));
        // Line numbers are 1-based and point at the offending line.
        let unwrap = f.iter().find(|x| x.rule == "no-unwrap").unwrap();
        assert_eq!(unwrap.line, 2);
        assert_eq!(unwrap.severity, "medium");
        let panic = f.iter().find(|x| x.rule == "no-panic").unwrap();
        assert_eq!(panic.line, 4);
        assert_eq!(panic.severity, "high");
    }

    #[test]
    fn allows_unwrap_and_panic_inside_test_module() {
        // Everything after `#[cfg(test)]` is treated as test code.
        let src = "fn run() -> Result<(), String> {\n    Ok(())\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        let v = x.unwrap();\n        panic!(\"fine in tests\");\n    }\n}\n";
        let f = audit(&[("src-tauri/src/lib.rs".into(), src.into())]);
        assert!(!has(&f, "no-unwrap"), "got: {:?}", rules(&f));
        assert!(!has(&f, "no-panic"), "got: {:?}", rules(&f));
    }

    #[test]
    fn does_not_flag_unwrap_or_variants_or_comments() {
        // `.unwrap_or(...)` / `.unwrap_or_else(...)` are not `.unwrap()`, and a
        // comment that merely mentions `.unwrap()` is exempt.
        let src = "fn run() {\n    let a = x.unwrap_or_default();\n    let b = y.unwrap_or_else(|_| 0);\n    // never call .unwrap() here\n    let c = z.expect_err_helper();\n}\n";
        let f = audit(&[("src-tauri/src/lib.rs".into(), src.into())]);
        assert!(f.is_empty(), "expected no findings, got: {f:?}");
    }

    #[test]
    fn flags_console_log_in_ts_only() {
        let ts = "function f() {\n  console.log(\"debug\");\n}\n";
        let f = audit(&[("src/main.ts".into(), ts.into())]);
        assert!(has(&f, "no-console"), "got: {:?}", rules(&f));
        assert_eq!(f.iter().find(|x| x.rule == "no-console").unwrap().line, 2);

        // The same text under a `.rs` path is not a TS file, so no console rule.
        let clean = "fn f() {\n    let x = 1;\n}\n";
        let g = audit(&[("src-tauri/src/lib.rs".into(), clean.into())]);
        assert!(!has(&g, "no-console"), "got: {:?}", rules(&g));
    }

    #[test]
    fn flags_tab_indentation_in_rust_and_ts() {
        let rs = "fn f() {\n\tlet x = 1;\n}\n";
        let f = audit(&[("src-tauri/src/lib.rs".into(), rs.into())]);
        assert!(has(&f, "tab-indentation"), "got: {:?}", rules(&f));
        assert_eq!(
            f.iter().find(|x| x.rule == "tab-indentation").unwrap().line,
            2
        );

        let ts = "function f() {\n\treturn 1;\n}\n";
        let g = audit(&[("src/main.ts".into(), ts.into())]);
        assert!(has(&g, "tab-indentation"), "got: {:?}", rules(&g));
    }

    #[test]
    fn flags_missing_trailing_newline_and_accepts_present_one() {
        let no_nl = "fn f() {}\n// last line, no newline";
        let f = audit(&[("src-tauri/src/lib.rs".into(), no_nl.into())]);
        assert!(has(&f, "missing-trailing-newline"), "got: {:?}", rules(&f));
        assert_eq!(
            f.iter()
                .find(|x| x.rule == "missing-trailing-newline")
                .unwrap()
                .line,
            2
        );

        let with_nl = "fn f() {}\n";
        let g = audit(&[("src-tauri/src/lib.rs".into(), with_nl.into())]);
        assert!(!has(&g, "missing-trailing-newline"), "got: {:?}", rules(&g));

        // An empty file has nothing to terminate.
        let empty = audit(&[("src-tauri/src/lib.rs".into(), String::new())]);
        assert!(empty.is_empty(), "got: {empty:?}");
    }

    #[test]
    fn skips_unknown_extensions() {
        let md = "# Notes\n\tthis.unwrap() panic! console.log(x)\n";
        let f = audit(&[("README.md".into(), md.into())]);
        assert!(f.is_empty(), "non-source files must be ignored, got: {f:?}");
    }

    #[test]
    fn is_deterministic_and_serializes_to_flat_json() {
        let src = "fn run() {\n    let a = x.unwrap();\n}\n";
        let files = vec![("src-tauri/src/lib.rs".to_string(), src.to_string())];
        let a = audit(&files);
        let b = audit(&files);
        assert_eq!(a, b, "audit must be deterministic");

        let v = serde_json::to_value(&a[0]).unwrap();
        for key in [
            "path",
            "line",
            "rule",
            "message",
            "severity",
            "suggested_fix",
        ] {
            assert!(v.get(key).is_some(), "missing {key} in {v}");
        }
        assert_eq!(v.get("path").unwrap(), "src-tauri/src/lib.rs");
    }

    #[test]
    fn every_finding_carries_a_non_empty_suggested_fix() {
        // One synthetic batch that trips every rule at least once: Rust pattern
        // rules + tab indentation, and a TypeScript file for the console rule and
        // a missing trailing newline.
        let rs = "fn run() {\n\tlet a = x.unwrap();\n    let b = y.expect(\"x\");\n    panic!(\"boom\");\n}\n";
        let ts = "function f() {\n  console.log(\"debug\");\n}"; // no trailing newline
        let f = audit(&[
            ("src-tauri/src/lib.rs".into(), rs.into()),
            ("src/main.ts".into(), ts.into()),
        ]);
        // Confirm we actually exercised every rule id.
        for rule in [
            "no-unwrap",
            "no-expect",
            "no-panic",
            "no-console",
            "tab-indentation",
            "missing-trailing-newline",
        ] {
            assert!(
                has(&f, rule),
                "rule {rule} not triggered; got: {:?}",
                rules(&f)
            );
        }
        // Each finding must carry a concrete, non-empty fix.
        for finding in &f {
            assert!(
                !finding.suggested_fix.trim().is_empty(),
                "rule {} has an empty suggested_fix",
                finding.rule
            );
        }
    }

    #[test]
    fn standard_summary_lists_every_audit_rule() {
        let s = standard_summary();
        let rules = s.get("rules").and_then(|r| r.as_array()).unwrap();
        let ids: Vec<&str> = rules
            .iter()
            .filter_map(|r| r.get("rule").and_then(|v| v.as_str()))
            .collect();
        for expected in [
            "no-unwrap",
            "no-expect",
            "no-panic",
            "no-console",
            "tab-indentation",
            "missing-trailing-newline",
        ] {
            assert!(ids.contains(&expected), "summary missing {expected}");
        }
    }
}
