// Hyperion — bundled HTML-effectiveness artifact templates (V2, Track 4).
//
// A first-class, pickable library of the exemplar documentation patterns that live
// as standalone pages under `hyperion/docs/artifacts/*.html`. Each one embodies a
// single "match the artifact to the shape of the information" idea (the doc-writer
// roster role): a progress dashboard for parallel status, a comparison table for a
// decision, a playbook for a procedure, and so on.
//
// The HTML is embedded at compile time via `include_str!`, so the catalog needs no
// runtime file IO, no resource bundling, and no path resolution — it is robust in
// both the dev tree and a packaged build. The webview lists the catalog (key /
// label / "use when…") and fetches a body to drop into the wiki editor as a
// themeable starting point. Every template renders in light *and* dark because it
// reads the app's `:root` design tokens (with a `[data-theme="dark"]` preset that
// mirrors `hyperion/src/styles.css`). Pure and read-only toward bOS.

use serde::Serialize;

/// One bundled artifact template. `key` is the stable identifier the webview asks
/// for; `label` is the human name; `description` is the "use when…" shape it fits;
/// `html` is the full, self-contained, themeable document. The HTML is omitted from
/// the catalog listing (it can be large) and fetched on demand via [`get`], so
/// [`ArtifactTemplate`] serializes to just `{key, label, description}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ArtifactTemplate {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    #[serde(skip_serializing)]
    pub html: &'static str,
}

/// The bundled catalog, in gallery order (see `docs/artifacts/index.html`). The
/// `description` is the information *shape* each pattern fits, so the picker reads
/// as guidance ("compare options across criteria") rather than a bare name.
const TEMPLATES: &[ArtifactTemplate] = &[
    ArtifactTemplate {
        key: "progress-dashboard",
        label: "Progress Dashboard",
        description: "Show the state of several parallel work-streams at a glance — labelled bars with a percentage and a phase colour.",
        html: include_str!("../../docs/artifacts/progress-dashboard.html"),
    },
    ArtifactTemplate {
        key: "term-card",
        label: "Definition / Term Cards",
        description: "Define glossary terms — pair each term with a tight definition and a worked example.",
        html: include_str!("../../docs/artifacts/term-card.html"),
    },
    ArtifactTemplate {
        key: "playbook",
        label: "Step-by-step Playbook",
        description: "Lay out a procedure the reader will execute — each step states intent, action, and a verification.",
        html: include_str!("../../docs/artifacts/playbook.html"),
    },
    ArtifactTemplate {
        key: "comparison-table",
        label: "Comparison Table",
        description: "Compare options across criteria — a decision aid with a consistent yes / partial / no vocabulary.",
        html: include_str!("../../docs/artifacts/comparison-table.html"),
    },
    ArtifactTemplate {
        key: "callout",
        label: "Callout / Warning Blocks",
        description: "Lift a note, tip, caveat, or hazard out of the running prose — colour plus an icon sets the severity.",
        html: include_str!("../../docs/artifacts/callout.html"),
    },
    ArtifactTemplate {
        key: "metric-tiles",
        label: "Metric / KPI Tiles",
        description: "Surface a handful of headline numbers — big value, small label, signed delta.",
        html: include_str!("../../docs/artifacts/metric-tiles.html"),
    },
    ArtifactTemplate {
        key: "faq",
        label: "FAQ Accordion",
        description: "Answer question-led reference where most readers want only one answer — collapsible Q&A, zero JavaScript.",
        html: include_str!("../../docs/artifacts/faq.html"),
    },
];

/// The full catalog (cheap clone of `&'static` fields). Serializes to the
/// `{key, label, description}` list the picker renders — the HTML body is fetched
/// separately via [`get`].
pub fn catalog() -> Vec<ArtifactTemplate> {
    TEMPLATES.to_vec()
}

/// The HTML body of the template named `key`, or a clean error when no such
/// template exists. The body is the full, themeable starting document the operator
/// drops into a wiki page editor.
pub fn get(key: &str) -> Result<&'static str, String> {
    TEMPLATES
        .iter()
        .find(|t| t.key == key)
        .map(|t| t.html)
        .ok_or_else(|| format!("unknown artifact template: {key:?}"))
}

// ----------------------------- live guide derivation (pure) -----------------------------
//
// The bundled templates embody the "match the artifact to the shape of the information"
// ideas catalogued at https://thariqs.github.io/html-effectiveness/. These pure helpers
// let the app refresh the *guidance around* the templates from that live source: a
// fetched-and-stripped guide page is distilled into a per-technique "use when…" note
// (the embedded template HTML is never touched). The fetch/network edge lives in
// `crawler.rs`; everything here is deterministic and unit-tested with synthetic text.

/// One derived "use when…" guidance note for a bundled template, distilled from the
/// live html-effectiveness guide. `key`/`label` identify the template; `guidance` is
/// the sentence the guide offered for when that information shape fits. Serializes to a
/// flat object the refresh command reports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuideNote {
    pub key: &'static str,
    pub label: &'static str,
    pub guidance: String,
}

/// The cue word that anchors a template to a sentence in the guide text: the first
/// segment of its `key` (e.g. `comparison-table` -> `comparison`, `faq` -> `faq`).
/// Each catalog key has a distinct leading segment, so this gives a stable per-technique
/// probe without maintaining a second hand-written list alongside `TEMPLATES`.
fn guide_cue(key: &str) -> &str {
    key.split('-').next().unwrap_or(key)
}

/// Derive a per-technique "use when…" note from the (already tag-stripped) html-
/// effectiveness guide text. Pure and deterministic: splits the guide into sentences
/// and, for each bundled template, records the FIRST sentence whose text mentions that
/// template's cue word ([`guide_cue`]) as its guidance. A template the guide never
/// mentions yields no note (guidance is distilled, never invented), so the result lists
/// only the techniques the source actually covers. No network, no I/O.
pub fn derive_guide_notes(guide_text: &str) -> Vec<GuideNote> {
    let sentences: Vec<&str> = guide_text
        .split(['.', '!', '?'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = Vec::new();
    for t in TEMPLATES {
        let cue = guide_cue(t.key).to_ascii_lowercase();
        if cue.is_empty() {
            continue;
        }
        if let Some(sentence) = sentences
            .iter()
            .find(|s| s.to_ascii_lowercase().contains(&cue))
        {
            out.push(GuideNote {
                key: t.key,
                label: t.label,
                guidance: cap_chars(sentence, 240),
            });
        }
    }
    out
}

/// Truncate `s` to at most `max` characters on a char boundary, appending an ellipsis
/// when it was cut, so one runaway sentence can't bloat the stored note.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// Render derived `notes` into a single project-knowledge body: a short header plus one
/// "Use when:" line per technique. Stored verbatim via the crawler's project-knowledge
/// storage so the guide's guidance is searchable in-app alongside the templates. Pure
/// and deterministic.
pub fn format_guide_knowledge(notes: &[GuideNote]) -> String {
    let mut s = String::from(
        "HTML-effectiveness artifact guide — when to use each bundled template \
         (refreshed from https://thariqs.github.io/html-effectiveness/).\n",
    );
    for n in notes {
        s.push_str(&format!(
            "\n## {} ({})\nUse when: {}\n",
            n.label, n.key, n.guidance
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_non_empty() {
        assert!(!catalog().is_empty());
    }

    #[test]
    fn every_key_resolves_to_non_empty_html() {
        for t in catalog() {
            let html = get(t.key).unwrap_or_else(|_| panic!("key {:?} did not resolve", t.key));
            assert!(!html.trim().is_empty(), "empty html for {:?}", t.key);
            // Each template is a full, self-contained, themeable document.
            assert!(
                html.contains("<!doctype html>"),
                "{:?} is not a full document",
                t.key
            );
            assert!(
                html.contains("var(--"),
                "{:?} does not read design tokens",
                t.key
            );
            assert!(
                html.contains(r#":root[data-theme="dark"]"#),
                "{:?} has no dark preset",
                t.key
            );
            // The catalog entries are descriptive, not blank.
            assert!(!t.label.trim().is_empty(), "blank label for {:?}", t.key);
            assert!(
                !t.description.trim().is_empty(),
                "blank description for {:?}",
                t.key
            );
        }
    }

    #[test]
    fn get_unknown_key_errors_cleanly() {
        let err = get("does-not-exist").unwrap_err();
        assert!(err.contains("does-not-exist"), "err: {err}");
        assert!(get("").is_err());
    }

    #[test]
    fn keys_are_unique() {
        let mut keys: Vec<&str> = catalog().iter().map(|t| t.key).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "duplicate template keys");
    }

    #[test]
    fn derive_guide_notes_extracts_use_when_per_technique() {
        // A synthetic guide page (post tag-strip) mentioning three of the bundled
        // techniques and one irrelevant sentence.
        let text = "Comparison tables shine when you must weigh options across several criteria. \
                    A playbook is the right choice when the reader will execute a procedure step by step. \
                    An FAQ accordion is best when most readers want only one answer. \
                    This unrelated sentence mentions nothing pickable.";
        let notes = derive_guide_notes(text);
        let by_key: std::collections::HashMap<&str, &str> =
            notes.iter().map(|n| (n.key, n.guidance.as_str())).collect();

        // Each covered technique gets the FIRST sentence that named its cue word.
        assert!(by_key
            .get("comparison-table")
            .unwrap()
            .contains("weigh options across several criteria"));
        assert!(by_key
            .get("playbook")
            .unwrap()
            .contains("execute a procedure step by step"));
        assert!(by_key.get("faq").unwrap().contains("only one answer"));
        // A technique the guide never mentions is not invented.
        assert!(by_key.get("callout").is_none());
        // The derived label tracks the catalog.
        let faq = notes.iter().find(|n| n.key == "faq").unwrap();
        assert_eq!(faq.label, "FAQ Accordion");
    }

    #[test]
    fn derive_guide_notes_empty_when_nothing_matches() {
        assert!(derive_guide_notes("totally unrelated prose about nothing here").is_empty());
        assert!(derive_guide_notes("").is_empty());
    }

    #[test]
    fn format_guide_knowledge_lists_each_technique_with_use_when() {
        let notes =
            derive_guide_notes("A playbook is ideal when a procedure must be followed in order.");
        let body = format_guide_knowledge(&notes);
        assert!(body.contains("HTML-effectiveness artifact guide"));
        assert!(body.contains("Step-by-step Playbook"));
        assert!(body.contains("(playbook)"));
        assert!(body.contains("Use when:"));
    }

    #[test]
    fn derive_guide_notes_caps_long_guidance() {
        // A single very long "playbook" sentence is truncated with an ellipsis.
        let long = format!("A playbook helps {} now", "x".repeat(400));
        let notes = derive_guide_notes(&long);
        let g = &notes.iter().find(|n| n.key == "playbook").unwrap().guidance;
        assert!(g.chars().count() <= 241, "len {}", g.chars().count());
        assert!(g.ends_with('…'));
    }

    #[test]
    fn catalog_listing_omits_the_html_body() {
        let t = &catalog()[0];
        let v = serde_json::to_value(t).unwrap();
        assert!(v.get("key").is_some());
        assert!(v.get("label").is_some());
        assert!(v.get("description").is_some());
        // The (potentially large) body is fetched separately, not in the listing.
        assert!(v.get("html").is_none(), "listing should not carry html");
    }
}
