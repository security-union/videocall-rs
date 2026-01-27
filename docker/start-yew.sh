#!/bin/sh
set -eu

# Generate runtime config.js
mkdir -p /app/yew-ui/scripts
cat > /app/yew-ui/scripts/config.js <<EOF
window.__APP_CONFIG = Object.freeze({
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
  screenBitrateKbps: ${SCREEN_BITRATE_KBPS:-100}
});
EOF

# Ensure trunk is on PATH (common install locations)
# trigger ci
export PATH="$PATH:/usr/local/cargo/bin:/root/.cargo/bin"

# Run the dev server
exec trunk serve --address 0.0.0.0 --port "${TRUNK_SERVE_PORT:-80}"
