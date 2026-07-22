<h1 align="center">browserserve</h1>

<p align="center">
  <strong>A self-hosted browser server.</strong>
  <br />
  One container runs isolated Chrome sessions over CDP: a warm pool for instant starts, host-measured capacity, per-session resource caps, zero cross-session state.
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

> **Status:** v0.1.0 is published: `ghcr.io/browser-gateway/browserserve:0.1.0` (multi-arch amd64/arm64, SLSA-signed, public). Pre-1.0: flags and configuration may still change between minor versions; the changelog records every change.

An open-source tool by [Monostellar Labs](https://monostellar.com), part of the [browser-gateway](https://github.com/browser-gateway/browser-gateway) open browser infrastructure stack. browserserve is a standalone product and does not require the gateway.

## Why

Running headless Chrome in production is an operations problem: sessions leak state into each other, memory grows until the box dies, zombie processes pile up, and cold starts cost seconds. browserserve packages the fixes into a single deployable instance:

- **Isolation by construction.** Every session gets its own freshly launched Chromium with its own empty profile directory. On disconnect the whole process tree is killed and the directory removed. Cookies, localStorage, IndexedDB, service workers: nothing survives, because nothing is shared.
- **Warm pool.** Browsers are pre-launched and ready before clients connect. The pool grows under load to a hard ceiling, queues briefly when full, and shrinks when idle.
- **Tier-detected resource control.** On a delegated Linux host, each session runs in its own cgroup with a kernel-enforced memory cap and one-syscall tree-kill; elsewhere, a portable RSS soft-cap applies. `browserserve doctor` reports the active tier.
- **CDP over pipes, not ports.** Internally, browsers speak CDP over process pipes. No localhost port pool, no port exhaustion, no scannable debug ports.
- **A pinned browser.** The image ships an exact Chromium build, identical in version on amd64 and arm64, verified by checksum at build time.

Written in Rust: a single static binary supervises everything, and `unsafe` code is forbidden across the crate.

## Quick start

```bash
docker run --rm -p 9222:9222 \
  --shm-size=1g \
  --security-opt seccomp=docker/seccomp.json \
  ghcr.io/browser-gateway/browserserve
```

Connect with any CDP client:

```js
// Puppeteer
const browser = await puppeteer.connect({ browserWSEndpoint: "ws://localhost:9222" });

// Playwright
const browser = await chromium.connectOverCDP("http://localhost:9222");
```

Every WebSocket connection gets its own isolated browser.

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
| Per-session kernel memory cap | No | No | No | Yes, where the host allows it |
| Runtime | Chrome only | Chrome only | Node.js | Single static binary |
| License | n/a | permissive | SSPL | MIT or Apache-2.0 |

Notes. "headless-shell in a container" means a bare Chromium build exposing a debug port, such as `chromedp/docker-headless-shell`; it behaves like a single shared browser and does not isolate connections. Browserless is a capable, mature product with per-session isolation, a queue, and auth; the differences that matter here are its SSPL license, its Node.js runtime, and the absence of a built-in warm pool. browserserve is MIT/Apache dual-licensed, ships as one small static binary, and keeps a warm pool by default.

Performance: a same-host baseline comparison (browserserve vs Browserless vs raw Chrome) is in [docs/BENCHMARKS.md](docs/BENCHMARKS.md). In short, browserserve is on par with Browserless on latency, per-session memory (~140 MB — the same Chrome underneath), and throughput, and both pools return to baseline after every session while raw Chrome leaked and crashed. Those numbers are a shared-cloud-host baseline with an honest caveats block; bare-metal, in-region absolutes are future work.

## Endpoints

| Endpoint | Purpose |
|---|---|
| `WS /` | Connect a CDP session. One connection = one isolated browser. |
| `GET /json/version` | CDP discovery. Points clients at the WebSocket endpoint. |
| `GET /live` | Process liveness. |
| `GET /ready` | The instance can serve a session now. |
| `GET /pressure` | Load, capacity, and the active isolation tier. |

## Sandbox

Chromium isolates every website inside its own sandboxed process. Docker's default security profile blocks the three system calls the sandbox needs (`clone`, `setns`, `unshare`), which is why most browser containers run with `--no-sandbox` and give that protection up. browserserve keeps it.

`docker/seccomp.json` (shipped in this repo) is Docker's default profile plus exactly those three calls. Run with `--security-opt seccomp=docker/seccomp.json` and the sandbox stays on. Without it, browserserve fails closed: sessions refuse to start rather than silently downgrading security, and the error tells you both ways forward. To run without the sandbox anyway, opt out explicitly:

```yaml
chrome:
  noSandbox: true
```

## Configuration

Everything works with zero configuration. Optional `browserserve.yml`:

```yaml
pool:
  minReady: 2        # browsers kept launched and ready
  maxSessions: 10    # hard ceiling of concurrent browsers
session:
  memoryMaxMb: 2048  # per-session memory cap
```

Environment: `PORT` (default 9222), `HOST`, `BROWSERSERVE_TOKEN` (enables auth when set), `BROWSERSERVE_CONFIG`, `BROWSERSERVE_DATA_DIR`. See `browserserve.example.yml` for the full surface.

## CLI

```bash
browserserve serve     # run the server
browserserve check     # launch one browser, verify CDP readiness, tear down, report timings
browserserve doctor    # diagnose the host: browser, fd limits, data dir, /dev/shm, isolation tier
```

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
