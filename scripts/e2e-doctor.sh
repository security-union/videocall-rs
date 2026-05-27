#!/usr/bin/env bash
#
# E2E preflight diagnostic. Checks every prerequisite for `make e2e` to
# work and prints a pass/fail report with copy-paste-friendly fixes.
#
# Run from anywhere via:
#   make e2e-doctor
#
# Or directly:
#   scripts/e2e-doctor.sh
#
# Exit code 0 if every check passes, 1 otherwise.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# ---------------------------------------------------------------------------
# Reporting helpers. We collect all failures + warnings before exiting so a
# single run surfaces every problem at once (the original symptom was that a
# missing tool, a stale cert, AND a stopped container compounded into one
# unhelpful timeout — fix that by reporting all three together).
# ---------------------------------------------------------------------------

PASS=0
FAIL=0
WARN=0
FAILURES=()
WARNINGS=()

log_pass() {
  echo "  [OK]   $1"
  PASS=$((PASS + 1))
}

log_fail() {
  echo "  [FAIL] $1"
  FAIL=$((FAIL + 1))
  if [[ -n "${2:-}" ]]; then
    FAILURES+=("$1"$'\n'"$2")
  else
    FAILURES+=("$1")
  fi
}

log_warn() {
  echo "  [WARN] $1"
  WARN=$((WARN + 1))
  if [[ -n "${2:-}" ]]; then
    WARNINGS+=("$1"$'\n'"$2")
  else
    WARNINGS+=("$1")
  fi
}

section() {
  echo
  echo "== $1 =="
}

# ---------------------------------------------------------------------------
# 1. Required tools on PATH.
# ---------------------------------------------------------------------------

section "Required tools"

check_tool() {
  local tool="$1"
  local fix="$2"
  if command -v "${tool}" >/dev/null 2>&1; then
    log_pass "${tool} found at $(command -v "${tool}")"
  else
    log_fail "${tool} not on PATH" "$(printf '  Fix: %s' "${fix}")"
  fi
}

check_tool openssl "Debian/WSL: sudo apt-get install -y openssl   |   macOS: brew install openssl"
check_tool docker "https://docs.docker.com/engine/install/"
check_tool make "Debian/WSL: sudo apt-get install -y make   |   macOS: xcode-select --install"
check_tool node "https://nodejs.org/  (LTS recommended)"

if command -v docker >/dev/null 2>&1; then
  if docker compose version >/dev/null 2>&1; then
    log_pass "docker compose v2 available"
  else
    log_fail "docker compose v2 not available" "  Fix: install Docker Compose v2 (the v1 \`docker-compose\` shim won't work)"
  fi
fi

# ---------------------------------------------------------------------------
# 2. WebTransport dev cert sanity (delegates to regen-dev-cert.sh --verify
#    so the rules stay in one place; the WT server's startup preflight
#    enforces the same constraints).
# ---------------------------------------------------------------------------

section "WebTransport dev cert"

# Capture full output so we can replay it if it fails — `--verify` already
# prints a copy-paste-friendly diagnostic to stderr that we want the user
# to see verbatim.
cert_output="$(bash "${SCRIPT_DIR}/regen-dev-cert.sh" --verify 2>&1)"
cert_rc=$?
if [[ $cert_rc -eq 0 ]]; then
  log_pass "cert + key + hash file all valid (ECDSA P-256, <= 14d, SAN OK, hash matches)"
  echo "${cert_output}" | sed -n '2,$p' | sed 's/^/         /'
else
  log_fail "cert preflight failed" "$(echo "${cert_output}" | sed 's/^/  /')"
fi

# ---------------------------------------------------------------------------
# 3. E2E stack containers.
# ---------------------------------------------------------------------------

section "E2E stack containers"

if ! command -v docker >/dev/null 2>&1; then
  log_warn "skipping container checks because docker is missing"
else
  expected_services=(
    videocall-e2e-postgres-1
    videocall-e2e-nats-1
    videocall-e2e-meeting-api-1
    videocall-e2e-websocket-api-1
    videocall-e2e-webtransport-api-1
    videocall-e2e-dioxus-ui-1
  )
  any_running=0
  for svc in "${expected_services[@]}"; do
    state="$(docker inspect -f '{{.State.Status}}' "${svc}" 2>/dev/null || true)"
    case "${state}" in
      running) log_pass "${svc} running"; any_running=1 ;;
      "")      log_warn "${svc} not created (run 'make e2e-up' to start the stack)" ;;
      *)       log_fail "${svc} state=${state}" "  Fix: docker logs ${svc}   then   docker restart ${svc}" ;;
    esac
  done
  if [[ ${any_running} -eq 0 ]]; then
    log_warn "no E2E containers are running" "  Fix: make e2e-up"
  fi
fi

# ---------------------------------------------------------------------------
# 4. Service reachability (only meaningful if containers are up).
# ---------------------------------------------------------------------------

section "Service reachability"

probe_http() {
  local label="$1" url="$2" expected="$3"
  if ! command -v curl >/dev/null 2>&1; then
    log_warn "${label} (no curl on PATH)"
    return
  fi
  local code
  code="$(curl -sk -o /dev/null -m 3 -w '%{http_code}' "${url}" 2>/dev/null || echo 000)"
  if [[ "${code}" == "${expected}" ]]; then
    log_pass "${label} responding (${url} -> ${code})"
  elif [[ "${code}" == "000" ]]; then
    log_warn "${label} unreachable at ${url} (HTTP code 000 — service may not be running yet)"
  else
    log_fail "${label} returned HTTP ${code} (expected ${expected}) at ${url}" \
             "  Fix: docker logs videocall-e2e-${label}-1   to see why."
  fi
}

probe_http dioxus-ui http://localhost:3001 200
probe_http meeting-api http://localhost:8081/session 401
probe_http websocket-api http://localhost:8080 404
probe_http webtransport-api http://localhost:5321/healthz 200

# ---------------------------------------------------------------------------
# 5. Playwright npm modules.
# ---------------------------------------------------------------------------

section "Playwright dependencies"

if [[ -d "${REPO_ROOT}/e2e/node_modules" ]]; then
  log_pass "e2e/node_modules present"
else
  log_fail "e2e/node_modules missing" "  Fix: make e2e-install"
fi

if [[ -d "${HOME}/.cache/ms-playwright" ]] && \
   ls "${HOME}/.cache/ms-playwright"/chromium-* >/dev/null 2>&1; then
  log_pass "Playwright Chromium installed"
else
  log_warn "Playwright Chromium not in ${HOME}/.cache/ms-playwright" \
           "  Fix: cd e2e && npx playwright install chromium"
fi

# ---------------------------------------------------------------------------
# Final report.
# ---------------------------------------------------------------------------

section "Summary"
echo "  ${PASS} passed, ${FAIL} failed, ${WARN} warning(s)"

if [[ ${FAIL} -gt 0 ]]; then
  echo
  echo "Failures:"
  for f in "${FAILURES[@]}"; do
    echo "  - ${f}"
    echo
  done
  exit 1
fi

if [[ ${WARN} -gt 0 ]]; then
  echo
  echo "Warnings (non-fatal but worth checking):"
  for w in "${WARNINGS[@]}"; do
    echo "  - ${w}"
    echo
  done
fi

echo
echo "OK: ready to run 'make e2e SPEC=...'"
exit 0
