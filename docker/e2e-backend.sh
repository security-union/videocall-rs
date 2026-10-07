#!/usr/bin/env bash
# Build-and-serve one E2E backend binary under `cargo watch` (issue #2513).

set -uo pipefail

STAMP_DIR="${E2E_STAMP_DIR:-/app/e2e/.stack-stamps}"
HEARTBEAT_SECS=30
STOP_TIMEOUT_SECS=10

ROLE="${1:?usage: e2e-backend.sh <supervise|build-run> <bin-name>}"
BIN="${2:?usage: e2e-backend.sh <supervise|build-run> <bin-name>}"
STAMP="${STAMP_DIR}/${BIN}.json"

CARGO_FLAGS=()
case "${E2E_CARGO_RELEASE:-}" in
  1) CARGO_FLAGS+=(-r) ;;
  "" | 0) ;;
  *)
    echo "e2e-backend[${BIN}]: E2E_CARGO_RELEASE='${E2E_CARGO_RELEASE}' is not 0 or 1." >&2
    exit 64
    ;;
esac
case "${VIDEOCALL_RELEASE_BUILD:-}" in
  1) export CARGO_INCREMENTAL=0 ;;
  "" | 0) ;;
  *)
    echo "e2e-backend[${BIN}]: VIDEOCALL_RELEASE_BUILD='${VIDEOCALL_RELEASE_BUILD}' is not 0 or 1." >&2
    exit 64
    ;;
esac

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

stop_child() {
  trap '' TERM INT
  [[ -n "${CHILD:=${!:-}}" ]] || exit 143
  kill -TERM "${CHILD}" 2>/dev/null
  local i
  for ((i = 0; i < STOP_TIMEOUT_SECS; i++)); do
    kill -0 "${CHILD}" 2>/dev/null || break
    sleep 1
  done
  if kill -0 "${CHILD}" 2>/dev/null; then
    echo "e2e-backend[${BIN}]: still running ${STOP_TIMEOUT_SECS}s after SIGTERM — sending SIGKILL." >&2
    kill -KILL "${CHILD}"
  fi
  wait "${CHILD}"
  exit $?
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
    CHILD=""
    trap stop_child TERM INT
    cargo run ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"} --bin "${BIN}" &
    CHILD=$!
    wait "${CHILD}"
    exit $?
    ;;
  *)
    echo "e2e-backend: unknown role '${ROLE}' (expected supervise|build-run)" >&2
    exit 64
    ;;
esac
