#!/bin/sh
set -eu

# Generate runtime config.js.
#
# Do NOT wrap the object in `Object.freeze()` here. The freeze silently breaks
# `dioxus-ui/scripts/config.local.js`'s override mechanism — that file does
# `Object.assign(window.__APP_CONFIG, {...})` and Object.assign no-ops (or
# throws under strict mode) when the target is frozen. The committed
# `scripts/config.js` is intentionally unfrozen for the same reason; the
# docker-generated version must match.
mkdir -p /app/dioxus-ui/scripts
cat > /app/dioxus-ui/scripts/config.js <<EOF
window.__APP_CONFIG = ({
  apiBaseUrl: "${API_BASE_URL:-http://localhost:${ACTIX_PORT:-8080}}",
  wsUrl: "${ACTIX_UI_BACKEND_URL:-ws://localhost:${ACTIX_PORT:-8080}}",
  webTransportHost: "${WEBTRANSPORT_HOST:-https://127.0.0.1:4433}",
  oauthEnabled: "${ENABLE_OAUTH:-false}",
  e2eeEnabled: "${E2EE_ENABLED:-false}",
  webTransportEnabled: "${WEBTRANSPORT_ENABLED:-false}",
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
  mockPeersEnabled: "${MOCK_PEERS_ENABLED:-false}"
});
EOF

# Stage the developer's optional config.local.js so it's available at serve
# time regardless of whether trunk built before or after it appeared.
if [ -f /app/dioxus-ui/scripts/config.local.js ]; then
    mkdir -p /app/dioxus-ui/dist
    cp -f /app/dioxus-ui/scripts/config.local.js /app/dioxus-ui/dist/config.local.js
fi

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

    # Copy runtime config into the built dist/ (trunk's copy-file directive only
    # runs at build time, config.js was generated above after any prior build).
    cp -f /app/dioxus-ui/scripts/config.js /app/dioxus-ui/dist/config.js
    if [ -f /app/dioxus-ui/scripts/config.local.js ]; then
        cp -f /app/dioxus-ui/scripts/config.local.js /app/dioxus-ui/dist/config.local.js
    fi

    # Serve statically. No file watcher, no recompilation, ~3MB RSS.
    exec miniserve \
        --port "${TRUNK_SERVE_PORT:-3001}" \
        --interfaces 0.0.0.0 \
        --index index.html \
        /app/dioxus-ui/dist
else
    # Development mode: hot-reload with trunk serve.
    # Mirror config into dist/ in case trunk has already done its initial build.
    if [ -d /app/dioxus-ui/dist ]; then
        cp -f /app/dioxus-ui/scripts/config.js /app/dioxus-ui/dist/config.js
    fi

    tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --watch --minify &

    exec trunk serve --address 0.0.0.0 --port "${TRUNK_SERVE_PORT:-3001}" --poll
fi
