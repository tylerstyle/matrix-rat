#!/usr/bin/env bash
# Everything a milestone must pass before review. Needs cargo, rustfmt and clippy from
# the same Rust release; `nix-shell --run scripts/check.sh` provides them.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets -- -D warnings  # clippy.toml: no blocking calls without a reason
# The core builds for the browser too (docs/WEB-PHASE-1.md): the library only, the command
# line is the terminal's. nixpkgs' rustc has the browser's standard library, and a check
# needs no linker. Its warnings are errors as well.
RUSTFLAGS="-D warnings" cargo check --lib --target wasm32-unknown-unknown
cargo test  # the server-backed tests are extra: cargo test -- --ignored --test-threads=1
cargo build  # keep target/debug/matrix-rat current for manual runs
