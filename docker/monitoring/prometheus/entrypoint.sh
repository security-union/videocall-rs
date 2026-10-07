#!/bin/sh
# Prometheus does not expand env vars in scrape targets, so the WS relay port
# (ACTIX_PORT on the dev stack) is substituted into a rendered copy.
set -eu

SRC=/etc/prometheus/prometheus.yml
OUT=/tmp/prometheus.yml
PORT="${WS_RELAY_PORT:-8080}"

case "${PORT}" in
  '' | *[!0-9]*)
    echo "prometheus entrypoint: WS_RELAY_PORT='${PORT}' is not a port number" >&2
    exit 1
    ;;
esac

TARGET="targets: \['websocket-api:8080'\]"
if [ "$(grep -c "${TARGET}" "${SRC}")" != 1 ]; then
  echo "prometheus entrypoint: ${SRC} must have exactly one \"${TARGET}\" line" >&2
  exit 1
fi

sed "s/${TARGET}/targets: ['websocket-api:${PORT}']/" "${SRC}" >"${OUT}"
exec /bin/prometheus --config.file="${OUT}" "$@"
