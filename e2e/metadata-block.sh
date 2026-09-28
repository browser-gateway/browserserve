#!/usr/bin/env bash
# Docker-image e2e for BROWSERSERVE_BLOCK_METADATA: off leaves the image unchanged;
# on blocks cloud metadata addresses for the runtime user and Chrome, rewrites a
# metadata-address resolver, and refuses to start without NET_ADMIN.
# Usage: IMAGE=browserserve:dev-arm64 e2e/metadata-block.sh   (needs Docker, and npm install in e2e/)
set -uo pipefail
cd "$(dirname "$0")"

IMAGE="${IMAGE:-browserserve:dev-arm64}"
NET="${NET:-mybridge}"
TOKEN=metadata-block-e2e
PORT=19444
C=bs-metadata-e2e
fail=0
check() { if [ "$2" = "1" ]; then echo "PASS  $1"; else echo "FAIL  $1  ($3)"; fail=1; fi; }
cleanup() { docker rm -f "$C" >/dev/null 2>&1 || true; }
trap cleanup EXIT

wait_ready() {
  for _ in $(seq 1 120); do
    [ "$(curl -s -m1 -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/ready")" = "200" ] && return 0
    sleep 0.5
  done
  return 1
}
curl_rc() { docker exec -u 999 "$C" curl -s -o /dev/null -m 3 "$@"; echo $?; }

cleanup
docker run -d --name "$C" --network "$NET" -p "$PORT:9222" --shm-size=1g -e BROWSERSERVE_TOKEN=$TOKEN "$IMAGE" >/dev/null
wait_ready; check "off: serves" "$([ $? = 0 ] && echo 1)" "not ready"
rules=$(docker exec "$C" sh -c 'iptables -S OUTPUT 2>&1 | grep -c REJECT')
check "off: no rules added" "$([ "$rules" = "0" ] && echo 1)" "rules=$rules"
check "off: resolv.conf untouched" "$(docker exec "$C" grep -q 'nameserver 1.1.1.1' /etc/resolv.conf || echo 1)" "rewritten"
cleanup

docker run -d --name "$C" --network "$NET" -p "$PORT:9222" --shm-size=1g --cap-add NET_ADMIN --dns 169.254.169.254 \
  -e BROWSERSERVE_TOKEN=$TOKEN -e BROWSERSERVE_BLOCK_METADATA=1 "$IMAGE" >/dev/null
wait_ready; check "on: serves" "$([ $? = 0 ] && echo 1)" "$(docker logs "$C" 2>&1 | tail -3)"
logs=$(docker logs "$C" 2>&1)
check "on: logs the block" "$(echo "$logs" | grep -q 'metadata addresses blocked' && echo 1)" "no log line"
v4=$(docker exec "$C" sh -c 'iptables -S OUTPUT | grep -c -E "REJECT|DROP"')
check "on: four IPv4 rules" "$([ "$v4" = "4" ] && echo 1)" "v4=$v4"
if docker exec "$C" test -e /proc/net/if_inet6; then
  v6=$(docker exec "$C" sh -c 'ip6tables -S OUTPUT | grep -c -E "REJECT|DROP"')
  check "on: four IPv6 rules" "$([ "$v6" = "4" ] && echo 1)" "v6=$v6"
fi
check "on: runtime user cannot flush rules" "$(docker exec -u 999 "$C" iptables -F OUTPUT >/dev/null 2>&1 || echo 1)" "flush succeeded"
rc=$(curl_rc http://169.254.169.254/); check "on: 169.254.169.254 refused" "$([ "$rc" = "7" ] && echo 1)" "curl rc=$rc"
rc=$(curl_rc http://169.254.170.2/); check "on: 169.254.170.2 refused" "$([ "$rc" = "7" ] && echo 1)" "curl rc=$rc"
rc=$(curl_rc http://100.100.100.200/); check "on: 100.100.100.200 refused" "$([ "$rc" = "7" ] && echo 1)" "curl rc=$rc"
rc=$(curl_rc -g 'http://[::ffff:169.254.169.254]/'); check "on: IPv4-mapped IPv6 refused" "$([ "$rc" = "7" ] && echo 1)" "curl rc=$rc"
check "on: resolv.conf rewritten" "$(docker exec "$C" grep -q 'nameserver 1.1.1.1' /etc/resolv.conf && echo 1)" "$(docker exec "$C" cat /etc/resolv.conf)"
check "on: public names resolve" "$(docker exec -u 999 "$C" getent hosts en.wikipedia.org >/dev/null && echo 1)" "no DNS"
BROWSERSERVE_URL="ws://127.0.0.1:$PORT" BROWSERSERVE_TOKEN=$TOKEN npx tsx metadata-block-browser.ts || fail=1
cleanup

out=$(timeout 60 docker run --rm --network "$NET" -e BROWSERSERVE_BLOCK_METADATA=1 "$IMAGE" 2>&1); code=$?
check "on without NET_ADMIN: refuses to start" "$([ "$code" != "0" ] && echo 1)" "exit=$code"
check "on without NET_ADMIN: names the fix" "$(echo "$out" | grep -q 'cap-add NET_ADMIN' && echo 1)" "$out"

[ "$fail" = "0" ] && echo "ALL PASS" || { echo "FAILURES"; exit 1; }
