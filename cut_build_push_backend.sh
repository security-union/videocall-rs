#!/bin/bash
set -e

REGISTRY="${REGISTRY:-securityunion}"

# Tag from argument or git SHA
TAG="${1:-$(git rev-parse HEAD)}"

IMAGE_URL="${REGISTRY}/videocall-media-server:${TAG}"
echo "Building image ${IMAGE_URL}"

if ! docker build -t "$IMAGE_URL" -f Dockerfile.actix .; then
    echo "Failed to build videocall-media-server"
    exit 1
fi

docker push "$IMAGE_URL"
echo "✓ Pushed ${IMAGE_URL}"
