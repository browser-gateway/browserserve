# browserserve

[![CI](https://github.com/browser-gateway/browserserve/actions/workflows/ci.yml/badge.svg)](https://github.com/browser-gateway/browserserve/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
![forbid unsafe](https://img.shields.io/badge/unsafe-forbidden-success.svg)

A self-hosted browser server. One container runs isolated Chrome sessions over CDP: warm pools for instant starts, per-session resource caps, zero cross-session state. Works with Puppeteer, Playwright, and any CDP client.

> **Status: pre-release (v0.1.0 unreleased).** The server, warm pool, isolation tiers, and container image are built and tested. Hardening, CI-published binaries, and benchmarks are in progress; APIs and flags may change until v0.1.0.

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
