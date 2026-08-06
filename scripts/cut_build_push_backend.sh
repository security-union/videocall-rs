#!/bin/bash
set -e

# This script lives in scripts/; run from the repo root
SCRIPTPATH="$( cd -- "$(dirname "$0")" >/dev/null 2>&1 ; pwd -P )"
cd "$( dirname "$SCRIPTPATH" )"

REGISTRY="${REGISTRY:-securityunion}"

TAG="${1:-$(git rev-parse HEAD)}"

GIT_SHA=$(git rev-parse --short HEAD)
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
BUILD_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

IMAGE_URL="${REGISTRY}/videocall-media-server:${TAG}"
echo "Building image ${IMAGE_URL} via nix (release.nix -A images.media-server)"

# nix.dev flow: docker load < $(nix-build …). NOTE: on macOS the payload is
# cross-compiled for the *host* Docker arch (arm64 on Apple silicon) — build
# from a Linux/amd64 host or CI for amd64 clusters.
script=$(nix-build release.nix -A images.media-server --no-out-link \
    --argstr gitSha "$GIT_SHA" \
    --argstr gitBranch "$GIT_BRANCH" \
    --argstr buildTimestamp "$BUILD_TIMESTAMP")
"$script" | docker load
docker tag videocall/media-server:dev "$IMAGE_URL"
docker push "$IMAGE_URL"
echo "New image uploaded to ${IMAGE_URL}"
