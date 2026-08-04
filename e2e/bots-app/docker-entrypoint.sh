#!/usr/bin/env bash
#
# docker-entrypoint.sh — maps container env vars to a single-bot `bots-app run`
# invocation (headless, clock-mode). Wired by:
#   - k8s/bot-pod.yaml       (Increment 1: one Pod, single-account path)
#   - k8s/statefulset.yaml   (Increment 2: N-pod fleet, per-ordinal accounts)
#
# Env → flags:
#   MEETING_URL        full meeting URL          (default: labsworkspace bottest room)
#   BOT_PARTICIPANT    bot handle + display name (default: k8s-bot-1; overridden in ordinal mode)
#   TTL                bot lifespan              (default: infinite)
#   BOT_AUTH           auth backend             (default: form-login; overridable)
#   BOT_EMAIL          login email  — REQUIRED for form-login (single mode; from the bot-creds Secret)
#   BOT_PASSWORD       login password — REQUIRED for form-login (single mode; from the bot-creds Secret)
#   BOT_HW_CONCURRENCY navigator.hardwareConcurrency cap → simulcast layer cap (default: 6 → 2 layers; "" omits)
#   BOT_IDENTITY_MODE  single | ordinal | auto (default: auto — see "Identity resolution" below)
#   BOT_EMAIL_<N> /    per-ordinal creds for ordinal mode, injected via `envFrom` from the
#   BOT_PASSWORD_<N>     `bot-accounts` Secret (see k8s/bot-accounts.example.yaml)
#   BOT_CTL_PORT       control-API port (Increment 3 #2072; UNSET ⇒ control server disabled — see "Control server" below)
#   BOT_CTL_BIND       control-API bind address (default: 0.0.0.0 when enabled)
#   BOT_CTL_TOKEN      control-API bearer token — REQUIRED when BOT_CTL_PORT is set (from the `bot-ctl-token` Secret; never logged)
#   BOT_RUN_DIR        writable dir for resource-sampler output (default: /tmp/bots-run).
#                      ⚠ The default is the CONTAINER filesystem, so artifacts die
#                      with the pod — k8s/statefulset.yaml overrides this to
#                      /var/lib/bots-run, a per-ordinal PVC, so a scale-to-0 does
#                      not destroy the run's CSVs (issue #2032; they were lost on
#                      the 2026-07-31 #2143 run).
#   BOT_CTL_STATE_DIR  dir the ctl-<pid>.token file is written to (issue #2157).
#                      UNSET ⇒ falls back to BOT_RUN_DIR (the pre-#2157 behavior,
#                      which keeps a plain `docker run` / local dev run unchanged).
#                      k8s/statefulset.yaml sets it to /var/lib/bots-ctl, a
#                      pod-lifetime emptyDir, so the CLEARTEXT bearer token does
#                      NOT land on the retained PVC — see "Ctl token lifetime".
#   BOT_EXTRA_ARGS     optional extra `run` flags (operator escape hatch, e.g. --network)
#
# Identity resolution (Increment 2):
#   A StatefulSet gives every pod the SAME env, so a per-pod identity cannot be
#   set via a plain env var. Instead, ordinal mode derives this pod's replica
#   index from its hostname (StatefulSet sets hostname = pod name =
#   "videocall-bots-<N>") and selects BOT_EMAIL_<N> / BOT_PASSWORD_<N> from the
#   fleet-wide `bot-accounts` Secret — one distinct test account per pod. The
#   in-meeting handle/display-name becomes "bot-<N>". This coexists with
#   Increment 1's single-account path (bot-pod.yaml sets BOT_EMAIL directly):
#   `auto` picks `single` when BOT_EMAIL is set, else `ordinal` when BOT_EMAIL_0
#   is present. The StatefulSet also sets BOT_IDENTITY_MODE=ordinal explicitly.
#
# NOTE — single-bot `--participant`, NOT `--users 1`:
#   The task brief specified `--users 1`, but `bots-app run --users N`
#   hard-requires a manifest (src/cli.ts: "--users requires a manifest") that
#   lives at repo-root bot/conversation/manifest.yaml — a generated file that is
#   NOT part of the copied e2e/ tree and would bloat this "lean, one bot" image.
#   `--participant` single-bot mode needs no manifest and is the correct fit for
#   one clock-mode bot. `--manifest ""` skips manifest loading cleanly (clock
#   mode uses a synthetic canvas, so no costume/audio assets are needed). Each
#   StatefulSet pod is likewise one `--participant` bot; the fleet is N pods.
#
# SECURITY: BOT_PASSWORD / BOT_PASSWORD_<N> are never echoed; the ordinal path
# logs only the ordinal + handle, never which email was selected.

set -euo pipefail

MEETING_URL="${MEETING_URL:-https://app.videocall.labsworkspace.fnxlabs.com/meeting/bottest}"
BOT_PARTICIPANT="${BOT_PARTICIPANT:-k8s-bot-1}"
TTL="${TTL:-infinite}"
BOT_AUTH="${BOT_AUTH:-form-login}"
BOT_RUN_DIR="${BOT_RUN_DIR:-/tmp/bots-run}"
# navigator.hardwareConcurrency cap → simulcast-layer cap. Fewer cores → fewer
# layers (6 → 2). This default applies to BOTH wirings INTENTIONALLY: a bot on a
# 32-core node would otherwise sniff a 3-layer ceiling and over-commit encode CPU
# (the #2035 field finding), so every bot — the Increment 1 single pod included —
# defaults to the realistic 2-layer cap unless overridden. Note the `-` (NOT
# `:-`): an UNSET var defaults to 6, but an explicitly EMPTY value (e.g.
# BOT_HW_CONCURRENCY="") is kept empty so it OMITS the flag entirely and the bot
# uses the browser's real core count — the per-pod escape hatch.
BOT_HW_CONCURRENCY="${BOT_HW_CONCURRENCY-6}"
BOT_IDENTITY_MODE="${BOT_IDENTITY_MODE:-auto}"

# ── Control server (Increment 3, #2072) ──────────────────────────────────────
# In-cluster remote control. When BOT_CTL_PORT is set, this entrypoint starts
# the `run` control server so an in-cluster conductor can drive THIS pod's
# single bot — introspect it and POST /netem to shape the pod's OWN network via
# `tc` (which is exactly why the pod is granted CAP_NET_ADMIN; iproute2 ships in
# the image). Each pod runs one bot, so its control server drives its own local
# bot. Leaving BOT_CTL_PORT UNSET (the Increment 1 single-pod / `docker run`
# smoke default) keeps the control server OFF — that path stays byte-for-byte
# unchanged.
#   BOT_CTL_PORT   control-API port  (unset ⇒ control server disabled)
#   BOT_CTL_BIND   bind address      (default 0.0.0.0 when enabled — needed so
#                                     the pod-network SVC name resolves to it.
#                                     What keeps the API cluster-only is that no
#                                     Ingress/LB fronts it, NOT the bind scope.)
#   BOT_CTL_TOKEN  bearer token      (REQUIRED when enabled — from the
#                                     `bot-ctl-token` Secret; never logged)
# Flag names (--ctl-port / --ctl-bind / --ctl-token) are coordinated with the
# parallel control-server task that makes `run` bindable to 0.0.0.0 + adds the
# /netem endpoint.
BOT_CTL_PORT="${BOT_CTL_PORT:-}"
BOT_CTL_BIND="${BOT_CTL_BIND:-0.0.0.0}"
BOT_CTL_TOKEN="${BOT_CTL_TOKEN:-}"

# ── Ctl token lifetime (issue #2157) ─────────────────────────────────────────
# The orchestrator writes the CLEARTEXT control-API bearer token to
# <dir>/ctl-<pid>.token (mode 0600). Before #2157 that <dir> was BOT_RUN_DIR,
# which #2154/#2032 had just moved onto a per-ordinal PVC so the resource CSVs
# survive `kubectl scale --replicas=0`. Persisting the CSVs is the point and must
# not regress — but the token piggy-backed on it, so a fleet-wide credential
# outlived the workload on media nothing reclaims (the StatefulSet never deletes
# these claims, and the nfs-subdir provisioner creates each subdir 0777).
# Rotating the bot-ctl-token Secret therefore did NOT invalidate the on-disk
# copies.
#
# Fix: BOT_CTL_STATE_DIR sends ONLY the token somewhere ephemeral (the
# StatefulSet points it at a pod-lifetime emptyDir) while --assets-dir keeps
# pointing at BOT_RUN_DIR, so the CSVs still land on the PVC. Unset ⇒ token
# stays in BOT_RUN_DIR, preserving the pre-#2157 behavior for `docker run` and
# local dev (cli.ts resolves the same fallback via resolveCtlStateDir()).
BOT_CTL_STATE_DIR="${BOT_CTL_STATE_DIR:-}"

# ── Identity resolution ──────────────────────────────────────────────────────
# Resolve `auto` to a concrete mode. `single` = Increment 1 (BOT_EMAIL set on
# the pod directly). `ordinal` = Increment 2 StatefulSet (per-ordinal accounts
# via the bot-accounts Secret; BOT_EMAIL_0 present as the sentinel). When
# neither is set we fall through to `single` so the form-login preflight below
# emits its clear "requires BOT_EMAIL" error rather than a silent no-op.
if [ "${BOT_IDENTITY_MODE}" = "auto" ]; then
  if [ -n "${BOT_EMAIL:-}" ]; then
    BOT_IDENTITY_MODE="single"
  elif [ -n "${BOT_EMAIL_0:-}" ]; then
    BOT_IDENTITY_MODE="ordinal"
  else
    BOT_IDENTITY_MODE="single"
  fi
fi

if [ "${BOT_IDENTITY_MODE}" = "ordinal" ]; then
  # StatefulSet sets hostname = pod name = "<statefulset>-<ordinal>", so the
  # trailing "-<N>" segment is this pod's ordinal. Prefer the HOSTNAME env var
  # (set by the container runtime) and fall back to the `hostname` command.
  POD_NAME="${HOSTNAME:-$(hostname 2>/dev/null || true)}"
  ORDINAL="${POD_NAME##*-}"
  if ! [[ "${ORDINAL}" =~ ^[0-9]+$ ]]; then
    echo "docker-entrypoint: FATAL — BOT_IDENTITY_MODE=ordinal but could not derive a numeric ordinal from hostname '${POD_NAME:-<unset>}' (expected 'videocall-bots-<N>'). Refusing to start." >&2
    exit 1
  fi
  # Indirect expansion selects THIS pod's account from the fleet-wide Secret.
  email_var="BOT_EMAIL_${ORDINAL}"
  password_var="BOT_PASSWORD_${ORDINAL}"
  BOT_EMAIL="${!email_var:-}"
  BOT_PASSWORD="${!password_var:-}"
  if [ -z "${BOT_EMAIL}" ] || [ -z "${BOT_PASSWORD}" ]; then
    echo "docker-entrypoint: FATAL — ordinal ${ORDINAL} has no provisioned account (${email_var}/${password_var} absent from the bot-accounts Secret)." >&2
    echo "docker-entrypoint: this pod's replica index exceeds the number of accounts in bot-accounts — provision an account for ordinal ${ORDINAL} or scale replicas down to match. Refusing to start (never reuse another ordinal's account)." >&2
    exit 1
  fi
  # BOT_EMAIL / BOT_PASSWORD were just reassigned from the BOT_EMAIL_<N> vars, so
  # export them — form-login reads process.env.BOT_EMAIL / .BOT_PASSWORD at
  # launch (bot.ts / cli.ts:268), and a plain shell assignment is NOT exported.
  export BOT_EMAIL BOT_PASSWORD
  # One distinct in-meeting handle per pod, ALWAYS derived from the ordinal —
  # never inherited from the shared pod template (all pods share one env, so a
  # template BOT_PARTICIPANT would collide across the fleet).
  BOT_PARTICIPANT="bot-${ORDINAL}"
  echo "docker-entrypoint: ordinal identity — ordinal=${ORDINAL} participant=${BOT_PARTICIPANT} (account selected from bot-accounts Secret)"
fi

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

# Control-server preflight + args. The server is enabled IFF BOT_CTL_PORT is
# set. Enabling it binds a fleet-control + /netem API on BOT_CTL_BIND (0.0.0.0
# in-cluster); binding that WITHOUT a token would expose an unauthenticated
# control surface on the pod network, so a missing BOT_CTL_TOKEN is a fail-fast
# — never a silent unauthenticated bind. Built as an array so the disabled case
# expands to zero args safely under `set -u`.
ctl_args=()
if [ -n "${BOT_CTL_PORT}" ]; then
  if [ -z "${BOT_CTL_TOKEN}" ]; then
    echo "docker-entrypoint: FATAL — BOT_CTL_PORT=${BOT_CTL_PORT} enables the remote control server but BOT_CTL_TOKEN is empty." >&2
    echo "docker-entrypoint: refusing to bind an UNAUTHENTICATED control/netem API on ${BOT_CTL_BIND}. Provide the token via the 'bot-ctl-token' Secret (see k8s/bot-ctl-token.example.yaml). Refusing to start." >&2
    exit 1
  fi
  # Token is NOT passed on argv (it would show in /proc/<pid>/cmdline / ps).
  # cli.ts reads it from the inherited BOT_CTL_TOKEN env var instead — same
  # value, off the command line. (The conductor Job uses --token-file for the
  # same reason.) The fail-fast above still guarantees it's set when enabled.
  ctl_args=(--ctl-port "${BOT_CTL_PORT}" --ctl-bind "${BOT_CTL_BIND}")
fi

mkdir -p "${BOT_RUN_DIR}"

# ── Stale-token sweep (issue #2157, belt-and-braces) ─────────────────────────
# Relocating the token via BOT_CTL_STATE_DIR only helps volumes created from HERE
# ON. Every PVC already provisioned by the #2154 deploy STILL CARRIES the token
# written before this fix, and a StatefulSet never reclaims those claims — so
# without this sweep those copies would persist forever and the fix would cover
# only future volumes. Deleting them on startup is what actually retires them:
# each pod restart/scale-up re-mounts its own claim and wipes its own leftovers.
#
# Safety (adversarial pass on the failure/empty paths, not just the happy one):
#   • `[ -n ... ]` guards an empty/unset BOT_RUN_DIR so this can never degrade
#     into `rm -f /ctl-*.token` against the container root.
#   • `rm -f` on a glob that matches NOTHING is a no-op and exits 0 (verified),
#     so a fresh volume — and the common case of no leftovers — is clean. Under
#     `set -e` the whole `[ … ] && rm …` AND-list is a compound condition, so a
#     false test does not abort the script (verified).
#   • The dir always exists by here (`mkdir -p` above), but `rm -f` tolerates a
#     missing one anyway.
#   • Only `ctl-*.token` is targeted — the #2032 CSVs (`*-raw.csv`,
#     `*-derived.csv`, `*-summary.txt`) are untouched, so the persistence win
#     this fix must not regress is preserved.
#   • `|| true`: the one failure path the list above missed. If a matching file
#     exists in a dir this uid cannot WRITE, `rm` exits 1 — and because `rm` is
#     the final command of the AND-list, `set -e` would kill the script, turning
#     a best-effort cleanup into a crashloop whose only output is a bare
#     "rm: cannot remove …: Permission denied" (verified). `mkdir -p` does not
#     catch it first: on an existing read-only dir it returns 0. Not reachable on
#     the documented deploy (0777 nfs-subdir volumes, uid 1000), but a failed
#     sweep must never stop the bot from starting.
# Unconditional (not gated on BOT_CTL_PORT): a pod whose control server is now
# DISABLED must still shed a token left by an earlier ctl-enabled run.
#
# Safe to sweep unconditionally rather than checking each token's pid for
# staleness, because this dir is never shared with a LIVE run:
#   • `k8s/statefulset.yaml` declares run-artifacts under `volumeClaimTemplates`
#     with `ReadWriteOnce`, so every pod gets its OWN claim
#     (`run-artifacts-videocall-bots-<N>`) — two pods cannot share this dir.
#   • Each pod runs exactly ONE `bots-app run` (this script `exec`s it), so
#     within a pod there is no second live token either.
#   • The per-pid `ctl-<pid>.token` naming in `control/auth.ts` exists for
#     concurrent `bots-app run` invocations on a DEV WORKSTATION. Those are
#     direct CLI runs — this entrypoint is not involved, so the sweep cannot
#     touch them. (An operator who deliberately bind-mounts one host dir into
#     two simultaneously-starting containers could still race; that is not the
#     documented deploy, and a pid-staleness check is the fix if it ever is.)
#
# The `|| warn` (rather than a bare `|| true`) exists because a SILENTLY failed
# sweep is the worst outcome: the stale cleartext token stays on the PVC while
# README.md tells the operator the sweep retired it. `rm`'s own stderr is just
# "rm: cannot remove …: Permission denied", which says nothing about the security
# consequence, so name the dir and the consequence explicitly.
sweep_tokens() {
  # $1 = directory to sweep, $2 = human label for the warning.
  [ -n "$1" ] || return 0
  if ! rm -f "$1"/ctl-*.token 2>/dev/null; then
    echo "docker-entrypoint: WARNING — could not sweep stale ctl-*.token from $1 ($2)." >&2
    echo "docker-entrypoint: a pre-#2157 cleartext control-API token may STILL be present there." >&2
    echo "docker-entrypoint: rotating the bot-ctl-token Secret will NOT retire that copy — delete the" >&2
    echo "docker-entrypoint: PVC (kubectl -n bot-load delete pvc …) or fix the directory permissions." >&2
    echo "docker-entrypoint: continuing startup anyway — the sweep is best-effort and must not crashloop." >&2
  fi
}
sweep_tokens "${BOT_RUN_DIR}" "run dir / retained PVC"

# Sweep the state dir too when it is a DIFFERENT directory.
#
# This IS load-bearing on the DEFAULT K8s wiring — an earlier version of this
# comment claimed it only mattered if an operator pointed BOT_CTL_STATE_DIR at
# durable storage, which is wrong. An emptyDir's lifetime is the POD, not the
# container, so it SURVIVES a container restart — and `k8s/statefulset.yaml`'s
# own header documents restarts as routine ("a StatefulSet pod's restartPolicy
# is always `Always` … if a bot exits (TTL reached, or crash) the container
# RESTARTS"). So on every crash-restart this dir is NOT born empty and the `rm`
# below is what clears the PREVIOUS container's token.
#
# The security delta is small (same fleet-wide Secret either way, sub-second
# overlap), but the lines below are NOT dead code and must not be deleted as
# such.
if [ -n "${BOT_CTL_STATE_DIR}" ] && [ "${BOT_CTL_STATE_DIR}" != "${BOT_RUN_DIR}" ]; then
  mkdir -p "${BOT_CTL_STATE_DIR}"
  # Same helper as the BOT_RUN_DIR sweep above: never aborts startup (an
  # unwritable dir makes `rm` exit 1, which under `set -e` would crashloop the
  # pod over a best-effort cleanup) but warns loudly rather than silently.
  sweep_tokens "${BOT_CTL_STATE_DIR}" "ctl state dir / emptyDir"
  # The token path is resolved inside the exec'd Node process (control/auth.ts
  # resolveCtlStateDir reads process.env), so the var must be in that process's
  # environment. This `export` is BELT-AND-BRACES, not load-bearing: reaching
  # this branch requires BOT_CTL_STATE_DIR to be non-empty, and the ONLY way it
  # can be is if it arrived from the container env — where it is already
  # exported, and a reassignment preserves the export attribute (verified; a
  # mutation test that deletes this line still passes, which is how we know).
  # Kept because it makes the inheritance contract explicit and would become
  # load-bearing the moment someone gives this var a shell-side default the way
  # every other var here has one. Contrast `export BOT_EMAIL BOT_PASSWORD`
  # above, which IS load-bearing — those are assigned from DIFFERENT vars
  # (BOT_EMAIL_<N>), so they carry no inherited export attribute.
  export BOT_CTL_STATE_DIR
fi

# NO SIGTERM TRAP HERE — DELIBERATE, and NOT an oversight. This script `exec`s
# tsx below (see the exec NOTE), which REPLACES this shell with the Node process:
# the shell is gone, so any `trap … TERM` it had installed can never fire. A trap
# added here would be pure dead code — exactly the "looks right, does nothing"
# shell defect the repo's adversarial-review rule calls out. Removal on shutdown
# would have to live in the Node process's own SIGTERM path
# (orchestrator.ts requestShutdown). We deliberately do NOT add it there either:
# with the token on an emptyDir it is already destroyed with the pod, so an
# in-process unlink would buy nothing on the K8s path and would DELETE a local
# dev's token file out from under a `ctl` session on the fallback path. The
# startup sweep above is what retires the pre-existing PVC copies.

# Non-sensitive startup line only — password is never logged; email is reported
# as present/absent to minimize PII in logs.
cred_state="absent"
[ -n "${BOT_EMAIL:-}" ] && cred_state="present"
# Control-server state — port + bind only; the token value is NEVER logged (it
# is guaranteed present here whenever ctl is enabled, by the preflight above).
ctl_state="disabled"
# token_dir is the DIRECTORY only — never the token value (#2157). Reported so an
# operator can confirm from the pod log that the credential is NOT going to the
# retained PVC.
[ -n "${BOT_CTL_PORT}" ] && ctl_state="port=${BOT_CTL_PORT} bind=${BOT_CTL_BIND} token=present token_dir=${BOT_CTL_STATE_DIR:-${BOT_RUN_DIR}}"
echo "docker-entrypoint: launching bot — url=${MEETING_URL} participant=${BOT_PARTICIPANT} auth=${BOT_AUTH} ttl=${TTL} identity_mode=${BOT_IDENTITY_MODE} hw_concurrency=${BOT_HW_CONCURRENCY:-<omitted>} credentials=${cred_state} control=[${ctl_state}]"

# Capability cap. `--hardware-concurrency <N>` (added by the parallel frontend
# task on the `run` command) sets the fake navigator.hardwareConcurrency the bot
# advertises, which caps how many simulcast layers it encodes (6 → 2). Built as
# an array so BOT_HW_CONCURRENCY="" cleanly omits the flag; expanded below with
# the `${arr[@]+...}` guard so an empty array is safe under `set -u` on every
# bash version (see the exec NOTE).
hw_args=()
if [ -n "${BOT_HW_CONCURRENCY}" ]; then
  hw_args=(--hardware-concurrency "${BOT_HW_CONCURRENCY}")
fi

# exec the tsx binary DIRECTLY (not `npm run`, which would make npm PID 1 and
# swallow SIGTERM) so a pod SIGTERM/SIGINT reaches the orchestrator's in-process
# clean-leave handler (graceful meeting exit) rather than being SIGKILLed at the
# termination grace deadline.
# NOTE: `${arr[@]+"${arr[@]}"}` (not a bare `"${arr[@]}"`) expands an EMPTY array
# to nothing under `set -u`. On bash < 4.4 a bare `"${arr[@]}"` on an empty array
# is an "unbound variable" error; bash 4.4+ (incl. the prod image's 5.2) is fine
# either way. This matters on a dev box whose `bash` is < 4.4 — e.g. macOS stock
# /bin/bash 3.2, where the docker-entrypoint unit tests surfaced this. The tests
# invoke `bash` from PATH, so CI (Linux, bash 5.x) does NOT exercise the < 4.4
# path — this is defensive portability, not a CI-locked regression. ctl_args is
# empty on every control-server-disabled run, hw_args when BOT_HW_CONCURRENCY is
# unset, so the empty case is common.
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
  ${hw_args[@]+"${hw_args[@]}"} \
  ${ctl_args[@]+"${ctl_args[@]}"} \
  ${BOT_EXTRA_ARGS:-}
