#!/bin/sh
# Root-to-drop entrypoint: when started as root on a host with a writable,
# delegated cgroup v2 subtree, self-delegate a per-session cgroup slice to the
# unprivileged runtime user, then exec the runtime as that user. Chrome must
# run non-root (its sandbox refuses root), so all real work happens as uid 999.
#
# When not root, or cgroupfs isn't writable, this is a no-op passthrough and the
# runtime rides the portable fallback tiers (killpg + rss-poll + plain-copy).
set -eu

RUNTIME_UID=999
RUNTIME_GID=999
CG=/sys/fs/cgroup

METADATA_V4="169.254.0.0/16 100.100.100.200/32"
METADATA_V6="fd00:ec2::254/128 fd20:ce::254/128"

block_fail() {
  echo "entrypoint: BROWSERSERVE_BLOCK_METADATA=1 but $1; refusing to start. Run as root with the NET_ADMIN capability (docker run --cap-add NET_ADMIN), or unset BROWSERSERVE_BLOCK_METADATA." >&2
  exit 1
}

in_blocked_v4() {
  case "$1" in
    169.254.*|100.100.100.200) return 0 ;;
  esac
  return 1
}

in_blocked_v6() {
  case "$1" in
    fd00:ec2::254|fd20:ce::254|::ffff:169.254.*) return 0 ;;
  esac
  return 1
}

# Blocks connections this container starts: TCP SYNs and all UDP. Replies to
# inbound traffic stay allowed, because a platform's own health checks can
# arrive from link-local addresses. REJECT fails fast; some kernels lack the
# IPv6 REJECT target, so DROP is the fallback. The rule must be confirmed
# active either way.
add_block_rule() {
  for proto in "tcp --syn" "udp"; do
    added=0
    for target in REJECT DROP; do
      # shellcheck disable=SC2086
      if "$1" -C OUTPUT -p $proto -d "$2" -j "$target" 2>/dev/null \
        || { "$1" -I OUTPUT -p $proto -d "$2" -j "$target" 2>/dev/null \
             && "$1" -C OUTPUT -p $proto -d "$2" -j "$target" 2>/dev/null; }; then
        added=1
        break
      fi
    done
    [ "$added" = "1" ] || block_fail "$1 could not add a $proto rule for $2"
  done
}

# Refuses every connection this container starts to cloud instance metadata
# addresses. Must run as root before any browser starts.
block_metadata() {
  [ "$(id -u)" = "0" ] || block_fail "the entrypoint is not running as root"
  command -v iptables >/dev/null 2>&1 || block_fail "iptables is not installed"
  for net in $METADATA_V4; do
    add_block_rule iptables "$net"
  done
  if [ -e /proc/net/if_inet6 ]; then
    command -v ip6tables >/dev/null 2>&1 || block_fail "ip6tables is not installed"
    for net in $METADATA_V6; do
      add_block_rule ip6tables "$net"
    done
  else
    echo "entrypoint: kernel has no IPv6; IPv6 metadata rules not needed" >&2
  fi
  if curl -s -o /dev/null -m 2 http://169.254.169.254/ 2>/dev/null; then
    block_fail "169.254.169.254 is still reachable after adding the rules"
  fi
  # The metadata address doubles as the DNS resolver on some clouds; once it is
  # blocked, name resolution must go to resolvers the browser may reach. Docker's
  # embedded resolver (127.0.0.11) forwards from inside this container, so its
  # upstreams, listed on the "# ExtServers: [...]" line, count too.
  needs_dns=0
  for server in $(sed -n -e 's/^nameserver[[:space:]]\{1,\}//p' \
      -e 's/^# ExtServers: \[\(.*\)\]$/\1/p' /etc/resolv.conf | tr ',' ' '); do
    server="${server#host(}"; server="${server%)}"
    if in_blocked_v4 "$server" || in_blocked_v6 "$server"; then needs_dns=1; fi
  done
  if [ "$needs_dns" = "1" ]; then
    resolvers="${BROWSERSERVE_DNS:-1.1.1.1 8.8.8.8}"
    { for r in $resolvers; do echo "nameserver $r"; done; } > /etc/resolv.conf \
      || block_fail "/etc/resolv.conf points at a blocked address and could not be rewritten"
    echo "entrypoint: /etc/resolv.conf pointed at a blocked address; now using $resolvers" >&2
  fi
  echo "entrypoint: cloud metadata addresses blocked for this container" >&2
}

if [ "${BROWSERSERVE_BLOCK_METADATA:-0}" = "1" ]; then
  block_metadata
fi

try_delegate() {
  [ "$(id -u)" = "0" ] || return 1
  [ -w "$CG/cgroup.subtree_control" ] || return 1
  # Delegate ONE parent ($CG/sessions) that holds both the runtime's supervisor
  # leaf AND every per-session leaf. cgroup v2 lets a delegatee migrate a process
  # between two cgroups only when it can write the common ancestor's cgroup.procs;
  # nesting supervisor under the delegated sessions dir makes that ancestor the
  # sessions dir itself, so the uid-999 runtime can move a session's browser out
  # of supervisor into session-N. (Sibling supervisor + sessions under the root
  # fails: their common ancestor is the root cgroup, which stays root-owned.)
  mkdir -p "$CG/sessions/supervisor" 2>/dev/null || return 1
  # "No internal process" rule: move PID 1 (and this shell) out of the root into
  # the supervisor leaf before enabling any controller on the root or sessions.
  if ! echo 1 > "$CG/sessions/supervisor/cgroup.procs" 2>/dev/null; then
    echo "entrypoint: could not move the runtime into $CG/sessions/supervisor; per-session limits will not apply (run with a private, writable cgroup: docker run --cgroupns=private --security-opt writable-cgroups=true)" >&2
  fi
  echo $$ > "$CG/sessions/supervisor/cgroup.procs" 2>/dev/null || true
  # Enable each controller one level at a time: root -> sessions -> leaves.
  # A controller the host did not delegate is skipped; its limit stays inactive.
  for controller in memory pids cpu; do
    echo "+$controller" > "$CG/cgroup.subtree_control" 2>/dev/null || true
    echo "+$controller" > "$CG/sessions/cgroup.subtree_control" 2>/dev/null || true
  done
  # Hand the whole sessions subtree (supervisor + future session leaves, and the
  # sessions dir's own cgroup.procs = the migration ancestor) to the runtime user.
  chown -R "$RUNTIME_UID:$RUNTIME_GID" "$CG/sessions" 2>/dev/null || return 1
  export BROWSERSERVE_CGROUP_BASE="$CG/sessions"
  return 0
}

if try_delegate; then
  echo "entrypoint: cgroup subtree delegated to uid $RUNTIME_UID at $CG/sessions" >&2
else
  echo "entrypoint: no cgroup delegation (not root or cgroupfs read-only); portable tiers" >&2
fi

if [ "$(id -u)" = "0" ]; then
  # Drop to the runtime user for all real work (Chrome sandbox needs non-root).
  # --init-groups restores audio/video; HOME must point at a writable dir or
  # Chrome's crashpad handler aborts.
  export HOME=/home/runtime
  exec setpriv --reuid "$RUNTIME_UID" --regid "$RUNTIME_GID" --init-groups \
    /usr/local/bin/browserserve "$@"
fi
exec /usr/local/bin/browserserve "$@"
