#!/usr/bin/env bash
# Fails if the three daily-deploy workflows disagree on their step-name sequence
# or their actions/checkout pin (issue #2255). Legitimate per-cluster differences
# all live inside step bodies. Exit: 0 in sync | 1 drift | 2 extractor broke.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REFERENCE="hcl"
OTHERS="ascend labsworkspace"
MIN_STEPS=15

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

line_count() { grep -c '' <"$1" || true; }

for wf in $REFERENCE $OTHERS; do
  file="$ROOT_DIR/.github/workflows/daily-deploy-${wf}.yaml"
  if [ ! -f "$file" ]; then
    echo "ERROR: workflow not found: $file" >&2
    echo "       Update REFERENCE/OTHERS in this script, or restore the file." >&2
    exit 2
  fi

  # Steps are the 6-space `      - name: X` list; run-block lines are deeper.
  sed -n 's/^      - name: //p' "$file" >"$WORK_DIR/names.$wf"
  count="$(line_count "$WORK_DIR/names.$wf")"
  if [ "$count" -lt "$MIN_STEPS" ]; then
    echo "ERROR: extracted only ${count} step names from ${file} (expected >= ${MIN_STEPS})." >&2
    echo "       The extractor likely broke — refusing to pass vacuously." >&2
    exit 2
  fi

  grep -oE 'actions/checkout@[A-Za-z0-9._-]+' "$file" | sort -u >"$WORK_DIR/pins.$wf" || true
  if [ "$(line_count "$WORK_DIR/pins.$wf")" -eq 0 ]; then
    echo "ERROR: no actions/checkout reference found in ${file}." >&2
    echo "       The extractor likely broke — refusing to pass vacuously." >&2
    exit 2
  fi
  cat "$WORK_DIR/pins.$wf" >>"$WORK_DIR/pins.all"
done

status=0

for wf in $OTHERS; do
  if ! diff -q "$WORK_DIR/names.$REFERENCE" "$WORK_DIR/names.$wf" >/dev/null; then
    echo "ERROR: daily-deploy-${wf}.yaml has drifted from daily-deploy-${REFERENCE}.yaml." >&2
    echo "       Step names must match exactly, in order (issue #2255)." >&2
    echo "       Diff (< ${REFERENCE}, > ${wf}):" >&2
    echo >&2
    diff "$WORK_DIR/names.$REFERENCE" "$WORK_DIR/names.$wf" >&2 || true
    echo >&2
    status=1
  fi
done

sort -u "$WORK_DIR/pins.all" >"$WORK_DIR/pins.unique"
if [ "$(line_count "$WORK_DIR/pins.unique")" -ne 1 ]; then
  echo "ERROR: the daily-deploy workflows do not agree on one actions/checkout pin:" >&2
  sed 's/^/       /' "$WORK_DIR/pins.unique" >&2
  status=1
fi

if [ "$status" -ne 0 ]; then
  exit 1
fi

echo "OK: the $(( $(echo "$OTHERS" | wc -w) + 1 )) daily-deploy workflows agree on \
$(line_count "$WORK_DIR/names.$REFERENCE") step names and on $(cat "$WORK_DIR/pins.unique")."
