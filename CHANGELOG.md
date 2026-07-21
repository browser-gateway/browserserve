# Changelog

All notable changes to browserserve are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
