FROM rust:1.82-slim
ENV DEBIAN_FRONTEND=noninteractive
ARG USER
ARG UID

RUN apt-get update && \
    apt-get -y install sudo \
        build-essential \
        gnupg \
        curl \
        protobuf-compiler

# Pinned to 3.7.2 to match the `protobuf` runtime crate the workspace depends on
# (Cargo.lock). 3.7.2's dependency tree (home@0.5.11) requires rustc >= 1.81, so
# the base image above is rust:1.82. Regenerating with the older 3.7.1 codegen
# would emit a VERSION_3_7_1 check that mismatches the 3.7.2 runtime crate.
RUN cargo install protobuf-codegen --vers 3.7.2 --locked

RUN useradd --create-home $USER --uid $UID && \
        adduser $USER sudo && \
        sed -i "s/\%sudo.*/%sudo ALL=(ALL) NOPASSWD: ALL/" /etc/sudoers
