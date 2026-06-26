#!/bin/sh
set -eu

# Generate runtime config.local.js.
#
# The e2e stack bind-mounts the repo at /app. Writing the generated e2e
# runtime config into the tracked `scripts/config.js` dirties the developer's
# worktree, so keep the generated values in the gitignored local override file
# that index.html loads after the committed defaults.
mkdir -p /app/dioxus-ui/scripts
if [ -n "${SEARCH_API_BASE_URL:-}" ]; then
    SEARCH_API_BASE_URL_CONFIG="\"${SEARCH_API_BASE_URL}\""
else
    SEARCH_API_BASE_URL_CONFIG="null"
fi

cat > /app/dioxus-ui/scripts/config.local.js <<EOF
if (window.__APP_CONFIG) {
  Object.assign(window.__APP_CONFIG, {
  apiBaseUrl: "${API_BASE_URL:-http://localhost:${ACTIX_PORT:-8080}}",
  wsUrl: "${ACTIX_UI_BACKEND_URL:-ws://localhost:${ACTIX_PORT:-8080}}",
  webTransportHost: "${WEBTRANSPORT_HOST:-https://127.0.0.1:4433}",
  oauthEnabled: "${ENABLE_OAUTH:-false}",
  e2eeEnabled: "${E2EE_ENABLED:-false}",
  webTransportEnabled: "${WEBTRANSPORT_ENABLED:-false}",
  transportBadgeEnabled: "${TRANSPORT_BADGE_ENABLED:-false}",
  showBuildGitInfo: "${SHOW_BUILD_GIT_INFO:-false}",
  firefoxEnabled: "${FIREFOX_ENABLED:-false}",
  usersAllowedToStream: "${USERS_ALLOWED_TO_STREAM:-}",
  serverElectionPeriodMs: ${SERVER_ELECTION_PERIOD_MS:-2000},
  audioBitrateKbps: ${AUDIO_BITRATE_KBPS:-65},
  videoBitrateKbps: ${VIDEO_BITRATE_KBPS:-100},
  screenBitrateKbps: ${SCREEN_BITRATE_KBPS:-1200},
  oauthProvider: "${OAUTH_PROVIDER:-}",
  vadThreshold: ${VAD_THRESHOLD:-0.02},
  oauthAuthUrl: "${OAUTH_AUTH_URL:-}",
  oauthClientId: "${OAUTH_CLIENT_ID:-}",
  oauthRedirectUrl: "${OAUTH_REDIRECT_URL:-}",
  oauthScopes: "${OAUTH_SCOPES:-openid email profile}",
  oauthTokenUrl: "${OAUTH_TOKEN_URL:-}",
  oauthIssuer: "${OAUTH_ISSUER:-}",
  oauthPrompt: "${OAUTH_PROMPT:-}",
  oauthFlow: "${OAUTH_FLOW:-}",
  searchApiBaseUrl: ${SEARCH_API_BASE_URL_CONFIG},
  mockPeersEnabled: "${MOCK_PEERS_ENABLED:-false}"
  });
}
EOF

# Stage the developer's optional config.local.js so it's available at serve
# time regardless of whether trunk built before or after it appeared.
mkdir -p /app/dioxus-ui/dist
cp -f /app/dioxus-ui/scripts/config.local.js /app/dioxus-ui/dist/config.local.js

# ---------------------------------------------------------------------------
# DIOXUS_SERVE_MODE controls runtime behavior:
#   "static" — build once, serve dist/ with miniserve (CI / E2E)
#   "dev"    — trunk serve with hot-reload (local development, default)
# ---------------------------------------------------------------------------
DIOXUS_SERVE_MODE="${DIOXUS_SERVE_MODE:-dev}"

if [ "$DIOXUS_SERVE_MODE" = "static" ]; then
    # One-shot tailwind build (no --watch)
    tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --minify

    # Build wasm once. Uses cached artifacts from the Docker volume on warm runs.
    trunk build

    # Copy runtime overrides into the built dist/ (trunk's copy-file directive
    # only copies config.js, and config.local.js is intentionally optional).
    cp -f /app/dioxus-ui/scripts/config.local.js /app/dioxus-ui/dist/config.local.js

    # Serve statically. No file watcher, no recompilation, ~3MB RSS.
    # --spa enables SPA fallback: unknown routes serve index.html so the
    # Dioxus client-side router handles /meeting/:id etc.
    exec miniserve \
        --port "${TRUNK_SERVE_PORT:-3001}" \
        --interfaces 0.0.0.0 \
        --index index.html \
        --spa \
        /app/dioxus-ui/dist
else
    # Development mode: hot-reload with trunk serve.
    # Mirror overrides into dist/ in case trunk has already done its initial build.
    if [ -d /app/dioxus-ui/dist ]; then
        cp -f /app/dioxus-ui/scripts/config.local.js /app/dioxus-ui/dist/config.local.js
    fi

    (
        while true; do
            if [ -d /app/dioxus-ui/dist ]; then
                cp -f /app/dioxus-ui/scripts/config.local.js /app/dioxus-ui/dist/config.local.js 2>/dev/null || true
            fi
            sleep 1
        done
    ) &

    tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --watch --minify &

    exec trunk serve --address 0.0.0.0 --port "${TRUNK_SERVE_PORT:-3001}" --poll
fi
