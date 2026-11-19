# AGENTS.md — `matrix-rat`

`matrix-rat` is a mouse-friendly, keyboard-first, themeable Matrix terminal client (Ratatui + `matrix-sdk`, Rust 2024 edition, MSRV 1.96, Apache-2.0). It also provides an agent daemon (`matrix-rat serve`) and per-room MCP server (`matrix-rat mcp`).

## Structure
- `src/` — Rust TUI, Matrix client, audio/media, and `src/agent/` (`serve`, `mcp`, `send`/`read`/`listen`)
- `docs/` — Design docs (`PLAN.md`, `REVIEW-LOG.md`, `reviews/`, `screenshots/`)
- `scripts/check.sh` — Milestone gate (`fmt`, `clippy`, `test`, `build`)
- `default.nix`, `shell.nix` — Nix package & matching Rust toolchain dev shell

## Build & Verification
Run inside `nix-shell` so `rustc`, `clippy`, and `rustfmt` match:
```bash
nix-shell --run scripts/check.sh
# Or individually inside nix-shell:
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check
# Server-backed ignored tests (requires local dev homeserver):
cargo test -- --ignored --test-threads=1
```

## Key Rules & Safety
- **Exact `matrix-sdk` pin**: `matrix-sdk` and `matrix-sdk-base` are pinned (`=0.19.1`) because minor releases break APIs; do not loosen carelessly.
- **No blocking calls in async code**: Respect `clippy.toml` bans on blocking calls without justification.
- **No concurrent profile sharing**: Interactive `matrix-rat` and `matrix-rat serve` cannot share the same `--profile` simultaneously; stop `serve` before opening that profile interactively.
- **Privacy & security**: Never log message content, secrets, IDs, paths, or raw error text in logs/`Debug` output. Never use `--allow-unverified` in agent MCP configs and never bind agents to direct chats.
- **Tags**: Never move a published tag; ship fixes as a new point release.

## Compact Instructions
When compacting, always preserve: current release/milestone state, `scripts/check.sh` results, active worktrees/branches, open review items, and pending decisions.
