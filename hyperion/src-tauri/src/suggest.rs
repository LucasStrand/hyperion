// Hyperion — proactive context suggestions (M1, Milestone #4).
//
// A deterministic, fully local heuristic that inspects the *active* project and
// tells the operator what grounding is missing, so the assistant can give better
// answers. It never reaches the network and never writes anything: it only reads
// the project DB through the existing public `projects` helpers (the active `.bos`
// snapshot, the ingested context files, and — for a query — keyword retrieval).
//
// The output is a small ordered list of `Suggestion { kind, message, severity }`
// values that serialize straight to JSON for the webview. Strictly read-only with
// respect to bOS data, exactly like the rest of this layer.

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::ingest;
use crate::projects;

/// A single piece of advice for the operator. `kind` groups it ("snapshot",
/// "context", "documentation"), `severity` ranks it ("high" | "medium" | "low"),
/// and `message` is the human-readable suggestion. Serializes to a flat JSON
/// object the renderer can list directly.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    pub kind: &'static str,
    pub severity: &'static str,
    pub message: String,
}

impl Suggestion {
    fn new(kind: &'static str, severity: &'static str, message: impl Into<String>) -> Self {
        Suggestion {
            kind,
            severity,
            message: message.into(),
        }
    }
}

/// Below this many ingested context files we nudge the operator to add the useful
/// staples (datasheets, a network map, commissioning notes). At or above it we stay
/// quiet on volume. Zero files is handled separately as a higher-severity gap.
const FEW_CONTEXT_FILES: i64 = 3;

/// At most this many "add documentation about `<term>`" suggestions per query, so a
/// long question can't bury the operator in noise. The most salient (alphabetically
/// stable) terms win.
const MAX_TERM_SUGGESTIONS: usize = 3;

/// Inspect the active project's DB and return ordered suggestions for the missing
/// context. `query` is the operator's pending question (if any); salient terms in it
/// that appear in no context chunk and no `.bos` node name yield a documentation
/// nudge. Read-only: it only calls existing `projects` helpers and `ingest::keywords`.
pub fn suggest(db: &Path, query: Option<&str>) -> Result<Vec<Suggestion>, String> {
    let mut out = Vec::new();

    // Heuristic 1: no active `.bos` snapshot — the assistant can't see the live
    // system at all. Highest-value gap.
    let snapshot = projects::active_snapshot(db)?;
    if snapshot.is_none() {
        out.push(Suggestion::new(
            "snapshot",
            "high",
            "No building configuration is loaded — upload the building's `.bos` file so the assistant can see the live system.",
        ));
    }

    // Heuristics 2 & 4: how much reference material is ingested.
    let files = projects::context_list(db)?;
    let file_count = files.len() as i64;
    if file_count == 0 {
        out.push(Suggestion::new(
            "context",
            "high",
            "No context files yet — add device datasheets and a network map so answers cite real specs instead of guessing.",
        ));
    } else if file_count < FEW_CONTEXT_FILES {
        out.push(Suggestion::new(
            "context",
            "low",
            format!(
                "Only {file_count} context file(s) loaded — consider adding device datasheets, a network/Modbus map, and commissioning notes."
            ),
        ));
    }

    // Heuristic 3: salient query terms that are documented nowhere. Only worth
    // running once there's *some* grounding to compare against — when the project is
    // empty, heuristics 1 & 2 already tell the operator to add everything.
    if let Some(q) = query {
        let has_grounding = snapshot.is_some() || file_count > 0;
        if has_grounding {
            // Lowercased node-name haystack from the active snapshot, matched by
            // substring the same way `ingest::score` matches context chunks.
            let bos_haystack = snapshot
                .as_ref()
                .map(|(_, nodes)| bos_name_haystack(nodes))
                .unwrap_or_default();

            // Deterministic order: `keywords` returns a set, so sort before taking.
            let mut terms: Vec<String> = ingest::keywords(q).into_iter().collect();
            terms.sort();

            for term in terms {
                if out.iter().filter(|s| s.kind == "documentation").count() >= MAX_TERM_SUGGESTIONS
                {
                    break;
                }
                if bos_haystack.contains(&term) {
                    continue; // named somewhere in the live config
                }
                // Present in a context chunk? Reuse the existing retriever: a single
                // term that matches no chunk comes back empty under the keyword ranker.
                if !projects::context_retrieve(db, &term, 1)?.is_empty() {
                    continue;
                }
                out.push(Suggestion::new(
                    "documentation",
                    "medium",
                    format!(
                        "Your question mentions `{term}`, but nothing in the loaded context or building config covers it — add documentation about `{term}`."
                    ),
                ));
            }
        }
    }

    Ok(out)
}

/// Flatten an active snapshot's nodes into one lowercased string of every node
/// `name` and `path`, newline-separated, for cheap substring membership tests.
fn bos_name_haystack(nodes: &Value) -> String {
    let mut s = String::new();
    if let Some(arr) = nodes.as_array() {
        for n in arr {
            for key in ["name", "path"] {
                if let Some(v) = n.get(key).and_then(|v| v.as_str()) {
                    s.push_str(&v.to_lowercase());
                    s.push('\n');
                }
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// A fresh, isolated projects root for one test (cleaned up by the caller).
    /// Mirrors the `temp_root` helper in `projects.rs` tests.
    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hyperion_suggest_test_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Create a synthetic project and return `(root, project.db path)`. Mirrors the
    /// `fresh_db` helper in `projects.rs` tests — no network, no keychain.
    fn fresh_db(tag: &str) -> (PathBuf, PathBuf) {
        let root = temp_root(tag);
        let summary = projects::create(&root, "Test Project").unwrap();
        let id = summary.get("id").unwrap().as_str().unwrap().to_string();
        let db = root.join(&id).join("project.db");
        (root, db)
    }

    fn kinds(s: &[Suggestion]) -> Vec<&str> {
        s.iter().map(|x| x.kind).collect()
    }

    #[test]
    fn flags_missing_snapshot_and_context_on_empty_project() {
        let (root, db) = fresh_db("empty");
        let s = suggest(&db, None).unwrap();
        let ks = kinds(&s);
        assert!(ks.contains(&"snapshot"), "got: {ks:?}");
        assert!(ks.contains(&"context"), "got: {ks:?}");
        // Both gaps are high-severity on a brand-new project.
        for sug in &s {
            assert_eq!(sug.severity, "high", "got: {sug:?}");
        }
        // Each suggestion serializes to a flat JSON object with the three fields.
        let v = serde_json::to_value(&s[0]).unwrap();
        assert!(
            v.get("kind").is_some() && v.get("message").is_some() && v.get("severity").is_some()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn quiet_when_well_provisioned_and_flags_undocumented_query_term() {
        let (root, db) = fresh_db("full");
        let nodes = json!([
            { "name": "Lobby AHU", "path": "Building\\Lobby\\AHU" },
            { "name": "Core Switch", "path": "Building\\IT\\Core Switch" },
        ]);
        projects::add_snapshot(&db, "v1", Some("building.bos"), &nodes).unwrap();
        // Three context files -> not "few", so no volume nudge.
        projects::context_add(&db, "ahu.csv", b"device,bus\nLobby AHU,Modbus slave 7\n").unwrap();
        projects::context_add(&db, "net.md", b"# network map\ncore switch uplink\n").unwrap();
        projects::context_add(&db, "notes.txt", b"commissioning notes recorded today\n").unwrap();

        // Nothing is missing -> no snapshot/context suggestions.
        let s = suggest(&db, None).unwrap();
        assert!(
            s.iter()
                .all(|x| x.kind != "snapshot" && x.kind != "context"),
            "got: {:?}",
            kinds(&s)
        );

        // A query term that is documented nowhere -> a medium documentation nudge.
        let s2 = suggest(&db, Some("chiller")).unwrap();
        assert!(
            s2.iter().any(|x| x.kind == "documentation"
                && x.severity == "medium"
                && x.message.contains("chiller")),
            "got: {s2:?}"
        );

        // A term present in a context chunk ("modbus") yields no documentation nudge.
        let s3 = suggest(&db, Some("modbus")).unwrap();
        assert!(s3.iter().all(|x| x.kind != "documentation"), "got: {s3:?}");

        // A term that appears only in a `.bos` node name ("switch") is also covered.
        let s4 = suggest(&db, Some("switch")).unwrap();
        assert!(s4.iter().all(|x| x.kind != "documentation"), "got: {s4:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn flags_few_context_files_with_low_severity() {
        let (root, db) = fresh_db("few");
        // Snapshot present so the snapshot gap is silent; isolate the volume nudge.
        let nodes = json!([{ "name": "Lobby AHU", "path": "Building\\Lobby\\AHU" }]);
        projects::add_snapshot(&db, "v1", Some("building.bos"), &nodes).unwrap();
        projects::context_add(&db, "one.csv", b"device,bus\nLobby AHU,Modbus\n").unwrap();

        let s = suggest(&db, None).unwrap();
        assert!(
            s.iter().any(|x| x.kind == "context" && x.severity == "low"),
            "got: {s:?}"
        );
        // With a snapshot loaded there is no snapshot suggestion.
        assert!(s.iter().all(|x| x.kind != "snapshot"), "got: {s:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn caps_documentation_suggestions_per_query() {
        let (root, db) = fresh_db("cap");
        let nodes = json!([{ "name": "Lobby AHU", "path": "Building\\Lobby\\AHU" }]);
        projects::add_snapshot(&db, "v1", Some("building.bos"), &nodes).unwrap();
        projects::context_add(&db, "ahu.csv", b"device,bus\nLobby AHU,Modbus\n").unwrap();

        // Five undocumented salient terms -> capped at MAX_TERM_SUGGESTIONS.
        let q = "chiller boiler damper economizer humidifier";
        let s = suggest(&db, Some(q)).unwrap();
        let docs = s.iter().filter(|x| x.kind == "documentation").count();
        assert_eq!(docs, MAX_TERM_SUGGESTIONS, "got: {s:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
