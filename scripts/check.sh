#!/usr/bin/env bash
# Run the same checks as CI (.github/workflows/ci.yml) before pushing:
# formatting, clippy with warnings as errors, and the unit tests.
# Pass --fix to let rustfmt and clippy apply their fixes first.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ "${1:-}" = "--fix" ]; then
    cargo fmt --all
    cargo clippy --fix --workspace --all-targets --allow-dirty --allow-staged --locked
fi

echo "== fmt"
cargo fmt --all --check
echo "== clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings
echo "== test"
cargo test --workspace --locked
echo "== install root steps"
sh scripts/install-root-check.sh
echo "CHECK PASS"
