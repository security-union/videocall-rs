#!/bin/bash
set -e

# Docker build context is the repo root; this script lives in scripts/
SCRIPTPATH="$( cd -- "$(dirname "$0")" >/dev/null 2>&1 ; pwd -P )"
cd "$( dirname "$SCRIPTPATH" )"

IMAGE_URL=securityunion/videocall-cli:staging
echo "Building image $IMAGE_URL"

if ! docker build -t $IMAGE_URL . --file Dockerfile.videocall-cli; then
    echo "Failed to build docker image"
else
    docker push $IMAGE_URL
    echo "New image uploaded to $IMAGE_URL"
fi
