# Contributing to browserserve

Thanks for your interest. browserserve is part of the
[browser-gateway](https://github.com/browser-gateway) open browser
infrastructure stack.

## Prerequisites

- Rust (the pinned toolchain in `rust-toolchain.toml` is installed automatically
  by `rustup` when you build).
- A local Chrome or Chromium for the integration and end-to-end tests.
- Docker, to build and run the container image.

## Build and run

```bash
cargo build                       # debug build
cargo run -- doctor               # diagnose the host
cargo run -- serve                # run the server on :9222
./scripts/build-image.sh arm64    # build the Docker image locally (arm64|amd64)
```

## Quality gates

Every change must pass the full gate before it is considered done:

```bash
./scripts/gate.sh
```

This runs, in order: `cargo fmt --check`, `cargo clippy --all-targets
--all-features -- -D warnings` (lints, including `clippy::pedantic`, are declared
in `Cargo.toml`), `cargo deny check`, `cargo audit`, the test suite (`cargo
nextest run`), a release build, and `cargo doc`. CI runs the same script.

Individually:

```bash
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run                                   # unit + property tests
cargo nextest run --run-ignored all                 # + real-Chrome tests
```

End-to-end (drives a running server with Puppeteer and Playwright):

```bash
cd e2e && npm install
BROWSERSERVE_URL=ws://localhost:9222 npm run smoke
```

## Code standards

- `#![forbid(unsafe_code)]`: no unsafe anywhere in the crate.
- No `unwrap`/`expect`/`panic!` in production paths; errors are typed with
  `thiserror` and propagated with `?`.
- Modern module layout (no `mod.rs`), edition 2024 idioms.
- Public items carry `///` documentation.
- After adding tests, verify they catch real failures: break the source,
  confirm the relevant test fails, then revert.

## Commits and pull requests

- Keep the subject line short and self-explanatory in plain domain terms.
- Open a PR with a short summary of what changed and how it was tested.

## License

This project is dual-licensed under [MIT](LICENSE-MIT) or
[Apache-2.0](LICENSE-APACHE), at the user's option. Unless you explicitly state
otherwise, any contribution you intentionally submit for inclusion, as defined
in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
