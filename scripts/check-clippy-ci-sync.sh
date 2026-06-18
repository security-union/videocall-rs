#!/usr/bin/env bash
# Guards the byte-for-byte sync between the `clippy-ci` Makefile target and the
# `cargo clippy` steps in the CI clippy job (issue #1500).
#
# `make clippy-ci` exists to let a contributor reproduce CI's exact clippy
# command set locally (CLAUDE.md mandates running it before every push). That
# guarantee only holds while the two lists stay identical. Both the Makefile
# comment and CLAUDE.md ask contributors to keep them "BYTE-IDENTICAL" by hand —
# a manual contract that is itself a drift vector: the next person who edits one
# side and forgets the other silently reintroduces the local-vs-CI lint
# divergence #1495 set out to eliminate. This check makes the contract
# self-enforcing instead of comment-enforced.
#
# It extracts the ordered list of `cargo clippy` invocations from each source,
# normalizes whitespace, and fails with a diff if they differ.
#
# Usage: bash scripts/check-clippy-ci-sync.sh
set -euo pipefail

ROOT_DIR="$(git rev-parse --show-toplevel)"
MAKEFILE="$ROOT_DIR/Makefile"
WORKFLOW="$ROOT_DIR/.github/workflows/pr-check-rust-hcl.yaml"

if [[ ! -f "$MAKEFILE" ]]; then
  echo "ERROR: Makefile not found: $MAKEFILE" >&2
  exit 2
fi
if [[ ! -f "$WORKFLOW" ]]; then
  echo "ERROR: workflow not found: $WORKFLOW" >&2
  exit 2
fi

# Normalize a `cargo clippy ...` invocation: strip leading/trailing whitespace
# and collapse internal runs of whitespace to a single space, so a difference in
# indentation (tab in the Makefile vs spaces in YAML) is never a false positive
# but a real flag/argument difference is caught.
normalize() {
  sed -E 's/^[[:space:]]+//; s/[[:space:]]+$//; s/[[:space:]]+/ /g'
}

# Makefile: extract the recipe lines of the `clippy-ci:` target ONLY. The block
# runs from the `clippy-ci:` rule line until the first line that is neither blank
# nor a recipe line (recipe lines are tab-indented). Scoping to this target
# keeps the unrelated `check:` target's `cargo clippy --deny warnings` out.
makefile_cmds="$(
  awk '
    /^clippy-ci:/ { inrule = 1; next }
    inrule {
      # A recipe line is tab-indented; a non-indented, non-blank line ends the rule.
      if ($0 ~ /^[^[:space:]]/ && $0 !~ /^$/) { inrule = 0; next }
      # Skip recipe comment lines (whitespace then `#`) so an explanatory comment
      # mentioning "cargo clippy" inside the recipe does not leak into the compare.
      if ($0 ~ /^[[:space:]]*#/) { next }
      if ($0 ~ /cargo clippy/) { print }
    }
  ' "$MAKEFILE" | normalize
)"

# Workflow: extract the `run: cargo clippy ...` lines from the `clippy:` job
# ONLY. The job block runs from the `  clippy:` key (2-space indent) until the
# next top-level job key at the same indent. Scoping to the job means a
# `cargo clippy` added to some other job in this file does not silently desync
# this check.
workflow_cmds="$(
  awk '
    /^  clippy:/ { injob = 1; next }
    injob {
      # Another 2-space-indented job key ends the clippy job. This assumes step
      # keys are indented deeper than 2 spaces (they are — 4+); if extraction
      # ever truncates early the empty-result guard below fails loud, not silent.
      if ($0 ~ /^  [A-Za-z0-9_-]+:/) { injob = 0; next }
      if ($0 ~ /run:[[:space:]]*cargo clippy/) {
        sub(/^[[:space:]]*run:[[:space:]]*/, "")
        print
      }
    }
  ' "$WORKFLOW" | normalize
)"

if [[ -z "$makefile_cmds" ]]; then
  echo "ERROR: no 'cargo clippy' lines found in the clippy-ci Makefile target." >&2
  echo "       The extractor likely broke — refusing to pass vacuously." >&2
  exit 2
fi
if [[ -z "$workflow_cmds" ]]; then
  echo "ERROR: no 'run: cargo clippy' lines found in the workflow clippy job." >&2
  echo "       The extractor likely broke — refusing to pass vacuously." >&2
  exit 2
fi

if [[ "$makefile_cmds" != "$workflow_cmds" ]]; then
  echo "ERROR: the 'clippy-ci' Makefile target and the workflow clippy job have drifted." >&2
  echo "       Keep them byte-identical (issue #1500). Diff (< Makefile, > workflow):" >&2
  echo >&2
  diff <(printf '%s\n' "$makefile_cmds") <(printf '%s\n' "$workflow_cmds") >&2 || true
  exit 1
fi

count="$(printf '%s\n' "$makefile_cmds" | wc -l | tr -d ' ')"
echo "OK: clippy-ci Makefile target and workflow clippy job are in sync ($count invocations)."
