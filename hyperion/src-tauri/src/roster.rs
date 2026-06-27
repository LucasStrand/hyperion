// Hyperion — multi-agent roster + versioned instincts + share protocol (M5).
//
// Phase 2 gives the co-pilot a *roster* of role-specialized agents instead of one
// monolith. Each agent carries: a one-line role, a block of standing role
// instincts (heuristics injected into its system prompt on top of the shared
// `agent::INSTINCTS`), routing keywords, and an explicit *share protocol* — when
// to hand a question to another agent. A Coordinator is the generalist fallback
// and the default when no specialist clearly fits.
//
// Instincts are *versioned and persistent*. The built-in role instincts are the
// baseline (conceptually "version 0", in-binary, never stored). An operator can
// override them per project; every save appends a new version, so history is
// append-only — a "revert" copies an old body forward as a brand-new version and
// never destroys anything. Overrides live in the project DB table
// `agent_instincts` (created in `projects::init_db` for forward-compat + self-heal).
//
// Strictly local and read-only toward bOS. Instinct bodies are scanned for
// plaintext secrets on write (they are spliced verbatim into the system prompt)
// and length-capped; real secrets belong only in the encrypted vault, never here.
//
// Trust note: an agent's role instincts sit in the *trusted instructions* region
// of the prompt (alongside `agent::INSTINCTS`), because they are authored by the
// operator deliberately configuring their own agent — not untrusted `.bos`/crawled
// data, which is fenced separately. They are framed as *additions that refine, but
// never override*, the standing instincts (above all the security reflex and the
// read-only rule), so a customization cannot quietly disable those guarantees.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};

use crate::vault;

/// One rule in an agent's share protocol: hand off to the agent `to` when any of
/// `when` (lowercased keywords / phrases) appears in the question.
pub struct Handoff {
    pub to: &'static str,
    pub when: &'static [&'static str],
}

/// A role-specialized agent in the roster.
pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    /// One-line description of what this agent is for (shown in the picker).
    pub role: &'static str,
    /// Built-in role instincts — the version-0 baseline an operator can override.
    pub instincts: &'static str,
    /// Lowercased routing keywords used by the deterministic Coordinator router.
    pub keywords: &'static [&'static str],
    /// Explicit share protocol: when to hand the question to another agent.
    pub handoffs: &'static [Handoff],
    /// True only for the generalist Coordinator (the routing fallback / default).
    pub coordinator: bool,
}

/// The built-in roster: a Coordinator plus the seven specialists from the plan.
/// Order is stable and used as the deterministic tie-break in `route`.
pub static ROSTER: &[Agent] = &[
    Agent {
        id: "coordinator",
        name: "Coordinator",
        role: "Routes each question to the right specialist and answers general or cross-cutting questions.",
        instincts: "You are the Coordinator. Decide which specialist a request belongs to. If it spans several, answer the general parts yourself and explicitly name which specialist should take the rest. Keep the user oriented: say who is best placed to help and why.",
        keywords: &[],
        handoffs: &[],
        coordinator: true,
    },
    Agent {
        id: "configurator",
        name: "Configurator-Expert",
        role: "Builds and debugs logic in the bOS Configurator: objects, programs, Modbus/KNX/LoRa device mapping, and runnable playbooks.",
        instincts: "Always cite real node paths from the loaded .bos — never invent objects. When the fix is a concrete sequence of Configurator edits, ALSO emit a runnable ```playbook``` block so it can be auto-graded. Prefer the minimal change and name the exact objects, registers, and properties involved.",
        keywords: &[
            "configurator", "object", "program", "logic", "modbus", "knx", "lora",
            "milesight", "register", "slave", "gateway", "node", "playbook", "wire",
            "mapping", "trigger", "expression",
        ],
        handoffs: &[
            Handoff { to: "service", when: &["schedule", "trend", "backup", "server", "driver"] },
            Handoff { to: "design", when: &["client", "visual", "theme", "dashboard"] },
            Handoff { to: "security", when: &["password", "secret", "token", "key", "credential", "plaintext"] },
            Handoff { to: "docwriter", when: &["document", "wiki", "guide"] },
        ],
        coordinator: false,
    },
    Agent {
        id: "service",
        name: "Service-Expert",
        role: "The bOS Service runtime: schedules, trends, drivers, logs, backups, licensing, and spinning up local bOS servers.",
        instincts: "Distinguish Configurator *design* from Service *runtime* behavior. For timing or scheduling, give concrete intervals and explain the trade-off. When the user needs a local bOS server, walk the lifecycle steps in order.",
        keywords: &[
            "service", "schedule", "trend", "driver", "backup", "server", "restart",
            "log", "logs", "history", "license", "cron", "poll", "interval", "uptime",
        ],
        handoffs: &[
            Handoff { to: "configurator", when: &["object", "program", "modbus", "knx", "logic"] },
            Handoff { to: "security", when: &["password", "secret", "token", "credential"] },
            Handoff { to: "docwriter", when: &["document", "wiki", "guide"] },
        ],
        coordinator: false,
    },
    Agent {
        id: "design",
        name: "Client/Design-Expert",
        role: "The bOS Client: customer-facing visualization, theming, layout, and design taste for stunning clients.",
        instincts: "Optimize for the customer's experience: clear hierarchy, restrained color, legible at a glance. Suggest concrete layout and theme choices, not vague advice, and respect the project's existing visual language.",
        keywords: &[
            "client", "design", "theme", "visual", "layout", "icon", "dashboard",
            "color", "colour", "style", "screen", "widget", "graphic", "aesthetic",
        ],
        handoffs: &[
            Handoff { to: "configurator", when: &["object", "logic", "modbus", "program"] },
            Handoff { to: "docwriter", when: &["document", "wiki", "guide"] },
        ],
        coordinator: false,
    },
    Agent {
        id: "crawler",
        name: "Crawler",
        role: "Fetches official ComfortClick docs/knowledgebase and tracks ComfortClick/IoT forums with provenance (full capability lands in M7).",
        instincts: "Cite sources with provenance — say where the fact came from. Be explicit that live crawling is not yet wired (M7): for now, point to where the answer would come from and exactly what to fetch.",
        keywords: &[
            "docs", "documentation", "forum", "manual", "datasheet", "knowledgebase",
            "reference", "official", "spec", "datasheets", "kb",
        ],
        handoffs: &[
            Handoff { to: "docwriter", when: &["wiki", "writeup", "document"] },
            Handoff { to: "configurator", when: &["modbus", "register", "object"] },
        ],
        coordinator: false,
    },
    Agent {
        id: "reviewer",
        name: "Standards/Reviewer",
        role: "Reviews configs and code against the project's standards: naming, structure, safety, and consistency.",
        instincts: "Be specific and prioritized: list findings worst-first with the exact location and a concrete fix. Separate must-fix from nice-to-have, and briefly note what is already done right.",
        keywords: &[
            "review", "standard", "standards", "lint", "audit", "quality", "naming",
            "convention", "conventions", "refactor", "consistency", "practice",
        ],
        handoffs: &[
            Handoff { to: "security", when: &["password", "secret", "token", "credential", "vault"] },
            Handoff { to: "configurator", when: &["object", "program", "playbook"] },
        ],
        coordinator: false,
    },
    Agent {
        id: "docwriter",
        name: "Doc-Writer",
        role: "Produces wiki artifacts and documentation, choosing the right shape (diagram, table, timeline) for the information.",
        instincts: "Match the artifact to the information's shape — a diff, a flowchart, a table, a timeline — not prose for everything. Write for the next integrator who opens this project cold.",
        keywords: &[
            "wiki", "document", "documentation", "writeup", "html", "diagram",
            "artifact", "guide", "explain", "handover", "manual",
        ],
        handoffs: &[
            Handoff { to: "design", when: &["theme", "visual", "style"] },
            Handoff { to: "reviewer", when: &["standard", "review"] },
        ],
        coordinator: false,
    },
    Agent {
        id: "security",
        name: "Security",
        role: "Secrets hygiene, the encrypted vault, plaintext-password detection, and the pre-PR security gate.",
        instincts: "Security reflex first: if you see a plaintext password, API key, or token, stop and flag it and recommend moving it into Hyperion's encrypted vault. Never repeat a secret in full. Default to the safer option and explain the risk concretely.",
        keywords: &[
            "security", "secret", "password", "vault", "token", "encrypt",
            "encryption", "credential", "credentials", "ssl", "tls", "auth",
            "plaintext", "leak", "exposed",
        ],
        handoffs: &[
            Handoff { to: "reviewer", when: &["standard", "review", "audit"] },
        ],
        coordinator: false,
    },
];

/// Upper bound on a stored instinct body (bytes) — mirrors the memory-note cap.
const MAX_INSTINCTS_LEN: usize = 8192;

/// High-confidence secret shapes refused in an instinct body (same set the memory
/// store rejects): instincts are spliced verbatim into the system prompt, so a
/// real credential pasted here would leak into context. Looser heuristics
/// false-positive on ordinary guidance, so only the unambiguous shapes are blocked.
const HIGH_CONFIDENCE_SECRETS: [&str; 3] = ["private_key", "aws_access_key", "bearer_token"];

/// Look up an agent by id.
pub fn get(id: &str) -> Option<&'static Agent> {
    ROSTER.iter().find(|a| a.id == id)
}

/// The generalist Coordinator (routing fallback and default agent).
pub fn coordinator() -> &'static Agent {
    ROSTER
        .iter()
        .find(|a| a.coordinator)
        .expect("roster always contains a Coordinator")
}

/// Split a string into lowercased alphanumeric word tokens.
fn tokens(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Does a keyword/phrase match the question? Multi-word keywords match as a
/// substring of the lowercased text; single words match a whole token (so "key"
/// does not fire on "monkey").
fn kw_hit(kw: &str, lower: &str, toks: &HashSet<String>) -> bool {
    if kw.contains(' ') {
        lower.contains(kw)
    } else {
        toks.contains(kw)
    }
}

/// Deterministically route a question to the best-fitting specialist. Scores each
/// specialist by whole-word keyword hits in the question (+1 if the focused node's
/// type also matches a keyword) and returns the highest scorer; ties break by
/// roster order. Falls back to the Coordinator when nothing scores. No model call —
/// routing is offline, pure, and unit-testable.
pub fn route(question: &str, focus_type: Option<&str>) -> &'static Agent {
    let lower = question.to_lowercase();
    let toks = tokens(question);
    // A focused node's `type` is a compound identifier (e.g. "ModbusRegister"),
    // so it is matched by substring — we *want* "modbus" to hit "ModbusRegister" —
    // unlike the question text, which is matched whole-word to avoid "key"/"monkey".
    let ftype = focus_type.unwrap_or("").to_lowercase();

    let mut best: Option<(&'static Agent, usize)> = None;
    for a in ROSTER {
        if a.coordinator {
            continue;
        }
        let mut score = 0usize;
        for kw in a.keywords {
            if kw_hit(kw, &lower, &toks) {
                score += 2;
            }
            if !ftype.is_empty() && ftype.contains(kw) {
                score += 1;
            }
        }
        if score > 0 {
            // Strictly greater keeps the earliest (highest-priority) agent on ties.
            if best.map(|(_, bs)| score > bs).unwrap_or(true) {
                best = Some((a, score));
            }
        }
    }
    best.map(|(a, _)| a).unwrap_or_else(coordinator)
}

/// The share protocol in action: which other agents this agent should hand the
/// question to, per its declared handoff rules. Deduplicated, in rule order.
pub fn suggest_handoffs(agent: &Agent, question: &str) -> Vec<&'static Agent> {
    let lower = question.to_lowercase();
    let toks = tokens(question);
    let mut out: Vec<&'static Agent> = Vec::new();
    for h in agent.handoffs {
        if h.when.iter().any(|w| kw_hit(w, &lower, &toks)) {
            if let Some(t) = get(h.to) {
                if !out.iter().any(|x| x.id == t.id) {
                    out.push(t);
                }
            }
        }
    }
    out
}

// ----------------------------- versioned instincts -----------------------------

/// The active (latest stored) instinct override for an agent, as
/// `(version, body, updated_at)`, or `None` if the agent uses its built-in baseline.
pub fn instincts_active(
    db: &Path,
    agent_id: &str,
) -> Result<Option<(i64, String, String)>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    conn.query_row(
        "SELECT version, body, updated_at FROM agent_instincts
         WHERE agent_id = ?1 ORDER BY version DESC LIMIT 1",
        rusqlite::params![agent_id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        },
    )
    .optional()
    .map_err(|e| format!("read instincts: {e}"))
}

/// The instinct body actually used to prompt this agent: the active override if
/// one exists, else the built-in baseline.
pub fn instincts_resolved(db: &Path, agent: &Agent) -> String {
    instincts_active(db, agent.id)
        .ok()
        .flatten()
        .map(|(_, body, _)| body)
        .unwrap_or_else(|| agent.instincts.to_string())
}

/// Validate an instinct body for storage: trim, require non-empty within the cap,
/// and refuse a high-confidence plaintext secret. Returns the cleaned body.
fn validate_body(body: &str) -> Result<String, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("instinct body cannot be empty".into());
    }
    if body.len() > MAX_INSTINCTS_LEN {
        return Err("instinct body is too long (max 8 KB)".into());
    }
    let has_secret = vault::scan_for_secrets(body).iter().any(|f| {
        f.get("kind")
            .and_then(|k| k.as_str())
            .is_some_and(|k| HIGH_CONFIDENCE_SECRETS.contains(&k))
    });
    if has_secret {
        return Err(
            "this instinct looks like it contains a plaintext secret — store secrets in the encrypted vault, not in instincts".into(),
        );
    }
    Ok(body.to_string())
}

/// Append a new instinct version for `agent_id` and return the new version number.
/// The body is validated and secret-scanned. The next version is `MAX(version)+1`,
/// computed and inserted inside one IMMEDIATE transaction so two writers cannot
/// race onto the same version (the `agent_instincts_ver_uq` index is the backstop).
pub fn instincts_set(db: &Path, agent_id: &str, body: &str) -> Result<i64, String> {
    if get(agent_id).is_none() {
        return Err(format!("unknown agent: {agent_id}"));
    }
    let body = validate_body(body)?;
    let mut conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| format!("begin tx: {e}"))?;
    let next: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM agent_instincts WHERE agent_id = ?1",
            rusqlite::params![agent_id],
            |r| r.get(0),
        )
        .map_err(|e| format!("compute version: {e}"))?;
    tx.execute(
        "INSERT INTO agent_instincts(agent_id, version, body, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))",
        rusqlite::params![agent_id, next, body],
    )
    .map_err(|e| format!("insert instincts: {e}"))?;
    tx.commit().map_err(|e| format!("commit: {e}"))?;
    Ok(next)
}

/// Full version history for an agent, newest first: `[{version, updated_at, chars,
/// preview}]`. The built-in baseline (version 0) is not stored and is not listed —
/// callers surface it separately.
pub fn instincts_history(db: &Path, agent_id: &str) -> Result<Vec<Value>, String> {
    let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT version, updated_at, body FROM agent_instincts
             WHERE agent_id = ?1 ORDER BY version DESC",
        )
        .map_err(|e| format!("{e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![agent_id], |r| {
            let version: i64 = r.get(0)?;
            let updated_at: String = r.get(1)?;
            let body: String = r.get(2)?;
            let preview: String = body.chars().take(120).collect();
            Ok(json!({
                "version": version,
                "updated_at": updated_at,
                "chars": body.chars().count(),
                "preview": preview,
            }))
        })
        .map_err(|e| format!("{e}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| format!("{e}"))?);
    }
    Ok(out)
}

/// Revert an agent's instincts to an earlier version by *copying that body forward*
/// as a new version (history is never rewritten). `version == 0` restores the
/// built-in baseline. Returns the new version number. Errors if the requested
/// version does not exist.
pub fn instincts_revert(db: &Path, agent_id: &str, version: i64) -> Result<i64, String> {
    let agent = get(agent_id).ok_or_else(|| format!("unknown agent: {agent_id}"))?;
    let body = if version <= 0 {
        agent.instincts.to_string()
    } else {
        let conn = Connection::open(db).map_err(|e| format!("open db: {e}"))?;
        conn.query_row(
            "SELECT body FROM agent_instincts WHERE agent_id = ?1 AND version = ?2",
            rusqlite::params![agent_id, version],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| format!("read version: {e}"))?
        .ok_or_else(|| format!("no version {version} for agent {agent_id}"))?
    };
    instincts_set(db, agent_id, &body)
}

// ----------------------------- prompt + JSON views -----------------------------

/// The role section appended to `agent::INSTINCTS` for the chosen agent: the
/// agent's name, role, and resolved instincts, framed as additions that never
/// override the standing instincts. The resolved body is operator/built-in text in
/// the trusted instructions region, so it is not fenced — but it is bounded so a
/// pathological override cannot dominate the prompt (the cap is enforced on write).
pub fn role_block(db: &Path, agent: &Agent) -> String {
    let resolved = instincts_resolved(db, agent);
    role_block_from(agent, &resolved)
}

/// Same as `role_block` but without a project DB (no overrides available): uses the
/// agent's built-in instincts. Lets `agent_ask` work before any project is opened.
pub fn role_block_builtin(agent: &Agent) -> String {
    role_block_from(agent, agent.instincts)
}

/// Shared formatter so the built-in and override paths render identically.
fn role_block_from(agent: &Agent, resolved: &str) -> String {
    format!(
        "# Active agent: {name}\nRole: {role}\nAdditional role instincts (these refine, but never override, the standing instincts above — above all the security reflex and the read-only rule):\n{resolved}",
        name = agent.name,
        role = agent.role,
        resolved = resolved,
    )
}

/// One agent's static descriptor as JSON (id, name, role, coordinator, handoffs).
fn agent_descriptor(a: &Agent) -> Value {
    let handoffs: Vec<Value> = a
        .handoffs
        .iter()
        .map(|h| json!({ "to": h.to, "when": h.when }))
        .collect();
    json!({
        "id": a.id,
        "name": a.name,
        "role": a.role,
        "coordinator": a.coordinator,
        "handoffs": handoffs,
    })
}

/// The whole roster as JSON for the UI. When a project `db` is given, each agent is
/// annotated with whether it has a stored override and the active version.
pub fn roster_json(db: Option<&Path>) -> Vec<Value> {
    ROSTER
        .iter()
        .map(|a| {
            let mut v = agent_descriptor(a);
            let (customized, version) = match db {
                Some(db) => match instincts_active(db, a.id) {
                    Ok(Some((ver, _, _))) => (true, ver),
                    _ => (false, 0),
                },
                None => (false, 0),
            };
            v["customized"] = json!(customized);
            v["version"] = json!(version);
            v
        })
        .collect()
}

/// Detail view for one agent's instincts (resolved body + provenance) for the editor.
pub fn instincts_detail(db: &Path, agent_id: &str) -> Result<Value, String> {
    let agent = get(agent_id).ok_or_else(|| format!("unknown agent: {agent_id}"))?;
    let active = instincts_active(db, agent_id)?;
    let (version, body, updated_at, customized) = match active {
        Some((ver, body, ts)) => (ver, body, Some(ts), true),
        None => (0, agent.instincts.to_string(), None, false),
    };
    Ok(json!({
        "id": agent.id,
        "name": agent.name,
        "role": agent.role,
        "builtin": agent.instincts,
        "body": body,
        "version": version,
        "updated_at": updated_at,
        "customized": customized,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects;
    use std::path::PathBuf;

    /// A fresh project DB under an isolated root; returns `(root, db_path)`.
    fn fresh_db(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hyperion_roster_test_{}_{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let summary = projects::create(&root, "Roster Test").unwrap();
        let id = summary.get("id").unwrap().as_str().unwrap().to_string();
        let db = root.join(&id).join("project.db");
        (root, db)
    }

    #[test]
    fn roster_has_coordinator_and_unique_ids() {
        assert!(ROSTER.iter().filter(|a| a.coordinator).count() == 1);
        assert_eq!(coordinator().id, "coordinator");
        let mut ids: Vec<&str> = ROSTER.iter().map(|a| a.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "agent ids must be unique");
        // Every handoff target must resolve to a real roster agent.
        for a in ROSTER {
            for h in a.handoffs {
                assert!(get(h.to).is_some(), "{} -> {} is dangling", a.id, h.to);
            }
        }
    }

    #[test]
    fn route_picks_the_right_specialist() {
        assert_eq!(
            route("How do I map a Modbus register to an object?", None).id,
            "configurator"
        );
        assert_eq!(
            route("Set up a nightly backup schedule on the server", None).id,
            "service"
        );
        assert_eq!(
            route("Make the client dashboard theme look cleaner", None).id,
            "design"
        );
        assert_eq!(
            route("Is this password stored safely?", None).id,
            "security"
        );
        assert_eq!(
            route("Review my naming conventions for standards", None).id,
            "reviewer"
        );
        assert_eq!(
            route("Write a wiki handover document with a diagram", None).id,
            "docwriter"
        );
        assert_eq!(
            route("Find the official datasheet in the knowledgebase", None).id,
            "crawler"
        );
        // Nothing matches -> the Coordinator is the fallback.
        assert_eq!(route("hello there", None).id, "coordinator");
        assert_eq!(route("", None).id, "coordinator");
    }

    #[test]
    fn route_avoids_substring_false_positives_and_uses_focus_type() {
        // "monkey" must NOT match the security keyword "key" (single-word tokens).
        assert_eq!(route("the monkey escaped", None).id, "coordinator");
        // A neutral question with no keyword hits falls to the Coordinator…
        assert_eq!(route("show me this", None).id, "coordinator");
        // …but the focused node's compound type ("ModbusRegister" ⊇ "modbus")
        // nudges the same neutral question to the Configurator-Expert.
        let a = route("show me this", Some("ModbusRegister"));
        assert_eq!(a.id, "configurator");
    }

    #[test]
    fn share_protocol_suggests_handoffs() {
        let cfg = get("configurator").unwrap();
        // A Configurator question that mentions a plaintext password hands to Security.
        let h = suggest_handoffs(cfg, "wire this object but the password is in plaintext");
        assert!(
            h.iter().any(|a| a.id == "security"),
            "expected security handoff"
        );
        // No trigger -> no handoff.
        assert!(suggest_handoffs(cfg, "map register 3 to the pump object").is_empty());
    }

    #[test]
    fn instincts_versioning_is_append_only() {
        let (root, db) = fresh_db("ver");

        // No override yet: resolved == built-in, active == None.
        let cfg = get("configurator").unwrap();
        assert_eq!(instincts_resolved(&db, cfg), cfg.instincts);
        assert!(instincts_active(&db, "configurator").unwrap().is_none());

        // First save -> version 1; second -> version 2; resolved tracks the latest.
        let v1 = instincts_set(&db, "configurator", "Always start with the loaded tree.").unwrap();
        assert_eq!(v1, 1);
        let v2 = instincts_set(&db, "configurator", "Prefer the smallest playbook.").unwrap();
        assert_eq!(v2, 2);
        assert_eq!(
            instincts_resolved(&db, cfg),
            "Prefer the smallest playbook."
        );
        let (active_ver, _, _) = instincts_active(&db, "configurator").unwrap().unwrap();
        assert_eq!(active_ver, 2);

        // History is newest-first and complete.
        let hist = instincts_history(&db, "configurator").unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0]["version"], 2);
        assert_eq!(hist[1]["version"], 1);

        // Revert to v1 appends v3 with v1's body (never destroys v2).
        let v3 = instincts_revert(&db, "configurator", 1).unwrap();
        assert_eq!(v3, 3);
        assert_eq!(
            instincts_resolved(&db, cfg),
            "Always start with the loaded tree."
        );
        assert_eq!(instincts_history(&db, "configurator").unwrap().len(), 3);

        // Revert to 0 restores the built-in baseline as a new version.
        let v4 = instincts_revert(&db, "configurator", 0).unwrap();
        assert_eq!(v4, 4);
        assert_eq!(instincts_resolved(&db, cfg), cfg.instincts);

        // Reverting to a non-existent version errors.
        assert!(instincts_revert(&db, "configurator", 99).is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn instincts_set_validates_agent_body_and_secrets() {
        let (root, db) = fresh_db("validate");

        // Unknown agent is rejected.
        assert!(instincts_set(&db, "nobody", "x").is_err());
        // Empty / whitespace body rejected.
        assert!(instincts_set(&db, "service", "   ").is_err());
        // Over-cap body rejected; at-cap accepted.
        let huge = "x".repeat(MAX_INSTINCTS_LEN + 1);
        assert!(instincts_set(&db, "service", &huge).is_err());
        let at_cap = "y".repeat(MAX_INSTINCTS_LEN);
        assert!(instincts_set(&db, "service", &at_cap).is_ok());
        // A pasted private key is refused.
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        let err = instincts_set(&db, "security", key).unwrap_err();
        assert!(
            err.contains("secret") || err.contains("vault"),
            "got: {err}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn role_block_includes_role_and_resolved_instincts() {
        let (root, db) = fresh_db("roleblock");
        let cfg = get("configurator").unwrap();

        // Built-in path mentions the role and the standing-instincts guardrail.
        let b = role_block(&db, cfg);
        assert!(b.contains("Configurator-Expert"));
        assert!(b.contains("never override"));
        assert!(b.contains(cfg.instincts));

        // After an override, the block carries the new body, not the built-in.
        instincts_set(&db, "configurator", "Custom: cite paths only.").unwrap();
        let b2 = role_block(&db, cfg);
        assert!(b2.contains("Custom: cite paths only."));
        assert!(!b2.contains(cfg.instincts));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn roster_json_annotates_customization() {
        let (root, db) = fresh_db("json");
        let before = roster_json(Some(&db));
        let cfg_before = before.iter().find(|v| v["id"] == "configurator").unwrap();
        assert_eq!(cfg_before["customized"], json!(false));
        assert_eq!(cfg_before["version"], json!(0));

        instincts_set(&db, "configurator", "tweak").unwrap();
        let after = roster_json(Some(&db));
        let cfg_after = after.iter().find(|v| v["id"] == "configurator").unwrap();
        assert_eq!(cfg_after["customized"], json!(true));
        assert_eq!(cfg_after["version"], json!(1));

        let _ = std::fs::remove_dir_all(&root);
    }
}
