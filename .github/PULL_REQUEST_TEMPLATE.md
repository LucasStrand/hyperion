<!-- A clear human narrative is required — it's how a reviewer (and your future self) understands the change. -->

## What changed


## Why


## How it was verified
<!-- e.g. cargo test / clippy / npm run build output, manual steps, screenshots -->


## Checklist
- [ ] `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --lib` pass
- [ ] `npm run build` passes
- [ ] No secrets, credentials, or customer `.bos` / `bos_map.json` are committed
- [ ] The app remains **read-only** toward bOS (no write-back paths added)
- [ ] Docs updated if the change is user-facing (`hyperion/docs/wiki/`)
