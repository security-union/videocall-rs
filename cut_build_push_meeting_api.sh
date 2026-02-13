#!/bin/bash
set -e

TAG=$1
if [ -z "$1" ]
then
    TAG=$(git rev-parse HEAD)
fi

IMAGE_URL="jboyd01/videocall-meeting-api:$TAG"
echo "Building image $IMAGE_URL"

if ! docker build -t "$IMAGE_URL" . --file Dockerfile.meeting-api; then
    echo "Failed to build meeting-api"
else
    docker push "$IMAGE_URL"
    echo "New image uploaded to $IMAGE_URL"
fi
