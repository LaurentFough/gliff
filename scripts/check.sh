#!/usr/bin/env bash
# Run the same checks as CI (.github/workflows/ci.yml) before pushing:
# formatting, clippy with warnings as errors, and the unit tests.
# Pass --fix to let rustfmt and clippy apply their fixes first.
set -euo pipefail
cd "$(dirname "$0")/.."

#= The workspace's minimum Rust (rust-version in Cargo.toml). An older
#= toolchain fails later with a confusing dependency error (e.g. clap_lex
#= needing edition 2024), so stop here with the fix instead.
need=$(sed -n 's/^rust-version *= *"\([0-9.]*\)".*/\1/p' Cargo.toml | head -n 1)
have=$(rustc --version 2>/dev/null | awk '{print $2}' | cut -d- -f1)
if [ -z "$have" ]; then
    echo "check.sh: rustc not found; install Rust with rustup (https://rustup.rs)" >&2
    exit 1
fi
if [ -n "$need" ] && [ "$(printf '%s\n%s\n' "$need" "$have" | sort -V | head -n 1)" != "$need" ]; then
    echo "check.sh: Rust $have is older than the required $need ($(command -v rustc))." >&2
    echo "  rustup:   rustup update stable" >&2
    echo "  Homebrew: brew upgrade rust   (or use rustup instead)" >&2
    echo "  Arch:     sudo pacman -Syu rust   (or rustup)" >&2
    exit 1
fi

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
