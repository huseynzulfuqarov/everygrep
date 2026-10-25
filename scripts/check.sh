#!/bin/sh
# Gate that must pass before every commit.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
echo "check: OK"
