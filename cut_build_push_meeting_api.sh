#!/bin/bash
set -e

# Registry from environment (set by GitHub variable in workflow)
# Defaults to 'securityunion' for backward compatibility
REGISTRY="${REGISTRY:-securityunion}"

# Tag from argument or git SHA
# Tag from argument or git SHA
TAG="${1:-$(git rev-parse HEAD)}"

IMAGE_URL="${REGISTRY}/securityunion/videocall-meeting-api:$TAG"
echo "Building image "$IMAGE_URL

if ! docker build -t $IMAGE_URL . --file Dockerfile.meeting-api; then
    echo "Failed to build meeting-api"
    exit 1
fi

docker push "$IMAGE_URL"
echo "✓ Pushed ${IMAGE_URL}"
