# browserserve dev commands. Run `just` to list them.

default:
    @just --list

# One-time: activate the committed git hooks (fmt+clippy on commit, full gate on push).
setup:
    git config core.hooksPath .githooks
    @echo "git hooks activated (.githooks). pre-commit: fmt+clippy. pre-push: scripts/gate.sh"

# Format, then run the full quality gate (fmt, clippy, deny, audit, tests, build, doc).
gate:
    ./scripts/gate.sh

# Auto-format the code.
fmt:
    cargo fmt

# Lint with the project's declared lints (including clippy::pedantic).
lint:
    cargo clippy --all-targets --all-features -- -D warnings

# Run unit and property tests.
test:
    cargo nextest run

# Run every test, including the ones that need a local Chrome.
test-all:
    cargo nextest run --run-ignored all

# Run the server locally on :9222.
serve:
    cargo run -- serve

# Diagnose the host and print the active isolation tier.
doctor:
    cargo run -- doctor

# Build the Docker image locally (arch = arm64 or amd64).
image arch="arm64":
    ./scripts/build-image.sh {{arch}}

# Drive a running server with the Puppeteer + Playwright smoke tests.
e2e url="ws://localhost:9222":
    cd e2e && npm install --silent && BROWSERSERVE_URL={{url}} npm run smoke
