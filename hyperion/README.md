# Hyperion

**An enterprise IoT workbench for ComfortClick bOS, operated by an AI agent.**

Hyperion is a local-first [Tauri](https://tauri.app/) desktop app (Rust backend + vanilla
TypeScript/Vite frontend) that lets an engineer load a building's `.bos` project, ask an
embedded agent *how to implement things* in the Configurator, and keep all project
knowledge — memory, instincts, docs, network credentials — in one encrypted, themeable
workbench. It is **strictly read-only toward bOS data**.

## Highlights

- **Configurator clone + ingestion** — upload any `.bos`, browse the parsed tree; add PDF/Word/text files as context (chunked + retrievable).
- **RAG over context** — cosine-embedding retrieval with a dependency-free keyword fallback (offline-safe).
- **The agent** — pluggable runtime (Claude Code → Codex → OpenRouter), pedagogical answers + runnable playbooks, per-project memory, versioned instincts, and a multi-agent roster.
- **Proactive context suggestions** and an **MCP/skill recommender** that reads the loaded context.
- **Multi-source knowledge crawler** — a curated per-project registry of official docs + forum URLs, swept in one tiered pass (cheap fetch+strip+store, then a deterministic eureka distill) that feeds the in-app PR loop.
- **Themeable wiki vault** — token-based themes with a switcher, editable & persisted pages, static-site export, and HTML-effectiveness artifacts.
- **Enterprise security** — AES-256-GCM vault (key in the OS keychain), Microsoft Entra SSO unlock, a plaintext-secret guardrail, a source security scan, and an enterprise-readiness gate.
- **Version control** — semantic `.bos` snapshot diffs, in-app PRs with comment threads, and a project timeline.
- **Vault-backed network registry** for the building's addresses and logins.

## Stack

- **Backend:** Rust (Tauri v2) — `src-tauri/src/` (`agent`, `vault`, `entra`, `ingest`, `embed`, `projects`, `roster`, `suggest`, `diff`, `standard`, `netreg`, `security`, `collab`, `tooling`, `export`).
- **Frontend:** vanilla TypeScript + Vite — `src/`.
- **Store:** per-project SQLite (`bundled` rusqlite); strictly local, git-ignored.

## Develop

```bash
npm install
npm run tauri dev      # run the desktop app
npm run build          # tsc + vite production build
cargo test --lib --manifest-path src-tauri/Cargo.toml   # Rust unit tests
```

CI (`.github/workflows/ci.yml`, at the repo root) runs `cargo fmt --check`,
`clippy -D warnings`, `cargo test --lib` on Ubuntu + Windows, plus the frontend build.

### Optional configuration

- `HYPERION_EMBED_API_KEY` / `HYPERION_EMBED_BASE_URL` / `HYPERION_EMBED_MODEL` — enable embedding-based RAG (otherwise keyword retrieval is used).
- `HYPERION_ENTRA_TENANT_ID` — Microsoft Entra tenant for SSO unlock.
- `HYPERION_OPENROUTER_MODEL`, OpenRouter key (env or vault) — agent runtime.
- `HYPERION_CRAWL_ENABLED=1` — allow the knowledge crawler to make network fetches (off by default, so offline/CI never reaches out). `HYPERION_FIRECRAWL_API_KEY` optionally routes fetches through Firecrawl for JS-heavy pages.

### Multi-source crawler & recurring sweeps

Curate official ComfortClick/IoT documentation and forum URLs in a project's **Crawl
sources** registry (Knowledge → Knowledge crawler). **Sweep now** fetches every enabled
source in one tiered pass:

- **Cheap pass** ("dumber crawler") — fetch + strip + store each source, deduped by
  url *and* content so a re-sweep is a safe no-op for unchanged pages.
- **Smart pass** ("smarter agent") — the deterministic eureka heuristic distills the
  refreshed corpus against the project's loaded context, surfacing novel terms that
  flow into the in-app PR loop (Propose PR from findings). This is a heuristic
  distiller, **not** an external LLM — nothing spawns a model from Rust.

**Recurring sweeps.** Hyperion is a desktop app and deliberately does **not** schedule
itself — there is no fake in-process cron. To sweep on a schedule, drive the
`crawl_sweep` command from *outside* the app with Claude Code's `CronCreate` / `loop`.
For example, have a scheduled Claude Code task open the project and run a tiny script
that invokes the `crawl_sweep` Tauri command (smart pass on), e.g. every morning:

```
# Claude Code, scheduled via CronCreate / `loop`:
loop 24h "open the Hyperion project <id> and run crawl_sweep(smart=true), then review any eureka findings and Propose PR"
```

The app provides the in-app sweep; the *schedule* lives in your Claude Code cron, never
inside Hyperion.

## Status

Progress is tracked live in [`docs/wiki/plan.html`](docs/wiki/plan.html). The app is read-only
toward bOS; all generated/imported data stays on the local machine.
