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
#   --target rustlemania-ui-build \
#   --tag rustlemania-ui-build .
# 
# Also take note of building the UI for multi-arch. Since UI is WASM and thus specific bytecode, 
# it is not required to build the UI for multiple platforms, which is very time intensive.
# The whole build-chain with multi-arch and only building the WASM once can be achieved like this:
#
# 1. Create a local build
# 
# docker buildx build --load --target rustlemania-ui-build --progress=plain --tag rustlemania-ui-build .
#
# 2. Create multiarch
#
# docker buildx build --platform linux/amd64,linux/arm64 --push --tag registry.stack.zeronone.io/rustlemania:latest .
#
# Those arguments are required to be passed accross multiple stages
ARG BASE_FROM=rustlemania-base
ARG API_FROM=rustlemania-api-build
ARG UI_FROM=rustlemania-ui-build
ARG ACTIX_UI_BACKEND_URL="wss://api.rustlemania.com"
ARG WEBTRANSPORT_HOST="https://transport.rustlemania.com"
ARG WEBTRANSPORT_ENABLED="false"
ARG ENABLE_OAUTH="false"
ARG LOGIN_URL=""
ARG E2EE_ENABLED="false"
ARG USERS_ALLOWED_TO_STREAM=""
FROM rust:1.78-slim as rustlemania-base
# Build Args Scoping
ARG BASE_FROM
ARG API_FROM
ARG UI_FROM
ARG ACTIX_UI_BACKEND_URL
ARG WEBTRANSPORT_HOST
ARG WEBTRANSPORT_ENABLED
ARG ENABLE_OAUTH
ARG LOGIN_URL
ARG E2EE_ENABLED
ARG USERS_ALLOWED_TO_STREAM
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

WORKDIR /usr/local/src
COPY . .

### Actix API build stage ###
# docker buildx build --target rustlemania-api-build -t rustlemania-api-build --load .
FROM ${BASE_FROM} as rustlemania-api-build
# Build Args Scoping
ARG BASE_FROM
ARG API_FROM
ARG UI_FROM
ARG ACTIX_UI_BACKEND_URL
ARG WEBTRANSPORT_HOST
ARG WEBTRANSPORT_ENABLED
ARG ENABLE_OAUTH
ARG LOGIN_URL
ARG E2EE_ENABLED
ARG USERS_ALLOWED_TO_STREAM

RUN ARCH=$(uname -m); \
  [ ${ARCH} = 'x86_64' ] && ARCH=amd64; \
  curl https://github.com/amacneil/dbmate/releases/download/v2.15.0/dbmate-linux-${ARCH} \
  -L -o /usr/local/bin/dbmate \
  && chmod +x /usr/local/bin/dbmate

WORKDIR /usr/local/src/actix-api
RUN cargo build --release

### Yew build stage ###
# docker buildx build --target rustlemania-ui-build -t rustlemania-ui-build --load .
FROM ${BASE_FROM} as rustlemania-ui-build
# Build Args Scoping
ARG BASE_FROM
ARG API_FROM
ARG UI_FROM
ARG ACTIX_UI_BACKEND_URL
ARG WEBTRANSPORT_HOST
ARG WEBTRANSPORT_ENABLED
ARG ENABLE_OAUTH
ARG LOGIN_URL
ARG E2EE_ENABLED
ARG USERS_ALLOWED_TO_STREAM

# WEBTRANSPORT_HOST, WEBSOCKET_HOST, WEBTRANSPORT_ENABLED
# Dario: Required by compile time
# TODO: Make yew-ui load a config for these values with defaults
ENV ENABLE_OAUTH=${ENABLE_OAUTH}
ENV LOGIN_URL=${LOGIN_URL}
ENV ACTIX_UI_BACKEND_URL=${ACTIX_UI_BACKEND_URL}
ENV WEBTRANSPORT_HOST=${WEBTRANSPORT_HOST}
ENV WEBTRANSPORT_ENABLED=${WEBTRANSPORT_ENABLED}
ENV E2EE_ENABLED=${E2EE_ENABLED}
ENV USERS_ALLOWED_TO_STREAM=${USERS_ALLOWED_TO_STREAM}

WORKDIR /usr/local/src/yew-ui

# This is done to allow multi-platform builds to skip compilation of WASM bytecode
RUN --mount=type=cache,target=/usr/local/src/yew-ui/dist \
  DIST_CONTENT="$(ls -L /usr/local/src/yew-ui/dist)" \
  && [ -z "${DIST_CONTENT}" ] \
  && rustup default nightly-2023-12-13 \
  && cargo install wasm-bindgen-cli --version 0.2.87 \
  && cargo install trunk --version 0.17.5 \
  && rustup target add wasm32-unknown-unknown \
  && trunk build --release || true

RUN mkdir -p /srv/rustlemania-ui
RUN --mount=type=cache,target=/usr/local/src/yew-ui/dist cp -ar /usr/local/src/yew-ui/dist/. /srv/rustlemania-ui/


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
# The reasons to include the UI is following: 
# - The SPA could be mounted from a webserver container (/srv/rustlemania-ui:/var/www/html - smthlt)
# - use the rustlemania-api as a source for a copy instruction (COPY --from=rustlemania /srv/rustlemania-ui/. /var/www/html)
COPY --from=rustlemania-ui-build /srv/rustlemania-ui/ /srv/rustlemania-ui/
VOLUME /srv/rustlemania-ui

WORKDIR /usr/local/bin

STOPSIGNAL SIGINT

CMD [ "startup.sh" ]
