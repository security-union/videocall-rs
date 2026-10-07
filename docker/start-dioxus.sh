#!/bin/sh
set -eu

APP_ROOT="${START_DIOXUS_APP_ROOT:-/app}"

CONFIG_LOCAL="${APP_ROOT}/dioxus-ui/scripts/config.local.js"
DIST="${APP_ROOT}/dioxus-ui/dist"
RELEASE_BUILD=0
case "${VIDEOCALL_RELEASE_BUILD:-}" in
    1)
        RELEASE_BUILD=1
        RELEASE_ROOT="${CARGO_TARGET_DIR:-${APP_ROOT}/dioxus-ui/target}/videocall-release"
        CONFIG_LOCAL="${RELEASE_ROOT}/config.local.js"
        DIST="${RELEASE_ROOT}/dist"
        DIOXUS_SERVE_MODE="static"
        export CARGO_INCREMENTAL=0
        ;;
    ""|0) ;;
    *)
        echo "start-dioxus: VIDEOCALL_RELEASE_BUILD='${VIDEOCALL_RELEASE_BUILD}' is not 0 or 1" >&2
        exit 64
        ;;
esac

# Generate runtime config.local.js.
#
# The e2e stack bind-mounts the repo at /app. Writing the generated e2e
# runtime config into the tracked `scripts/config.js` dirties the developer's
# worktree, so keep the generated values in the gitignored local override file
# that index.html loads after the committed defaults.
mkdir -p "$(dirname "$CONFIG_LOCAL")"
if [ -n "${SEARCH_API_BASE_URL:-}" ]; then
    SEARCH_API_BASE_URL_CONFIG="\"${SEARCH_API_BASE_URL}\""
else
    SEARCH_API_BASE_URL_CONFIG="null"
fi

# Local dev only: WT cert hash for serverCertificateHashes (Playwright injects its own in e2e).
WT_CERT_HASHES_JS=""
if [ "${WT_DEV_CERT_HASH_INJECT:-false}" = "true" ] && [ "${WEBTRANSPORT_ENABLED:-false}" = "true" ]; then
    wt_hash_file="${APP_ROOT}/actix-api/certs/localhost.cert-sha256.txt"
    wt_hash="$(grep -v '^[[:space:]]*#' "$wt_hash_file" 2>/dev/null | grep -v '^[[:space:]]*$' | head -n 1 | tr -d '[:space:]' || true)"
    case "$wt_hash" in
        ''|*[!A-Za-z0-9+/=]*)
            echo "start-dioxus: WT dev cert hash missing/malformed at $wt_hash_file; run 'make e2e-cert', then restart webtransport-api and dioxus-ui" >&2
            ;;
        *)
            WT_CERT_HASHES_JS="if (location.hostname === \"localhost\" || location.hostname === \"127.0.0.1\" || location.hostname.endsWith(\".localhost\")) { window.__VC_WT_CERT_HASHES__ = [\"${wt_hash}\"]; }"
            ;;
    esac
fi

cat > "$CONFIG_LOCAL" <<EOF
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
${WT_CERT_HASHES_JS}
EOF

# Stage the developer's optional config.local.js so it's available at serve
# time regardless of whether trunk built before or after it appeared.
mkdir -p "$DIST"
cp -f "$CONFIG_LOCAL" "$DIST/config.local.js"

# ---------------------------------------------------------------------------
# DIOXUS_SERVE_MODE controls runtime behavior:
#   "static" — build once, serve dist/ with miniserve (CI / E2E)
#   "dev"    — trunk serve with hot-reload (local development, default)
# ---------------------------------------------------------------------------
DIOXUS_SERVE_MODE="${DIOXUS_SERVE_MODE:-dev}"

# Extract scheme://host[:port] only. The host character class deliberately
# EXCLUDES quote, space and semicolon so a stray value can never break out of
# the header quoting or inject a directive (mirrors the Helm helper regex).
csp_origin() {
    printf '%s\n' "$1" | sed -nE 's#^([A-Za-z][A-Za-z0-9+.-]*://[A-Za-z0-9._:-]+).*$#\1#p'
}

csp_connect_src="'self'"
for csp_url in \
    "${API_BASE_URL:-http://localhost:${ACTIX_PORT:-8080}}" \
    "${MEETING_API_BASE_URL:-}" \
    "${ACTIX_UI_BACKEND_URL:-ws://localhost:${ACTIX_PORT:-8080}}" \
    "${WEBTRANSPORT_HOST:-https://127.0.0.1:4433}" \
    "${SEARCH_API_BASE_URL:-}"
do
    OLD_IFS="$IFS"
    IFS=","
    set -- $csp_url
    IFS="$OLD_IFS"
    for csp_part in "$@"; do
        csp_origin_value="$(csp_origin "$csp_part")"
        if [ -n "$csp_origin_value" ] && ! printf '%s\n' " $csp_connect_src " | grep -Fq " $csp_origin_value "; then
            csp_connect_src="$csp_connect_src $csp_origin_value"
        fi
    done
done

if [ "${ENABLE_OAUTH:-false}" = "true" ] && [ "${OAUTH_FLOW:-}" = "pkce" ]; then
    csp_idp_url="${OAUTH_TOKEN_URL:-}"
    if [ -z "$csp_idp_url" ] && [ "${OAUTH_PROVIDER:-}" = "google" ]; then
        csp_idp_url="https://oauth2.googleapis.com/token"
    elif [ -z "$csp_idp_url" ]; then
        csp_idp_url="${OAUTH_ISSUER:-}"
    fi
    csp_origin_value="$(csp_origin "$csp_idp_url")"
    if [ -n "$csp_origin_value" ] && ! printf '%s\n' " $csp_connect_src " | grep -Fq " $csp_origin_value "; then
        csp_connect_src="$csp_connect_src $csp_origin_value"
    fi
fi

CSP_REPORT_ONLY_HEADER="default-src 'self'; script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; media-src 'self' blob:; worker-src 'self' blob:; connect-src $csp_connect_src; frame-src 'none'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'; form-action 'self'; upgrade-insecure-requests"

if [ "$DIOXUS_SERVE_MODE" = "static" ]; then
    # One-shot tailwind build (no --watch)
    tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --minify

    # Build wasm once. Uses cached artifacts from the Docker volume on warm runs.
    if [ "$RELEASE_BUILD" = 1 ]; then
        trunk build --dist "$DIST" --release
    else
        trunk build
    fi

    # Copy runtime overrides into the built dist/ (trunk's copy-file directive
    # only copies config.js, and config.local.js is intentionally optional).
    cp -f "$CONFIG_LOCAL" "$DIST/config.local.js"

    # Serve statically. No file watcher, no recompilation, ~3MB RSS.
    # --spa enables SPA fallback: unknown routes serve index.html so the
    # Dioxus client-side router handles /meeting/:id etc.
    exec miniserve \
        --port "${TRUNK_SERVE_PORT:-3001}" \
        --interfaces 0.0.0.0 \
        --index index.html \
        --spa \
        --header "Content-Security-Policy-Report-Only: ${CSP_REPORT_ONLY_HEADER}" \
        "$DIST"
else
    # Development mode: hot-reload with trunk serve.
    # Mirror overrides into dist/ in case trunk has already done its initial build.
    if [ -d "$DIST" ]; then
        cp -f "$CONFIG_LOCAL" "$DIST/config.local.js"
    fi

    (
        while true; do
            if [ -d "$DIST" ]; then
                cp -f "$CONFIG_LOCAL" "$DIST/config.local.js" 2>/dev/null || true
            fi
            sleep 1
        done
    ) &

    tailwindcss -i ./static/leptos-style.css -o ./static/tailwind.css --watch --minify &

    exec trunk serve --address 0.0.0.0 --port "${TRUNK_SERVE_PORT:-3001}" --poll
fi
