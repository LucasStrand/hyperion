// Hyperion — knowledge crawler sidecar (Phase 7, M7; Requirements #24/#25/#26).
//
// A small, mostly-pure module that turns official docs / forum pages into cached
// project knowledge and proposes deterministic "eureka" improvements from what was
// found. It is split exactly like `embed.rs`:
//
//   - PURE, unit-tested core: `extract_text` (strip HTML to title + body text) and
//     `eureka` (propose "look at X" when crawled docs mention terms that are absent
//     from the project's loaded context). Both are deterministic and offline.
//   - NETWORK edge: `fetch` makes a single capped, timed `GET` over `ureq`, mirroring
//     `embed::embed` / `agent::ask_openrouter`. It is gated behind an explicit
//     opt-in env flag, so a blank/disabled config returns `Err` and offline/CI stays
//     green. Only `http(s)` URLs are allowed and any error text is redacted.
//
// Security: the crawler only ever READS remote pages — it never writes to bOS and
// never sends project data off the machine (it ships only the URL). Stored page text
// is secret-scanned by the `projects::crawl_*` layer before it lands in the project
// DB. Error/response text surfaced to the UI is scrubbed through `agent::redact_secrets`.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

use serde::Serialize;

use crate::agent::{redact_secrets, tail_chars};
use crate::ingest;

/// Hard ceiling on a single page fetch. Generous for a slow docs/forum host but
/// bounded so a stalled endpoint can't wedge the crawl — on expiry `fetch` errors
/// and the caller simply reports it (knowledge crawling is best-effort).
const CRAWL_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on the fetched HTTP body (mirrors `agent::MAX_CAPTURE` x2): a runaway or
/// malicious page cannot exhaust memory before the timeout. Anything past this is
/// truncated; `extract_text` still parses what arrived.
const MAX_CAPTURE: u64 = 4 * 1024 * 1024;

/// Identifies the client to docs/forum hosts; harmless metadata (mirrors the
/// `HTTP-Referer`/`X-Title` headers `agent::ask_openrouter` sets).
const USER_AGENT: &str = "Hyperion-bOS-Copilot/1.0 (+https://hyperion.app)";

/// bOS terms whose novelty is the most actionable for these two integrators, so a
/// crawled mention of them outranks generic novel vocabulary in `eureka`. The
/// Configurator, the Service, and the Client are the three pillars of a ComfortClick
/// bOS install (see `agent::INSTINCTS`); `comfortclick`/`bos` anchor the product
/// itself. Documented weighting per the M7 handoff.
pub const PRIORITY_TERMS: [&str; 5] = ["configurator", "service", "client", "comfortclick", "bos"];

/// Added to a novel term's doc-frequency score when it is one of `PRIORITY_TERMS`,
/// so a bOS pillar term always sorts ahead of incidental vocabulary regardless of
/// how often the latter appears across the crawled corpus.
const PRIORITY_BOOST: u32 = 100;

/// At most this many eureka suggestions, so a large crawled corpus can't bury the
/// operator. The highest-weighted (then alphabetically-stable) terms win.
const MAX_SUGGESTIONS: usize = 8;

// ----------------------------- eureka (pure) -----------------------------

/// One proposed "you should look at X" improvement. `term` is the novel vocabulary
/// the crawled docs surfaced, `weight` ranks it (doc frequency + bOS priority
/// boost), `source` is the title of the first crawled doc that mentioned it, and
/// `message` is the human-readable nudge. Serializes to a flat JSON object the
/// renderer lists directly (same shape contract as `suggest::Suggestion`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Suggestion {
    pub term: String,
    pub weight: u32,
    pub source: String,
    pub message: String,
}

/// Propose improvements from crawled `docs` (`(title, text)` pairs) relative to the
/// project's loaded `context_terms`. A term that appears in the crawled corpus but
/// in NONE of the context terms is "novel" — the operator likely hasn't grounded the
/// assistant in it yet. Each novel term yields one suggestion; its weight is the
/// number of distinct docs that mention it, plus `PRIORITY_BOOST` when it is a bOS
/// pillar term (Configurator/Service/Client/ComfortClick/bOS). Fully deterministic:
/// suggestions are sorted by weight (desc) then term (asc) and truncated to
/// `MAX_SUGGESTIONS`, and the source is the first mentioning doc in slice order.
/// No network, no I/O — unit-tested with synthetic inputs.
pub fn eureka(docs: &[(String, String)], context_terms: &[String]) -> Vec<Suggestion> {
    // Terms already present in the loaded context (lowercased to match `keywords`).
    let known: HashSet<String> = context_terms.iter().map(|t| t.to_lowercase()).collect();

    // novel term -> (distinct-doc count, first source title). A BTreeMap keeps the
    // intermediate map ordering deterministic; the final order is set by the sort.
    let mut found: BTreeMap<String, (u32, String)> = BTreeMap::new();
    for (title, text) in docs {
        let source = {
            let t = title.trim();
            if t.is_empty() {
                "(untitled)".to_string()
            } else {
                t.to_string()
            }
        };
        // `ingest::keywords` already lowercases, drops stop-words, and returns a
        // DISTINCT set, so each term counts at most once per doc — reusing it keeps
        // the crawler's notion of a "term" identical to the retrieval stack's.
        for term in ingest::keywords(text) {
            if known.contains(&term) {
                continue;
            }
            let entry = found.entry(term).or_insert((0, source.clone()));
            entry.0 += 1;
        }
    }

    let mut out: Vec<Suggestion> = found
        .into_iter()
        .map(|(term, (count, source))| {
            let weight = if is_priority(&term) {
                count + PRIORITY_BOOST
            } else {
                count
            };
            let message = format!(
                "Crawled docs mention \"{term}\" (e.g. in \"{source}\"), which isn't in your loaded \
                 project context yet — you should look at it."
            );
            Suggestion {
                term,
                weight,
                source,
                message,
            }
        })
        .collect();

    // Highest weight first; ties broken alphabetically for a stable, testable order.
    out.sort_by(|a, b| b.weight.cmp(&a.weight).then_with(|| a.term.cmp(&b.term)));
    out.truncate(MAX_SUGGESTIONS);
    out
}

/// Is `term` (already lowercased by `keywords`) a prioritized bOS pillar term?
fn is_priority(term: &str) -> bool {
    PRIORITY_TERMS.contains(&term)
}

// ----------------------------- eureka -> PR proposal (pure) -----------------------------

/// A human-approvable in-app PR drafted from eureka `Suggestion`s. `title` is the
/// list-view one-liner ("Knowledge proposal: N findings from crawl"); `narrative` is
/// the human-readable case (what was discovered, why it matters, a suggested next
/// action per finding — phrased so an operator approves or rejects it); `ai_docs` is
/// the structured, machine-readable list of the same findings (each novel term + its
/// source doc + weight). `count` is the number of findings. Built by `format_proposal`
/// and handed straight to `collab::pr_create`, which secret-scans every field before
/// it lands in the project DB.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProposalDraft {
    pub title: String,
    pub narrative: String,
    pub ai_docs: String,
    pub count: usize,
}

/// Format eureka `suggestions` into a human-approvable `ProposalDraft`, or `None` when
/// there is nothing novel to propose (an empty slice) so the caller reports "nothing
/// to propose" instead of opening an empty PR. The narrative and ai_docs are a direct,
/// ordered rendering of the input — fully deterministic, no network, no I/O — so the
/// formatting is unit-tested with synthetic suggestions. The order of `suggestions` is
/// preserved (the caller passes them already ranked by `eureka`).
pub fn format_proposal(suggestions: &[Suggestion]) -> Option<ProposalDraft> {
    if suggestions.is_empty() {
        return None;
    }
    let count = suggestions.len();
    let title = format!(
        "Knowledge proposal: {count} finding{} from crawl",
        plural(count)
    );
    Some(ProposalDraft {
        title,
        narrative: proposal_narrative(suggestions),
        ai_docs: proposal_ai_docs(suggestions),
        count,
    })
}

/// `""` for one, `"s"` otherwise — keeps the proposal copy grammatical.
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The human-readable case for the proposal: a short framing paragraph, then one
/// numbered finding per suggestion with a concrete "review and, if relevant, ground
/// the assistant on it" next action. Phrased as a proposal the operator approves or
/// rejects; nothing here is applied to bOS.
fn proposal_narrative(suggestions: &[Suggestion]) -> String {
    let count = suggestions.len();
    let mut out = format!(
        "The knowledge crawler compared this project's cached crawl docs against the \
         assistant's loaded context and surfaced {count} term{} that appear in the docs \
         but are not grounded in the project yet. This is a proposal for review: approve \
         the finding{} worth pursuing and reject the rest. Nothing here is written back \
         to bOS.\n\nFindings:\n",
        plural(count),
        plural(count)
    );
    for (i, s) in suggestions.iter().enumerate() {
        let pillar = if is_priority(&s.term) {
            " (bOS pillar term)"
        } else {
            ""
        };
        out.push_str(&format!(
            "\n{}. \"{}\"{} — seen in crawled doc \"{}\" (weight {}).\n   \
             Suggested next action: review \"{}\" in that source and, if relevant, add it \
             to the project context (a memory note or context file) so the assistant is \
             grounded on it.\n",
            i + 1,
            s.term,
            pillar,
            s.source,
            s.weight,
            s.term,
        ));
    }
    out
}

/// The structured, machine-readable side of the proposal: a stable JSON document
/// listing each finding (novel `term`, its `source` doc, and `weight`). Pretty-printed
/// for human-legibility in the PR's AI-docs pane. The plain fields never fail to
/// serialize, but a debug fallback keeps the proposal non-empty if it ever did.
fn proposal_ai_docs(suggestions: &[Suggestion]) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "kind": "crawl-eureka-proposal",
        "count": suggestions.len(),
        "findings": suggestions,
    }))
    .unwrap_or_else(|_| format!("{suggestions:?}"))
}

// ----------------------------- HTML extraction (pure) -----------------------------

/// Reduce an HTML page to `(title, text)`: the `<title>` contents (or empty) and the
/// visible body text with all tags, `<script>`/`<style>` blocks, and comments
/// removed, common entities decoded, and whitespace collapsed. Deterministic and
/// dependency-free (no HTML parser) — good enough to cache a docs/forum page as
/// searchable knowledge, and trivially unit-testable.
pub fn extract_text(html: &str) -> (String, String) {
    let title = extract_title(html);
    let stripped = strip_tags(html);
    let text = collapse_ws(&decode_entities(&stripped));
    (title, text)
}

/// Pull the first `<title>…</title>` contents (entity-decoded, whitespace-collapsed),
/// or an empty string when the page has no title.
fn extract_title(html: &str) -> String {
    // `to_ascii_lowercase` preserves byte length, so indices into `lower` map 1:1
    // onto `html` and slicing stays on valid UTF-8 boundaries (tags are ASCII).
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(gt) = lower[start..].find('>') else {
        return String::new();
    };
    let content_start = start + gt + 1;
    match lower[content_start..].find("</title>") {
        Some(end) => collapse_ws(&decode_entities(&html[content_start..content_start + end])),
        None => String::new(),
    }
}

/// Strip every tag from `html`, dropping `<script>`/`<style>` element contents and
/// HTML comments wholesale (their text is not page content). Each removed tag is
/// replaced by a single space so adjacent cells/words don't fuse (`<td>a</td><td>b</td>`
/// -> `a b`); `collapse_ws` later squeezes the runs.
fn strip_tags(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let lb = lower.as_bytes();
    let n = html.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if lb[i] == b'<' {
            // HTML comment: skip to the matching `-->` (or end of input).
            if lower[i..].starts_with("<!--") {
                i = lower[i + 4..]
                    .find("-->")
                    .map(|o| i + 4 + o + 3)
                    .unwrap_or(n);
                out.push(' ');
                continue;
            }
            // Raw-text elements: drop the whole element including its contents.
            if let Some(j) = skip_element(&lower, i, "<script", "</script>") {
                i = j;
                out.push(' ');
                continue;
            }
            if let Some(j) = skip_element(&lower, i, "<style", "</style>") {
                i = j;
                out.push(' ');
                continue;
            }
            // Any other tag: skip to its closing `>` (or end of input).
            match lower[i..].find('>') {
                Some(off) => i += off + 1,
                None => i = n,
            }
            out.push(' ');
        } else {
            // Copy one whole char (handles multi-byte UTF-8 between tags).
            let ch = html[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// If a raw-text element (`open` like `"<script"`) starts at byte `i`, return the
/// index just past its closing tag (`close` like `"</script>"`), consuming the whole
/// element. An unterminated element consumes to the end of input.
fn skip_element(lower: &str, i: usize, open: &str, close: &str) -> Option<usize> {
    if !lower[i..].starts_with(open) {
        return None;
    }
    let after_open = i + open.len();
    Some(
        lower[after_open..]
            .find(close)
            .map(|off| after_open + off + close.len())
            .unwrap_or(lower.len()),
    )
}

/// Decode the handful of HTML entities that actually show up in docs/forum body
/// text. `&amp;` is decoded LAST so an escaped sequence like `&amp;lt;` becomes the
/// literal `&lt;` rather than being double-decoded into `<`.
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Collapse every run of ASCII/Unicode whitespace (incl. newlines/tabs) to a single
/// space and trim the ends, so cached text is one clean searchable blob.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ----------------------------- the network fetch -----------------------------

/// Is remote crawling explicitly enabled? Reads `HYPERION_CRAWL_ENABLED`; an absent,
/// blank, or non-truthy value means OFF — so `fetch` returns `Err` and offline/CI
/// stays green with no network access (mirrors `embed::embed`'s API-key gate).
fn crawl_enabled() -> bool {
    matches!(
        std::env::var("HYPERION_CRAWL_ENABLED")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// The configured Firecrawl API key, if any. Reads `HYPERION_FIRECRAWL_API_KEY`,
/// trims surrounding whitespace, and returns `Some` only when what remains is
/// non-empty — so an absent or blank value leaves the crawler on its direct-`GET`
/// path (mirrors `embed.rs`'s key gate). The key is never logged or surfaced.
fn firecrawl_key() -> Option<String> {
    let key = std::env::var("HYPERION_FIRECRAWL_API_KEY")
        .unwrap_or_default()
        .trim()
        .to_string();
    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

/// True when a Firecrawl API key is configured (`HYPERION_FIRECRAWL_API_KEY` set and
/// non-blank). A thin public probe over the private [`firecrawl_key`] gate so callers
/// (e.g. the artifact-guide refresh) can give a clear "set the key" no-op message
/// without forcing a fetch. Never reveals the key itself.
pub fn firecrawl_configured() -> bool {
    firecrawl_key().is_some()
}

/// True if `ip` is one the crawler must never reach: loopback, RFC-1918 private,
/// link-local (incl. the `169.254.169.254` cloud-metadata endpoint), unspecified,
/// broadcast, documentation, or `0.0.0.0/8`. IPv6 loopback/unspecified, ULA
/// (`fc00::/7`), link-local (`fe80::/10`), and v4-mapped equivalents included.
fn ip_is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => v4_blocked(a),
        IpAddr::V6(a) => {
            if let Some(v4) = a.to_ipv4_mapped() {
                return v4_blocked(v4);
            }
            if a.is_loopback() || a.is_unspecified() {
                return true;
            }
            let s = a.segments();
            (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

fn v4_blocked(a: Ipv4Addr) -> bool {
    a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_broadcast()
        || a.is_documentation()
        || a.octets()[0] == 0
}

/// Parse `host` and `port` from an `http(s)` URL without a URL crate. Strips
/// scheme, path/query/fragment, and userinfo; understands `[ipv6]:port`.
fn host_port(url: &str) -> Option<(String, u16)> {
    let https = url.starts_with("https://");
    let after = url.split_once("://")?.1;
    let authority = after.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let default = if https { 443 } else { 80 };
    if let Some(rest) = authority.strip_prefix('[') {
        let (h, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default);
        return Some((h.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            Some((h.to_string(), p.parse().unwrap_or(default)))
        }
        _ => Some((authority.to_string(), default)),
    }
}

/// SSRF guard: resolve the URL's host and refuse if ANY resolved address is a
/// private/loopback/link-local/metadata IP. Conservative — a single blocked
/// address fails the whole fetch so a dual-homed name can't slip past. Residual:
/// DNS rebinding between this check and ureq's own resolve, acceptable for an
/// operator-driven desktop tool.
fn guard_public_host(url: &str) -> Result<(), String> {
    let (host, port) =
        host_port(url).ok_or_else(|| "crawler: could not parse URL host".to_string())?;
    let mut resolved = false;
    for sa in (host.as_str(), port).to_socket_addrs().map_err(|e| {
        format!(
            "crawler: host did not resolve: {}",
            redact_secrets(&e.to_string())
        )
    })? {
        resolved = true;
        if ip_is_blocked(sa.ip()) {
            return Err("crawler: refusing to fetch a private/loopback/link-local address".into());
        }
    }
    if !resolved {
        return Err("crawler: host did not resolve".into());
    }
    Ok(())
}

/// Fetch the raw HTML at `url` over a single capped, timed `GET`. Disabled by
/// default: without `HYPERION_CRAWL_ENABLED` set truthy this returns `Err` before
/// touching the network (so CI/offline stays green). Only `http(s)` URLs are allowed
/// — any other scheme is refused before egress. The response body is read under a
/// hard cap; any HTTP/transport error text is redacted (never echoed verbatim) so a
/// hostile page can't smuggle a credential-looking token into a surfaced error.
///
/// When `HYPERION_FIRECRAWL_API_KEY` is set, the fetch is first routed through the
/// hosted Firecrawl scrape API (which returns cleaner content for JS-heavy pages); a
/// Firecrawl failure transparently FALLS BACK to the direct `GET` below, so the key
/// is a pure enhancement and never weakens the offline/cap/redaction guarantees. The
/// gate (`crawl_enabled`) and scheme check apply identically to both paths.
pub fn fetch(url: &str) -> Result<String, String> {
    if !crawl_enabled() {
        return Err(
            "crawler is disabled — set HYPERION_CRAWL_ENABLED=1 to allow network fetches".into(),
        );
    }
    let url = url.trim();
    // Only http(s): refuse file://, data://, ftp://, or a typo'd scheme that could
    // read a local file or reach an unintended target.
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("crawler: only http:// and https:// URLs are allowed".into());
    }
    // Optional enhancement: when a Firecrawl key is configured, try the hosted scrape
    // API first. On any failure (network, HTTP, parse) we silently fall through to the
    // direct `GET` below — Firecrawl never makes a fetch fail that would otherwise work.
    if let Some(key) = firecrawl_key() {
        if let Ok(body) = fetch_via_firecrawl(url, &key) {
            return Ok(body);
        }
    }
    // SSRF guard for the direct egress path: refuse private/loopback/link-local/
    // metadata hosts, and do NOT follow redirects so a public URL can't 30x-bounce
    // onto an internal address.
    guard_public_host(url)?;
    let agent = ureq::builder().redirects(0).build();
    let resp = agent
        .get(url)
        .timeout(CRAWL_TIMEOUT)
        .set("User-Agent", USER_AGENT)
        .call();
    match resp {
        Ok(r) => Ok(read_body_capped(r)),
        // Surface the host's own error text (redacted, never raw) so the user can act.
        Err(ureq::Error::Status(code, r)) => {
            let detail = read_body_capped(r);
            Err(format!(
                "crawl HTTP {code}: {}",
                tail_chars(&redact_secrets(&detail), 400)
            ))
        }
        Err(e) => Err(format!(
            "crawl request failed: {}",
            redact_secrets(&e.to_string())
        )),
    }
}

/// Read an HTTP body under a hard cap so a large or malicious page cannot exhaust
/// memory (mirrors `embed::read_body_capped` / `agent::read_body_capped`).
fn read_body_capped(r: ureq::Response) -> String {
    let mut buf = Vec::new();
    let _ = r.into_reader().take(MAX_CAPTURE).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

// ----------------------------- Firecrawl (optional enhancement) -----------------------------

/// Firecrawl's hosted scrape endpoint — a single POST returns clean page content for
/// JS-heavy docs/forum pages that a raw `GET` would only see as an empty shell.
const FIRECRAWL_SCRAPE_URL: &str = "https://api.firecrawl.dev/v1/scrape";

/// Build the JSON request body for a Firecrawl `/v1/scrape` call: the target `url`
/// plus a request for the `"html"` format (which `extract_text` then strips exactly
/// like a direct fetch). Pure and deterministic — no network, no key — so it is
/// unit-tested in isolation from the network edge.
#[must_use]
pub fn build_firecrawl_request(url: &str) -> serde_json::Value {
    serde_json::json!({
        "url": url,
        "formats": ["html"],
    })
}

/// Parse a Firecrawl `/v1/scrape` JSON response and extract the page content.
///
/// On `{"success": true, "data": {…}}` this returns `data.html` when present and
/// non-empty, falling back to `data.markdown` (also non-empty) — either is fine input
/// for the caller, which strips/caches it. On `{"success": false, …}` it returns the
/// redacted `error` field (scrubbed through `redact_secrets`, never echoed raw, so a
/// hostile response can't smuggle a credential-looking token into a surfaced error).
/// Malformed JSON, a missing `data`, or an empty payload all yield a short `Err`.
/// Pure and offline — unit-tested with synthetic JSON.
pub fn parse_firecrawl_response(body: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("firecrawl: malformed JSON response: {e}"))?;

    // An explicit `success: false` carries the host's own (redacted) error text.
    if v.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
        let detail = v
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        return Err(format!(
            "firecrawl error: {}",
            tail_chars(&redact_secrets(detail), 400)
        ));
    }

    let data = v
        .get("data")
        .ok_or_else(|| "firecrawl: response missing `data`".to_string())?;

    // Prefer cleaned HTML; fall back to markdown. A present-but-empty field counts as
    // absent so we never hand the caller a blank page as if it were content.
    for field in ["html", "markdown"] {
        if let Some(s) = data.get(field).and_then(serde_json::Value::as_str) {
            if !s.trim().is_empty() {
                return Ok(s.to_string());
            }
        }
    }
    Err("firecrawl: response contained no html or markdown content".into())
}

/// Fetch `url` through Firecrawl: POST `build_firecrawl_request(url)` to the scrape
/// endpoint with `Authorization: Bearer <key>`, the shared `CRAWL_TIMEOUT`, read the
/// body under the same `MAX_CAPTURE` cap as the direct path, and hand it to
/// `parse_firecrawl_response`. All HTTP/transport error text is redacted via
/// `redact_secrets` + `tail_chars` exactly like `fetch`; the key itself is never
/// echoed. Returns the extracted page content (HTML or markdown) on success.
fn fetch_via_firecrawl(url: &str, key: &str) -> Result<String, String> {
    // Serialize the (pure) request body ourselves and send it as a raw JSON string so
    // we don't depend on ureq's optional `json` feature — `send_string` is always
    // available and the explicit Content-Type keeps Firecrawl happy.
    let body = build_firecrawl_request(url).to_string();
    let resp = ureq::post(FIRECRAWL_SCRAPE_URL)
        .timeout(CRAWL_TIMEOUT)
        .set("User-Agent", USER_AGENT)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {key}"))
        .send_string(&body);
    match resp {
        Ok(r) => parse_firecrawl_response(&read_body_capped(r)),
        // Surface the host's own error text (redacted, never raw) so the caller can act.
        Err(ureq::Error::Status(code, r)) => {
            let detail = read_body_capped(r);
            Err(format!(
                "firecrawl HTTP {code}: {}",
                tail_chars(&redact_secrets(&detail), 400)
            ))
        }
        Err(e) => Err(format!(
            "firecrawl request failed: {}",
            redact_secrets(&e.to_string())
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serializes any test that mutates process-global env vars (mirrors embed.rs).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn extract_text_strips_tags_scripts_styles_and_decodes() {
        let html = "<html><head><title>  Hello &amp; World </title>\
             <style>.a{color:red}</style></head>\
             <body><h1>Hi</h1><script>alert(1)</script>\
             <p>Some <b>bold</b> text &lt;ok&gt;.</p></body></html>";
        let (title, text) = extract_text(html);
        assert_eq!(title, "Hello & World");
        assert!(text.contains("Hi"), "got: {text}");
        // Tags become separators, entities decode, whitespace collapses.
        assert!(text.contains("Some bold text <ok>."), "got: {text}");
        // Script/style contents are gone entirely.
        assert!(!text.contains("alert"), "got: {text}");
        assert!(!text.contains("color:red"), "got: {text}");
    }

    #[test]
    fn extract_text_drops_comments_and_handles_no_title() {
        let html = "<div>before<!-- secret comment -->after</div>";
        let (title, text) = extract_text(html);
        assert_eq!(title, "");
        assert_eq!(text, "before after");
    }

    #[test]
    fn extract_text_survives_unterminated_tag() {
        // A truncated page (no closing `>`) must not panic; the dangling tag is
        // simply consumed to the end.
        let (_t, text) = extract_text("<p>kept<broken");
        assert_eq!(text, "kept");
    }

    #[test]
    fn eureka_flags_novel_terms_and_prioritizes_bos_pillars() {
        let docs = vec![
            (
                "Modbus Guide".to_string(),
                "The Modbus gateway maps registers to the Configurator service.".to_string(),
            ),
            (
                "KNX Notes".to_string(),
                "KNX scenes bind to the client.".to_string(),
            ),
        ];
        // The project already knows about modbus/gateway/registers.
        let ctx = vec![
            "modbus".to_string(),
            "gateway".to_string(),
            "registers".to_string(),
        ];
        let s = eureka(&docs, &ctx);

        // Known terms never appear as suggestions.
        assert!(!s.iter().any(|x| x.term == "modbus"));
        assert!(!s.iter().any(|x| x.term == "gateway"));

        // The three bOS pillar terms (configurator/service/client) are novel here and
        // must outrank the generic novel vocabulary; among the equally-weighted
        // pillars the order is alphabetical and stable.
        let top3: Vec<&str> = s.iter().take(3).map(|x| x.term.as_str()).collect();
        assert_eq!(top3, vec!["client", "configurator", "service"]);
        assert!(s[0].weight > PRIORITY_BOOST);
        // A non-pillar novel term is still surfaced, just ranked below.
        assert!(s.iter().any(|x| x.term == "scenes"));
        // Source is the first doc that mentioned the term.
        let conf = s.iter().find(|x| x.term == "configurator").unwrap();
        assert_eq!(conf.source, "Modbus Guide");
    }

    #[test]
    fn eureka_is_empty_when_nothing_is_novel() {
        let docs = vec![("Doc".to_string(), "alpha beta gamma".to_string())];
        let ctx = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        assert!(eureka(&docs, &ctx).is_empty());
    }

    #[test]
    fn eureka_counts_distinct_docs_and_caps_output() {
        // A term mentioned in two docs outweighs one mentioned once (no pillars here).
        let docs = vec![
            ("A".to_string(), "alpha alpha beta".to_string()),
            ("B".to_string(), "alpha delta".to_string()),
        ];
        let s = eureka(&docs, &[]);
        let alpha = s.iter().find(|x| x.term == "alpha").unwrap();
        // Distinct-doc count: 2 (not 3 despite the repeat in doc A).
        assert_eq!(alpha.weight, 2);
        assert_eq!(alpha.source, "A");
        // Result never exceeds the cap.
        assert!(s.len() <= MAX_SUGGESTIONS);
    }

    #[test]
    fn format_proposal_renders_narrative_and_structured_ai_docs() {
        // Two findings, a bOS pillar term first (as `eureka` would rank it).
        let suggestions = vec![
            Suggestion {
                term: "configurator".to_string(),
                weight: PRIORITY_BOOST + 1,
                source: "Modbus Guide".to_string(),
                message: "ignored by the formatter".to_string(),
            },
            Suggestion {
                term: "scenes".to_string(),
                weight: 1,
                source: "KNX Notes".to_string(),
                message: "ignored by the formatter".to_string(),
            },
        ];
        let draft = format_proposal(&suggestions).expect("non-empty -> Some draft");

        // Title states the count.
        assert_eq!(draft.title, "Knowledge proposal: 2 findings from crawl");
        assert_eq!(draft.count, 2);

        // Narrative names each term, its source doc, a per-finding next action, and
        // flags the pillar term — and is framed as an approve/reject proposal.
        assert!(
            draft.narrative.contains("proposal for review"),
            "{}",
            draft.narrative
        );
        assert!(
            draft.narrative.contains("\"configurator\""),
            "{}",
            draft.narrative
        );
        assert!(
            draft.narrative.contains("(bOS pillar term)"),
            "{}",
            draft.narrative
        );
        assert!(
            draft.narrative.contains("Modbus Guide"),
            "{}",
            draft.narrative
        );
        assert!(
            draft.narrative.contains("\"scenes\""),
            "{}",
            draft.narrative
        );
        assert!(draft.narrative.contains("KNX Notes"), "{}", draft.narrative);
        assert!(
            draft.narrative.contains("Suggested next action"),
            "{}",
            draft.narrative
        );

        // ai_docs is structured/machine-readable: parse it back and check the findings
        // carry term + source + weight in the same (ranked) order.
        let v: serde_json::Value = serde_json::from_str(&draft.ai_docs).expect("ai_docs is JSON");
        assert_eq!(v["kind"], "crawl-eureka-proposal");
        assert_eq!(v["count"], 2);
        let findings = v["findings"].as_array().expect("findings array");
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0]["term"], "configurator");
        assert_eq!(findings[0]["source"], "Modbus Guide");
        assert_eq!(findings[0]["weight"], PRIORITY_BOOST + 1);
        assert_eq!(findings[1]["term"], "scenes");
    }

    #[test]
    fn format_proposal_singular_title_and_grammar() {
        let suggestions = vec![Suggestion {
            term: "alpha".to_string(),
            weight: 1,
            source: "Doc".to_string(),
            message: String::new(),
        }];
        let draft = format_proposal(&suggestions).unwrap();
        // Singular noun (no trailing "s") for exactly one finding.
        assert_eq!(draft.title, "Knowledge proposal: 1 finding from crawl");
        assert!(
            draft.narrative.contains("1 term that appear"),
            "{}",
            draft.narrative
        );
    }

    #[test]
    fn format_proposal_is_none_when_no_suggestions() {
        // Empty eureka -> no draft, so the caller reports "nothing novel to propose"
        // instead of opening an empty PR.
        assert!(format_proposal(&[]).is_none());
    }

    #[test]
    fn fetch_disabled_by_default_is_recoverable_err() {
        // With the opt-in flag unset, fetch must Err before any network access so
        // offline/CI stays green. Guard the process-global env for determinism.
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("HYPERION_CRAWL_ENABLED").ok();
        std::env::remove_var("HYPERION_CRAWL_ENABLED");
        let err = fetch("https://example.com/docs").unwrap_err();
        assert!(err.contains("disabled"), "got: {err}");
        if let Some(p) = prev {
            std::env::set_var("HYPERION_CRAWL_ENABLED", p);
        }
    }

    #[test]
    fn fetch_rejects_non_http_scheme_even_when_enabled() {
        // A configured (enabled) crawler with a non-http(s) URL must fail before any
        // network/file access. Guard the process-global env for determinism.
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("HYPERION_CRAWL_ENABLED").ok();
        std::env::set_var("HYPERION_CRAWL_ENABLED", "1");
        let err = fetch("file:///etc/passwd").unwrap_err();
        assert!(err.contains("http"), "got: {err}");
        match prev {
            Some(p) => std::env::set_var("HYPERION_CRAWL_ENABLED", p),
            None => std::env::remove_var("HYPERION_CRAWL_ENABLED"),
        }
    }

    #[test]
    fn build_firecrawl_request_is_pure_scrape_body() {
        let v = build_firecrawl_request("https://docs.example.com/page");
        assert_eq!(v["url"], "https://docs.example.com/page");
        assert_eq!(v["formats"], serde_json::json!(["html"]));
    }

    #[test]
    fn parse_firecrawl_response_prefers_html() {
        let body =
            r##"{"success":true,"data":{"html":"<h1>Hi</h1>","markdown":"# Hi","metadata":{}}}"##;
        assert_eq!(parse_firecrawl_response(body).unwrap(), "<h1>Hi</h1>");
    }

    #[test]
    fn parse_firecrawl_response_falls_back_to_markdown() {
        // Empty/absent html must fall through to non-empty markdown.
        let body = r##"{"success":true,"data":{"html":"   ","markdown":"# Heading"}}"##;
        assert_eq!(parse_firecrawl_response(body).unwrap(), "# Heading");
        let body2 = r##"{"success":true,"data":{"markdown":"# Only"}}"##;
        assert_eq!(parse_firecrawl_response(body2).unwrap(), "# Only");
    }

    #[test]
    fn parse_firecrawl_response_reports_failure_error_field() {
        let body = r#"{"success":false,"error":"rate limit exceeded"}"#;
        let err = parse_firecrawl_response(body).unwrap_err();
        assert!(err.contains("firecrawl error"), "got: {err}");
        assert!(err.contains("rate limit exceeded"), "got: {err}");
    }

    #[test]
    fn parse_firecrawl_response_errors_on_empty_and_malformed() {
        // Success but no usable content.
        let empty = r#"{"success":true,"data":{"html":"","markdown":""}}"#;
        assert!(parse_firecrawl_response(empty).is_err());
        // Missing data object.
        assert!(parse_firecrawl_response(r#"{"success":true}"#).is_err());
        // Not even JSON.
        assert!(parse_firecrawl_response("<not json>").is_err());
    }

    #[test]
    fn firecrawl_key_trims_and_gates_on_blank() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("HYPERION_FIRECRAWL_API_KEY").ok();

        std::env::remove_var("HYPERION_FIRECRAWL_API_KEY");
        assert_eq!(firecrawl_key(), None);

        std::env::set_var("HYPERION_FIRECRAWL_API_KEY", "   ");
        assert_eq!(firecrawl_key(), None);

        std::env::set_var("HYPERION_FIRECRAWL_API_KEY", "  fc-secret  ");
        assert_eq!(firecrawl_key(), Some("fc-secret".to_string()));

        match prev {
            Some(p) => std::env::set_var("HYPERION_FIRECRAWL_API_KEY", p),
            None => std::env::remove_var("HYPERION_FIRECRAWL_API_KEY"),
        }
    }

    #[test]
    fn firecrawl_configured_reflects_key_presence() {
        // The no-key gate the artifact-guide refresh uses for its graceful no-op.
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("HYPERION_FIRECRAWL_API_KEY").ok();

        std::env::remove_var("HYPERION_FIRECRAWL_API_KEY");
        assert!(!firecrawl_configured());

        std::env::set_var("HYPERION_FIRECRAWL_API_KEY", "   ");
        assert!(!firecrawl_configured(), "blank key is not configured");

        std::env::set_var("HYPERION_FIRECRAWL_API_KEY", "fc-secret");
        assert!(firecrawl_configured());

        match prev {
            Some(p) => std::env::set_var("HYPERION_FIRECRAWL_API_KEY", p),
            None => std::env::remove_var("HYPERION_FIRECRAWL_API_KEY"),
        }
    }

    #[test]
    fn crawl_enabled_reads_truthy_values() {
        let _env = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("HYPERION_CRAWL_ENABLED").ok();
        for (val, want) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("on", true),
            ("0", false),
            ("", false),
            ("nope", false),
        ] {
            std::env::set_var("HYPERION_CRAWL_ENABLED", val);
            assert_eq!(crawl_enabled(), want, "for {val:?}");
        }
        match prev {
            Some(p) => std::env::set_var("HYPERION_CRAWL_ENABLED", p),
            None => std::env::remove_var("HYPERION_CRAWL_ENABLED"),
        }
    }

    #[test]
    fn ssrf_blocks_private_and_metadata_addresses() {
        use std::net::{Ipv4Addr, Ipv6Addr};
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.3.4",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata
            "0.0.0.0",
        ] {
            let a: Ipv4Addr = ip.parse().unwrap();
            assert!(ip_is_blocked(IpAddr::V4(a)), "{ip} must be blocked");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            let a: Ipv4Addr = ip.parse().unwrap();
            assert!(!ip_is_blocked(IpAddr::V4(a)), "{ip} must be allowed");
        }
        assert!(ip_is_blocked(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        // ULA fc00::/7 and link-local fe80::/10
        assert!(ip_is_blocked("fd00::1".parse::<Ipv6Addr>().unwrap().into()));
        assert!(ip_is_blocked("fe80::1".parse::<Ipv6Addr>().unwrap().into()));
        // v4-mapped metadata address must also be caught
        assert!(ip_is_blocked(
            "::ffff:169.254.169.254".parse::<Ipv6Addr>().unwrap().into()
        ));
        assert!(!ip_is_blocked(
            "2606:4700:4700::1111".parse::<Ipv6Addr>().unwrap().into()
        ));
    }

    #[test]
    fn host_port_parses_scheme_userinfo_and_ipv6() {
        assert_eq!(
            host_port("https://docs.example.com/a/b?x=1"),
            Some(("docs.example.com".into(), 443))
        );
        assert_eq!(
            host_port("http://example.com:8080/x"),
            Some(("example.com".into(), 8080))
        );
        assert_eq!(
            host_port("https://user:pass@host.tld/p"),
            Some(("host.tld".into(), 443))
        );
        assert_eq!(host_port("http://[::1]:9000/"), Some(("::1".into(), 9000)));
    }
}
