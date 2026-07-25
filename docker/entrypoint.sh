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
  echo 1 > "$CG/sessions/supervisor/cgroup.procs" 2>/dev/null || true
  echo $$ > "$CG/sessions/supervisor/cgroup.procs" 2>/dev/null || true
  # Enable memory + pids one level at a time: root -> sessions -> leaves.
  echo "+memory" > "$CG/cgroup.subtree_control" 2>/dev/null || true
  echo "+pids" > "$CG/cgroup.subtree_control" 2>/dev/null || true
  echo "+memory" > "$CG/sessions/cgroup.subtree_control" 2>/dev/null || true
  echo "+pids" > "$CG/sessions/cgroup.subtree_control" 2>/dev/null || true
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
