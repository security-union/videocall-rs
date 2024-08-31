TAG=$(git rev-parse --short HEAD)
FROM=type=local,src=/var/lib/docker/volumes/buildx_buildkit_jovial_mcclintock0_state
TO=type=local,dest=cachebuildx_buildkit_jovial_mcclintock0_state
echo "Building and pushing docker image with tag $TAG"
docker buildx create --use
docker buildx build \
--platform linux/arm64 \
-f ../docker/Dockerfile.website \
--cache-from=$FROM \
--cache-to=$TO \
-t securityunion/video-call-rs-website \
--load . 
# docker push securityunion/video-call-rs-website:$TAG