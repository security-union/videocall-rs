#!/usr/bin/env bash
set -euo pipefail

# check-grafana-panel-ids.sh (#1087)
#
# Fails the build if any Grafana dashboard JSON under helm/grafana/dashboards/
# contains a DUPLICATE panel `id` across BOTH top-level `panels[]` AND nested
# `row.panels[]` (rows included — a row is itself a panel object with an id).
#
# Why this guard exists: Grafana keys panels by `id`. When a dashboard reuses an
# id between a top-level panel and a panel nested inside a `collapsed: true` row,
# the collision is dormant — until someone expands the row and Grafana
# re-serializes on save, at which point it de-dupes by keeping the FIRST
# occurrence and silently DROPS the colliding panel(s). That is latent data loss.
# meeting-investigation.json accumulated exactly this (ids 43/44/45/46 reused
# between top-level Viewport panels and nested Adaptive-Quality-Debugging panels,
# with id 46 colliding a panel against a row). It was caught by manual audit, not
# at PR time. This script is the recurrence guardrail.
#
# Detection: every object anywhere in the dashboard tree that has BOTH an `id`
# and a `gridPos` is a panel or a row (verified against the real dashboards: the
# only matching types are row/table/timeseries — never datasource/target/etc.,
# which carry no gridPos). We collect those ids per file and assert uniqueness.
#
# Usage:
#   scripts/check-grafana-panel-ids.sh                 # lint the tracked dashboards
#   scripts/check-grafana-panel-ids.sh path/to/dash.json [more.json ...]

usage() {
  cat <<'EOF'
Usage: scripts/check-grafana-panel-ids.sh [DASHBOARD.json ...]

With no arguments, lints every *.json under helm/grafana/dashboards/.
With arguments, lints exactly the given dashboard JSON files.

Exit codes:
  0  all dashboards have unique panel/row ids
  1  a duplicate id was found (offending file + ids are printed)
  2  usage error or a dashboard failed to parse as JSON
EOF
}

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq is required but not installed." >&2
  exit 2
fi

# Build the file list BEFORE changing directory. Explicit args may be relative
# to the caller's cwd, so resolve each to an absolute path while still in that
# cwd — otherwise the later `cd "$ROOT_DIR"` would reinterpret them relative to
# the repo root and a `../helm/...` arg from a subdirectory would not be found.
files=()
if [[ $# -gt 0 ]]; then
  for arg in "$@"; do
    if [[ "$arg" = /* ]]; then
      files+=("$arg")
    else
      files+=("$PWD/$arg")
    fi
  done
fi

ROOT_DIR="$(git rev-parse --show-toplevel)"
cd "$ROOT_DIR"

# With no explicit args, lint every dashboard under the canonical directory,
# RECURSING into subdirectories. The workflow path filter is
# `helm/grafana/dashboards/**` (recursive), so a dashboard added in a
# subdirectory triggers the check — the file list must match that scope, or such
# a file would trigger CI yet be silently skipped (a false green). `find -print0`
# + null-delimited read is robust to any path characters.
if [[ ${#files[@]} -eq 0 ]]; then
  if [[ -d helm/grafana/dashboards ]]; then
    while IFS= read -r -d '' f; do
      files+=("$f")
    done < <(find helm/grafana/dashboards -type f -name '*.json' -print0)
  fi
fi

if [[ ${#files[@]} -eq 0 ]]; then
  echo "No dashboard JSON files to check (helm/grafana/dashboards/ is empty)." >&2
  # Nothing to validate is not a failure — there is no duplicate.
  exit 0
fi

found_dup=0

for f in "${files[@]}"; do
  if [[ ! -f "$f" ]]; then
    echo "ERROR: dashboard file not found: $f" >&2
    exit 2
  fi

  # Validate the dashboard parses AND has an object root before analysing — a
  # malformed dashboard is a hard error (exit 2), distinct from a duplicate-id
  # finding (exit 1). We compare jq's emitted root `type` rather than using
  # `jq empty` / `jq -e 'type=="object"'`: on a 0-byte or whitespace-only file
  # jq reads NO input, so the filter never runs and both of those exit 0 —
  # letting truncated/empty corruption through, exactly what this guard must
  # catch. Capturing the type and string-comparing it rejects empty, truncated,
  # and non-object-root (array/number/string/null) files alike.
  root_type="$(jq -r 'type' "$f" 2>/dev/null || true)"
  if [[ "$root_type" != "object" ]]; then
    echo "ERROR: $f is not a valid Grafana dashboard JSON object (root type: ${root_type:-empty/parse-error})." >&2
    exit 2
  fi

  # Emit any id that appears more than once among all panel/row objects
  # (objects carrying both `id` and `gridPos`), recursively across the whole
  # tree (top-level panels[] and nested row.panels[]). `group_by` + `select`
  # keeps only collisions.
  #
  # This substitution is INTENTIONALLY unguarded (no `|| true`): the root-type
  # check above already proved the file parses, so a jq failure here is a real
  # error and `set -e` SHOULD abort (fail-closed). Do NOT add `|| true` — that
  # would turn a jq fault into a silent pass (false green) for a guard.
  dups="$(
    jq -r '
      [ .. | objects | select(has("id") and has("gridPos")) | .id ]
      | group_by(.)
      | map(select(length > 1))
      | map({id: .[0], count: length})
      | .[]
      | "    id \(.id) used \(.count) times"
    ' "$f"
  )"

  if [[ -n "$dups" ]]; then
    echo "DUPLICATE panel/row id(s) in $f:"
    echo "$dups"
    found_dup=1
  fi
done

if [[ "$found_dup" -ne 0 ]]; then
  echo
  echo "Grafana keys panels by id; duplicate ids across panels[] and nested" >&2
  echo "row.panels[] cause Grafana to silently drop colliding panels when a" >&2
  echo "collapsed row is expanded and re-saved. Renumber the offending panels" >&2
  echo "so every panel and row id is unique within its dashboard." >&2
  exit 1
fi

echo "All Grafana dashboards have unique panel/row ids (${#files[@]} file(s) checked)."
