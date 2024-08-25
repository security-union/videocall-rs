TAG=$(git rev-parse --short HEAD)
echo "Building and pushing docker image with tag $TAG"
docker build -v -f ../docker/Dockerfile.website -t securityunion/video-call-rs-website:$TAG .
docker push securityunion/video-call-rs-website:$TAG