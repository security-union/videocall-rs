#!/bin/bash
set -e

TAG=$1
if [ -z "$1" ]
then
    TAG=$(git rev-parse HEAD)
fi

IMAGE_URL=securityunion/rustlemania-ui:$TAG
echo "Building image "$IMAGE_URL

if ! docker build -t $IMAGE_URL --build-arg USERS_ALLOWED_TO_STREAM="dario,griffin,hamdy" . --file Dockerfile.yew; then
    echo "Failed to build server_rust"
else
    docker push $IMAGE_URL
    echo "New image uploaded to "$IMAGE_URL
fi
