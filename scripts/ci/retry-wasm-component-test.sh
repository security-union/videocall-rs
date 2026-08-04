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
# Each attempt runs in its OWN POSIX session (`setsid`), and the browser cleanup
# kills exactly that session's members — never a process this script did not
# start. See the PROCESS OWNERSHIP MODEL block below for why (#2150).
#
# Each attempt's combined output goes to a FILE, never through a pipe, and is
# streamed to the CI log by a background `tail -f`. A pipe would be inherited by
# every process in the attempt's tree, so a single leaked browser could hold its
# write end open and hang the step long after `timeout` had exited (#2151). See
# the STDOUT PLUMBING block at the loop below.
#
# Exit-code contract for coreutils `timeout` (GNU coreutils, verified against
# `timeout --help` / `info coreutils 'timeout invocation'`):
#     124  COMMAND timed out (TERM sent, --preserve-status not set)
#     137  COMMAND was sent KILL(9) after the -k grace period (128 + 9)
# Both are treated as a timeout/flake here. We use `timeout -k 10 <secs>`:
# SIGTERM at <secs>, then SIGKILL 10s later if the process is still alive.
# `setsid -w` re-raises the child's status unchanged, so these codes survive the
# extra layer (verified on util-linux 2.37.2, not assumed).

set -euo pipefail

CARGO="${CARGO:-cargo}"
TIMEOUT_SECS="${WASM_TEST_TIMEOUT_SECS:-150}"
MAX_ATTEMPTS="${WASM_TEST_ATTEMPTS:-3}"
KILL_GRACE_SECS=10
# How often the background log streamer re-checks whether the attempt is still
# alive (GNU tail --sleep-interval). This also bounds how long the streamer
# lingers after the attempt exits, so keep it small: it is added latency on
# every attempt, healthy ones included.
STREAM_POLL_SECS=0.2
# Hard bound on the post-attempt drain: 50 fractional-sleep attempts, nominally
# 5s but potentially longer under scheduler or process-launch contention. Only
# reached if the streamer does NOT exit on its own.
STREAM_DRAIN_TICKS=50

if [[ $# -eq 0 ]]; then
  echo "::error::retry-wasm-component-test.sh: no cargo test arguments given (expected e.g. --lib or --test <name>)" >&2
  exit 2
fi

# A human-readable label for log grouping, derived from the args
# (e.g. "--test device_selector" or "--lib").
TEST_LABEL="$*"

# ---------------------------------------------------------------------------
# PROCESS OWNERSHIP MODEL (#2150 — replaces the #1271 pgrep PID baseline)
# ---------------------------------------------------------------------------
# Every attempt is launched in its own POSIX SESSION (`setsid`). Cleanup then
# kills exactly the members of that session id (SID) and nothing else. Ownership
# is asserted POSITIVELY by the kernel, not inferred from a name match plus a
# PID diff.
#
# Why the previous approach had to go. It ran `pgrep -f 'chrome'` — and
# `pgrep -f` matches the whole COMMAND LINE, not the executable. When PR #2122
# added a `wasm-pack test --headless --chrome` step to pr-check-rust-hcl.yaml,
# the literal word "chrome" in that step's ARGV started matching. Both workflows
# run on the same physical host (`videocallci`, runners videocallci-2/-3) in one
# PID namespace, and their concurrency groups are per-workflow, so they overlap
# freely. The sweep SIGTERMed the sibling job's `wasm-pack` — observed as
# "Process completed with exit code 143" (128+SIGTERM) with a still-healthy
# browser left to reap (job 1754466, PR #2144).
#
# Narrowing the pattern to an executable anchor would have closed that one
# symptom but not the bug class: the rust job's OWN chromedriver/chromium are
# started AFTER our baseline snapshot, so they are post-baseline from our point
# of view and a narrowed pattern would still have killed them. The defect is not
# the pattern — it is that the script asserted ownership of processes it never
# started.
#
# Why SESSION and not process group or a descendant walk. The kernel semantics
# below were measured on Linux (procps-ng 3.3.17 / util-linux 2.37.2) before
# choosing, not reasoned about:
#   * A PROCESS-GROUP kill misses the browser. #1271 observed that the browser
#     ends up outside our process group; a setpgid(0,0) in the launch path is
#     the usual cause. MEASURED: a child that calls setpgid(0,0) does leave our
#     PGID, so a PGID kill cannot reach it.
#   * A DESCENDANT (ppid) walk misses leaked browsers, which is the entire point
#     of the cleanup: when the parent dies the browser is reparented to PID 1
#     and drops out of the tree.
#   * The SID survives BOTH. MEASURED: a setpgid(0,0) child keeps the inherited
#     SID, and an orphan reparented to PID 1 keeps it too — reparenting changes
#     PPID, never the session. So one `sess ==` test reaches every process this
#     script is responsible for, including the orphans, and by construction
#     reaches nothing else: another job's processes live in another job's
#     session, which we never create and can never name.
#
# What is GUARANTEED vs ASSUMED, stated precisely because the whole point of
# this rewrite is not to overclaim again:
#   GUARANTEED (kernel): cleanup_browser can only signal PIDs whose session
#     equals a SID that `setsid` minted for us. A process in another job's
#     session is unreachable by that cleanup regardless of how its argv is
#     spelled. stop_stream separately signals only STREAM_PID, the background
#     child this script launched and must reap.
#   ASSUMED (not verified against Chromium/chromedriver source): that nothing in
#     the cargo -> wasm-bindgen-test-runner -> chromedriver -> Chromium chain
#     calls setsid() and detaches itself into a session of its own. If one ever
#     did, we would LEAK that process, never kill someone else's — the failure
#     direction is the safe one, but the process would require manual host
#     remediation because the janitor only removes old profile directories.
SCRIPT_SID="$(ps -o sess= -p $$ 2>/dev/null | tr -d '[:space:]' || true)"

# Functional preflight, not a `command -v` guess: this runs the exact mechanism
# the cleanup depends on and checks the exact contract it needs — that `setsid`
# exists, that `-w` (util-linux >= 2.24) waits, and that it propagates the
# child's exit status verbatim. That last property is load-bearing twice over:
# is_flake() keys on timeout's rc 124/137, so a setsid that swallowed the status
# would silently disable the flake retry.
SESSION_CLEANUP_ENABLED=0
if [[ -n "${SCRIPT_SID}" ]]; then
  set +e
  setsid -w bash -c 'exit 77' >/dev/null 2>&1
  _preflight_rc=$?
  set -e
  if [[ "${_preflight_rc}" -eq 77 ]]; then
    SESSION_CLEANUP_ENABLED=1
  fi
  unset _preflight_rc
fi
if [[ "${SESSION_CLEANUP_ENABLED}" -ne 1 ]]; then
  # Fail SAFE, not silent: run the tests but sweep nothing. Leaking a browser is
  # recoverable through host remediation; killing another job's process is not.
  # The janitor workflow removes old profile directories, not live processes, so
  # never imply that it bounds this degraded path. Never fall back to a name match.
  echo "::warning::retry-wasm-component-test.sh: session-scoped browser cleanup is DISABLED (setsid -w unavailable or 'ps -o sess=' unsupported on this host). Tests will still run, but leaked Chrome/chromedriver processes will NOT be reaped by this script. This is a runner-provisioning regression — see #2150."
fi

# Set per attempt to the SID of that attempt's session; "" when we have no
# verified session to sweep.
ATTEMPT_SID=""

# PID of the current attempt's background log streamer; "" when none is running.
# stop_stream() reports its outcome: the streamer's exit status, and whether WE
# stopped it (1) or it exited on its own (0).
STREAM_PID=""
STREAM_RC=0
STREAM_KILLED=0

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
  #
  # MARKER-COLLISION INVARIANT (#1271): these two literal strings must remain
  # test-content-free — no test (its name, its assertion output, or a string it
  # prints/asserts on) may ever contain them verbatim. If one did, a GENUINE
  # failure whose output happened to include the literal would be misclassified
  # as flake and retried. That is bounded waste, not masking (the final
  # `exit "$rc"` below still propagates the real code after the retry budget is
  # spent), but it would slow CI and muddy triage. Keep these markers unique to
  # the harness.
  if grep -qF 'Failed to detect test as having been run' <<<"$output"; then
    return 0
  fi
  if grep -qF 'Timed out receiving message from renderer' <<<"$output"; then
    return 0
  fi

  return 1
}

# Stop the current attempt's live log streamer and reap it. Sets STREAM_RC (the
# streamer's exit status) and STREAM_KILLED (1 if WE stopped it, 0 if it exited
# on its own). Always succeeds; safe to call when no streamer is running.
#
# BOUNDED ON EVERY PATH, deliberately. `tail --pid` normally exits by itself
# within STREAM_POLL_SECS of the attempt being reaped, after a final flush — but
# an unbounded wait on it would re-introduce exactly the class of hang this
# script exists to avoid (#2151), one process further out. So the wait is capped
# and the streamer is killed outright if it overstays.
# The blocking wait also guarantees all streamed output lands before the caller
# closes the GitHub Actions log group.
#
# ONE UNBOUNDED WAIT REMAINS, stated rather than hidden: the final `wait` that
# reaps the streamer after the SIGTERM has no cap. It is the same shape this fix
# set out to remove, and it is left because `tail` does not trap or ignore
# SIGTERM — a streamer that survived one would have to be a `tail` that is not
# `tail`. If that assumption ever needs to hold harder, escalate to SIGKILL here
# rather than waiting longer.
stop_stream() {
  local drain=0

  STREAM_RC=0
  STREAM_KILLED=0
  [[ -n "${STREAM_PID}" ]] || return 0

  while kill -0 "${STREAM_PID}" 2>/dev/null && [[ "${drain}" -lt "${STREAM_DRAIN_TICKS}" ]]; do
    sleep 0.1 2>/dev/null || true
    drain=$((drain + 1))
  done
  if kill -0 "${STREAM_PID}" 2>/dev/null; then
    kill "${STREAM_PID}" 2>/dev/null || true
    STREAM_KILLED=1
  fi
  # `|| STREAM_RC=$?` rather than a bare `wait`: this function also runs from the
  # EXIT trap, where errexit is still in effect and a non-zero wait would abort
  # the trap before cleanup_browser ever ran.
  wait "${STREAM_PID}" 2>/dev/null || STREAM_RC=$?
  STREAM_PID=""
  return 0
}

# Reap everything still alive in the CURRENT attempt's session — the test
# runner, chromedriver, the browser and its renderers, including any that were
# orphaned onto PID 1 — and, by construction, nothing outside it. Always
# succeeds; never inspects a process name.
#
# No name matching happens here on purpose. There is no pattern to get wrong,
# so a sibling job cannot be caught by one however its argv is spelled.
cleanup_browser() {
  local sid="${ATTEMPT_SID}"
  local pids

  [[ "${SESSION_CLEANUP_ENABLED}" -eq 1 ]] || return 0

  # Fall back to the on-disk SID when the variable is unset. This is the path
  # taken if we are torn down between launching an attempt and reading its SID
  # back — `cancel-in-progress: true` on this workflow makes that a routine
  # event, not a hypothetical. The file is truncated whenever a sweep drains the
  # session, so this can only ever resurrect a SID that still has members.
  if [[ -z "${sid}" && -r "${ATTEMPT_SID_FILE}" ]]; then
    sid="$(tr -d '[:space:]' <"${ATTEMPT_SID_FILE}" 2>/dev/null || true)"
  fi
  # Digits only: anything else means we failed to read a real SID.
  [[ "${sid}" =~ ^[0-9]+$ ]] || return 0
  # SID 0 is the kernel-thread sentinel; SID 1 is init's session (every service
  # on the host). Neither is ever a session we created.
  [[ "${sid}" != "0" && "${sid}" != "1" ]] || return 0
  # THE INTERLOCK. If the attempt somehow landed in OUR session, sweeping it
  # would kill this script, the GitHub Actions runner, and every sibling step on
  # this runner. Refuse, loudly. This is the one failure that must never be
  # silent.
  if [[ "${sid}" == "${SCRIPT_SID}" ]]; then
    echo "::warning::retry-wasm-component-test.sh: refusing to sweep session ${sid} — it is this script's OWN session, so setsid did not isolate the attempt. Skipping cleanup (see #2150)."
    return 0
  fi

  # Live members of the session. Zombies are excluded: they are already dead,
  # cannot be killed again, and would otherwise make the post-sweep re-scan look
  # non-empty forever — costing a pointless second sweep and 3s per step.
  session_pids() {
    ps -eo pid=,sess=,stat= 2>/dev/null \
      | awk -v s="$1" '$2 == s && $3 !~ /^Z/ {print $1}' || true
  }

  # SIGTERM every session member in ONE kill, rather than walking the list.
  # Sequential kills let chromedriver notice the browser died and respawn it
  # before its own turn comes; a single signal delivery closes that window.
  pids="$(session_pids "${sid}")"
  if [[ -z "${pids}" ]]; then
    ATTEMPT_SID=""
    : >"${ATTEMPT_SID_FILE}" 2>/dev/null || true
    return 0
  fi
  # shellcheck disable=SC2086 # deliberate word splitting: one kill, many PIDs
  kill -TERM ${pids} 2>/dev/null || true

  # Same 2s settle the previous implementation used, but now only paid when
  # there was actually something to kill (12 steps x 2s of unconditional sleep
  # was pure latency on the healthy path).
  sleep 2

  # Escalate to SIGKILL for anything that ignored SIGTERM — a wedged renderer
  # is exactly the case that leaked before, and TERM alone never removed it.
  pids="$(session_pids "${sid}")"
  if [[ -n "${pids}" ]]; then
    # shellcheck disable=SC2086 # deliberate word splitting: one kill, many PIDs
    kill -KILL ${pids} 2>/dev/null || true
    sleep 1
    pids="$(session_pids "${sid}")"
  fi

  # Drop the SID once the session is drained so neither the EXIT trap nor the
  # file fallback can re-sweep a number the kernel may since have recycled onto
  # an unrelated session.
  if [[ -z "${pids}" ]]; then
    ATTEMPT_SID=""
    : >"${ATTEMPT_SID_FILE}" 2>/dev/null || true
  fi
  return 0
}

cleanup_temp_files() {
  [[ -z "${ATTEMPT_LOG}" ]] || rm -f "${ATTEMPT_LOG}"
  [[ -z "${ATTEMPT_SID_FILE}" ]] || rm -f "${ATTEMPT_SID_FILE}"
}

# Temp files: one is the attempt's real stdout+stderr AND the source the live
# stream reads from AND what is inspected for the flake markers afterwards; the
# other carries the attempt's SID back out of the session it creates. Cleaned up
# on exit.
#
# These variables and the trap sit HERE, below every function the trap calls,
# rather than up with the other globals. A trap installed above the definitions
# can fire while its handlers are still unknown commands: under errexit the
# first 127 aborts the handler, and everything after it — the sweep and file
# cleanup — silently does not run. Install the trap before either mktemp so a
# failure or cancellation between them still removes the file already created.
ATTEMPT_LOG=""
ATTEMPT_SID_FILE=""
# The EXIT sweep is not belt-and-braces: the genuine-failure path below exits
# WITHOUT calling cleanup_browser, so before this trap a real test failure left
# its browser running. That is one of the ways orphans accumulated on the host.
#
# stop_stream runs FIRST. The streamer writes to this script's stdout, which IS
# the step's stdout, so a streamer outliving the script would hold the step open
# — the very failure mode this plumbing exists to prevent. On normal paths it
# has already been reaped and this is a no-op. On cancellation, skip the normal
# drain so browser cleanup starts immediately; stop_stream still kills and waits
# for the owned streamer.
trap 'STREAM_DRAIN_TICKS=0; stop_stream; cleanup_browser; cleanup_temp_files' EXIT
ATTEMPT_LOG="$(mktemp -t wasm-retry-XXXXXX.log)"
ATTEMPT_SID_FILE="$(mktemp -t wasm-retry-sid-XXXXXX)"

attempt=1
while [[ "$attempt" -le "$MAX_ATTEMPTS" ]]; do
  echo "::group::wasm component test ${TEST_LABEL} (attempt ${attempt}/${MAX_ATTEMPTS}, per-attempt timeout ${TIMEOUT_SECS}s)"

  # -------------------------------------------------------------------------
  # STDOUT PLUMBING (#2151 — replaces `... 2>&1 | tee "${ATTEMPT_LOG}"`)
  # -------------------------------------------------------------------------
  # The attempt's combined stdout+stderr goes STRAIGHT TO A FILE, and a
  # background `tail -f` streams that file to the CI log so output stays live.
  # The same file is what gets inspected for the harness-timeout markers once
  # the attempt finishes.
  #
  # Why not `| tee`. Every process in the attempt's tree inherits the WRITE END
  # of the pipe into `tee`. A browser or chromedriver that leaks and is orphaned
  # onto PID 1 keeps that write end open, so `tee` never sees EOF and this shell
  # blocks on the pipeline — long after `timeout` has done its job and exited.
  # WASM_TEST_TIMEOUT_SECS then bounds cargo but NOT the step: a bounded 150s
  # attempt became an unbounded step that burned the job's timeout-minutes and
  # surfaced as a job timeout rather than as the leak it was. Seen in production
  # as the 5s gap between `Terminated` and `exit 143` in job 1754466, with a
  # surviving child still writing; reproduced in a container, where the same
  # topology wedged for 8+ minutes and completed instantly once the orphan's
  # stdio was detached.
  #
  # A regular file cannot be pinned that way. A leaked child still inherits the
  # attempt's stdout, but it is now a descriptor onto ${ATTEMPT_LOG}: this shell
  # waits for the attempt itself and for nothing else, so the step is bounded by
  # `timeout` again. Leaked writes after the attempt land in a log nobody reads
  # — and when session cleanup is enabled, cleanup_browser reaps the writer
  # moments later anyway.
  #
  # rc: with the pipeline gone there is no ${PIPESTATUS[0]} to read. `wait` on
  # the attempt returns the identical value — that array element WAS the
  # attempt's status — including timeout's 124 (SIGTERM path) and 137 (SIGKILL
  # after the -k grace). is_flake() keys on exactly those, so its timeout
  # detection is unchanged. `set +e` is still required, because `wait` returns
  # that non-zero status and errexit would otherwise abort before we inspect it.
  #
  # ONE SIDE EFFECT of backgrounding, recorded because it is invisible otherwise:
  # bash sets SIGINT and SIGQUIT to SIG_IGN for an asynchronous command, and that
  # disposition survives exec, so the whole attempt tree inherits it
  # (SigIgn 0000000000000006 where the old foreground pipeline had
  # 0000000000000000). Impact is low by construction — under `setsid` the attempt
  # is already in a session of its own, so a process-group SIGINT never reached
  # it either way, and on the unwrapped path a cancellation still arrives as the
  # runner's SIGTERM escalation, which is not ignored.
  #
  # SESSION ISOLATION (#2150): `setsid -w` runs the attempt as a session leader,
  # so cargo, wasm-bindgen-test-runner, chromedriver and the browser all inherit
  # a fresh SID that exists only for this attempt. The inner shell reports the
  # session it actually landed in (not an assumed $$) via ATTEMPT_SID_FILE, then
  # `exec`s the real command — exec keeps the PID, so the leader IS `timeout`.
  # `-w` waits and re-raises the child's status, which is what preserves
  # timeout's 124/137 for is_flake(). Arguments are passed positionally, never
  # interpolated into the -c string. When the preflight could not verify
  # `setsid -w`, LAUNCHER is empty and the attempt runs unwrapped; isolation is a
  # cleanup concern and must never become a reason the tests cannot run.
  LAUNCHER=()
  if [[ "${SESSION_CLEANUP_ENABLED}" -eq 1 ]]; then
    # shellcheck disable=SC2016 # single quotes REQUIRED: $$/$1/$@ must expand in
    # the inner session-leader shell, not here. Expanding them now would record
    # THIS script's pid and defeat the isolation.
    LAUNCHER=(setsid -w bash -c 'ps -o sess= -p $$ | tr -d "[:space:]" >"$1"; shift; exec "$@"'
              wasm-retry-session "${ATTEMPT_SID_FILE}")
  fi

  : >"${ATTEMPT_LOG}"
  : >"${ATTEMPT_SID_FILE}"
  ATTEMPT_SID=""
  set +e
  # `>>` and not `>`: the file was just truncated above, and appending keeps the
  # streamer's view monotonic — nothing can reset the offset under it.
  ${LAUNCHER[@]+"${LAUNCHER[@]}"} \
    timeout -k "${KILL_GRACE_SECS}" "${TIMEOUT_SECS}" \
    "${CARGO}" test --target wasm32-unknown-unknown "$@" >>"${ATTEMPT_LOG}" 2>&1 &
  attempt_pid=$!

  # Live stream. `-n +1` starts at byte 0, so nothing written between the launch
  # above and the streamer's start can be missed. `--pid` makes it exit — after a
  # final flush of whatever the attempt wrote last — once the attempt is gone,
  # and `-s` bounds how quickly it notices. Both contracts were exercised on GNU
  # coreutils 8.32 rather than taken from the man page.
  tail -n +1 -f -s "${STREAM_POLL_SECS}" --pid="${attempt_pid}" "${ATTEMPT_LOG}" &
  STREAM_PID=$!

  wait "${attempt_pid}"
  rc=$?
  stop_stream
  set -e

  # If the streamer died on its OWN with an error — no `tail` on PATH, or a
  # build that rejects --pid/-s — the attempt's output would otherwise be
  # silently missing from the CI log, the worst outcome for whoever triages the
  # failure. Print it once, batched. A streamer WE stopped is handled separately
  # below because its live output may be incomplete even though the file used
  # for classification remains intact.
  if [[ "${STREAM_KILLED}" -eq 0 && "${STREAM_RC}" -ne 0 ]]; then
    echo "::warning::retry-wasm-component-test.sh: live log streaming failed (streamer rc=${STREAM_RC}); printing the attempt log in full instead."
    cat "${ATTEMPT_LOG}" || true
  fi
  # Having to kill the streamer means it did NOT honour --pid. The attempt log
  # remains complete for classification, but live CI output may be incomplete if
  # the streamer itself blocked. The full drain bound is also a silent tax:
  # roughly 5s plus host scheduling overhead, up to 12 steps x 3 attempts against
  # a 20-minute job cap.
  if [[ "${STREAM_KILLED}" -eq 1 ]]; then
    echo "::warning::retry-wasm-component-test.sh: the log streamer did not exit on its own after the attempt finished and had to be stopped; this costs roughly $((STREAM_DRAIN_TICKS / 10))s plus scheduling overhead per attempt. The attempt log used for classification is intact, but live CI output may be incomplete if the streamer blocked — check whether this host's \`tail\` honours --pid."
  fi

  # Read back the session the attempt actually ran in. Empty (killed before the
  # inner shell got that far, or session cleanup disabled) means cleanup_browser
  # sweeps nothing — the safe direction.
  ATTEMPT_SID="$(tr -d '[:space:]' <"${ATTEMPT_SID_FILE}" 2>/dev/null || true)"
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
