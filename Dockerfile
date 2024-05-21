# Multi-Stage - Multi-Target Dockerfile.
# The base is rather big, but only needed for build but makes caching a bit easier and faster.
#
# The docker commands over the FROM instruction are merely a suggestion.
# The --load parameter will not work with multi-arch builds.
#
# To create multi-arch builds use following command and substitute the variables accordingly:
#
# docker buildx build \
#   --push \
#   --target $BUILD_TARGET \
#   --tag $OCI_REGISTRY/$IMAGE_NAME:$IMAGE_TAG \
#   --platform $ARCH1,$ARCH2,$ARCH3 .
#
# Examples - Multiarch with remote repository:
#
# docker buildx build \
#   --push \
#   --target rustlemania-base \
#   --tag docker.io/securityunion/rustlemania-base:1.78-slim \
#   --platform linux/amd64,linux/arm64 .
#
# docker buildx build \
#   --push \
#   --target rustlemania \
#   --tag docker.io/securityunion/rustlemania:latest \
#   --platform linux/amd64,linux/arm64 .
#
# The --push parameter is sadly required for multi-arch builds since docker is not able to export multi-arch images to the 
# local image storage.
# 
# The platforms are merely an example, could be different ones, if supported.
#
# Example - Single Arch with local (e.g. for development):
#
# docker buildx build \
#   --load \
#   --target rustlemania-api-build \
#   --tag rustlemania-api-build .
# 
# Those arguments are required to be passed accross multiple stages
ARG BASE_FROM=rustlemania-base
ARG API_FROM=rustlemania-api-build
FROM rust:1.78-slim as rustlemania-base
# Build Args Scoping
ARG BASE_FROM
ARG API_FROM
# git and build-essential is extra since we want the recommends here
RUN apt-get --yes update \
  && apt-get --yes install git build-essential \
  && apt-get --yes install --no-install-recommends \
    pkg-config \
    libssl-dev \
    libvpx-dev \
    libglib2.0-dev \
    libgtk-3-dev \
    libsoup2.4 \
    libjavascriptcoregtk-4.0-dev \
    libclang-dev \
    clang \
    libwebkit2gtk-4.0-dev

RUN ARCH=$(uname -m); \
  [ ${ARCH} = 'x86_64' ] && ARCH=amd64; \
  curl https://github.com/amacneil/dbmate/releases/download/v2.15.0/dbmate-linux-${ARCH} \
  -L -o /usr/local/bin/dbmate \
  && chmod +x /usr/local/bin/dbmate

WORKDIR /usr/local/src
COPY . .
RUN cargo install cargo-watch
RUN rustup component add clippy
RUN rustup component add rustfmt


### Actix API build stage ###
# docker buildx build --target rustlemania-api-build -t rustlemania-api-build --load .
FROM ${BASE_FROM} as rustlemania-api-build
# Build Args Scoping
ARG BASE_FROM
ARG API_FROM

WORKDIR /usr/local/src/actix-api
RUN cargo build --release

### Application Stage ###
# docker buildx build --target rustlemania -t rustlemania --load .
# With args
# docker buildx build --target rustlemania -t rustlemania \
# --build-arg "WEBTRANSPORT_HOST=https://webtransport.rustlemania.com"  \
# --build-arg "ACTIX_UI_BACKEND_URL=wss://api.rustlemania.com" \
# --build-arg "WEBTRANSPORT_ENABLED=false" \
# --load .
FROM debian:bookworm-slim as rustlemania

RUN apt-get --yes update && apt-get --yes install libssl-dev

COPY --from=rustlemania-api-build /usr/local/bin/dbmate /usr/local/bin/dbmate
COPY --from=rustlemania-api-build /usr/local/src/dbmate /usr/share/dbmate
COPY --from=rustlemania-api-build /usr/local/src/target/release/websocket_server /usr/local/bin/websocket_server
COPY --from=rustlemania-api-build /usr/local/src/target/release/webtransport_server /usr/local/bin/webtransport_server
COPY --from=rustlemania-api-build /usr/local/src/actix-api/startup.sh /usr/local/bin/startup.sh

WORKDIR /usr/local/bin

STOPSIGNAL SIGINT

