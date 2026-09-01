#!/usr/bin/env bash
# Build-and-serve one E2E backend binary under `cargo watch` (issue #2513).

set -uo pipefail

STAMP_DIR="/app/e2e/.stack-stamps"
HEARTBEAT_SECS=30

ROLE="${1:?usage: e2e-backend.sh <supervise|build-run> <bin-name>}"
BIN="${2:?usage: e2e-backend.sh <supervise|build-run> <bin-name>}"
STAMP="${STAMP_DIR}/${BIN}.json"

CARGO_FLAGS=()
if [[ "${E2E_CARGO_RELEASE:-}" == "1" ]]; then
  CARGO_FLAGS+=(-r)
fi

write_stamp() {
  mkdir -p "${STAMP_DIR}" 2>/dev/null
  chmod 0777 "${STAMP_DIR}" 2>/dev/null
  if ! printf '{"service":"%s","build":"%s","at":"%s"}\n' \
       "${BIN}" "$1" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"${STAMP}"; then
    echo "e2e-backend[${BIN}]: CANNOT WRITE ${STAMP} — the e2e freshness guard will fail closed." >&2
    return 1
  fi
}

heartbeat() {
  while true; do
    sleep "${HEARTBEAT_SECS}"
    [[ -f "${STAMP}" ]] && touch "${STAMP}"
  done
}

case "${ROLE}" in
  supervise)
    write_stamp building
    heartbeat &
    # No -w: that flag disables cargo-watch's discovery of local path deps,
    # which is what keeps the watch set correct as crates are added.
    exec cargo watch --why -- "${BASH_SOURCE[0]}" build-run "${BIN}"
    ;;
  build-run)
    write_stamp building
    if ! cargo build ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} --bin "${BIN}"; then
      write_stamp failed
      echo "e2e-backend[${BIN}]: build FAILED — the previous binary is no longer being served." >&2
      exit 1
    fi
    write_stamp ok
    exec cargo run ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} --bin "${BIN}"
    ;;
  *)
    echo "e2e-backend: unknown role '${ROLE}' (expected supervise|build-run)" >&2
    exit 64
    ;;
esac
