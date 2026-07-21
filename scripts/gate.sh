#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== fmt"
cargo fmt --check
echo "== clippy"
cargo clippy --all-targets --all-features -- -D warnings
echo "== deny"
cargo deny check
echo "== audit"
cargo audit
echo "== test"
if command -v cargo-nextest >/dev/null 2>&1; then cargo nextest run; else cargo test; fi
echo "== release build"
cargo build --release
echo "== doc"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --quiet
echo "== gate PASSED"
