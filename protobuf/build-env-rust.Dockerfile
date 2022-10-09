# syntax = docker/dockerfile:1.2
FROM rust:1.62-slim
RUN rm -f /etc/apt/apt.conf.d/docker-clean; echo 'Binary::apt::APT::Keep-Downloaded-Packages "true";' > /etc/apt/apt.conf.d/keep-cache
ENV DEBIAN_FRONTEND=noninteractive
ARG USER
ARG UID

RUN --mount=type=cache,target=/var/cache/apt,id=focal-var-cache-apt --mount=type=cache,target=/var/lib/apt,id=focal-var-lib-apt \
    apt-get update && \
    apt-get -y install sudo \
        build-essential \
        gnupg \
        curl \
        protobuf-compiler

RUN cargo install protobuf-codegen --vers 3.2.0

RUN useradd --create-home $USER --uid $UID && \
        adduser $USER sudo && \
        sed -i "s/\%sudo.*/%sudo ALL=(ALL) NOPASSWD: ALL/" /etc/sudoers
