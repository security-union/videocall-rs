#!/bin/bash -e

TARGET=aarch64-unknown-linux-gnu

sudo apt update
sudo apt install -y libclang-dev libv4l-dev llvm gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu

# build binary
cargo build --release --target $TARGET
