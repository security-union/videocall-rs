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

# Mirror the live config into dist/ in case trunk has already done its initial
# build (the `<link data-trunk rel="copy-file" href="./scripts/config.js" />`
# directive only runs at build time, so a regeneration of scripts/config.js
# AFTER the first build wouldn't propagate without a touch-rebuild).
if [ -d /app/dioxus-ui/dist ]; then
    cp -f /app/dioxus-ui/scripts/config.js /app/dioxus-ui/dist/config.js
fi

# Stage the developer's optional config.local.js into dist/ so the dev server
# can serve it. The Trunk.toml post_build hook also copies it on every build,
# but trunk doesn't watch files that didn't exist when `trunk serve` started,
# so a config.local.js created after the first build wouldn't auto-stage. This
# copy at container startup makes the docker workflow predictable: if the file
# exists on the host bind-mount at container-up time, it's available.
if [ -f /app/dioxus-ui/scripts/config.local.js ]; then
    mkdir -p /app/dioxus-ui/dist
    cp -f /app/dioxus-ui/scripts/config.local.js /app/dioxus-ui/dist/config.local.js
fi

tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --watch --minify &

exec trunk serve --address 0.0.0.0 --port "${TRUNK_SERVE_PORT:-3001}" --poll
