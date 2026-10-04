#!/usr/bin/env bash
# Reports whether the bundled browser is behind; with --apply, moves the pins
# and checksums to the newest builds.
#   amd64: Google's Chrome for Testing, Stable channel.
#   arm64: Chromium build of the newest released playwright-core.
set -euo pipefail
cd "$(dirname "$0")/.."

DOCKERFILE=docker/Dockerfile
CFT_JSON=https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json
APPLY="${1:-}"

current_version=$(sed -n 's/^ARG CHROMIUM_VERSION=//p' "$DOCKERFILE")
current_revision=$(sed -n 's/^ARG CHROMIUM_REVISION=//p' "$DOCKERFILE")

stable_version=$(curl -fsSL "$CFT_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["channels"]["Stable"]["version"])')
pw_version=$(npm view playwright-core version)
read -r pw_revision pw_browser < <(curl -fsSL "https://unpkg.com/playwright-core@${pw_version}/browsers.json" \
  | python3 -c 'import json,sys; b=[b for b in json.load(sys.stdin)["browsers"] if b["name"]=="chromium"][0]; print(b["revision"], b["browserVersion"])')

echo "amd64: pinned ${current_version}, stable ${stable_version}"
echo "arm64: pinned revision ${current_revision}, playwright-core ${pw_version} ships revision ${pw_revision} (${pw_browser})"

behind=0
[[ "$current_version" != "$stable_version" ]] && behind=1
[[ "$current_revision" != "$pw_revision" ]] && behind=1
if [[ "$behind" == 0 ]]; then
  echo "up to date"
  exit 0
fi
if [[ "$APPLY" != "--apply" ]]; then
  echo "behind (run with --apply to update the pins)"
  exit 2
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

amd64_url="https://storage.googleapis.com/chrome-for-testing-public/${stable_version}/linux64/chrome-linux64.zip"
arm64_url="https://cdn.playwright.dev/dbazure/download/playwright/builds/chromium/${pw_revision}/chromium-linux-arm64.zip"
curl -fsSL -o "$tmp/amd64.zip" "$amd64_url"
curl -fsSL -o "$tmp/arm64.zip" "$arm64_url"
amd64_sha=$(shasum -a 256 "$tmp/amd64.zip" | cut -d' ' -f1)
arm64_sha=$(shasum -a 256 "$tmp/arm64.zip" | cut -d' ' -f1)

python3 - "$DOCKERFILE" "$stable_version" "$pw_revision" "$pw_version" "$pw_browser" <<'PY'
import re, sys
path, version, revision, pw_version, pw_browser = sys.argv[1:]
text = open(path).read()
text = re.sub(r"^ARG CHROMIUM_VERSION=.*$", f"ARG CHROMIUM_VERSION={version}", text, flags=re.M)
text = re.sub(r"^ARG CHROMIUM_REVISION=.*$", f"ARG CHROMIUM_REVISION={revision}", text, flags=re.M)
text = re.sub(r"playwright-core \([^)]*\)", f"playwright-core ({pw_version}, Chromium {pw_browser})", text)
open(path, "w").write(text)
PY
echo "${amd64_sha}  chrome-linux64.zip" > docker/checksums/chrome-linux64.sha256
echo "${arm64_sha}  chromium-linux-arm64.zip" > docker/checksums/chromium-linux-arm64.sha256
echo "updated: amd64 ${stable_version}, arm64 revision ${pw_revision} (${pw_browser})"
