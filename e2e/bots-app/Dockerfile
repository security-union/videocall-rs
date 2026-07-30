# syntax=docker/dockerfile:1
#
# bots-app — containerized headless, clock-mode videocall bot (Increment 1, #2035/#2072).
#
# Runs ONE browser bot via Playwright-driven headless Chrome. Built on the
# OFFICIAL Playwright image whose tag MATCHES the locked @playwright/test
# version in e2e/package-lock.json (1.58.2). The version match is load-bearing:
# the image already ships the correct Chromium revision under
# $PLAYWRIGHT_BROWSERS_PATH (/ms-playwright), so we skip the browser download at
# npm-install time and the runtime launch resolves the bundled browser.
#
# BUILD CONTEXT: the REPO ROOT (not e2e/). The repo-root .dockerignore already
# excludes **/target, **/node_modules, **/dist and .git, keeping the context
# lean for the remote podman builder. See build.sh.
#
# The e2e/ tree is copied to /app/e2e so the bots-app's repoRoot() derivation
# (cli.ts: three dirs up from bots-app/src) resolves to /app — preserving the
# code's default path assumptions.

FROM mcr.microsoft.com/playwright:v1.58.2-noble

# iproute2 provides `tc`/`ip` for netem shaping in a LATER increment. Installed
# now (as root, before the USER switch) so the image is ready; unused today.
RUN apt-get update \
    && apt-get install -y --no-install-recommends iproute2 \
    && rm -rf /var/lib/apt/lists/*

# The bundled browsers (correct revision for 1.58.2) live at /ms-playwright and
# are found via the image's $PLAYWRIGHT_BROWSERS_PATH. Skip the postinstall
# download so the build is hermetic (no egress) and needs no write to the
# root-owned /ms-playwright while running as a non-root user.
ENV PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 \
    BOT_RUN_DIR=/tmp/bots-run

# Non-root by default. The Playwright image ships user `pwuser` (uid 1000).
# Chromium here runs WITHOUT --no-sandbox (see src/bot.ts CHROME_ARGS), so it
# relies on the setuid chrome-sandbox — which requires a non-root caller +
# privilege escalation. Running as root would trip "Running as root without
# --no-sandbox is not supported". The pod's securityContext mirrors uid 1000.
RUN mkdir -p /app/e2e && chown -R pwuser:pwuser /app
USER pwuser
WORKDIR /app/e2e

# Dependency layer first for cache reuse. ALL bots-app deps (tsx, commander,
# @playwright/test, …) live in devDependencies, so we MUST include dev deps —
# a plain production install (NODE_ENV=production / --omit=dev) would install
# nothing and the bot could not run.
COPY --chown=pwuser:pwuser e2e/package.json e2e/package-lock.json ./
RUN npm ci --include=dev

# Application source (node_modules is excluded by .dockerignore, so this COPY
# does not clobber the freshly-installed dependencies).
COPY --chown=pwuser:pwuser e2e/ ./

RUN chmod +x bots-app/docker-entrypoint.sh

ENTRYPOINT ["/app/e2e/bots-app/docker-entrypoint.sh"]
