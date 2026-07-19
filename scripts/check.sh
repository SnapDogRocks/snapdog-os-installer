#!/bin/sh
set -eu

export RUSTFLAGS="-Dwarnings"

cargo fmt --check
cargo clippy --locked --all-targets --all-features
cargo test --locked
cargo +1.88.0 check --locked --all-targets

if command -v cargo-audit >/dev/null 2>&1; then
  cargo audit
else
  echo "cargo-audit is not installed; skipping dependency audit" >&2
fi
if command -v cargo-deny >/dev/null 2>&1; then
  cargo deny check
else
  echo "cargo-deny is not installed; skipping policy check" >&2
fi
