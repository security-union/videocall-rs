#!/bin/bash
set -e

REGISTRY="${REGISTRY:-securityunion}"

TAG="${1:-$(git rev-parse HEAD)}"

GIT_SHA=$(git rev-parse --short HEAD)
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
BUILD_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# --- Yew UI ---
IMAGE_URL="${REGISTRY}/videocall-web-ui:${TAG}"
echo "Building image ${IMAGE_URL}"
if ! docker build -t "$IMAGE_URL" \
    --build-arg USERS_ALLOWED_TO_STREAM="dario,griffin,hamdy" \
    --build-arg GIT_SHA="$GIT_SHA" \
    --build-arg GIT_BRANCH="$GIT_BRANCH" \
    --build-arg BUILD_TIMESTAMP="$BUILD_TIMESTAMP" \
    -f Dockerfile.yew .; then
    echo "Failed to build yew-ui"
else
    docker push "$IMAGE_URL"
    echo "New image uploaded to ${IMAGE_URL}"
fi

# --- Dioxus UI ---
DIOXUS_IMAGE_URL="${REGISTRY}/videocall-dioxus-ui:${TAG}"
echo "Building image ${DIOXUS_IMAGE_URL}"
if ! docker build -t "$DIOXUS_IMAGE_URL" \
    --build-arg GIT_SHA="$GIT_SHA" \
    --build-arg GIT_BRANCH="$GIT_BRANCH" \
    --build-arg BUILD_TIMESTAMP="$BUILD_TIMESTAMP" \
    -f Dockerfile.dioxus .; then
    echo "Failed to build dioxus-ui"
else
    docker push "$DIOXUS_IMAGE_URL"
    echo "New image uploaded to ${DIOXUS_IMAGE_URL}"
fi
