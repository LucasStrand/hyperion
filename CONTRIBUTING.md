# Contributing to Hyperion

Thanks for your interest. Hyperion is a Tauri 2 desktop app (Rust core + Vite/TypeScript
webview) that helps integrators understand and safely evolve ComfortClick bOS systems.
See the [README](README.md) for the architecture and phase status.

## Development setup

Prerequisites: [Rust](https://rustup.rs), Node 18+, and the
[Tauri prerequisites](https://tauri.app/start/prerequisites/) for your OS. The `.bos` parser is
pure Rust (`src-tauri/src/bosparse.rs`), so Python 3 is optional — it is only used as an
automatic fallback for the parser (`pip install nrbf`).

```bash
cd hyperion
npm install
npm run tauri dev      # run the app (NOT `cargo run` — that skips the webview)
```

## Before you open a PR

All of these must pass locally (and are enforced by CI on every PR):

```bash
# Rust core
cd hyperion/src-tauri
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib

# Frontend
cd hyperion
npm run build
```

## Pull request flow

1. Branch from `main`.
2. Keep the change focused; write a clear PR description — **what** changed, **why**, and
   **how you verified it** (the PR template prompts for this).
3. CI (ubuntu + windows) must be green, and the automated review (CodeRabbit) plus a
   maintainer review must pass before merge.

## Ground rules for the codebase

- **Read-only toward bOS.** The app is advisory and never writes back to a live system.
  Don't add write-back paths.
- **Secrets only in the vault.** Never log, echo, or render a secret, and never put one
  into an agent prompt. Never commit secrets or customer `.bos` exports — the
  `.gitignore` blocks `*.bos`, `bos_map.json`, vault blobs, and project databases.
- **No new unsafe Rust** without a clear, reviewed justification.
- Match the surrounding code's style and comment density. Keep `clippy -D warnings` clean.

## Reporting bugs / security issues

Bugs: open an issue using the template. Security vulnerabilities: see
[SECURITY.md](SECURITY.md) — please report privately, not as a public issue.
