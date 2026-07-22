# Benchmarks

Comparative measurements of browserserve against other ways to run a headless
browser. **These are same-host *relative* numbers, not bare-metal absolutes** —
read the methodology and caveats before quoting any figure.

## Baseline comparison — 2026-07-22

**Host:** one shared cloud container per contender (Railway, x86, kernel 6.12,
8 GB, `pids.max` capped) — deliberately NOT dedicated bare metal.
**Client:** the same `puppeteer-core` script for every contender, run from a
developer machine (so latency includes a cross-region network hop).
**Workload:** `connect → newPage → goto https://example.com` (domcontentloaded).
**Memory:** cgroup `anon` (real process RAM, excludes reclaimable page cache).
**Runs:** 3 per contender, aggregated (mean). Contenders on the same host, same
day, same client.

| Contender | Model | Warm latency | Mem / session | Throughput | Teardown |
|---|---|---|---|---|---|
| **browserserve** | pool (fresh browser/connection) | 2286 ms | 144 MB | 1.62 sess/s @4 | **clean 3/3** |
| **Browserless v2** | pool (fresh browser/connection) | 2452 ms | 143 MB | 1.69 sess/s @4 | **clean 3/3** |
| **raw Chrome** (`chromedp/headless-shell`) | one shared browser | 2665 ms | ~133 MB* | 0.33 sess/s @1 | **leaked; 1 run crashed** |

\* raw Chrome accumulates state (no session lifecycle), so its per-session
memory is not cleanly separable and its throughput degrades run-over-run.

### What it shows

- **browserserve is on par with Browserless** — a mature commercial runtime —
  on latency, per-session memory, throughput, and teardown.
- **A runtime's value is lifecycle, not a lighter browser.** Per-session memory
  is ~140 MB for all three because they run the *same* Chrome. The difference is
  cleanup: both pools return to baseline after every session; **raw Chrome
  leaked pages and one run crashed outright** — the exact failure a runtime
  prevents.
- At concurrency 4 the two pools are throughput-equal and **network-bound**
  here; raw Chrome is single-browser (concurrency 1), so ~5× lower.

### Caveats (do not over-read)

- **Not bare metal.** A shared Railway container: noisy neighbors, `--no-sandbox`
  (the platform can't apply a seccomp profile), and the fallback isolation tier
  (no cgroup delegation). Absolute numbers are a *baseline*; the *relative*
  comparison on one identical host is the fair part.
- **Latency is network-dominated** (~2 s is the developer-machine → Railway hop),
  not browserserve's intrinsic speed. Intrinsic latency needs an in-region
  client.
- **Steel Local was deferred:** it is single-browser (concurrency 1) and its
  session-proxied CDP would not complete a `puppeteer-core` handshake through the
  Railway TCP proxy (it expects Steel's SDK / Playwright `connectOverCDP`).
  Excluded rather than measured with a different client, which would be unfair.
- **Per-contender Chrome:** all ran Chrome 149.x (browserserve/Browserless
  Chrome-for-Testing; raw Chrome the chromedp headless-shell build).

### Reproduce

The harness is a project-local tier-3 suite (`tests/browserserve/compare.ts`,
not shipped): it deploys each contender to the same host, drives all of them
with one `puppeteer-core` client, samples each container's cgroup `anon` memory
and process/thread counts over `railway ssh`, and runs every metric 3×.

**Publishable, bare-metal numbers are future work** (a dedicated VPS / the Pi, an
in-region client). Per project policy, no "faster/lighter than X" claim ships
without same-box measurement — which is what this table is, with its host
honestly labeled.
