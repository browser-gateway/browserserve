#!/usr/bin/env bash
# Run the test suite on native Linux in a container, so #[cfg(target_os =
# "linux")] tests (cgroup, rss/procfs) actually execute off a macOS host.
# Uses a separate target dir so it never clobbers host build artifacts.
set -euo pipefail
cd "$(dirname "$0")/.."

IMAGE="${RUST_IMAGE:-rust:1-bookworm}"
exec docker run --rm \
  -v "$PWD":/src:ro \
  -e CARGO_TARGET_DIR=/target \
  -w /src \
  "$IMAGE" \
  bash -c "cargo test --locked"
