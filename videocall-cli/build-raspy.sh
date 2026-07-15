#!/bin/bash -e

TARGET=aarch64-unknown-linux-gnu

sudo apt update
# libclang-dev/llvm: required by the V4L camera bindings (v4l2-sys-mit runs bindgen).
sudo apt install -y libclang-dev libv4l-dev llvm gcc-aarch64-linux-gnu libssl-dev
rustup target add aarch64-unknown-linux-gnu

# build binary
cargo build --release --target $TARGET
