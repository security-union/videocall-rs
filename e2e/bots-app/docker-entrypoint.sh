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
#   BOT_HW_CONCURRENCY navigator.hardwareConcurrency cap → simulcast layer cap (default: 10 → 3 layers; "" omits)
#   BOT_HW_CONCURRENCY_<N> / BOT_NETEM_PROFILE[_<N>] / BOT_NETEM_IFACE / BOT_MAX_JOIN_STAGGER_SECS  see README
#   BOT_CAMERA_{ON,OFF}_SECS_{MIN,MAX}  camera duty cycle (#2362); all four unset ⇒ camera always on
#   BOT_IDENTITY_MODE  single | ordinal | auto (default: auto — see "Identity resolution" below)
#   BOT_INDEX          fleet index → clock capture geometry (#2236); ordinal mode OVERWRITES it
#                      with the pod ordinal. Unset ⇒ flag omitted ⇒ index 0 ⇒ 640x480.
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

# CR/LF become a space, so a value cannot forge a second, unprefixed line. $2 = stream.
say() { printf '%s\n' "${1//[$'\r\n']/ }" >&"${2:-1}"; }

MEETING_URL="${MEETING_URL:-https://app.videocall.labsworkspace.fnxlabs.com/meeting/bottest}"
BOT_PARTICIPANT="${BOT_PARTICIPANT:-k8s-bot-1}"
TTL="${TTL:-infinite}"
BOT_AUTH="${BOT_AUTH:-form-login}"
BOT_RUN_DIR="${BOT_RUN_DIR:-/tmp/bots-run}"
# navigator.hardwareConcurrency cap → simulcast-layer cap (10 → 3). Pinned rather
# than left to the node's real core count, which a container reports unchanged
# (the #2035 field finding), so a bot's ladder depth is a choice and not a
# property of whichever node scheduled it. Three rungs, not two: this is a
# PUBLISH-side cap, and applying it fleet-wide is what gives every receiver a
# middle rung for the #1256 tile lid to land on — but only at >= 7 decoded
# tiles; below that both ladders pick the top rung and the extra rung is pure
# encode cost (#2248).
# Applies to BOTH wirings intentionally. Note the `-` (NOT `:-`): an UNSET var
# defaults to 10, but an explicitly EMPTY value (e.g. BOT_HW_CONCURRENCY="") is
# kept empty so it OMITS the flag entirely and the bot uses the browser's real
# core count — the per-pod escape hatch.
BOT_HW_CONCURRENCY="${BOT_HW_CONCURRENCY-10}"
BOT_IDENTITY_MODE="${BOT_IDENTITY_MODE:-auto}"
BOT_INDEX="${BOT_INDEX:-}"
BOT_NETEM_PROFILE="${BOT_NETEM_PROFILE:-}"
BOT_NETEM_IFACE="${BOT_NETEM_IFACE:-eth0}"
# Mirrors netem.ts's NETEM_IFB_DEV / NETEM_INGRESS_QDISC_MARKER; not operator config.
NETEM_IFB_DEV="ifb0"
# iproute2 clears its own caps unless CAP_NET_ADMIN is INHERITABLE, which a file
# cap does not populate — so every `ip` here goes through netem_ip (#2428).
NETEM_SETPRIV="${NETEM_SETPRIV:-/usr/local/bin/netem-setpriv}"
NETEM_INGRESS_MARKER="qdisc ingress"
# A fresh ifb comes up at 32, which would bind ahead of every profile's netem limit.
NETEM_IFB_TXQUEUELEN="1000"
BOT_MAX_JOIN_STAGGER_SECS="${BOT_MAX_JOIN_STAGGER_SECS:-}"
# Validated (not consumed) here; src/camera-cycle.ts reads them from the env.
BOT_CAMERA_ON_SECS_MIN="${BOT_CAMERA_ON_SECS_MIN:-}"
BOT_CAMERA_ON_SECS_MAX="${BOT_CAMERA_ON_SECS_MAX:-}"
BOT_CAMERA_OFF_SECS_MIN="${BOT_CAMERA_OFF_SECS_MIN:-}"
BOT_CAMERA_OFF_SECS_MAX="${BOT_CAMERA_OFF_SECS_MAX:-}"

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
    say "docker-entrypoint: FATAL — BOT_IDENTITY_MODE=ordinal but could not derive a numeric ordinal from hostname '${POD_NAME:-<unset>}' (expected 'videocall-bots-<N>'). Refusing to start." 2
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
  # Same reason for the fleet index: the ordinal wins over a template BOT_INDEX.
  BOT_INDEX="${ORDINAL}"
  # `-`, not `:-`: an explicitly EMPTY value opts this pod out, not the fleet.
  hw_var="BOT_HW_CONCURRENCY_${ORDINAL}"
  BOT_HW_CONCURRENCY="${!hw_var-${BOT_HW_CONCURRENCY}}"
  netem_var="BOT_NETEM_PROFILE_${ORDINAL}"
  BOT_NETEM_PROFILE="${!netem_var-${BOT_NETEM_PROFILE}}"
  echo "docker-entrypoint: ordinal identity — ordinal=${ORDINAL} participant=${BOT_PARTICIPANT} (account selected from bot-accounts Secret)"
  # A per-ordinal var reaches a pod only when its suffix SPELLS that pod's
  # ${ORDINAL} (so `_01` never reaches pod 1), and only provisioned ordinals
  # start. `_<digits>` on a misspelt stem is left to the stem sweep below.
  unreachable=""
  for env_name in ${!BOT_NETEM_PROFILE_@} ${!BOT_HW_CONCURRENCY_@}; do
    [[ "${env_name}" =~ ^(BOT_NETEM_PROFILE|BOT_HW_CONCURRENCY)_(.*)$ ]] || continue
    suffix="${BASH_REMATCH[2]}"
    [ "${suffix}" = "${ORDINAL}" ] && continue
    if [[ "${suffix}" =~ ^(0|[1-9][0-9]*)$ ]]; then
      email_var_n="BOT_EMAIL_${suffix}"
      password_var_n="BOT_PASSWORD_${suffix}"
      [ -n "${!email_var_n:-}" ] && [ -n "${!password_var_n:-}" ] && continue
    elif [[ "${suffix}" =~ _[0-9]+$ ]]; then
      continue
    fi
    unreachable="${unreachable}${env_name} "
  done
  if [ -n "${unreachable}" ]; then
    say "docker-entrypoint: WARNING — ignoring (${unreachable% }): no pod reads them — a per-ordinal suffix must be a plain integer naming an ordinal with a provisioned BOT_EMAIL_<N>/BOT_PASSWORD_<N>; this pod is ordinal ${ORDINAL}." 2
  fi
else
  per_pod="$(printf '%s ' "${!BOT_NETEM_PROFILE_@}" "${!BOT_HW_CONCURRENCY_@}")"
  if [ -n "${per_pod% }" ]; then
    say "docker-entrypoint: WARNING — ignoring per-ordinal overrides (${per_pod% }): they are read only when BOT_IDENTITY_MODE=ordinal (resolved to '${BOT_IDENTITY_MODE}')." 2
  fi
fi

# Stem, not prefix: BOT_NETEM_PROFILE_TYPO_1 is read by nothing too, in either mode.
inert_suffixed=""
for env_name in ${!BOT_@}; do
  [[ "${env_name}" =~ ^(.*)_[0-9]+$ ]] || continue
  case "${BASH_REMATCH[1]}" in
    BOT_EMAIL | BOT_PASSWORD | BOT_NETEM_PROFILE | BOT_HW_CONCURRENCY) continue ;;
  esac
  inert_suffixed="${inert_suffixed}${env_name} "
done
if [ -n "${inert_suffixed}" ]; then
  say "docker-entrypoint: WARNING — ignoring (${inert_suffixed% }): only BOT_EMAIL_<N>, BOT_PASSWORD_<N>, BOT_NETEM_PROFILE_<N> and BOT_HW_CONCURRENCY_<N> take a per-ordinal suffix; the rest of this pod's config is fleet-wide." 2
fi

# Every operator string this script puts on the launch line or exports to the bot
# process. Downstream TypeScript composes `[label] …` lines from these without
# collapsing CR/LF, so the value is refused here rather than at each writer.
for raw_var in MEETING_URL BOT_PARTICIPANT TTL BOT_AUTH BOT_RUN_DIR \
  BOT_HW_CONCURRENCY BOT_INDEX BOT_CTL_PORT BOT_CTL_BIND BOT_CTL_STATE_DIR \
  BOT_EMAIL BOT_EXTRA_ARGS; do
  case "${!raw_var-}" in
    *$'\r'* | *$'\n'*)
      say "docker-entrypoint: FATAL — ${raw_var} contains a carriage return or newline, which a downstream log line would emit verbatim. A Secret created with 'kubectl create secret --from-file' carries a trailing newline; use --from-literal or stringData. Refusing to start." 2
      exit 1
      ;;
  esac
done

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
    say "docker-entrypoint: FATAL — BOT_CTL_PORT=${BOT_CTL_PORT} enables the remote control server but BOT_CTL_TOKEN is empty." 2
    say "docker-entrypoint: refusing to bind an UNAUTHENTICATED control/netem API on ${BOT_CTL_BIND}. Provide the token via the 'bot-ctl-token' Secret (see k8s/bot-ctl-token.example.yaml). Refusing to start." 2
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
    say "docker-entrypoint: WARNING — could not sweep stale ctl-*.token from $1 ($2)." 2
    say "docker-entrypoint: a pre-#2157 cleartext control-API token may STILL be present there." 2
    say "docker-entrypoint: rotating the bot-ctl-token Secret will NOT retire that copy — delete the" 2
    say "docker-entrypoint: PVC (kubectl -n bot-load delete pvc …) or fix the directory permissions." 2
    say "docker-entrypoint: continuing startup anyway — the sweep is best-effort and must not crashloop." 2
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

# Nothing unlinks the ctl token: on K8s the pod-lifetime `ctl-state` emptyDir
# takes it.

netem_fatal() {
  say "docker-entrypoint: FATAL — BOT_NETEM_PROFILE=${BOT_NETEM_PROFILE}: $1 failed. Refusing to start on a link that does not match the profile." 2
  echo "docker-entrypoint: check the netem-preload DaemonSet is Ready on this node and that cap_net_admin reached tc and ${NETEM_SETPRIV} (k8s/netem-preload-daemonset.yaml, #2428)." >&2
  exit 1
}

netem_ip() {
  "${NETEM_SETPRIV}" --inh-caps +net_admin --ambient-caps +net_admin -- ip "$@"
}

# Failure is expected when the object is absent; only rc>=126 is fatal.
netem_try() {
  local rc=0
  "$@" >/dev/null 2>&1 || rc=$?
  [ "${rc}" -lt 126 ] || netem_fatal "$* did not execute, rc=${rc}"
  return "${rc}"
}

# Mirrors buildNetemMirrorInstallArgs: probe before `ip link add` (the netns
# outlives the container), delete the hook before adding, `protocol all` for IPv6.
netem_mirror_install() {
  netem_ip link show "${NETEM_IFB_DEV}" >/dev/null 2>&1 ||
    netem_ip link add "${NETEM_IFB_DEV}" type ifb ||
    netem_fatal "ip link add ${NETEM_IFB_DEV} type ifb"
  netem_ip link set "${NETEM_IFB_DEV}" up || netem_fatal "ip link set ${NETEM_IFB_DEV} up"
  netem_ip link set "${NETEM_IFB_DEV}" txqueuelen "${NETEM_IFB_TXQUEUELEN}" ||
    netem_fatal "ip link set ${NETEM_IFB_DEV} txqueuelen ${NETEM_IFB_TXQUEUELEN}"
  netem_try tc qdisc del dev "${BOT_NETEM_IFACE}" ingress || true
  tc qdisc add dev "${BOT_NETEM_IFACE}" handle ffff: ingress ||
    netem_fatal "tc qdisc add dev ${BOT_NETEM_IFACE} handle ffff: ingress"
  tc filter add dev "${BOT_NETEM_IFACE}" parent ffff: protocol all u32 match u32 0 0 \
    action mirred egress redirect dev "${NETEM_IFB_DEV}" ||
    netem_fatal "tc filter add dev ${BOT_NETEM_IFACE} parent ffff: (ingress redirect to ${NETEM_IFB_DEV})"
  tc qdisc replace dev "${NETEM_IFB_DEV}" root netem "$@" ||
    netem_fatal "tc qdisc replace dev ${NETEM_IFB_DEV} root netem"
}

# Hook first: no filter may outlive its target; its status is the only proof.
netem_mirror_clear() {
  netem_try tc qdisc del dev "${BOT_NETEM_IFACE}" ingress || return 0
  netem_try tc qdisc del dev "${NETEM_IFB_DEV}" root || true
  netem_try netem_ip link del "${NETEM_IFB_DEV}" || true
}

netem_apply() {
  local netem_rc=0 netem_err="" netem_err_lc=""
  if [ "$1" = "clear" ]; then
    netem_err="$(tc qdisc del dev "${BOT_NETEM_IFACE}" root 2>&1)" || netem_rc=$?
    if [ "${netem_rc}" -ne 0 ]; then
      # >=126 is the shell's own "tc never ran": judge status before wording.
      if [ "${netem_rc}" -ge 126 ]; then
        netem_fatal "tc did not execute, rc=${netem_rc} (${netem_err})"
      fi
      netem_err_lc="$(printf '%s' "${netem_err}" | tr '[:upper:]' '[:lower:]')"
      case "${netem_err_lc}" in
        *"cannot delete"* | *"no such file"*) ;;
        *) netem_fatal "tc qdisc del dev ${BOT_NETEM_IFACE} root (${netem_err})" ;;
      esac
    fi
    netem_mirror_clear
    return 0
  fi
  shift
  tc qdisc replace dev "${BOT_NETEM_IFACE}" root netem "$@" ||
    netem_fatal "tc qdisc replace dev ${BOT_NETEM_IFACE} root netem"
  netem_mirror_install ${netem_ingress_params[@]+"${netem_ingress_params[@]}"}
}

# Sets netem_read_rc/_root/_netem/_ingress; reads ifb too when a hook is present.
netem_read() {
  local out="" line
  netem_read_rc=0
  netem_read_root=""
  netem_read_netem=""
  netem_read_ingress=""
  # No pipeline: the status must be captured before anything matches the output.
  out="$(tc qdisc show dev "${BOT_NETEM_IFACE}" 2>&1)" || netem_read_rc=$?
  if [ "${netem_read_rc}" -ne 0 ]; then
    netem_read_root="${out%%$'\n'*}"
    return 0
  fi
  netem_read_root="${out%%$'\n'*}"
  case "${out}" in
    *"qdisc netem"*)
      line="qdisc netem${out#*qdisc netem}"
      netem_read_netem="${line%%$'\n'*}"
      ;;
  esac
  case "${out}" in
    *"${NETEM_INGRESS_MARKER}"*)
      out="$(tc qdisc show dev "${NETEM_IFB_DEV}" 2>&1)" || true
      [ -n "${out}" ] || out="unread"
      netem_read_ingress="${out%%$'\n'*}"
      ;;
  esac
}

# Validated before the first interface mutation: a reject must not leave it shaped.
stagger_max=0
if [ -n "${BOT_MAX_JOIN_STAGGER_SECS}" ]; then
  if ! [[ "${BOT_MAX_JOIN_STAGGER_SECS}" =~ ^[0-9]{1,5}$ ]]; then
    say "docker-entrypoint: FATAL — BOT_MAX_JOIN_STAGGER_SECS must be a non-negative integer of at most 5 digits (got '${BOT_MAX_JOIN_STAGGER_SECS}')." 2
    exit 1
  fi
  # 10#: "08" is a valid stagger, not an octal literal.
  stagger_max=$((10#${BOT_MAX_JOIN_STAGGER_SECS}))
  if [ "${stagger_max}" -gt 32767 ]; then
    say "docker-entrypoint: FATAL — BOT_MAX_JOIN_STAGGER_SECS=${stagger_max} exceeds the 32767s the stagger can draw." 2
    exit 1
  fi
fi

# Before the first interface mutation. A PARTIAL set is a hard error (#2362).
CAMERA_CYCLE_VARS=(BOT_CAMERA_ON_SECS_MIN BOT_CAMERA_ON_SECS_MAX BOT_CAMERA_OFF_SECS_MIN BOT_CAMERA_OFF_SECS_MAX)
CAMERA_CYCLE_SECS_CEILING=86400
camera_cycle_state="off"
camera_set=""
camera_unset=""
for camera_var in "${CAMERA_CYCLE_VARS[@]}"; do
  if [ -n "${!camera_var}" ]; then
    camera_set="${camera_set}${camera_var} "
  else
    camera_unset="${camera_unset}${camera_var} "
  fi
done
if [ -n "${camera_set}" ]; then
  if [ -n "${camera_unset}" ]; then
    say "docker-entrypoint: FATAL — camera cycling needs all four of ${CAMERA_CYCLE_VARS[*]}; missing: ${camera_unset% }. Set them all, or none (none = camera on for the whole run)." 2
    exit 1
  fi
  for camera_var in "${CAMERA_CYCLE_VARS[@]}"; do
    if ! [[ "${!camera_var}" =~ ^[0-9]{1,5}$ ]]; then
      say "docker-entrypoint: FATAL — ${camera_var} must be a positive integer of at most 5 digits (seconds), got '${!camera_var}'." 2
      exit 1
    fi
    if [ "$((10#${!camera_var}))" -lt 1 ]; then
      say "docker-entrypoint: FATAL — ${camera_var} must be >= 1 second, got '${!camera_var}'." 2
      exit 1
    fi
    if [ "$((10#${!camera_var}))" -gt "${CAMERA_CYCLE_SECS_CEILING}" ]; then
      say "docker-entrypoint: FATAL — ${camera_var} must be <= ${CAMERA_CYCLE_SECS_CEILING} seconds, got '${!camera_var}'." 2
      exit 1
    fi
  done
  camera_on_min=$((10#${BOT_CAMERA_ON_SECS_MIN}))
  camera_on_max=$((10#${BOT_CAMERA_ON_SECS_MAX}))
  camera_off_min=$((10#${BOT_CAMERA_OFF_SECS_MIN}))
  camera_off_max=$((10#${BOT_CAMERA_OFF_SECS_MAX}))
  if [ "${camera_on_min}" -gt "${camera_on_max}" ]; then
    say "docker-entrypoint: FATAL — BOT_CAMERA_ON_SECS_MIN=${camera_on_min} must be <= BOT_CAMERA_ON_SECS_MAX=${camera_on_max}." 2
    exit 1
  fi
  if [ "${camera_off_min}" -gt "${camera_off_max}" ]; then
    say "docker-entrypoint: FATAL — BOT_CAMERA_OFF_SECS_MIN=${camera_off_min} must be <= BOT_CAMERA_OFF_SECS_MAX=${camera_off_max}." 2
    exit 1
  fi
  camera_duty=$((
    (camera_on_min + camera_on_max) * 100 / (camera_on_min + camera_on_max + camera_off_min + camera_off_max)
  ))
  # "configured", not "applied" — only the bot's CAMERA_CYCLE_* receipt knows.
  camera_cycle_state="configured on=[${camera_on_min}-${camera_on_max}]s off=[${camera_off_min}-${camera_off_max}]s target_duty=${camera_duty}%"
fi

# Same grammar assertIface() enforces; also blocks a newline in the launch line.
if ! [[ "${BOT_NETEM_IFACE}" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,14}$ ]]; then
  say "docker-entrypoint: FATAL — BOT_NETEM_IFACE '${BOT_NETEM_IFACE}' is not a valid device name (netem.ts IFACE_PATTERN)." 2
  exit 1
fi

netem_state="unknown"
if [ -n "${BOT_NETEM_PROFILE}" ]; then
  # Mirrors NETEM_PROFILES / ingressNetemParams; the two differ only in `rate`.
  netem_params=()
  netem_ingress_params=()
  netem_op="shape"
  netem_verb="applied"
  case "${BOT_NETEM_PROFILE}" in
    clean | none)
      netem_op="clear"
      netem_verb="cleared"
      ;;
    good_wifi)
      netem_params=(delay 20ms 5ms loss 0.1% rate 20000kbit limit 100)
      netem_ingress_params=(delay 20ms 5ms loss 0.1% rate 50000kbit limit 100)
      ;;
    good_4g)
      netem_params=(delay 50ms 15ms loss 0.5% rate 10000kbit limit 100)
      netem_ingress_params=(delay 50ms 15ms loss 0.5% rate 30000kbit limit 100)
      ;;
    congested_wifi)
      netem_params=(delay 80ms 30ms loss 2% rate 2000kbit limit 55)
      netem_ingress_params=(delay 80ms 30ms loss 2% rate 4000kbit limit 55)
      ;;
    lossy_mobile)
      netem_params=(delay 150ms 50ms loss 5% rate 800kbit limit 40)
      netem_ingress_params=(delay 150ms 50ms loss 5% rate 2000kbit limit 40)
      ;;
    satellite)
      netem_params=(delay 600ms 50ms loss 1% rate 1500kbit limit 300)
      netem_ingress_params=(delay 600ms 50ms loss 1% rate 10000kbit limit 300)
      ;;
    dialup)
      netem_params=(delay 200ms 40ms loss 3% rate 56kbit limit 10)
      netem_ingress_params=(delay 200ms 40ms loss 3% rate 56kbit limit 10)
      ;;
    *)
      say "docker-entrypoint: FATAL — unknown BOT_NETEM_PROFILE '${BOT_NETEM_PROFILE}'. Known: clean none good_wifi good_4g congested_wifi lossy_mobile satellite dialup (src/control/netem.ts)." 2
      exit 1
      ;;
  esac
  netem_apply "${netem_op}" ${netem_params[@]+"${netem_params[@]}"}
  netem_read
  if [ "${netem_read_rc}" -ne 0 ]; then
    netem_fatal "tc qdisc show dev ${BOT_NETEM_IFACE} after ${netem_op}, rc=${netem_read_rc} (${netem_read_root})"
  fi
  if [ "${netem_op}" = "shape" ]; then
    [ -n "${netem_read_netem}" ] || netem_fatal "post-read found no netem on ${BOT_NETEM_IFACE}"
    case "${netem_read_ingress}" in
      *"qdisc netem"*) ;;
      *) netem_fatal "post-read found no ingress netem on ${NETEM_IFB_DEV} (${netem_read_ingress:-no mirror})" ;;
    esac
    netem_state="shape profile=${BOT_NETEM_PROFILE} iface=${BOT_NETEM_IFACE} direction=both egress=[${netem_params[*]-}] ingress=[dev ${NETEM_IFB_DEV} ${netem_ingress_params[*]-}]"
  else
    [ -z "${netem_read_netem}" ] || netem_fatal "post-read still shows netem on ${BOT_NETEM_IFACE} (${netem_read_netem})"
    [ -z "${netem_read_ingress}" ] || netem_fatal "post-read still shows an ingress mirror on ${BOT_NETEM_IFACE} (${netem_read_ingress})"
    netem_state="clear profile=${BOT_NETEM_PROFILE} iface=${BOT_NETEM_IFACE} direction=both tc=[${netem_read_root}] ingress=none"
  fi
  say "docker-entrypoint: netem ${netem_verb} — ${netem_state}"
else
  netem_read
  netem_ingress_state="none"
  [ -z "${netem_read_ingress}" ] || netem_ingress_state="[${netem_read_ingress}]"
  if [ "${netem_read_rc}" -ne 0 ]; then
    netem_state="unread iface=${BOT_NETEM_IFACE} probe-failed rc=${netem_read_rc}"
    say "docker-entrypoint: WARNING — could not read the qdisc, so this pod's link posture is UNKNOWN — ${netem_state}: ${netem_read_root}" 2
  elif [ -n "${netem_read_netem}" ] || [ -n "${netem_read_ingress}" ]; then
    qdisc_line="${netem_read_netem:-${netem_read_root}}"
    # A root that shapes (e.g. tbf) outranks a netem nested under it.
    [ "${netem_read_root}" = "${qdisc_line}" ] || qdisc_line="${netem_read_root}; ${qdisc_line}"
    netem_state="inherited iface=${BOT_NETEM_IFACE} tc=[${qdisc_line}] ingress=${netem_ingress_state}"
    say "docker-entrypoint: WARNING — shaping was already installed and this run neither applied nor cleared it — ${netem_state}" 2
  else
    # "No netem" is not "no shaping", so report the evidence, never a bare claim.
    netem_state="no-netem iface=${BOT_NETEM_IFACE} tc=[${netem_read_root}] ingress=none"
  fi
fi

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

stagger_state="off"
stagger_note=""
if [ "${stagger_max}" -gt 0 ]; then
  stagger_secs=$((RANDOM % (stagger_max + 1)))
  stagger_state="${stagger_secs}s"
  # TERM and INT both: an operator stop must not be swallowed by this sleep.
  trap 'exit 143' TERM
  trap 'exit 130' INT
  say "docker-entrypoint: join stagger — sleeping ${stagger_secs}s (max ${stagger_max}s) before joining"
  sleep "${stagger_secs}" &
  if ! wait $!; then
    stagger_note="(INCOMPLETE)"
    say "docker-entrypoint: WARNING — stagger sleep failed; joining without the full ${stagger_secs}s" 2
  fi
fi

say "docker-entrypoint: launching bot — url=${MEETING_URL} participant=${BOT_PARTICIPANT} auth=${BOT_AUTH} ttl=${TTL} identity_mode=${BOT_IDENTITY_MODE} hw_concurrency=${BOT_HW_CONCURRENCY:-<omitted>} credentials=${cred_state} control=[${ctl_state}] netem=[${netem_state}] join_stagger=${stagger_state}${stagger_note} camera_cycle=[${camera_cycle_state}]"

# Capability cap. `--hardware-concurrency <N>` (added by the parallel frontend
# task on the `run` command) sets the fake navigator.hardwareConcurrency the bot
# advertises, which caps how many simulcast layers it encodes (10 → 3). Built as
# an array so BOT_HW_CONCURRENCY="" cleanly omits the flag; expanded below with
# the `${arr[@]+...}` guard so an empty array is safe under `set -u` on every
# bash version (see the exec NOTE).
hw_args=()
if [ -n "${BOT_HW_CONCURRENCY}" ]; then
  hw_args=(--hardware-concurrency "${BOT_HW_CONCURRENCY}")
fi

posture_args=()
if [ -n "${BOT_INDEX}" ]; then
  posture_args=(--bot-index "${BOT_INDEX}")
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
  ${posture_args[@]+"${posture_args[@]}"} \
  ${ctl_args[@]+"${ctl_args[@]}"} \
  ${BOT_EXTRA_ARGS:-}
