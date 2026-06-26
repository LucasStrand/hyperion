# Hyperion

**An enterprise IoT workbench for ComfortClick bOS — operated by an AI agent.**

Hyperion is a desktop application ([Tauri 2](https://tauri.app) — a Rust core with a
Vite/TypeScript webview) that integrators use to understand, document, and safely evolve
real building-automation systems. It ingests a project's context (a ComfortClick `.bos`
export, gateway configs, datasheets), renders a faithful **read-only** clone of the bOS
Configurator, and lets an AI agent answer *"how do I implement X in ComfortClick"* with
pedagogical steps **and** finished, auto-gradable playbooks — all grounded in the system
that is actually loaded.

> **Read-only by design.** Hyperion never writes back to a live bOS system. It is advisory:
> you apply changes yourself in the real Configurator and re-export. The loaded `.bos` is
> treated as the source of truth.

## Status

Built in reviewable, adversarially-reviewed phases:

| Phase | Milestone | What shipped |
|------:|-----------|--------------|
| 0 | Foundation | Tauri scaffold, read-only Configurator view, `.bos` parser, per-project SQLite store |
| 1 | Secure shell | AES-256-GCM encrypted vault (key in the OS keychain), Microsoft Entra SSO unlock (OAuth 2.0 + PKCE), plaintext-secret guardrail |
| 2 | The agent | Local-first runtime adapter (Claude Code → Codex → OpenRouter), instinct-driven grounded answers, chat dock |

Phases 3–5 (context ingestion + RAG, themeable wiki & standards enforcement, GitHub-style
semantic diff/PRs) are designed and on the roadmap.

## Architecture

```
Tauri 2 desktop app
├─ Webview (Vite + TypeScript)   Configurator view · agent chat dock · vault UI
└─ Rust core (Tauri commands)    the trusted layer
   ├─ projects.rs   per-project SQLite store (snapshots, memory, wiki, timeline)
   ├─ vault.rs      AES-256-GCM secrets, DEK in the OS keychain
   ├─ entra.rs      Microsoft Entra OAuth (auth-code + PKCE) → vault unlock
   └─ agent.rs      runtime adapter: Claude Code | Codex | OpenRouter
```

All secret-handling lives in the Rust core (smallest attack surface). The Python parser
(`bos_explore.py`) and the original renderer are reused from the `bos-copilot` prototype.

## Security model

- **Secrets only in the vault.** Encrypted at rest (AES-256-GCM); the data-encryption key
  lives in the OS keychain and vault unlock is gated behind Microsoft Entra sign-in.
- **Nothing secret is ever logged or rendered.** The vault is never read into an agent prompt.
- **The agent runs no shell.** Local CLIs are spawned by exact executable with constant args;
  the prompt travels on stdin (data, never flags), under a hard timeout that kills the whole
  process tree.
- **Sample/customer `.bos` exports are never committed** — see `.gitignore`. Import one at
  runtime.

## Build & run

Prerequisites: [Rust](https://rustup.rs), Node 18+, and the
[Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS. Python 3 is used
by the `.bos` parser.

```bash
cd hyperion
npm install
npm run tauri dev      # run the desktop app (NOT `cargo run` — that skips the webview)

# Rust-only checks:
cd src-tauri
cargo test
cargo clippy --all-targets -- -D warnings
```

Optional environment overrides: `HYPERION_AGENT_RUNTIME` (`claude`/`codex`/`openrouter`),
`HYPERION_OPENROUTER_MODEL`, `OPENROUTER_API_KEY`, `HYPERION_ENTRA_CLIENT_ID` /
`HYPERION_ENTRA_TENANT_ID`.

## Repository layout

```
hyperion/            the Tauri app
  index.html         webview shell
  src/               TypeScript (Configurator view, vault UI, agent dock)
  src-tauri/         Rust core
  docs/wiki/         themeable feature documentation (the project "vault")
bos_explore.py       the .bos parser (produces bos_map.json)
playbooks/           auto-gradable change playbooks
```

## Contributing

Pull requests are reviewed by [CodeRabbit](https://coderabbit.ai) (config in
`.coderabbit.yaml`) and by an internal adversarial multi-agent review before merge.

## License

[Apache-2.0](LICENSE) © 2026 Lucas Strand.
