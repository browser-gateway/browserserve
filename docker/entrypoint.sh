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

try_delegate() {
  [ "$(id -u)" = "0" ] || return 1
  [ -w "$CG/cgroup.subtree_control" ] || return 1
  # Satisfy the "no internal process" rule: move PID 1 (and us) into a leaf
  # before enabling controllers on the container-root cgroup.
  mkdir -p "$CG/supervisor" 2>/dev/null || return 1
  echo 1 > "$CG/supervisor/cgroup.procs" 2>/dev/null || true
  echo $$ > "$CG/supervisor/cgroup.procs" 2>/dev/null || true
  # Enable memory (and pids) for child leaves; ignore controllers not present.
  echo "+memory" > "$CG/cgroup.subtree_control" 2>/dev/null || true
  echo "+pids" > "$CG/cgroup.subtree_control" 2>/dev/null || true
  mkdir -p "$CG/sessions" 2>/dev/null || return 1
  # Session leaves need memory.max, which requires memory enabled in the
  # sessions subtree too (cgroup v2 enables controllers one level at a time).
  echo "+memory" > "$CG/sessions/cgroup.subtree_control" 2>/dev/null || true
  echo "+pids" > "$CG/sessions/cgroup.subtree_control" 2>/dev/null || true
  chown -R "$RUNTIME_UID:$RUNTIME_GID" "$CG/sessions" 2>/dev/null || return 1
  # Some kernels require the delegated dir's own control files to be writable.
  chown "$RUNTIME_UID:$RUNTIME_GID" "$CG/sessions/cgroup.procs" \
    "$CG/sessions/cgroup.subtree_control" 2>/dev/null || true
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
