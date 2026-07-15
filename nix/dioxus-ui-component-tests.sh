# Dioxus UI wasm component tests, run against the nix-pinned headless
# Chrome + chromedriver (see browserTestInputs in flake.nix). Invoke via:
#
#   nix develop .#frontend-tests --command dioxus-ui-component-tests
#
# Packaged with writeShellApplication, so shellcheck gates the build and
# `set -euo pipefail` is prepended automatically.
#
# Headless Chrome intermittently wedges its renderer at webdriver session
# startup (chromedriver's 300s "Timed out receiving message from
# renderer"), before any test code runs. Each test binary spawns its own
# chromedriver+Chrome, so retry per binary rather than per suite, and (in
# CI) kill leftover Chrome processes between attempts so a wedged renderer
# can't starve later startups.

if [ ! -f flake.nix ] || [ ! -d dioxus-ui ]; then
  echo "error: must be run from the videocall-rs repo root" >&2
  exit 1
fi

CHROMEDRIVER="${CHROMEDRIVER:-$(command -v chromedriver)}"
export CHROMEDRIVER
chrome="$(command -v google-chrome)"

case "${chrome}" in
  /nix/store/*) ;;
  *)
    echo "error: google-chrome is not the nix-pinned one: ${chrome}" >&2
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

chrome_version="$("${chrome}" --version)"
driver_version="$("${CHROMEDRIVER}" --version)"
echo "google-chrome: ${chrome} (${chrome_version})"
echo "chromedriver:  ${CHROMEDRIVER} (${driver_version})"
chrome_major="$(grep -oE '[0-9]+' <<<"${chrome_version}" | head -1)"
driver_major="$(grep -oE '[0-9]+' <<<"${driver_version}" | head -1)"
if [ "${chrome_major}" != "${driver_major}" ]; then
  echo "error: Chrome ${chrome_major} / chromedriver ${driver_major} major version mismatch" >&2
  exit 1
fi

cd dioxus-ui

# Pin the browser binary explicitly in the capabilities so chromedriver
# launches exactly the nix Chrome, independent of its binary-discovery
# heuristics. The checked-in webdriver.json is restored on exit so local
# runs don't leave the working tree dirty.
webdriver_json="${PWD}/webdriver.json"
webdriver_backup="$(mktemp)"
cp "${webdriver_json}" "${webdriver_backup}"
trap 'cp "${webdriver_backup}" "${webdriver_json}"; rm -f "${webdriver_backup}"' EXIT
jq --arg bin "${chrome}" '."goog:chromeOptions".binary = $bin' \
  "${webdriver_backup}" > "${webdriver_json}"

reap_stray_chrome() {
  # Only in CI: on a dev machine this would kill the developer's browser.
  if [ "${GITHUB_ACTIONS:-}" = "true" ]; then
    pkill -9 -f 'chromedriver|chrome|chromium' || true
    sleep 2
  fi
}

run_target() {
  local desc="$1"
  shift
  local max_attempts=3
  local attempt
  for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    echo "::group::dioxus-ui ${desc} (attempt ${attempt}/${max_attempts})"
    if cargo test --target wasm32-unknown-unknown "$@"; then
      echo "::endgroup::"
      echo "dioxus-ui ${desc} passed on attempt ${attempt}"
      return 0
    fi
    echo "::endgroup::"
    reap_stray_chrome
    if [ "${attempt}" -ge "${max_attempts}" ]; then
      echo "::error::dioxus-ui ${desc} failed after ${max_attempts} attempts"
      return 1
    fi
    echo "::warning::dioxus-ui ${desc} failed on attempt ${attempt}; retrying (headless-Chrome renderer flake)"
  done
}

failed_targets=()
run_target "unit tests" --lib --bins || failed_targets+=("unit-tests")
shopt -s nullglob
for test_file in tests/*.rs; do
  test_name="$(basename "${test_file}" .rs)"
  run_target "integration test ${test_name}" --test "${test_name}" || failed_targets+=("${test_name}")
done

if [ "${#failed_targets[@]}" -gt 0 ]; then
  echo "::error::dioxus-ui targets failed: ${failed_targets[*]}"
  exit 1
fi
echo "all dioxus-ui component test targets passed"
