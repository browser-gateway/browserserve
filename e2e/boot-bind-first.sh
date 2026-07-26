#!/usr/bin/env bash
# Regression test for the bind-first startup (issue: Railway 502 on boot).
#
# The server MUST bind and answer /json/version with 200 immediately, even when
# the boot browser is slow or stuck. We simulate a stuck browser with a fake
# "chrome" that never speaks CDP, and assert the port serves within 2s. If the
# boot ever blocks on a browser launch again, this fails (it took ~launch_timeout
# before the fix).
#
# Also asserts scale-to-zero: with pool.minReady=0, no browser launches at boot.
#
# Usage: e2e/boot-bind-first.sh   (builds the release binary if missing)
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/browserserve
[ -x "$BIN" ] || cargo build --release
DIR=$(mktemp -d)
trap 'pkill -f "$BIN serve" 2>/dev/null || true; rm -rf "$DIR"' EXIT

cat > "$DIR/fakechrome" <<'EOF'
#!/bin/sh
exec sleep 120
EOF
chmod +x "$DIR/fakechrome"

serve_ok() { # $1=config $2=port  -> echoes seconds-to-first-200 (or "TIMEOUT")
  BROWSERSERVE_CONFIG="$1" PORT="$2" HOST=127.0.0.1 "$BIN" serve >"$DIR/log" 2>&1 &
  local start=$SECONDS
  while [ $((SECONDS - start)) -le 6 ]; do
    if [ "$(curl -s -m1 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$2/json/version" 2>/dev/null)" = "200" ]; then
      echo $((SECONDS - start)); pkill -f "$BIN serve" 2>/dev/null || true; return 0
    fi
    sleep 1
  done
  echo TIMEOUT; pkill -f "$BIN serve" 2>/dev/null || true; return 1
}

fail=0

echo "== warm mode (minReady=1) binds fast despite a stuck boot browser =="
cat > "$DIR/warm.yml" <<EOF
pool: { minReady: 1 }
chrome: { executablePath: $DIR/fakechrome, launchTimeoutMs: 8000, noSandbox: true }
EOF
t=$(serve_ok "$DIR/warm.yml" 9401)
if [ "$t" = TIMEOUT ] || [ "$t" -gt 2 ]; then echo "  FAIL: /json/version took ${t}s (>2s = boot blocked on browser)"; fail=1; else echo "  PASS: serving in ${t}s"; fi

echo "== scale-to-zero (minReady=0) boots with ZERO browsers =="
cat > "$DIR/cold.yml" <<EOF
pool: { minReady: 0 }
chrome: { executablePath: $DIR/fakechrome, launchTimeoutMs: 8000, noSandbox: true }
EOF
BROWSERSERVE_CONFIG="$DIR/cold.yml" PORT=9402 HOST=127.0.0.1 "$BIN" serve >"$DIR/cold.log" 2>&1 &
sleep 2
code=$(curl -s -m2 -o /dev/null -w '%{http_code}' http://127.0.0.1:9402/json/version || echo 000)
nchrome=$( { pgrep -f "$DIR/fakechrome" || true; } | wc -l | tr -d ' ')
pkill -f "$BIN serve" 2>/dev/null || true
if [ "$code" = 200 ] && [ "$nchrome" = 0 ]; then echo "  PASS: serves ($code) with $nchrome browsers at boot"; else echo "  FAIL: code=$code browsers=$nchrome (want 200 / 0)"; fail=1; fi

[ "$fail" = 0 ] && echo "BOOT-BIND-FIRST OK" || { echo "BOOT-BIND-FIRST FAILED"; exit 1; }
