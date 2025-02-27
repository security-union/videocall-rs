#!/bin/bash
set -e

IMAGE_URL=securityunion/videocall-cli:staging
echo "Building image $IMAGE_URL"

if ! docker build -t $IMAGE_URL . --file Dockerfile.videocall-cli; then
    echo "Failed to build docker image"
else
    docker push $IMAGE_URL
    echo "New image uploaded to $IMAGE_URL"
fi
