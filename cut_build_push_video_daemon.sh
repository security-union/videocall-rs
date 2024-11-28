#!/bin/bash
set -e

IMAGE_URL=securityunion/video-call-daemon:staging
echo "Building image $IMAGE_URL"

if ! docker build -t $IMAGE_URL . --file Dockerfile.video-call-daemon; then
    echo "Failed to build docker image"
else
    docker push $IMAGE_URL
    echo "New image uploaded to $IMAGE_URL"
fi
