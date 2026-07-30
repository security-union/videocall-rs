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
"${BUILD[@]}" \
  --platform linux/amd64 \
  -t "${IMAGE_TAGGED}" \
  -t "${IMAGE_LATEST}" \
  -f "${DOCKERFILE}" \
  "${REPO_ROOT}"

echo "==> Built and tagged:"
echo "    ${IMAGE_TAGGED}"
echo "    ${IMAGE_LATEST}"

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
  echo "==> Pushing ${IMAGE_TAGGED} and ${IMAGE_LATEST}"
  "${PUSH_CMD[@]}" "${IMAGE_TAGGED}"
  "${PUSH_CMD[@]}" "${IMAGE_LATEST}"
  echo "==> Pushed. Update k8s/bot-pod.yaml image: to ${IMAGE_TAGGED}"
else
  echo "==> PUSH not set (build-only). To publish:"
  echo "    ${PUSH_CMD[*]} ${IMAGE_TAGGED}"
  echo "    ${PUSH_CMD[*]} ${IMAGE_LATEST}"
  echo "    then set k8s/bot-pod.yaml image: to ${IMAGE_TAGGED}"
fi
