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

## Status

Progress is tracked live in [`docs/wiki/plan.html`](docs/wiki/plan.html). The app is read-only
toward bOS; all generated/imported data stays on the local machine.
