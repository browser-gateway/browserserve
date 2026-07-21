#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== fmt"
cargo fmt --check
echo "== clippy (host)"
cargo clippy --all-targets --all-features -- -D warnings

# The bulk of the isolation code is #[cfg(target_os = "linux")] and never
# compiles on a macOS host. Cross-clippy the Linux target so Linux-only lint
# and compile errors are caught locally, not only in CI. Requires the target:
#   rustup target add x86_64-unknown-linux-musl
if [ "$(uname -s)" = "Darwin" ] && rustup target list --installed | grep -q x86_64-unknown-linux-musl; then
  echo "== clippy (linux cross-check)"
  cargo clippy --target x86_64-unknown-linux-musl --all-targets --all-features -- -D warnings
else
  echo "== clippy (linux cross-check) SKIPPED — add x86_64-unknown-linux-musl target to enable"
fi

echo "== deny"
cargo deny check
echo "== audit"
cargo audit
echo "== unused deps (machete)"
if command -v cargo-machete >/dev/null 2>&1; then cargo machete; else echo "  SKIPPED — cargo install cargo-machete to enable"; fi
echo "== duplication (jscpd)"
if command -v npx >/dev/null 2>&1; then npx --yes jscpd; else echo "  SKIPPED — node/npx not found"; fi
echo "== test"
if command -v cargo-nextest >/dev/null 2>&1; then cargo nextest run; else cargo test; fi
echo "== release build"
cargo build --release
echo "== doc"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --quiet
echo "== gate PASSED"
echo "note: Linux-only tests (cgroup, rss, procfs) run in CI or via scripts/test-linux.sh;"
echo "      the host test pass above skips #[cfg(target_os=\"linux\")] tests on macOS."
