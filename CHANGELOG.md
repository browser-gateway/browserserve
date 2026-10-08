# Changelog

All notable changes to browserserve are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.17] - 2026-10-08

### Fixed
- Resuming a parked session took about 6 seconds and left the previous client's target discovery switched on, so Puppeteer saw no pages after a resume. The cleanup commands sent before the new client attaches used message ids above the 32-bit range Chrome accepts, so Chrome rejected them and each call ran out its timeout. Cleanup now uses ids inside that range; a resumed browser answers in under 100 ms and every client sees its pages.

## [0.1.16] - 2026-10-08

### Added
- `session.resumeWindowMs` / `BROWSERSERVE_RESUME_WINDOW_MS` (default `0`, off): when a client goes away (clean close, crash, or a connection cut by a proxy restart), the browser is kept for the window instead of destroyed. The upgrade response carries `Browserserve-Resume-Token`; `WS /?resume=<token>` within the window reattaches to the same browser with its pages, cookies and storage. Replies to the previous client's in-flight commands are discarded while parked, and its page sessions are detached before the next client is attached. Unknown, expired or already-claimed tokens get `404 unknown_resume_token`. `/pressure` reports `parked`. Not applied to profile sessions or single-use mode.

### Docs
- README: "Self-hosting on a VPS" section (compose file with scale-to-zero, session cap, idle timeout and resource limits; HTTPS behind a reverse proxy; Dokploy/Coolify steps; checks; sandbox, thread-limit and cloud-credential notes). The environment-variable list now names every override the server reads.

## [0.1.15] - 2026-10-04

### Changed
- The image now bundles Chrome 154.0.8037.92 on amd64 (Google's Chrome for Testing, current Stable; was 149.0.7827.55) and Chromium 153.0.8010.12 on arm64 (Playwright revision 1243, the newest arm64 build; Google publishes no linux-arm64 Chrome). amd64 now downloads from Google's own Chrome for Testing storage.
- `scripts/chrome-update.sh` reports when either browser pin is behind and, with `--apply`, moves the pins and checksums. A weekly workflow runs it and opens a pull request; updates are never merged automatically.

### Fixed
- Profile capture lost localStorage on Chrome 153 and later. Chrome moved localStorage from LevelDB to a SQLite file (`Default/LocalStorage`), with large values compressed (zstd or snappy). browserserve now reads the SQLite store, decompressing values, and falls back to LevelDB for older Chrome.

## [0.1.14] - 2026-10-03

### Fixed
- On containers with a thread limit (cgroup `pids.max`, for example 1000 on some hosting platforms), busy sessions with several tabs could use up the limit. New browsers then crashed at startup (`SIGTRAP`/`SIGABRT`) while `GET /pressure` still reported `isAvailable: true`. browserserve now reads the live thread count and refuses a new session with `503 thread_limit` while fewer threads remain than one browser needs. `GET /pressure` reports `isAvailable: false` with `reason: "threads"` and a `threads` field (`current`, `max`). Running sessions are never stopped. Hosts without a thread limit are unaffected.
- A browser that fails to launch is now logged at error level, and every refused session at warn level, with the reason sent to the client. Previously these left no trace in the logs.

## [0.1.13] - 2026-09-28

### Added
- Docker image: `BROWSERSERVE_BLOCK_METADATA=1` blocks every connection the container starts to cloud instance metadata addresses (`169.254.0.0/16`, `100.100.100.200`, `fd00:ec2::254`, `fd20:ce::254`) before the browser runs, so a page cannot read the machine's credentials. TCP connection attempts and UDP are refused; replies to inbound traffic are untouched, so platform health checks from link-local addresses keep working. Fails closed: without `NET_ADMIN`, or if a rule cannot be confirmed, the container exits with a message naming the fix. Rewrites `/etc/resolv.conf` to `BROWSERSERVE_DNS` (default `1.1.1.1 8.8.8.8`) when the resolver, or Docker's embedded resolver upstream, is a blocked address. Off by default.
- The image now includes `iptables`.

### Fixed
- `GET /pressure` reported `isAvailable: true` during boot calibration and after a single-use instance was spent, while every connection was refused. It now reports `isAvailable: false` with `reason` `calibrating` or `spent`, matching `GET /ready`.
- README: single-use mode now says to point the platform's readiness check at `GET /ready`; without it the platform can send a new connection to the instance that is exiting.

## [0.1.12] - 2026-09-24

### Added
- `session.singleUse` / `BROWSERSERVE_SINGLE_USE`: serve exactly one browser session, then shut down and exit 0. The one use is taken atomically before any browser is handed out, so every later or concurrent connection is refused with `503 single_use_spent`. A spent instance exits even when its session never started (failed launch, unknown profile token, aborted upgrade). Auth failures and pressure refusals do not spend it. `GET /ready` reports `spent`. Forces `pool.maxSessions: 1` and disables boot calibration. For orchestrators that replace exited instances, so two clients never share a process or machine.

### Fixed
- README no longer lists `session.maxSessionMs`, which was removed and is rejected at startup. `browserserve.example.yml` now shows `cpuPercent` and `pidsMax`.

## [0.1.11] - 2026-09-19

### Fixed
- Per-session cgroup limits now cover every Chrome process. The browser is started inside its cgroup leaf (join, then exec) instead of being moved in after launch, which left the zygote, GPU and renderer processes outside the limit.
- `session.memoryMaxMb` default documented as `0` (disabled), matching the code.

### Added
- `session.cpuPercent` / `BROWSERSERVE_CPU_PERCENT`: per-session CPU cap as a percentage of one core (`cpu.max`). Default `0` (off).
- `session.pidsMax` / `BROWSERSERVE_PIDS_MAX`: per-session cap on processes plus threads (`pids.max`). Default `0` (off).
- Startup reports which per-session limits the host can enforce (`cgroup limits: memoryMaxMb ..., cpuPercent ..., pidsMax ...`).
- Every refused session is logged at INFO with its reason (`pressure`, `queue_full`, `queue_timeout`, `draining`, `launch_failed`, `calibrating`).

### Changed
- Container entrypoint also delegates the `cpu` controller and prints a clear message when it cannot move the runtime into its own cgroup (previously silent).
- Kernel-enforced limits in Docker need a private, writable cgroup: `docker run --cgroupns=private --security-opt writable-cgroups=true` (Docker Engine 28+). Bind-mounting the host's `/sys/fs/cgroup` does not provide this on most Linux hosts.

## [0.1.10] - 2026-08-25

### Added
- **Boot capacity calibration: browserserve measures the host's real concurrent-session ceiling.** On first boot (when no explicit `pool.maxSessions` / `BROWSERSERVE_MAX_SESSIONS` is set), it briefly ramps real recording sessions on the host, one at a time, and at every step re-checks that all the sessions already running are still producing frames. The ceiling is the highest count that not only launched but kept recording together — so the moment adding one more starves an earlier one, or the first launch failure or a container-memory safety line is hit, the ramp stops; above a step cap it extrapolates from the measured per-session footprint. Verifying sustained concurrent recording (rather than just that each session can start) is what makes the advertised number match what a real client actually gets under load. Because the ramp only ever adds one session at a time and backs off before pressure, calibration can never drive the host into an OOM. That number sets both the enforced pool ceiling and the advertised `Browserserve-MaxConcurrent`, so a router gets an honest number instead of an optimistic guess. During calibration the instance reports not-ready (`GET /ready` 503, `Browserserve-Calibrating: true` on `/json/version`) and refuses sessions so calibration has the host to itself; it opens for business the moment the ceiling is set. Disable with `BROWSERSERVE_CALIBRATE=false` (or `pool.calibrate: false`) to use the conservative estimate immediately. Precedence: explicit `maxSessions` > calibration > estimate.

### Fixed
- **Auto-capacity no longer over-advertises before a browser is measured.** At startup, before the warm pool has launched a browser, `Browserserve-MaxConcurrent` was derived from a CPU-only guess (2 × cores) — on an 8 GB / 8-core host that advertised 16 while the real ceiling is ~5, so downstream consumers over-scheduled the instance and sessions failed at connect. Auto-capacity now applies the memory and thread ceilings with a conservative per-session estimate even when nothing has been measured yet, so the advertised number is honest and safely low from the first request. (Foundation for boot-time self-calibration, which will refine this to a measured number.)
- **`chrome.extraFlags` containing `--disable-features`/`--enable-features` no longer silently overrides the built-in default.** Chromium keeps only the last occurrence of these switches on the command line — a user-supplied `--disable-features=Foo` was previously appended as a second, separate flag, which discarded everything the default flag set disabled (`Translate`, `MediaRouter`, `DialMediaRouteProvider`, `OptimizationHints`, `AcceptCHFrame`, `DestroyProfileOnBrowserClose`), with no warning. Values are now merged into one flag before launch.

### Changed
- **`/dev/shm` is auto-detected: Chrome is launched off it (`--disable-dev-shm-usage`) only when `/dev/shm` is under 512 MiB.** A too-small `/dev/shm` (the 64 MiB Docker default) crashes Chrome renderers under concurrency, not just slows them; routing shared memory to disk prevents that. When `/dev/shm` is sized adequately (`docker run --shm-size=1g`), the faster shared-memory path is kept, so there is no performance cost on a correctly-configured host. Operators no longer need to remember `--shm-size` to avoid crashes; sizing it just restores full performance. `doctor`/`check` still report the `/dev/shm` size. Same detect-and-degrade approach as the sandbox and cgroup fallbacks.

## [0.1.9] - 2026-08-01

### Changed
- **`session.memoryMaxMb` default is now `0` (disabled).** Previously defaulted to `2048` (2 GB), which was too tight for real websites — a single Chrome instance with a modern page routinely exceeds 2 GB across its process tree (main browser + renderer + GPU + network service + storage), and browserserve would kill the session after ~30 seconds. Operators who want the per-session cap can still set it explicitly (recommended sizing: `container_memory_mb / max_sessions × 0.8`); operators who leave it at the default now let the container's own memory limit (Docker `--memory`, K8s limits, Railway) be the OOM boundary. Same intent — one runaway session can't eat the host — different layer.

### Added
- Environment variables for every remaining tuning knob so managed-platform deploys never need a mounted YAML:
  - `BROWSERSERVE_MEMORY_MAX_MB` — mirrors `session.memoryMaxMb`.
  - `BROWSERSERVE_MAX_SESSIONS` — mirrors `pool.maxSessions`. Must be ≥ 1.
  - `BROWSERSERVE_MAX_QUEUE` — mirrors `pool.maxQueue`.
  - `BROWSERSERVE_QUEUE_TIMEOUT_MS` — mirrors `pool.queueTimeoutMs`.
  - `BROWSERSERVE_NO_SANDBOX` — mirrors `chrome.noSandbox`. Boolean (`1`/`true`/`yes` are truthy). Still validated against `chrome.requireSandbox` — setting both fails startup.
- All env vars follow the existing precedence: env wins over YAML. Documented in the `browserserve.mdx` reference on `docs.browsergateway.com`.

## [0.1.8] - 2026-08-01

### Added
- `session.idleTimeoutMs` config field and `BROWSERSERVE_IDLE_TIMEOUT_MS` env var.
  Kills a session whose client has not sent a CDP message in the configured
  number of milliseconds. Default `0` disables (existing behaviour). Only
  client→server traffic resets the clock; browser→client screencast frames or
  events do not, because they do not prove the client is still alive. On idle,
  the client receives a `1013` WebSocket close with reason
  `idle-timeout after {N}ms`, then the browser process is killed and its slot
  returns to the pool. Recommended starting value for operators who need
  runaway-session insurance: `300000` (5 min). Reported on `GET /pressure` as
  `idleTimeoutMs`.

### Removed
- `session.maxSessionMs` config field. It was declared in v0.1.0 with a
  docstring but never read anywhere in the codebase (dead config). Operators
  who set it in YAML got no behaviour change and no warning. If your config
  contains `session.maxSessionMs`, remove it or replace with
  `session.idleTimeoutMs` (which is what you probably wanted). YAML with the
  old key now fails startup with an unknown-field error.

## [0.1.7] - 2026-08-01

### Fixed
- `Page.startScreencast` now emits the initial frame reliably on hosts under CPU
  contention (Railway amd64 was the observed repro). The default launch flag set
  was missing `--disable-renderer-backgrounding`, which every major headless
  launcher (Puppeteer, chrome-launcher, Playwright) ships as part of the
  renderer-liveness trio alongside `--disable-background-timer-throttling` and
  `--disable-backgrounding-occluded-windows`. Without it, a headless renderer
  that is not the foreground window can be deprioritized by the OS scheduler,
  which starves the post-load paint that would produce the first screencast
  frame. Active/repainting pages were unaffected in local measurement, but
  static pages on contended hosts silently emitted zero frames.

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
