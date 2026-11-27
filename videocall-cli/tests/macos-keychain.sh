#!/usr/bin/env bash
# videocall-cli device key storage against the REAL macOS keychain.
#
#   macos-keychain.sh <scenario>
#
# CI only (.github/workflows/macos-cli-keychain.yaml): it swaps the default
# keychain, so never run it on a machine whose keychain you care about.
# Needs: CLI (path to videocall-cli), MOCK_PYTHON (python3 with `cryptography`)
# and the mock backend (nix/tests/mock-meeting-api.py) already listening on
# $PORT (the workflow starts it once). macOS has no `timeout`,
# so every CLI call runs under `perl -e 'alarm ...; exec'`.
#
# How the keychain is made to refuse: the temporary keychain that is the
# default is deleted. Every keychain call then fails at once with "A default
# keychain could not be found." (A locked keychain does not refuse, it blocks
# forever: see the `locked` scenario.) The file is copied first so the keychain
# can be restored with its older items.
#
# Scenarios: works | refuses-on-save | older-item-then-refuse | logout-refused | locked

set -uo pipefail

SCENARIO=${1:?scenario}
CLI=${CLI:?path to videocall-cli}
MOCK_PYTHON=${MOCK_PYTHON:-python3}
HERE=$(cd "$(dirname "$0")" && pwd)
PORT=${PORT:-18081}
API=http://127.0.0.1:$PORT
WORK=$(mktemp -d)
CFG=$WORK/config
KC=$WORK/ci.keychain-db
export VIDEOCALL_CONFIG_DIR=$CFG
export RUST_LOG=warn

tmo() { perl -e 'alarm shift; exec @ARGV' "$@"; }
say() { printf '\n== %s (%s)\n' "$*" "$(date +%T)"; }
die() { printf '\nFAIL: %s\n' "$*" >&2; [ -f "$WORK/login.out" ] && sed 's/^/  login.out: /' "$WORK/login.out" >&2; exit 1; }
check() { local what=$1; shift; "$@" || die "$what"; }
wait_for() { local n=$(($2 * 4)); while [ $n -gt 0 ]; do eval "$1" && return 0; sleep 0.25; n=$((n - 1)); done; return 1; }

# ---- mock backend (started once by the workflow, shared by all scenarios) ---
curl -sf --max-time 5 "$API/version" >/dev/null || die "no mock backend on $API"
mock_state() { curl -sf "$API/_mock/state" | "$MOCK_PYTHON" -c "import json,sys; print(json.load(sys.stdin)[sys.argv[1]])" "$1"; }
DELETES_BEFORE=$(mock_state deletes)
revokes() { echo $(($(mock_state deletes) - DELETES_BEFORE)); }

# ---- the keychain -----------------------------------------------------------
kc_item() { tmo 10 security find-generic-password -s videocall-cli -a "$API" >/dev/null 2>&1; }

kc_default() { # a fresh, unlocked temporary keychain as the only (default) one
  security create-keychain -p pw "$KC" &&
    security default-keychain -s "$KC" &&
    security list-keychains -d user -s "$KC" &&
    security set-keychain-settings -t 3600 "$KC" &&
    security unlock-keychain -p pw "$KC" || die "could not create the temporary keychain"
}

kc_refuse() { # keep a copy, then delete the default keychain
  cp "$KC" "$KC.bak" && security delete-keychain "$KC" || die "could not delete the keychain"
  if tmo 10 security default-keychain >/dev/null 2>&1; then die "a default keychain still exists"; fi
}

kc_restore() { # bring the deleted keychain back with whatever it held
  cp "$KC.bak" "$KC" &&
    security default-keychain -s "$KC" &&
    security list-keychains -d user -s "$KC" &&
    security unlock-keychain -p pw "$KC" || die "could not restore the keychain"
}

# ---- the CLI ----------------------------------------------------------------
cli() { tmo 30 "$CLI" "$@"; }
status() { STATUS_OUT=$(cli auth status --api-url "$API" 2>&1); STATUS_RC=$?; }
logout() { LOGOUT_OUT=$(cli auth logout --api-url "$API" 2>&1); LOGOUT_RC=$?; }
key_files() { ls "$CFG"/*.toml 2>/dev/null; }
mode_of() { stat -f '%Lp' "$1"; }

# login [seconds]: `auth login --remote`, approved by the "human" through the mock.
# Sets LOGIN_RC (142 = killed by the alarm, i.e. the CLI blocked) and LOGIN_FP.
login() {
  local limit=${1:-90}
  rm -f "$WORK"/login.in "$WORK"/login.out "$WORK"/login.rc
  mkfifo "$WORK/login.in"
  (
    exec 3<>"$WORK/login.in"
    tmo "$limit" "$CLI" auth login --remote --api-url "$API" --name "ci keychain" <&3 >"$WORK/login.out" 2>&1
    echo $? >"$WORK/login.rc"
  ) </dev/null >/dev/null 2>&1 &
  if ! wait_for "grep -q 'Paste the code' $WORK/login.out" 40; then
    # Blocked before the prompt (e.g. reading the keychain): wait for the alarm.
    wait_for "test -s $WORK/login.rc" "$limit"
    LOGIN_RC=$(cat "$WORK/login.rc" 2>/dev/null || echo "?"); LOGIN_FP=""; return
  fi
  LOGIN_FP=$(sed -n 's/^Key fingerprint[^:]*: //p' "$WORK/login.out")
  local url code
  url=$(grep -o "$API/cli/authorize?[^ ]*" "$WORK/login.out" | head -1)
  curl -sSf -o /dev/null "$url" || die "authorize URL rejected by the mock"
  code=$(curl -sSf -X POST "$API/_mock/approve" | "$MOCK_PYTHON" -c "import json,sys; print(json.load(sys.stdin)['code'])")
  echo "$code" >"$WORK/login.in"
  wait_for "test -s $WORK/login.rc" "$limit"
  LOGIN_RC=$(cat "$WORK/login.rc" 2>/dev/null || echo "?")
}

check_file_store() { # the key is in a private file, not in the keychain
  local file
  file=$(key_files)
  [ -n "$file" ] || die "no key file in $CFG"
  [ "$(mode_of "$CFG")" = 700 ] || die "config dir mode is $(mode_of "$CFG"), want 700"
  [ "$(mode_of "$file")" = 600 ] || die "key file mode is $(mode_of "$file"), want 600"
}

case "$SCENARIO" in

works)
  say "keychain works: login stores the device in the keychain"
  kc_default
  login
  [ "$LOGIN_RC" = 0 ] || die "login exited $LOGIN_RC"
  grep -q "OS keychain" "$WORK/login.out" || die "login did not report the keychain"
  check "the keychain has no videocall-cli item" kc_item
  [ -z "$(key_files)" ] || die "a key file exists next to the keychain item: $(key_files)"
  status
  [ "$STATUS_RC" = 0 ] || die "auth status exited $STATUS_RC: $STATUS_OUT"
  echo "$STATUS_OUT" | grep -q "Fingerprint: *$LOGIN_FP" || die "status shows another fingerprint: $STATUS_OUT"
  say "logout removes the keychain item"
  logout
  [ "$LOGOUT_RC" = 0 ] || die "logout exited $LOGOUT_RC: $LOGOUT_OUT"
  ! kc_item || die "the keychain item is still there"
  [ "$(revokes)" = 1 ] || die "the mock did not see exactly one revoke"
  ;;

refuses-on-save)
  say "keychain refuses on save: the key goes to a private file"
  kc_default; kc_refuse
  login
  [ "$LOGIN_RC" = 0 ] || die "login exited $LOGIN_RC"
  grep -q "storing the key in a file instead" "$WORK/login.out" || die "login did not warn about the fallback"
  check_file_store
  status
  [ "$STATUS_RC" = 0 ] || die "auth status exited $STATUS_RC: $STATUS_OUT"
  echo "$STATUS_OUT" | grep -q "$CFG" || die "status does not point at the key file: $STATUS_OUT"
  say "logout removes the file"
  logout
  echo "(logout exit code: $LOGOUT_RC, see logout-refused)"
  [ -z "$(key_files)" ] || die "the key file survived logout"
  ;;

older-item-then-refuse)
  say "an older keychain item, then the keychain refuses and a new login lands in the file"
  kc_default
  login
  [ "$LOGIN_RC" = 0 ] || die "first login exited $LOGIN_RC"
  check "the first login left no keychain item" kc_item
  FIRST_FP=$LOGIN_FP
  kc_refuse
  login
  [ "$LOGIN_RC" = 0 ] || die "second login exited $LOGIN_RC"
  check_file_store
  [ "$LOGIN_FP" != "$FIRST_FP" ] || die "both logins have the same fingerprint"
  say "status while the keychain refuses"
  status
  [ "$STATUS_RC" = 0 ] || die "auth status exited $STATUS_RC: $STATUS_OUT"
  echo "$STATUS_OUT" | grep -q "Fingerprint: *$LOGIN_FP" || die "status shows the wrong device: $STATUS_OUT"
  say "keychain back, older item readable again: the file still wins"
  kc_restore
  check "the older item is not back in the keychain" kc_item
  status
  [ "$STATUS_RC" = 0 ] || die "auth status exited $STATUS_RC: $STATUS_OUT"
  echo "$STATUS_OUT" | grep -q "Fingerprint: *$LOGIN_FP" || die "status read the older keychain item instead of the file: $STATUS_OUT"
  echo "$STATUS_OUT" | grep -q "Fingerprint: *$FIRST_FP" && die "status shows the older device"
  ;;

logout-refused)
  say "logout while the keychain refuses: file removed, keychain error reported"
  kc_default; kc_refuse
  login
  [ "$LOGIN_RC" = 0 ] || die "login exited $LOGIN_RC"
  check_file_store
  logout
  echo "logout exit code: $LOGOUT_RC"
  echo "logout output: $LOGOUT_OUT"
  [ -z "$(key_files)" ] || die "the key file survived logout"
  [ "$(revokes)" = 1 ] || die "the mock did not see exactly one revoke"
  [ "$LOGOUT_RC" != 0 ] || die "logout reported success although the keychain refused"
  echo "$LOGOUT_OUT" | grep -q "keychain error" || die "logout did not report the keychain error"
  ;;

locked)
  say "locked keychain: does the CLI error or block? (informational)"
  kc_default
  security lock-keychain "$KC" || die "could not lock the keychain"
  start=$(date +%s)
  login 20
  took=$(($(date +%s) - start))
  if [ "$LOGIN_RC" = 142 ]; then
    echo "::warning title=locked keychain::videocall-cli auth login BLOCKS on a locked keychain (killed by the alarm after ${took}s, no error, no output)"
  else
    echo "::notice title=locked keychain::login exited $LOGIN_RC after ${took}s"
  fi
  sed 's/^/  login.out: /' "$WORK/login.out"
  ;;

*) die "unknown scenario $SCENARIO" ;;
esac

say "PASS: $SCENARIO"
