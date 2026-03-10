#!/bin/bash
set -e

REGISTRY="${REGISTRY:-securityunion}"

# Tag from argument or git SHA
TAG="${1:-$(git rev-parse HEAD)}"


# --- Dioxus UI ---
DIOXUS_IMAGE_URL=${REGISTRY}/videocall-dioxus-ui:$TAG
echo "Building image $DIOXUS_IMAGE_URL"
if ! docker build -t $DIOXUS_IMAGE_URL . --file Dockerfile.dioxus; then
    echo "Failed to build dioxus-ui"
    exit 1
else
    docker push $DIOXUS_IMAGE_URL
    echo "New image uploaded to $DIOXUS_IMAGE_URL"
fi
