TAG=$(git rev-parse --short HEAD)
echo "Building and pushing docker image with tag $TAG"
docker buildx create --use
docker build -f ../docker/Dockerfile.website -t securityunion/video-call-rs-website:$TAG .
# docker buildx build --platform linux/amd64,linux/arm64 -f ../docker/Dockerfile.website -t securityunion/video-call-rs-website:$TAG .
# docker push securityunion/video-call-rs-website:$TAG