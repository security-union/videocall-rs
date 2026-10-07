#!/usr/bin/env bash
# Usage: CARGO_SWEEP=<cargo-sweep 0.8.0> trim-cargo-targets.sh [--dry-run] TARGET_DIR...
# TRIM_TIERS="days:gate_gib ...": a tier runs only while free space is below its gate;
# --dry-run reports every tier.
#
# Never delete single files by atime: a build script's out/ file goes stale while
# the fingerprint vouching for it stays fresh, and cargo then cannot read it.
# cargo-sweep removes a unit's fingerprint and artifacts together.

set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
  DRY_RUN=1
  shift
fi
if [[ $# -eq 0 ]]; then
  echo "::error::trim-cargo-targets.sh: no target dirs given" >&2
  exit 2
fi
: "${CARGO_SWEEP:?CARGO_SWEEP must point at a cargo-sweep binary}"
TIERS="${TRIM_TIERS:-7:150 3:100}"
LOCK_WAIT="${LOCK_WAIT_SECS:-300}"
FAILED=0

gib() { awk -v b="$1" 'BEGIN { printf "%.1f GiB", b / 1073741824 }'; }
avail_bytes() { df -P -B1 "$1" | awk 'NR==2 {print $4}'; }
used_bytes() { { du -s -B1 "$1" 2>/dev/null || true; } | cut -f1; }

require_atime() {
  local opts
  opts="$(findmnt -no OPTIONS --target "$1")"
  if [[ ",${opts}," == *",noatime,"* ]]; then
    echo "::error::$1 is on a noatime mount (${opts}); an age-based trim would delete nothing."
    exit 1
  fi
}

profile_dirs() {
  find "$1" \( -name deps -o -name build -o -name incremental \) -prune \
    -o -type d -name .fingerprint -printf '%h\n'
}

# Holds cargo's build-directory locks so a concurrent build waits for the sweep.
# All-or-nothing with no blocking wait, so lock order cannot deadlock with cargo.
lock_all() {
  local deadline=$((SECONDS + LOCK_WAIT)) p lock fd got
  while :; do
    fds=()
    got=1
    for p in "$@"; do
      for lock in .cargo-build-lock .cargo-lock; do
        if ! exec {fd}>>"$p/$lock"; then
          unlock_all
          echo "::error::cannot open $p/$lock; skipping this target dir."
          return 2
        fi
        fds+=("$fd")
        flock -n "$fd" || { got=0; break 2; }
      done
    done
    [[ $got -eq 0 ]] || return 0
    unlock_all
    [[ $SECONDS -lt $deadline ]] || return 1
    sleep 5
  done
}

unlock_all() {
  local fd
  for fd in "${fds[@]}"; do exec {fd}>&-; done
  fds=()
}

sweep_dir() {
  local dir="$1" days="$2" project="$3" dry=() log rc lrc=0 before after profiles fds=()
  [[ "$4" -eq 1 ]] && dry=(--dry-run)
  mapfile -t profiles < <(profile_dirs "$dir")
  before="$(used_bytes "$dir")"
  lock_all "${profiles[@]}" || lrc=$?
  if [[ $lrc -eq 2 ]]; then
    FAILED=1
    return 0
  elif [[ $lrc -ne 0 ]]; then
    echo "::warning::$dir busy for ${LOCK_WAIT}s; skipping it this tier."
    return 0
  fi
  log="$(mktemp)"
  set +e
  CARGO_TARGET_DIR="$dir" "$CARGO_SWEEP" sweep --time "$days" "${dry[@]}" "$project" >"$log" 2>&1
  rc=$?
  set -e
  unlock_all
  cat "$log"
  if [[ $rc -ne 0 ]] || grep -qE '^\[(ERROR|WARN)\]' "$log"; then
    echo "::error::cargo-sweep failed on $dir (rc=$rc); see the log above."
    FAILED=1
  fi
  rm -f "$log"
  after="$(used_bytes "$dir")"
  echo "$dir: $(gib "$before") -> $(gib "$after"), reclaimed $((before - after)) bytes ($(gib $((before - after))))"
}

new_selftest_crate() {
  local d="$1"
  mkdir -p "$d/src"
  printf '[package]\nname = "trim_selftest"\nversion = "0.0.0"\nedition = "2021"\n\n[workspace]\n' >"$d/Cargo.toml"
  printf 'fn main() { std::fs::write(std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("gen.rs"), "pub const X: u32 = 1;").unwrap(); }\n' >"$d/build.rs"
  printf 'include!(concat!(env!("OUT_DIR"), "/gen.rs"));\n' >"$d/src/lib.rs"
}

selftest_build() {
  CARGO_TARGET_DIR="$1/target" cargo build --offline --quiet --manifest-path "$1/Cargo.toml"
}

backdate_atime() {
  find "$1" -type f -exec touch -a -d "@$(($(date +%s) - 30 * 86400))" {} +
}

# (A) a live unit whose out/ file went stale keeps it; (B) stale units go and
# the target still rebuilds.
self_test() {
  local st gen
  st="$(mktemp -d)"
  new_selftest_crate "$st"
  selftest_build "$st"
  backdate_atime "$st/target"
  selftest_build "$st"
  sweep_dir "$st/target" 7 "$st" 0
  gen="$(find "$st/target" -path '*/out/gen.rs' -print -quit)"
  touch "$st/src/lib.rs"
  if [[ -z "$gen" ]] || ! selftest_build "$st"; then
    echo "::error::self-test A: sweep removed build-script output of a live unit."
    exit 1
  fi
  backdate_atime "$st/target"
  sweep_dir "$st/target" 7 "$st" 0
  if [[ -n "$(find "$st/target" -path '*/out/gen.rs' -print -quit)" ]] || ! selftest_build "$st"; then
    echo "::error::self-test B: sweep left stale units or a target dir cargo cannot rebuild."
    exit 1
  fi
  [[ $FAILED -eq 0 ]] || { echo "::error::self-test: cargo-sweep reported errors."; exit 1; }
  rm -rf "$st"
  echo "self-test passed"
}

for dir in "$@"; do
  [[ -d "$dir" ]] || { echo "::error::$dir is not a directory"; exit 1; }
  require_atime "$dir"
done
self_test

project="$(mktemp -d)"
new_selftest_crate "$project"
free_start="$(avail_bytes "$1")"
echo "free before: $(gib "$free_start")"
for tier in $TIERS; do
  days="${tier%%:*}"
  gate=$((${tier##*:} * 1073741824))
  free="$(avail_bytes "$1")"
  if [[ $DRY_RUN -eq 0 && $free -ge $gate ]]; then
    echo "tier ${days}d: $(gib "$free") free >= $(gib "$gate"); skipped."
    continue
  fi
  echo "=== tier ${days}d (gate $(gib "$gate"), $(gib "$free") free$([[ $DRY_RUN -eq 1 ]] && echo ', DRY RUN')) ==="
  for dir in "$@"; do
    sweep_dir "$dir" "$days" "$project" "$DRY_RUN"
  done
done
rm -rf "$project"
free_end="$(avail_bytes "$1")"
echo "free after: $(gib "$free_end") (mount gained $((free_end - free_start)) bytes)"
exit "$FAILED"
