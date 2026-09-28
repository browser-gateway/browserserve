<h1 align="center">browserserve</h1>

<p align="center">
  <strong>A self-hosted browser server.</strong>
  <br />
  One container runs isolated Chrome sessions over CDP: a warm pool for instant starts, host-derived capacity, per-session resource caps, optional save and restore of session profiles, and no cross-session state by default.
  <br />
  Works with Puppeteer, Playwright, and any CDP client.
</p>

<p align="center">
  <a href="https://github.com/browser-gateway/browserserve/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/browser-gateway/browserserve/ci.yml?style=flat-square&label=CI" alt="CI" /></a>
  <a href="https://github.com/browser-gateway/browserserve/pkgs/container/browserserve"><img src="https://img.shields.io/badge/ghcr.io-browser--gateway%2Fbrowserserve-blue?style=flat-square&logo=docker&logoColor=white" alt="Container image" /></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square" alt="License: MIT OR Apache-2.0" /></a>
  <img src="https://img.shields.io/badge/unsafe-forbidden-success?style=flat-square" alt="forbid unsafe" />
  <a href="https://github.com/browser-gateway/browserserve"><img src="https://img.shields.io/github/stars/browser-gateway/browserserve?style=flat-square&logo=github&logoColor=white" alt="GitHub stars" /></a>
</p>

<p align="center">
  <a href="https://browsergateway.com">Website</a>
  &nbsp;·&nbsp;
  <a href="https://docs.browsergateway.com/browserserve">Docs</a>
  &nbsp;·&nbsp;
  <a href="#quick-start">Quick start</a>
  &nbsp;·&nbsp;
  <a href="#profiles">Profiles</a>
  &nbsp;·&nbsp;
  <a href="docs/BENCHMARKS.md">Benchmarks</a>
  &nbsp;·&nbsp;
  <a href="CHANGELOG.md">Changelog</a>
</p>

## Why

Running headless Chrome in production is an operations problem: sessions leak state into each other, memory grows until the box dies, zombie processes pile up, and cold starts cost seconds. browserserve packages the fixes into a single deployable instance:

- **Isolation by default.** Every session gets its own freshly launched Chromium with its own empty profile directory. On disconnect the whole process tree is killed and the directory removed, so by default nothing (cookies, localStorage, IndexedDB, service workers) carries from one session to the next. When you *want* state to persist, opt a session into a saved profile (see [Profiles](#profiles)).
- **Warm pool, or scale to zero.** Browsers are pre-launched and ready before clients connect; the pool grows under load to a ceiling, queues briefly when full, and shrinks when idle. Set `pool.minReady: 0` for scale-to-zero: no browser runs while idle, and one launches on demand at the first connection. You trade a higher first-request latency for near-zero idle browser cost, which suits pay-as-you-go hosts.
- **Instant startup.** The server binds and answers `/json/version` in well under a second, then warms browsers in the background. It never blocks startup on a browser launch, so it comes up cleanly even on a slow or constrained host.
- **Capacity derived from the host.** If you don't set a ceiling, browserserve measures the host's real limits (available memory, the PID/thread budget, CPU count) and a launched browser's footprint, then picks a safe maximum. On PID-capped containers the thread budget is often the real limit, not memory, so a box with plenty of free RAM can still cap out around a handful of browsers. `GET /pressure` reports the ceiling and which limit set it.
- **Tier-detected resource control.** On a delegated Linux host, each browser is started inside its own cgroup, so kernel-enforced memory, CPU and process limits cover every process of that session, with a one-syscall tree-kill; elsewhere, a portable RSS soft-cap applies. In Docker (Engine 28+) a delegated host means `docker run --cgroupns=private --security-opt writable-cgroups=true`. `browserserve doctor` reports the active tier and which limits are enforceable.
- **CDP over pipes, not ports.** Internally, browsers speak CDP over process pipes. No localhost port pool, no port exhaustion, no scannable debug ports.
- **A pinned browser.** The image ships the same pinned Chromium milestone on amd64 and arm64, verified by checksum at build time.

Written in Rust: a single static binary supervises everything, and `unsafe` code is forbidden across the crate.

## Quick start

```bash
docker run --rm -p 9222:9222 --shm-size=1g ghcr.io/browser-gateway/browserserve
```

The image runs out of the box. Where the host blocks Chromium's sandbox, browserserve falls back to `--no-sandbox` automatically (see [Sandbox](#sandbox)).

Connect with any CDP client:

```js
// Puppeteer
const browser = await puppeteer.connect({ browserWSEndpoint: "ws://localhost:9222" });

// Playwright
const browser = await chromium.connectOverCDP("http://localhost:9222");
```

Every WebSocket connection gets its own isolated browser. With authentication enabled (set `BROWSERSERVE_TOKEN`), pass the token as `?token=...` on the URL or an `Authorization: Bearer` header.

## Profiles

By default a session starts blank and is wiped on disconnect. A **profile** lets a session start from saved state and captures that state back when the session closes, so a logged-in session can be resumed later.

A profile has two layers:

- **Portable core: cookies + localStorage.** Applied and captured over CDP, so this layer works against any CDP browser, not only browserserve.
- **Native layer: IndexedDB + service workers.** browserserve owns the browser's disk, so it moves these as on-disk stores. This is state a plain CDP connection cannot restore at all, which matters for apps that keep their session in IndexedDB (many single-page apps and auth SDKs).

Use it through a one-shot token channel:

```bash
# 1. Drop off a profile, get a one-time token (bearer-authed, 64 MiB max, token lives ~2 min)
curl -X POST http://localhost:9222/v1/profile \
  -H "Authorization: Bearer $BROWSERSERVE_TOKEN" \
  -H "Content-Type: application/json" --data @profile.json
# -> { "profileToken": "…", "expiresInSec": 120 }

# 2. Connect a session seeded with that profile:
#      ws://localhost:9222/?profileToken=<token>

# 3. After the session closes, pick up the captured state (single use):
curl http://localhost:9222/v1/profile/<token> -H "Authorization: Bearer $BROWSERSERVE_TOKEN"
```

Add `?readOnly=1` to seed a profile without capturing it back, so any number of sessions can share one read-only profile at once.

localStorage is read from the browser's on-disk store after it exits, so every origin is captured, including ones that set no cookie. Cookies are restored through a drop-only sanitizer: it keeps every security attribute (`Secure`, `HttpOnly`, `SameSite`, partitioning) exactly, and drops any cookie it cannot reproduce safely rather than weakening it.

**Current limitations, being tightened before a 1.0.** IndexedDB and service workers are browserserve-native, so that layer does not transfer to a different provider. Service-worker restore fidelity is not yet independently measured. Importing a profile captured on a different Chromium build is untested. Cookies with an opaque partition key (CHIPS) are dropped. Profiles are proven on the portable isolation tier; the kernel-cgroup tier together with profiles has been validated in a Docker Desktop environment, with broad production validation ongoing.

## Why not run Chrome yourself?

You can start Chrome with `--remote-debugging-port=9222` and point a client at it. The difference is what happens when more than one thing connects.

Plain Chrome with a debug port is **one shared browser**. Every client that connects lands in the same browser, shares the same cookies and storage, and can see the same tabs. Two scripts running at once collide: log in on one and you are logged in on the other, and a crash takes down everyone. The debug port also has no authentication, so anyone who can reach it controls the browser.

browserserve gives **each connection its own fresh browser**. Session A and session B have separate cookies, separate storage, separate tabs, and cannot see each other. When a client disconnects, its browser is killed and its profile directory is deleted, so nothing leaks into the next session. On top of that you get a warm pool for instant starts, a limit and queue for concurrency, token authentication, and health endpoints for load balancers.

In short: plain Chrome is one browser you share; browserserve turns a machine into a browser service that hands out many isolated sessions behind one endpoint. If you only ever need a single browser for a single script, plain Chrome (or letting Puppeteer launch its own) is enough. browserserve earns its place the moment you need more than one isolated session at a time, which is the usual case for AI agents, scraping at scale, and shared or multi-tenant workloads.

## How it compares

| | Plain Chrome debug port | headless-shell in a container | Browserless | browserserve |
|---|---|---|---|---|
| One isolated browser per connection | No (one shared browser) | No (one shared browser) | Yes | Yes |
| Killed and wiped on disconnect | No (you manage it) | No | Yes | Yes |
| Warm pool for instant starts | No | No | No (preboot deprecated) | Yes |
| Concurrency limit + queue | No | No | Yes | Yes |
| Token authentication | No | No | Yes | Yes |
| Health / pressure endpoints | No | No | Yes | Yes |
| Restore IndexedDB / service workers across sessions | No | No | No | Yes |
| Per-session kernel memory cap | No | No | No | Yes, where the host allows it |
| Runtime | Chrome only | Chrome only | Node.js | Single static binary |
| License | n/a | permissive | SSPL | MIT or Apache-2.0 |

Notes. "headless-shell in a container" means a bare Chromium build exposing a debug port, such as `chromedp/docker-headless-shell`; it behaves like a single shared browser and does not isolate connections. Browserless is a capable, mature product with per-session isolation, a queue, and auth; the differences that matter here are its SSPL license, its Node.js runtime, and the absence of a built-in warm pool. browserserve is MIT/Apache dual-licensed, ships as one small static binary, and keeps a warm pool by default.

Performance: a same-host baseline comparison (browserserve vs Browserless vs raw Chrome) is in [docs/BENCHMARKS.md](docs/BENCHMARKS.md). In short, browserserve is on par with Browserless on latency, per-session memory (~140 MB, the same Chrome underneath), and throughput, and both pools return to baseline after every session while raw Chrome leaked and crashed. Those numbers are a shared-cloud-host baseline with an honest caveats block; bare-metal, in-region absolutes are future work.

## Endpoints

| Endpoint | Purpose |
|---|---|
| `WS /` | Connect a CDP session; one connection = one isolated browser. `?profileToken=` seeds a saved profile, `?readOnly=1` shares one without capturing, `?token=` authenticates. |
| `GET /json/version` | CDP discovery. Also advertises `Browserserve-Version` and `Browserserve-MaxConcurrent` so a router can auto-detect the instance and its capacity. |
| `POST /v1/profile` | Drop off a profile; returns a one-time `profileToken`. Bearer-authed, 64 MiB max. |
| `GET /v1/profile/{token}` | Pick up the captured profile after the session closes. Bearer-authed, single use. |
| `GET /live` | Process liveness. |
| `GET /ready` | The instance can serve a session now. |
| `GET /pressure` | Load, capacity (and which host limit set it), and the active isolation tier. `isAvailable` is `false` whenever a new session would be refused, with `reason` one of `sandbox`, `spent`, `draining`, `calibrating`, `full`, `cpu`, `memory`. |

## Sandbox

Chromium isolates every website inside its own sandboxed process. Docker's default security profile blocks the three system calls the sandbox needs (`clone`, `setns`, `unshare`), which is why most browser containers run with `--no-sandbox` and give that protection up. browserserve keeps it.

By default, browserserve keeps it where it can and falls back where it can't. It tries to launch with the sandbox on; if the host blocks it, browserserve logs a warning and relaunches with `--no-sandbox` so the deploy still works. Isolation between sessions is unaffected, because it comes from the fresh, per-session profile directory, not the sandbox.

To keep the sandbox on in Docker, run with the shipped `docker/seccomp.json` (Docker's default profile plus exactly those three calls):

```bash
docker run --security-opt seccomp=docker/seccomp.json ... ghcr.io/browser-gateway/browserserve
```

For untrusted content, require the sandbox and refuse the fallback with `chrome.requireSandbox: true`. Sessions then fail to start on a host that blocks the sandbox rather than downgrading. To skip the sandbox explicitly, set `chrome.noSandbox: true`.

## Security

- **Sandbox, on by default, with a safe fallback.** The sandbox stays on where the host allows it and falls back to `--no-sandbox` where the host blocks it; isolation between sessions does not depend on the sandbox. Set `chrome.requireSandbox: true` to refuse the fallback and fail closed instead (see [Sandbox](#sandbox)).
- **Non-root.** The server does all real work as an unprivileged user (uid 999). It starts as root only to self-delegate a per-session cgroup slice when the host allows it, then drops privileges.
- **Cloud metadata block (Docker image, opt-in).** On a cloud machine, a page the browser loads can try to reach the instance metadata server (`169.254.169.254` and friends) and read the machine's credentials. Set `BROWSERSERVE_BLOCK_METADATA=1` and the entrypoint, while still root, refuses every connection the container starts to link-local addresses (`169.254.0.0/16`), Alibaba's `100.100.100.200`, and the AWS and Google IPv6 metadata addresses, then drops privileges; the unprivileged runtime cannot undo it. Needs the `NET_ADMIN` capability (`docker run --cap-add NET_ADMIN`, or `capabilities: add: [NET_ADMIN]` in a Kubernetes `securityContext`). The container refuses to start if the block cannot be applied. When the resolver is a metadata address (common on cloud VMs), DNS switches to `BROWSERSERVE_DNS` (default `1.1.1.1 8.8.8.8`); container-name lookups on a Docker network stop working in that case.
- **Authenticated surface.** When `BROWSERSERVE_TOKEN` is set, the CDP WebSocket and the `/v1/profile*` endpoints require the token; `/live`, `/ready`, and `/pressure` stay open for load balancers.

See [SECURITY.md](SECURITY.md) for the reporting policy and deployment notes.

## Configuration

Everything works with zero configuration. To tune it, mount a `browserserve.yml`:

| Key | Default | Meaning |
|---|---|---|
| `pool.minReady` | `1` | Browsers kept launched and ready ahead of demand. Set to `0` for scale-to-zero: no idle browsers, one launches on demand at the first connection. |
| `pool.maxSessions` | unset (auto) | Hard ceiling of concurrent browsers. **Left unset, it is derived from the host** (memory, PID/thread budget, CPUs). |
| `pool.maxQueue` | `10` | Clients allowed to wait for a slot before rejection. |
| `pool.queueTimeoutMs` | `30000` | How long a queued client waits before a 503. |
| `session.memoryMaxMb` | `0` | Per-session memory cap (kernel-enforced on a delegated host, RSS soft-cap otherwise; `0` = uncapped). |
| `session.cpuPercent` | `0` | Per-session CPU cap as a percentage of one core (`100` = one core, `150` = one and a half). Kernel-enforced on a delegated host only; `0` = uncapped. |
| `session.pidsMax` | `0` | Per-session cap on processes plus threads. Kernel-enforced on a delegated host only; `0` = uncapped. |
| `session.singleUse` | `false` | Serve exactly one session, then exit. Every later connection gets `503 single_use_spent`. Forces `maxSessions: 1` and skips calibration. Meant for orchestrators that replace exited instances (Cloud Run, Kubernetes, Fly Machines), so no two clients ever share a process or machine. Point the platform's readiness check at `GET /ready` with a short interval: it turns unready the moment the one session is claimed, so the platform stops sending new connections to an instance that is about to exit. Env: `BROWSERSERVE_SINGLE_USE`. |
| `session.killGraceMs` | `5000` | SIGTERM-to-SIGKILL grace during teardown. |
| `pressure.maxCpuPercent` | `95` | Reject new sessions above this host CPU usage. |
| `pressure.maxMemoryPercent` | `95` | Reject new sessions above this host memory usage. |
| `chrome.noSandbox` | `false` | Opt out of the Chromium sandbox (see [Sandbox](#sandbox)). |
| `chrome.extraFlags` | `[]` | Extra Chromium flags, appended after the built-in set. |
| `externalAddress` | Host header | Public address advertised in `webSocketDebuggerUrl`; set this when running behind a proxy or load balancer. |
| `dataDir` | `.browserserve` | Where per-session state lives. |

See [`browserserve.example.yml`](browserserve.example.yml) for every key with inline notes. Unknown keys are rejected, so a typo in the config fails fast at startup.

Environment: `PORT` (default 9222), `HOST` (default `0.0.0.0`), `BROWSERSERVE_TOKEN` (enables auth when set), `BROWSERSERVE_CONFIG`, `BROWSERSERVE_CHROME_PATH`, `BROWSERSERVE_DATA_DIR`. Docker image only: `BROWSERSERVE_BLOCK_METADATA`, `BROWSERSERVE_DNS` (see [Security](#security)).

## CLI

```bash
browserserve serve                  # run the server
browserserve check [--chrome PATH]  # launch one browser, verify CDP readiness, tear down, report timings
browserserve doctor                 # diagnose the host: browser, fd limits, data dir, /dev/shm, isolation tiers
```

All three accept `--config <path>`. Config resolves in order: `--config`, then `BROWSERSERVE_CONFIG`, then `./browserserve.yml`.

## Building from source

Requires the Rust toolchain (auto-installed from `rust-toolchain.toml`) and, for the tests, a local Chrome.

```bash
cargo build --release          # binary at target/release/browserserve
just gate                      # run the full quality gate
./scripts/build-image.sh amd64 # build the Docker image (amd64|arm64)
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow.

## License

© 2026 Monostellar Limited.

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option. Unless you explicitly state otherwise, any contribution
intentionally submitted for inclusion in this project, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any additional
terms or conditions.

---

<sub>An open-source tool by <a href="https://monostellar.com">Monostellar Labs</a>.</sub>
