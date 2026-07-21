#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

ARCH="${1:-arm64}"
case "$ARCH" in
  arm64) TRIPLE="aarch64-unknown-linux-musl" ;;
  amd64) TRIPLE="x86_64-unknown-linux-musl" ;;
  *) echo "usage: $0 [arm64|amd64]" >&2; exit 1 ;;
esac

CHROMIUM_SHA256_ARM64="$(cut -d' ' -f1 docker/checksums/chromium-linux-arm64.sha256 2>/dev/null || true)"
CHROMIUM_SHA256_AMD64="$(cut -d' ' -f1 docker/checksums/chrome-linux64.sha256 2>/dev/null || true)"

cargo zigbuild --release --target "$TRIPLE"
mkdir -p "docker/bin/$ARCH"
cp "target/$TRIPLE/release/browserserve" "docker/bin/$ARCH/browserserve"

docker build \
  --platform "linux/$ARCH" \
  --build-arg CHROMIUM_SHA256_ARM64="$CHROMIUM_SHA256_ARM64" \
  --build-arg CHROMIUM_SHA256_AMD64="$CHROMIUM_SHA256_AMD64" \
  -f docker/Dockerfile \
  -t "browserserve:dev-$ARCH" \
  docker/
echo "built browserserve:dev-$ARCH"
