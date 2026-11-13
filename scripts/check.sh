#!/usr/bin/env bash
# Everything a milestone must pass before review. Needs cargo, rustfmt and clippy from
# the same Rust release; `nix-shell --run scripts/check.sh` provides them.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test  # the server-backed tests are extra: cargo test -- --ignored --test-threads=1
cargo build  # keep target/debug/matrix-rat current for manual runs
