# Changelog

All notable changes to browserserve are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Profiles: sessions can be launched from a saved profile and captured back at
  session end. Cookies and localStorage are the portable core (applied over CDP,
  so they work on any provider); IndexedDB and service workers are moved as
  on-disk store directories, so they persist across browserserve sessions. A
  one-shot token channel (`POST /v1/profile`, `GET /v1/profile/{token}`) hands a
  profile to a `?profileToken=` session and returns the captured state on close.
  localStorage is read directly from the on-disk LevelDB, so every origin is
  captured (including cookieless ones); cookie inject uses a drop-only sanitizer
  that never downgrades security attributes. Validated on macOS and real Linux.

### Changed
- Chrome launch flags now suppress the crash-restore prompt
  (`--disable-session-crashed-bubble`, `--hide-crash-restore-bubble`) so a
  seeded profile directory (which reads as "crashed" after a kill-based
  teardown) loads without an interstitial.

## [0.1.1] - 2026-07-22

### Added
- Auto-capacity: when `pool.maxSessions` is unset, the session ceiling is
  derived at startup from the host's real limits (cgroup v2 `memory.max` /
  `pids.max`, total memory, CPU count) and the measured footprint of a browser
  launched on this host. The result and its binding constraint are logged and
  reported by `/pressure` (`capacitySource`).
- Gateway discovery: `/json/version` now carries `Browserserve-Version` and
  `Browserserve-MaxConcurrent`, letting the browser-gateway router auto-detect a
  browserserve provider and adopt its capacity.

### Fixed
- A browser that cannot launch (for example when the container's thread/PID
  ceiling is reached) now returns `503 Service Unavailable`, not `500`: the
  server is at capacity, a condition clients should retry, not a server fault.

## [0.1.0] - 2026-07-22

### Added
- Session server (`browserserve serve`): warm browser pool, CDP WebSocket
  endpoint, and `/live` `/ready` `/pressure` `/json/version` HTTP probes.
- One fresh Chrome process and one fresh profile directory per session, killed
  and wiped on disconnect (Class A isolation).
- CDP transport over inherited pipes (no TCP debug ports).
- Tier-detected kernel isolation: per-session cgroup v2 `memory.max` hard cap and
  `cgroup.kill` on delegated Linux hosts; RSS-poll soft cap elsewhere. The active
  tier is reported by `doctor` and `/pressure`.
- Warmed copy-on-write profile template so sessions skip Chrome's first-run cost.
- Constant-time token authentication and pressure-based admission control.
- Graceful drain on SIGTERM with a bounded deadline.
- Multi-arch Docker image (`linux/amd64`, `linux/arm64`) with a pinned Chromium
  build verified by checksum, non-root user, `dumb-init`, and a seccomp profile
  that keeps Chromium's sandbox enabled.
- `browserserve check` and `browserserve doctor` diagnostics.

[Unreleased]: https://github.com/browser-gateway/browserserve/commits/main
