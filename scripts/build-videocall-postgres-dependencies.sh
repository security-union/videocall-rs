#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(git rev-parse --show-toplevel)"
CHART_DIR="${1:-$ROOT_DIR/helm/videocall-postgres}"
HELM_BIN="${HELM_BIN:-helm}"

if ! command -v "$HELM_BIN" >/dev/null 2>&1; then
  echo "ERROR: helm is required." >&2
  exit 2
fi

repository="$(
  "$HELM_BIN" dependency list "$CHART_DIR" |
    awk '$1 == "postgresql" { print $3; found = 1 } END { exit !found }'
)"
if [[ "$repository" != oci://* ]]; then
  "$HELM_BIN" repo add videocall-postgres-dependency "$repository" \
    --force-update >/dev/null
fi
"$HELM_BIN" dependency build --skip-refresh "$CHART_DIR" >/dev/null

printf '%s\n' "$CHART_DIR"
