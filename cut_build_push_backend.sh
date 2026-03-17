#!/bin/bash
set -e

REGISTRY="${REGISTRY:-securityunion}"

TAG="${1:-$(git rev-parse HEAD)}"

GIT_SHA=$(git rev-parse --short HEAD)
GIT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
BUILD_TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

IMAGE_URL="${REGISTRY}/videocall-media-server:${TAG}"
echo "Building image ${IMAGE_URL}"

if ! docker build -t "$IMAGE_URL" \
    --build-arg GIT_SHA="$GIT_SHA" \
    --build-arg GIT_BRANCH="$GIT_BRANCH" \
    --build-arg BUILD_TIMESTAMP="$BUILD_TIMESTAMP" \
    -f Dockerfile.actix .; then
    echo "Failed to build videocall-media-server"
    exit 1
fi

docker push "$IMAGE_URL"
echo "Pushed ${IMAGE_URL}"
