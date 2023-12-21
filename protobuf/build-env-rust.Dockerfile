FROM rust:1.72-slim
ENV DEBIAN_FRONTEND=noninteractive
ARG USER
ARG UID

RUN apt-get update && \
    apt-get -y install sudo \
        build-essential \
        gnupg \
        curl \
        protobuf-compiler

RUN cargo install protobuf-codegen --vers 3.3.0

RUN useradd --create-home $USER --uid $UID && \
        adduser $USER sudo && \
        sed -i "s/\%sudo.*/%sudo ALL=(ALL) NOPASSWD: ALL/" /etc/sudoers
