#!/bin/bash
set -e

REGISTRY="${REGISTRY:-securityunion}"

# Tag from argument or git SHA
TAG="${1:-$(git rev-parse HEAD)}"

IMAGE_URL="${REGISTRY}/videocall-web-ui:${TAG}"
echo "Building image ${IMAGE_URL}"

if ! docker build -t "$IMAGE_URL" \
    --build-arg USERS_ALLOWED_TO_STREAM="dario,griffin,hamdy" \
    -f Dockerfile.yew .; then
    echo "Failed to build videocall-web-ui"
    exit 1
fi

docker push "$IMAGE_URL"
echo "✓ Pushed ${IMAGE_URL}"
