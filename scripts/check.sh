#!/usr/bin/env bash
# Everything a milestone must pass before review.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
