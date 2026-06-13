#!/usr/bin/env bash
# retry-wasm-component-test.sh
#
# CI flake guard for the dioxus-ui wasm-bindgen component tests (issue #1267).
#
# The headless-Chrome / chromedriver harness used by wasm-bindgen-test
# intermittently HANGS during a test's browser session ("Visiting http://..."
# then no result). It ends in either the wasm-bindgen-test-runner's internal
# ~5-minute timeout:
#     Failed to detect test as having been run. It might have timed out.
# or the renderer-side message timeout:
#     Timed out receiving message from renderer
# This is harness flake, not a code failure, and has hit unrelated tests on
# back-to-back CI runs.
#
# This wrapper runs `cargo test --target wasm32-unknown-unknown "$@"` (so it
# works for both `--lib` and `--test <name>`) with a per-attempt wall-clock
# timeout and a bounded retry that fires ONLY on the flake signature. A genuine
# assertion failure (a clean `test result: FAILED`, or any non-zero exit with no
# flake marker) FAILS FAST on the first attempt and is never masked.
#
# Usage (from the dioxus-ui working directory):
#     ../scripts/ci/retry-wasm-component-test.sh --lib
#     ../scripts/ci/retry-wasm-component-test.sh --test device_selector
#
# Env overrides:
#     WASM_TEST_TIMEOUT_SECS   per-attempt timeout in seconds (default 150)
#     WASM_TEST_ATTEMPTS       max attempts                   (default 3)
#     CARGO                    cargo binary to invoke         (default "cargo")
#
# This script does NOT change the working directory: cargo runs in whatever
# directory the caller invoked it from (the workflow sets
# working-directory: dioxus-ui-repo/dioxus-ui).
#
# Exit-code contract for coreutils `timeout` (GNU coreutils, verified against
# `timeout --help` / `info coreutils 'timeout invocation'`):
#     124  COMMAND timed out (TERM sent, --preserve-status not set)
#     137  COMMAND was sent KILL(9) after the -k grace period (128 + 9)
# Both are treated as a timeout/flake here. We use `timeout -k 10 <secs>`:
# SIGTERM at <secs>, then SIGKILL 10s later if the process is still alive.

set -euo pipefail

CARGO="${CARGO:-cargo}"
TIMEOUT_SECS="${WASM_TEST_TIMEOUT_SECS:-150}"
MAX_ATTEMPTS="${WASM_TEST_ATTEMPTS:-3}"
KILL_GRACE_SECS=10

if [[ $# -eq 0 ]]; then
  echo "::error::retry-wasm-component-test.sh: no cargo test arguments given (expected e.g. --lib or --test <name>)" >&2
  exit 2
fi

# A human-readable label for log grouping, derived from the args
# (e.g. "--test device_selector" or "--lib").
TEST_LABEL="$*"

# Temp file used to capture each attempt's output for marker inspection while it
# is also streamed live to the CI log. Cleaned up on exit.
ATTEMPT_LOG="$(mktemp -t wasm-retry-XXXXXX.log)"
trap 'rm -f "${ATTEMPT_LOG}"' EXIT

# Returns 0 (true) if the given output + rc match the known harness-flake
# signature and the attempt should be retried; returns 1 (false) otherwise.
#
# IMPORTANT: this is the load-bearing flake-vs-real-failure gate. A clean
# assertion failure must NOT match here, so we key only on the timeout exit
# codes and the two specific harness-timeout log markers.
is_flake() {
  local rc="$1"
  local output="$2"

  # Per-attempt wall-clock timeout fired (124 = SIGTERM path, 137 = SIGKILL
  # after the -k grace period). Either way the harness hung.
  if [[ "$rc" -eq 124 || "$rc" -eq 137 ]]; then
    return 0
  fi

  # Harness reported its own internal timeout in the captured output. These
  # markers are emitted by wasm-bindgen-test-runner / the renderer driver and
  # are distinct from any test assertion text.
  if grep -qF 'Failed to detect test as having been run' <<<"$output"; then
    return 0
  fi
  if grep -qF 'Timed out receiving message from renderer' <<<"$output"; then
    return 0
  fi

  return 1
}

# Kill any lingering headless-Chrome / chromedriver processes so renderer state
# does not accumulate across attempts (folds in the per-step cleanup the
# workflow previously did inline). Always succeeds.
cleanup_browser() {
  pkill -f chromedriver || true
  pkill -f chrome || true
  sleep 2
}

attempt=1
while [[ "$attempt" -le "$MAX_ATTEMPTS" ]]; do
  echo "::group::wasm component test ${TEST_LABEL} (attempt ${attempt}/${MAX_ATTEMPTS}, per-attempt timeout ${TIMEOUT_SECS}s)"

  # Run the test under a per-attempt wall-clock timeout. Stream cargo's combined
  # stdout+stderr to the CI log in real time (via tee to this script's stdout)
  # AND capture it to a file so we can inspect it for the harness-timeout markers
  # after the attempt finishes. Capturing to a file (rather than `tee /dev/stderr`
  # inside a command substitution) keeps the live output and our log annotations
  # from interleaving nondeterministically.
  #
  # PIPESTATUS handling: `set -euo pipefail` is in effect, so a non-zero rc from
  # the timeout/cargo side of the pipe would otherwise abort the script before we
  # can inspect it. We therefore disable errexit around the pipeline (`set +e`),
  # read the LEFT side of the pipe (timeout+cargo) via ${PIPESTATUS[0]} rather
  # than tee's rc, then re-enable errexit immediately afterward.
  : >"${ATTEMPT_LOG}"
  set +e
  timeout -k "${KILL_GRACE_SECS}" "${TIMEOUT_SECS}" \
    "${CARGO}" test --target wasm32-unknown-unknown "$@" 2>&1 | tee "${ATTEMPT_LOG}"
  rc="${PIPESTATUS[0]}"
  set -e
  output="$(cat "${ATTEMPT_LOG}")"

  echo "::endgroup::"

  if [[ "$rc" -eq 0 ]]; then
    cleanup_browser
    if [[ "$attempt" -gt 1 ]]; then
      echo "::notice::wasm component test ${TEST_LABEL} recovered on attempt ${attempt}/${MAX_ATTEMPTS} (earlier attempt(s) were harness flake)."
    fi
    exit 0
  fi

  if is_flake "$rc" "$output"; then
    # Harness flake. Clean up and retry (if attempts remain).
    cleanup_browser
    if [[ "$attempt" -lt "$MAX_ATTEMPTS" ]]; then
      echo "::warning::wasm component test ${TEST_LABEL} hit the headless-Chrome harness flake on attempt ${attempt}/${MAX_ATTEMPTS} (rc=${rc}); retrying."
      attempt=$((attempt + 1))
      continue
    fi
    echo "::error::wasm component test ${TEST_LABEL} kept hitting the harness flake through all ${MAX_ATTEMPTS} attempts (last rc=${rc}); failing the step."
    exit "$rc"
  fi

  # Not a flake: a genuine test/build failure. FAIL FAST, no retry, do not
  # mask the real exit code. (This is the requirement that a real failure must
  # still fail the CI step on the first attempt.)
  echo "::error::wasm component test ${TEST_LABEL} FAILED (rc=${rc}) — genuine failure, not the harness flake. Failing immediately without retry."
  exit "$rc"
done

# Unreachable in practice (the loop exits via the branches above), but guard
# against MAX_ATTEMPTS <= 0 being passed in.
echo "::error::retry-wasm-component-test.sh: exhausted attempts for ${TEST_LABEL} without a conclusive result (WASM_TEST_ATTEMPTS=${MAX_ATTEMPTS})." >&2
exit 1
