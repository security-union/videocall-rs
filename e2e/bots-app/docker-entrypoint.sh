#!/usr/bin/env bash
#
# docker-entrypoint.sh — maps container env vars to a single-bot `bots-app run`
# invocation (headless, clock-mode). Wired by e2e/bots-app/k8s/bot-pod.yaml.
#
# Env → flags:
#   MEETING_URL      full meeting URL          (default: labsworkspace bottest room)
#   BOT_PARTICIPANT  bot handle + display name (default: k8s-bot-1)
#   TTL              bot lifespan              (default: infinite)
#   BOT_AUTH         auth backend             (default: form-login; overridable)
#   BOT_EMAIL        login email  — REQUIRED for form-login (from the bot-creds Secret)
#   BOT_PASSWORD     login password — REQUIRED for form-login (from the bot-creds Secret)
#   BOT_RUN_DIR      writable dir for resource-sampler output (default: /tmp/bots-run)
#   BOT_EXTRA_ARGS   optional extra `run` flags (operator escape hatch, e.g. --network)
#
# NOTE — single-bot `--participant`, NOT `--users 1`:
#   The task brief specified `--users 1`, but `bots-app run --users N`
#   hard-requires a manifest (src/cli.ts: "--users requires a manifest") that
#   lives at repo-root bot/conversation/manifest.yaml — a generated file that is
#   NOT part of the copied e2e/ tree and would bloat this "lean, one bot" image.
#   `--participant` single-bot mode needs no manifest and is the correct fit for
#   one clock-mode bot. `--manifest ""` skips manifest loading cleanly (clock
#   mode uses a synthetic canvas, so no costume/audio assets are needed).
#
# SECURITY: BOT_PASSWORD is never echoed.

set -euo pipefail

MEETING_URL="${MEETING_URL:-https://app.videocall.labsworkspace.fnxlabs.com/meeting/bottest}"
BOT_PARTICIPANT="${BOT_PARTICIPANT:-k8s-bot-1}"
TTL="${TTL:-infinite}"
BOT_AUTH="${BOT_AUTH:-form-login}"
BOT_RUN_DIR="${BOT_RUN_DIR:-/tmp/bots-run}"

# form-login (added by the separate frontend task) reads BOT_EMAIL/BOT_PASSWORD
# straight from the environment and drives the identity-service login form.
# Fail fast with a clear message so a missing Secret surfaces as an explicit
# startup error rather than a confusing login-form timeout inside the browser.
if [ "${BOT_AUTH}" = "form-login" ]; then
  missing=""
  [ -z "${BOT_EMAIL:-}" ] && missing="${missing} BOT_EMAIL"
  [ -z "${BOT_PASSWORD:-}" ] && missing="${missing} BOT_PASSWORD"
  if [ -n "${missing}" ]; then
    echo "docker-entrypoint: FATAL — --auth form-login requires:${missing}" >&2
    echo "docker-entrypoint: create them via the 'bot-creds' Secret (see k8s/bot-creds.example.yaml). Refusing to start." >&2
    exit 1
  fi
fi

mkdir -p "${BOT_RUN_DIR}"

# Non-sensitive startup line only — password is never logged; email is reported
# as present/absent to minimize PII in logs.
cred_state="absent"
[ -n "${BOT_EMAIL:-}" ] && cred_state="present"
echo "docker-entrypoint: launching bot — url=${MEETING_URL} participant=${BOT_PARTICIPANT} auth=${BOT_AUTH} ttl=${TTL} credentials=${cred_state}"

# exec the tsx binary DIRECTLY (not `npm run`, which would make npm PID 1 and
# swallow SIGTERM) so a pod SIGTERM/SIGINT reaches the orchestrator's in-process
# clean-leave handler (graceful meeting exit) rather than being SIGKILLed at the
# termination grace deadline.
# shellcheck disable=SC2086 # BOT_EXTRA_ARGS is an intentional word-split escape hatch.
exec node_modules/.bin/tsx bots-app/src/cli.ts run \
  --meeting-url "${MEETING_URL}" \
  --participant "${BOT_PARTICIPANT}" \
  --display-name "${BOT_PARTICIPANT}" \
  --headless \
  --video-mode clock \
  --auth "${BOT_AUTH}" \
  --ttl "${TTL}" \
  --manifest "" \
  --assets-dir "${BOT_RUN_DIR}" \
  ${BOT_EXTRA_ARGS:-}
