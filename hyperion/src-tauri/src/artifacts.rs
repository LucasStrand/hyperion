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
