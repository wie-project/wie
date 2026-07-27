#!/usr/bin/env bash
# Pre-PR checklist: format, lint, unit tests, integration (micro-suite).
# Exits non-zero on first failure.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "=== cargo fmt ==="
cargo fmt --all --check --manifest-path "$ROOT/Cargo.toml"

echo "=== cargo clippy ==="
cargo clippy --workspace --all-targets --manifest-path "$ROOT/Cargo.toml" -- -D warnings

echo "=== cargo test ==="
cargo test --workspace --manifest-path "$ROOT/Cargo.toml"

echo "=== micro-suite ==="
# Separate statements, not `make && suite`: `set -e` exempts the left operand of
# `&&`, so a micro-exe compile failure used to skip the suite and still report success.
make -C "$ROOT/micro-exes"
"$ROOT/scripts/run-micro-suite.sh"

echo "=== all checks passed ==="
