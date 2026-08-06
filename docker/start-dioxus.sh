#!/bin/sh
# Native dev server for the Dioxus UI (`make dev-ui`): generates the runtime
# config.js from env, runs tailwind in watch mode, serves via trunk with hot
# reload. Needs trunk + tailwindcss + the wasm32 target on PATH — the pinned
# versions come from `nix-shell -A shells.frontend`.
set -eu

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
UI_DIR="$REPO_ROOT/dioxus-ui"

# Generate runtime config.js
mkdir -p "$UI_DIR/scripts"
cat > "$UI_DIR/scripts/config.js" <<EOF
window.__APP_CONFIG = Object.freeze({
  apiBaseUrl: "${API_BASE_URL:-http://localhost:8081}",
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
  screenBitrateKbps: ${SCREEN_BITRATE_KBPS:-100},
  oauthProvider: "${OAUTH_PROVIDER:-}",
  vadThreshold: ${VAD_THRESHOLD:-0.02}
});
EOF

cd "$UI_DIR"

tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --watch --minify &

exec trunk serve --address 0.0.0.0 --port "${TRUNK_SERVE_PORT:-3001}" --poll
