#!/bin/bash
set -e

REGISTRY="${REGISTRY:-securityunion}"

TAG="${1:-$(git rev-parse HEAD)}"

GIT_SHA=$(git rev-parse --short HEAD)
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
BUILD_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# --- Dioxus UI ---
DIOXUS_IMAGE_URL="${REGISTRY}/videocall-dioxus-ui:${TAG}"
echo "Building image ${DIOXUS_IMAGE_URL}"
if ! docker build -t "$DIOXUS_IMAGE_URL" \
    --build-arg GIT_SHA="$GIT_SHA" \
    --build-arg GIT_BRANCH="$GIT_BRANCH" \
    --build-arg BUILD_TIMESTAMP="$BUILD_TIMESTAMP" \
    -f Dockerfile.dioxus .; then
    echo "Failed to build dioxus-ui"
    exit 1
fi

docker push "$DIOXUS_IMAGE_URL"
echo "Pushed ${DIOXUS_IMAGE_URL}"
