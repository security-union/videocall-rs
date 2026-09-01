#!/usr/bin/env bash
#
# build.sh — build + tag the bots-app image for the Harbor registry (#2035/#2072).
#
# Authoring note: this script is provided for the orchestrator to run. It does
# NOT push by default (set PUSH=1 to enable). The build CONTEXT is the repo
# root (the repo-root .dockerignore keeps it lean); the Dockerfile lives at
# e2e/bots-app/Dockerfile.
#
# ── Registry: Harbor `hclcr.io/hcllabs` (same as all videocall images) ───────
# Pushes go to hclcr.io/hcllabs/videocall-bots-app. Auth = the same Harbor
# account CI uses (HARBOR_USERNAME / HARBOR_PASSWORD → `docker login hclcr.io`).
# Pods pull with an `hclcr-io` imagePullSecret, reused from the existing
# hclcr-io dockerconfigjson secrets already on the cluster (copy one into the
# target namespace). Keep REGISTRY/IMAGE_NAME in lockstep with k8s/bot-pod.yaml.
#
# ── Remote builder ──────────────────────────────────────────────────────────
# The qsk8s CI agents build via `podman --remote` over SSH to the shared
# builder jenkins@10.190.112.123 (socket /run/user/1003/podman/podman.sock).
# Set PODMAN_REMOTE=1 to use it (requires the builder SSH key + a system
# connection). Otherwise a local `docker`/`podman` build is used.

set -euo pipefail

# Without the exit, bash resumes past an interrupt and reports a ref to pin.
trap 'exit 130' INT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DOCKERFILE="${SCRIPT_DIR}/Dockerfile"

# ── Configurable inputs (override via env) ──────────────────────────────────
REGISTRY="${REGISTRY:-hclcr.io}"
IMAGE_NAME="${IMAGE_NAME:-hcllabs/videocall-bots-app}"
VERSION="${VERSION:-0.1.0}"
GIT_SHA="$(git -C "${REPO_ROOT}" rev-parse --short=7 HEAD 2>/dev/null || echo nogit)"
DATE="$(date -u +%Y%m%d)"
TAG="${TAG:-${VERSION}-${DATE}-${GIT_SHA}}"

IMAGE="${REGISTRY}/${IMAGE_NAME}"
IMAGE_TAGGED="${IMAGE}:${TAG}"
IMAGE_LATEST="${IMAGE}:latest"

PUSH_LATEST="${PUSH_LATEST:-1}"
case "${PUSH_LATEST}" in
0 | 1) ;;
*)
  echo "PUSH_LATEST must be 0 or 1, got: ${PUSH_LATEST}" >&2
  exit 1
  ;;
esac

# ── Builder selection ───────────────────────────────────────────────────────
PODMAN_REMOTE="${PODMAN_REMOTE:-0}"
if [ "${PODMAN_REMOTE}" = "1" ]; then
  # Assumes a preconfigured podman system connection (e.g. `conn01`) or the
  # CI-style inline setup. See the QS-CI onboarding runbook.
  BUILD=(podman --remote build)
  PUSH_CMD=(podman --remote push)
  LOGIN_CMD=(podman --remote login)
elif command -v podman >/dev/null 2>&1; then
  BUILD=(podman build)
  PUSH_CMD=(podman push)
  LOGIN_CMD=(podman login)
else
  BUILD=(docker build)
  PUSH_CMD=(docker push)
  LOGIN_CMD=(docker login)
fi

echo "==> Building ${IMAGE_TAGGED}"
echo "    context:    ${REPO_ROOT}"
echo "    dockerfile: ${DOCKERFILE}"
echo "    builder:    ${BUILD[*]}"

# --platform linux/amd64 is pinned so a build from an arm64 host (e.g. an
# Apple-silicon Mac using the local docker/podman fallback) can't publish an
# arm64 image that CrashLoopBackOffs on the cluster's amd64 nodes with
# "exec format error". The remote podman builder is already amd64, so this is
# a no-op there and a safety net for the local path.
TAGS=(-t "${IMAGE_TAGGED}")
if [ "${PUSH_LATEST}" = "1" ]; then
  TAGS+=(-t "${IMAGE_LATEST}")
fi

"${BUILD[@]}" \
  --platform linux/amd64 \
  "${TAGS[@]}" \
  -f "${DOCKERFILE}" \
  "${REPO_ROOT}"

echo "==> Built and tagged:"
echo "    ${IMAGE_TAGGED}"
if [ "${PUSH_LATEST}" = "1" ]; then
  echo "    ${IMAGE_LATEST}"
fi

# Guard: the `setcap cap_net_admin+eip` xattrs must survive the build — the
# Dockerfile's own re-read runs inside the setcap layer and cannot see a backend
# dropping them at layer-commit. Binary list locked to the Dockerfile's by
# src/docker-entrypoint.test.ts; `readlink -f` runs in-image (getcap won't follow).
#
# The image sets an ENTRYPOINT (the bot launcher) and no CMD, so a bare
# `<runtime> run IMAGE getcap …` would APPEND getcap as args to the entrypoint
# (which ignores argv and aborts on its login preflight) instead of running
# getcap — the check would then warn on every build. `--entrypoint sh`
# actually invokes it. Derive the runtime from BUILD by dropping its trailing
# `build` subcommand, so the PODMAN_REMOTE path inspects the image on the SAME
# (remote) daemon that built it rather than a local one that lacks it.
RUNTIME=("${BUILD[@]:0:${#BUILD[@]}-1}")
if "${RUNTIME[@]}" run --rm --entrypoint sh "${IMAGE_TAGGED}" -c \
  'for b in /usr/sbin/tc /usr/sbin/ip /usr/local/bin/netem-setpriv; do
     p="$(readlink -f "$b")"
     getcap "$p" | grep -q cap_net_admin || { echo "MISSING cap_net_admin: $b"; exit 1; }
   done' 2>/dev/null; then
  echo "==> OK: every shaping binary retains cap_net_admin+eip"
else
  echo "==> FATAL: cap_net_admin missing from a shaping binary in the built image (path above)." >&2
  echo "==> A shaped pod (BOT_NETEM_PROFILE) would crash-loop on netem_fatal. Refusing to tag this build." >&2
  exit 1
fi

# ── Push (opt-in) ────────────────────────────────────────────────────────────
# Enable with PUSH=1. Auth uses the Harbor push user (HARBOR_USERNAME; never
# commit the password; pass it via env / stdin).
#   Example (Harbor — same creds CI uses):
#     REGISTRY_USER="$HARBOR_USERNAME" REGISTRY_PASS="$HARBOR_PASSWORD" \
#     PUSH=1 ./build.sh
if [ "${PUSH:-0}" = "1" ]; then
  if [ -n "${REGISTRY_USER:-}" ] && [ -n "${REGISTRY_PASS:-}" ]; then
    echo "==> Logging in to ${REGISTRY} as ${REGISTRY_USER}"
    printf '%s' "${REGISTRY_PASS}" | "${LOGIN_CMD[@]}" "${REGISTRY}" -u "${REGISTRY_USER}" --password-stdin
  else
    echo "==> No REGISTRY_USER/REGISTRY_PASS set; assuming an existing login session."
  fi
  echo "==> Pushing ${IMAGE_TAGGED}"
  "${PUSH_CMD[@]}" "${IMAGE_TAGGED}"
  if [ "${PUSH_LATEST}" = "1" ]; then
    echo "==> Pushing ${IMAGE_LATEST}"
    "${PUSH_CMD[@]}" "${IMAGE_LATEST}"
  else
    echo "==> PUSH_LATEST=0: leaving :latest where it is"
  fi
  # The digest to pin is the one the REGISTRY serves for this tag.
  DIGEST=""
  if command -v skopeo >/dev/null 2>&1; then
    SKOPEO_AUTH=()
    if [ -n "${REGISTRY_USER:-}" ] && [ -n "${REGISTRY_PASS:-}" ] &&
      SKOPEO_AUTHDIR="$(mktemp -d)"; then
      # skopeo fatals on a zero-length authfile, so create the dir and not the file.
      trap 'rm -rf "${SKOPEO_AUTHDIR}"' EXIT
      if printf '%s' "${REGISTRY_PASS}" |
        skopeo login "${REGISTRY}" -u "${REGISTRY_USER}" --password-stdin \
          --authfile "${SKOPEO_AUTHDIR}/auth.json" >/dev/null; then
        SKOPEO_AUTH=(--authfile "${SKOPEO_AUTHDIR}/auth.json")
      else
        echo "==> WARNING: skopeo login failed" >&2
      fi
    fi
    DIGEST="$(skopeo inspect --retry-times 3 ${SKOPEO_AUTH[@]+"${SKOPEO_AUTH[@]}"} \
      --format '{{.Digest}}' "docker://${IMAGE_TAGGED}" || true)"
    [[ "${DIGEST}" =~ ^sha256:[0-9a-f]{64}$ ]] || DIGEST=""
  fi
  echo "==> Pushed. This tag is reused by a later build at the same HEAD on the same"
  echo "    day, so pin it BY DIGEST in ALL of"
  echo "    k8s/{statefulset,bot-pod,conductor-job}.yaml, then re-warm the fleet with"
  echo "    ./k8s/prepull-image.sh (which refuses to run if the three disagree):"
  if [ -n "${DIGEST}" ]; then
    echo "      image: ${IMAGE_TAGGED}@${DIGEST}"
    echo "    or move all three at once:"
    echo "      ./k8s/repin.sh ${IMAGE_TAGGED}@${DIGEST}"
    [ -z "${PINNED_REF_FILE:-}" ] || printf '%s\n' "${IMAGE_TAGGED}@${DIGEST}" >"${PINNED_REF_FILE}"
  else
    echo "      image: ${IMAGE_TAGGED}@sha256:<digest>"
    echo "    Could not read the registry digest (skopeo absent, unauthenticated, no"
    echo "    egress to ${REGISTRY}, or the tag not yet readable). Read it from Harbor's"
    echo "    UI or:"
    echo "    skopeo inspect --format '{{.Digest}}' docker://${IMAGE_TAGGED}"
    [ -z "${PINNED_REF_FILE:-}" ] || exit 1
  fi
else
  echo "==> PUSH not set (build-only). To publish:"
  echo "    ${PUSH_CMD[*]} ${IMAGE_TAGGED}"
  if [ "${PUSH_LATEST}" = "1" ]; then
    echo "    ${PUSH_CMD[*]} ${IMAGE_LATEST}"
  fi
  echo "    The push prints the digest to pin; set image: to ${IMAGE_TAGGED}@<digest>"
  echo "    in ALL of k8s/{statefulset,bot-pod,conductor-job}.yaml"
fi
