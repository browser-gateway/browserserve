# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately. Do not open a public issue for
a security report.

- Use GitHub's [private vulnerability reporting](https://github.com/browser-gateway/browserserve/security/advisories/new)
  ("Report a vulnerability" under the Security tab), or
- email hello@monostellar.com.

Include a description, reproduction steps, affected versions, and any known
impact. We aim to acknowledge reports within a few business days.

## Scope

browserserve runs untrusted web content in Chromium on your infrastructure.
Notes relevant to a secure deployment:

- Chromium's sandbox stays on where the host allows it. Where the host blocks
  it, browserserve falls back to `--no-sandbox` and warns; isolation between
  sessions is unaffected, since it comes from the per-session profile directory.
  To keep the sandbox on in Docker, run with the shipped `docker/seccomp.json`.
  For untrusted content, set `chrome.requireSandbox: true` to refuse the fallback
  and fail closed instead.
- The runtime performs all real work as a non-root user (uid 999). It starts as
  root only to self-delegate a cgroup slice when the host allows it, then drops
  privileges.
- Session isolation is process- and directory-per-session; where the host
  permits, per-session cgroup memory caps and atomic tree-kill are added. Run
  `browserserve doctor` to see the active isolation tier.

Pre-release software: security guarantees are not final until a 1.0 release.
