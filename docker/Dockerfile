# syntax=docker/dockerfile:1
FROM debian:trixie-slim@sha256:020c0d20b9880058cbe785a9db107156c3c75c2ac944a6aa7ab59f2add76a7bd

# Browser pin tracks RELEASED playwright-core (1.61.1). Both arches are version
# 149.0.7827.55: amd64 ships Google's Chrome-for-Testing build, arm64 ships
# Playwright's Chromium build (Google publishes no linux-arm64 CfT).
ARG TARGETARCH
ARG CHROMIUM_REVISION=1228
ARG CHROMIUM_VERSION=149.0.7827.55
ARG CHROMIUM_SHA256_AMD64=""
ARG CHROMIUM_SHA256_ARM64=""

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    dumb-init \
    unzip \
    libasound2t64 \
    libatk-bridge2.0-0t64 \
    libatk1.0-0t64 \
    libatspi2.0-0t64 \
    libcairo2 \
    libcups2t64 \
    libdbus-1-3 \
    libdrm2 \
    libgbm1 \
    libglib2.0-0t64 \
    libnspr4 \
    libnss3 \
    libpango-1.0-0 \
    libx11-6 \
    libxcb1 \
    libxcomposite1 \
    libxdamage1 \
    libxext6 \
    libxfixes3 \
    libxkbcommon0 \
    libxrandr2 \
    fonts-liberation \
    fonts-freefont-ttf \
    fonts-ipafont-gothic \
    fonts-wqy-zenhei \
    fonts-tlwg-loma-otf \
    fonts-noto-color-emoji \
    fonts-unifont \
  && rm -rf /var/lib/apt/lists/* \
  && fc-cache -f

RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) \
        url="https://cdn.playwright.dev/builds/cft/${CHROMIUM_VERSION}/linux64/chrome-linux64.zip"; \
        inner="chrome-linux64"; sha="$CHROMIUM_SHA256_AMD64" ;; \
      arm64) \
        url="https://cdn.playwright.dev/dbazure/download/playwright/builds/chromium/${CHROMIUM_REVISION}/chromium-linux-arm64.zip"; \
        inner="chrome-linux"; sha="$CHROMIUM_SHA256_ARM64" ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    test -n "$sha" || { echo "missing chromium sha256 for $TARGETARCH" >&2; exit 1; }; \
    curl -fsSL -o /tmp/chromium.zip "$url"; \
    echo "${sha}  /tmp/chromium.zip" | sha256sum -c -; \
    unzip -q /tmp/chromium.zip -d /opt; \
    mv "/opt/${inner}" /opt/chrome; \
    rm /tmp/chromium.zip; \
    /opt/chrome/chrome --version

RUN groupadd -r runtime \
  && useradd --uid 999 -r -g runtime -G audio,video -m -d /home/runtime runtime \
  && mkdir -p /var/lib/browserserve \
  && chown runtime:runtime /var/lib/browserserve

COPY --chmod=755 bin/${TARGETARCH}/browserserve /usr/local/bin/browserserve
COPY --chmod=755 entrypoint.sh /usr/local/bin/entrypoint.sh

ENV BROWSERSERVE_CHROME_PATH=/opt/chrome/chrome \
    BROWSERSERVE_DATA_DIR=/var/lib/browserserve \
    PORT=9222 \
    HOST=0.0.0.0

# Starts as root ONLY to self-delegate a cgroup slice, then the entrypoint drops
# to uid 999 for all real work (Chrome sandbox requires non-root). On hosts
# without a delegated cgroup, it is a plain non-root passthrough.
WORKDIR /home/runtime
EXPOSE 9222
STOPSIGNAL SIGTERM

RUN ["browserserve", "--version"]

ENTRYPOINT ["dumb-init", "--", "/usr/local/bin/entrypoint.sh"]
CMD ["serve"]
