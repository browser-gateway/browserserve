# Changelog

All notable changes to browserserve are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.6] - 2026-07-27

### Added
- `BROWSERSERVE_MIN_READY` environment variable, mirroring `pool.minReady`. Lets
  container and serverless deploys (which configure via env, not a mounted YAML)
  set scale-to-zero with `BROWSERSERVE_MIN_READY=0`. An idle instance then holds
  no browser and emits no outbound traffic, so platforms that sleep idle services
  (Railway app-sleeping, Fly/Cloud Run scale-to-zero, KEDA) can suspend it; the
  next connection wakes it and launches a browser on demand. Default is unchanged
  (`1`, one warm browser).

## [0.1.5] - 2026-07-26

### Added
- Automatic sandbox fallback. When a host blocks Chromium's OS sandbox (Railway,
  Fly, Cloud Run, restrictive Docker), the runtime detects the sandbox-specific
  startup abort, logs one warning, and retries with `--no-sandbox` so the deploy
  works with no configuration. Session isolation is unaffected: each session
  still gets a fresh, private `--user-data-dir` that is wiped on disconnect, so
  no state leaks between sessions with or without the sandbox.
- `chrome.requireSandbox` (env `BROWSERSERVE_REQUIRE_SANDBOX`), default off. When
  set, the runtime refuses to fall back: on a host that blocks the sandbox it
  still binds and answers health checks but serves no sessions, and `GET /ready`
  returns 503 with the reason. For operators who render untrusted content and
  need the sandbox enforced. Rejected at startup if combined with `noSandbox`.
- Sandbox state is reported in `GET /pressure` and `GET /ready` (`sandbox` field)
  and printed by `browserserve check`: `on`, `on (required)`, `off (config)`, or
  `off (auto-fallback: host blocks the sandbox)`.

## [0.1.4] - 2026-07-26

### Fixed
- Startup no longer blocks on launching a browser. The server binds its port and
  starts serving before any Chrome launch, so it is reachable within a second even on
  a slow or constrained host; previously the port could stay closed for up to the
  launch timeout, which returned 502 on platforms like Railway. `GET /json/version`
  now returns 200 immediately (with the connect URL and version/capacity headers)
  instead of 503 until the first browser had launched.

### Added
- Scale-to-zero mode. With `pool.minReady: 0`, no browser launches at boot and an idle
  instance holds zero browsers, launching one on demand at the first connection. This
  trades a higher first-request latency for near-zero idle browser cost, which suits
  pay-as-you-go hosts.

## [0.1.3] - 2026-07-25

### Fixed
- Per-session memory cap on delegated cgroup hosts. Sessions now run inside
  their own cgroup with the configured `memory.max` hard cap. A delegation
  boundary error previously left every session uncapped on delegated Docker
  while `doctor` still reported a hard cap. The runtime now delegates a single
  parent cgroup, so a session's browser can be moved into its own leaf; it
  verifies a real process migration before reporting the cgroup tier; and it
  falls back to the RSS soft cap, reported honestly, on any host where the
  migration is refused.

## [0.1.2] - 2026-07-24

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

[0.1.4]: https://github.com/browser-gateway/browserserve/releases/tag/v0.1.4
[0.1.3]: https://github.com/browser-gateway/browserserve/releases/tag/v0.1.3
[0.1.2]: https://github.com/browser-gateway/browserserve/releases/tag/v0.1.2
