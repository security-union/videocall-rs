# Dioxus UI wasm component tests, run against the nix-pinned headless
# Chrome + chromedriver (see browserTestInputs in nix/shells.nix). Invoke via:
#
#   nix-shell default.nix -A shells.frontend-tests --run dioxus-ui-component-tests
#
# Packaged with writeShellApplication, so shellcheck gates the build and
# `set -euo pipefail` is prepended automatically (also set here so the raw
# script behaves the same when run directly with bash).
#
# Headless Chrome intermittently wedges its renderer at webdriver session
# startup (chromedriver's 300s "Timed out receiving message from
# renderer"), before any test code runs. The 300s wait is necessary — do
# NOT cap the pageLoad timeout. Instead, wall-clock relief comes from
# running test binaries in PARALLEL so a wedged session overlaps useful
# work in other sessions, plus a per-binary retry.
#
# Parallelism model:
#   - `cargo test --no-run` prebuilds every test executable once (cargo
#     parallelizes the compile), then the executables are run directly via
#     wasm-bindgen-test-runner. Running through `cargo test` per target
#     would serialize on cargo's target-dir lock.
#   - DIOXUS_UI_TEST_JOBS concurrent runner sessions (default 3). Each
#     session spawns its own chromedriver+Chrome on ephemeral ports.
#   - Each attempt gets a private TMPDIR; a wedged session's Chrome carries
#     that path in its --user-data-dir, so cleanup can pkill exactly that
#     session's processes without touching sibling sessions (the old global
#     CI reap would have killed them).
#
# macOS: the nix-pinned Chrome is Linux-only, so local runs use the system
# browser via the escape hatch (refused in CI):
#   DIOXUS_UI_TESTS_SYSTEM_BROWSER=1 \
#   CHROMEDRIVER=/path/to/matching/chromedriver \
#     nix-shell default.nix -A shells.frontend-tests --run dioxus-ui-component-tests
# Chrome/chromedriver major versions must still match; grab a matching
# driver from https://googlechromelabs.github.io/chrome-for-testing/.

set -euo pipefail

if [ ! -f default.nix ] || [ ! -d dioxus-ui ]; then
  echo "error: must be run from the videocall-rs repo root" >&2
  exit 1
fi

system_browser="${DIOXUS_UI_TESTS_SYSTEM_BROWSER:-0}"
if [ "${system_browser}" = "1" ] && [ "${GITHUB_ACTIONS:-}" = "true" ]; then
  echo "error: DIOXUS_UI_TESTS_SYSTEM_BROWSER is a local-only escape hatch; CI must use the nix-pinned browser" >&2
  exit 1
fi

if [ "${system_browser}" = "1" ]; then
  chrome="${CHROME_BIN:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"
  if [ ! -x "${chrome}" ]; then
    echo "error: system browser not found at '${chrome}' (set CHROME_BIN)" >&2
    exit 1
  fi
  CHROMEDRIVER="${CHROMEDRIVER:-$(command -v chromedriver)}"
else
  chrome="$(command -v google-chrome)"
  CHROMEDRIVER="${CHROMEDRIVER:-$(command -v chromedriver)}"
  case "${chrome}" in
    /nix/store/*) ;;
    *)
      echo "error: google-chrome is not the nix-pinned one: ${chrome}" >&2
      echo "hint: on macOS use DIOXUS_UI_TESTS_SYSTEM_BROWSER=1 with a matching chromedriver" >&2
      exit 1
      ;;
  esac
  case "${CHROMEDRIVER}" in
    /nix/store/*) ;;
    *)
      echo "error: chromedriver is not the nix-pinned one: ${CHROMEDRIVER}" >&2
      exit 1
      ;;
  esac
fi
export CHROMEDRIVER

chrome_version="$("${chrome}" --version)"
driver_version="$("${CHROMEDRIVER}" --version)"
echo "chrome:       ${chrome} (${chrome_version})"
echo "chromedriver: ${CHROMEDRIVER} (${driver_version})"
chrome_major="$(grep -oE '[0-9]+' <<<"${chrome_version}" | head -1)"
driver_major="$(grep -oE '[0-9]+' <<<"${driver_version}" | head -1)"
if [ "${chrome_major}" != "${driver_major}" ]; then
  echo "error: Chrome ${chrome_major} / chromedriver ${driver_major} major version mismatch" >&2
  exit 1
fi

test_jobs="${DIOXUS_UI_TEST_JOBS:-3}"
echo "running up to ${test_jobs} browser sessions in parallel (DIOXUS_UI_TEST_JOBS)"

cd dioxus-ui

# Pin the browser binary explicitly in the capabilities so chromedriver
# launches exactly the chosen Chrome, independent of its binary-discovery
# heuristics. The checked-in webdriver.json is restored on exit so local
# runs don't leave the working tree dirty. All parallel sessions read the
# same file; it is only written here, before any session starts.
webdriver_json="${PWD}/webdriver.json"
webdriver_backup="$(mktemp)"
cp "${webdriver_json}" "${webdriver_backup}"
trap 'cp "${webdriver_backup}" "${webdriver_json}"; rm -f "${webdriver_backup}"' EXIT
jq --arg bin "${chrome}" '."goog:chromeOptions".binary = $bin' \
  "${webdriver_backup}" > "${webdriver_json}"

# Prebuild every test executable in one cargo invocation (compile uses all
# cores), then harvest the artifact paths from a second, fully-fresh
# --message-format=json pass.
echo "::group::dioxus-ui compile test executables"
cargo test --target wasm32-unknown-unknown --no-run --lib --bins --tests
echo "::endgroup::"

# Lines of "<desc>\t<executable>". Kind is folded into desc so a lib and a
# bin with the same target name don't collide.
mapfile -t targets < <(
  cargo test --target wasm32-unknown-unknown --no-run --lib --bins --tests \
    --message-format=json 2>/dev/null \
    | jq -r 'select(.reason == "compiler-artifact" and .executable != null
                    and .profile.test == true)
             | [(.target.kind | join("-")) + ":" + .target.name, .executable]
             | @tsv' \
    | sort -u
)
if [ "${#targets[@]}" -eq 0 ]; then
  echo "::error::no test executables found" >&2
  exit 1
fi
echo "test executables (${#targets[@]}):"
printf '  %s\n' "${targets[@]%%$'\t'*}"

logdir="$(mktemp -d)"

# One test executable, up to 3 attempts. Runs as a background job; writes
# $logdir/<desc>.log (full output) and touches $logdir/<desc>.ok on pass.
# Each attempt gets a private TMPDIR so a wedged session's stray
# Chrome/chromedriver processes can be killed by matching that unique path
# in their command lines — precise enough to be safe alongside sibling
# sessions and on developer machines.
run_target_job() {
  local desc="$1" exe="$2"
  local slug="${desc//[^A-Za-z0-9_-]/_}"
  local log="${logdir}/${slug}.log"
  local max_attempts=3 attempt attempt_tmp
  for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    attempt_tmp="$(mktemp -d)"
    echo "[${desc}] attempt ${attempt}/${max_attempts} started"
    if TMPDIR="${attempt_tmp}" wasm-bindgen-test-runner "${exe}" \
      >>"${log}" 2>&1; then
      rm -rf "${attempt_tmp}"
      echo "[${desc}] passed on attempt ${attempt}"
      touch "${logdir}/${slug}.ok"
      return 0
    fi
    # Reap exactly this attempt's browser processes (their user-data-dir
    # lives under the private TMPDIR), then retry.
    pkill -9 -f "${attempt_tmp}" 2>/dev/null || true
    sleep 1
    rm -rf "${attempt_tmp}"
    echo "[${desc}] failed on attempt ${attempt} (headless-Chrome renderer flake?)"
  done
  echo "[${desc}] FAILED after ${max_attempts} attempts"
  return 1
}

# Launch with bounded concurrency; failures are collected via the .ok
# marker files rather than job exit codes.
for entry in "${targets[@]}"; do
  desc="${entry%%$'\t'*}"
  exe="${entry#*$'\t'}"
  while [ "$(jobs -rp | wc -l)" -ge "${test_jobs}" ]; do
    wait -n || true
  done
  run_target_job "${desc}" "${exe}" &
done
wait || true

# Replay per-target logs sequentially so CI groups don't interleave.
failed_targets=()
for entry in "${targets[@]}"; do
  desc="${entry%%$'\t'*}"
  slug="${desc//[^A-Za-z0-9_-]/_}"
  if [ -e "${logdir}/${slug}.ok" ]; then
    echo "::group::dioxus-ui ${desc} (passed)"
  else
    echo "::group::dioxus-ui ${desc} (FAILED)"
    failed_targets+=("${desc}")
  fi
  cat "${logdir}/${slug}.log" 2>/dev/null || true
  echo "::endgroup::"
done
rm -rf "${logdir}"

if [ "${#failed_targets[@]}" -gt 0 ]; then
  echo "::error::dioxus-ui targets failed: ${failed_targets[*]}"
  exit 1
fi
echo "all ${#targets[@]} dioxus-ui component test targets passed"
