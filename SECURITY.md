# Security Policy

Hyperion handles real building-automation credentials and is built security-first.
We take vulnerability reports seriously.

## Reporting a vulnerability

**Please do not open a public issue for security problems.** Use GitHub's private
vulnerability reporting: the repository **Security** tab → **Report a vulnerability**.
That opens a private advisory visible only to the maintainers.

Include: what you found, how to reproduce it, the impact, and (if you have one) a
suggested fix. We aim to acknowledge within a few days.

## Supported versions

The project is pre-1.0 and evolves on `main`. Fixes land on `main`; there are no
back-ported release branches yet.

## Security model (what to attack, and what's by-design)

- **Secrets live only in the encrypted vault.** Secrets are sealed with AES-256-GCM;
  the data-encryption key lives in the OS keychain and vault unlock is gated behind
  Microsoft Entra sign-in. Secrets are never logged, never rendered to the UI in full,
  and never read into an agent prompt.
- **Read-only toward bOS.** Hyperion never writes back to a live ComfortClick bOS
  system. It is advisory; changes are applied by the operator in the real Configurator.
- **The agent runs no shell.** Local CLI runtimes are spawned by exact executable with
  constant arguments; the prompt is passed on stdin (data, never flags), under a hard
  timeout that kills the whole process tree.
- **Untrusted `.bos` content is fenced** before reaching the model, and model output is
  HTML-escaped before rendering.

Findings that strengthen any of the above — vault/keychain handling, the Entra/OAuth
flow, subprocess handling, prompt-injection surfaces, or secret leakage into logs or
the DOM — are especially welcome.

## Out of scope

- Issues requiring a compromised local OS account (the threat model is a single trusted
  operator on their own machine).
- The intentional read-only posture toward bOS.
