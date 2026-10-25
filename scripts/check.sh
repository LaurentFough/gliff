#!/usr/bin/env bash
# Run the same checks as CI (.github/workflows/ci.yml) before pushing:
# formatting, clippy with warnings as errors, and the unit tests.
# Pass --fix to let rustfmt and clippy apply their fixes first.
set -euo pipefail
cd "$(dirname "$0")/.."

packages=(--workspace)
if [ "$(uname -s)" = Darwin ]; then
    packages=(-p gliff-proto -p gliff-transport -p gliff-sw -p gliff-macos -p gliff -p gliff-server)
fi

if [ "${1:-}" = "--fix" ]; then
    cargo fmt --all
    cargo clippy --fix "${packages[@]}" --all-targets --allow-dirty --allow-staged --locked
fi

echo "== fmt"
cargo fmt --all --check
echo "== clippy"
cargo clippy "${packages[@]}" --all-targets --locked -- -D warnings
echo "== test"
cargo test "${packages[@]}" --locked
echo "== discovery service"
python3 scripts/discovery-service-check.py
if [ "$(uname -s)" = Darwin ]; then
    echo "== native bridge memory"
    python3 scripts/macos-memory-check.py
    echo "== native viewer input"
    python3 scripts/macos-input-check.py
    echo "== native keyboard layout"
    python3 scripts/macos-keyboard-layout-check.py
    echo "== native picker keyboard"
    python3 scripts/macos-picker-check.py
fi
echo "CHECK PASS"
