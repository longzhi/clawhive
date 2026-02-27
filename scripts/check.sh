#!/usr/bin/env bash
set -euo pipefail

echo "[check] cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "[check] cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "[check] cargo test --workspace"
cargo test --workspace
