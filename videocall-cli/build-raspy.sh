#!/bin/bash -e

TARGET=aarch64-unknown-linux-gnu

sudo apt update
# NOTE: libclang-dev/llvm were required by env-libvpx-sys (bindgen) to build C
# libvpx from source. VP9 encoding is now pure Rust, so libvpx itself is gone,
# but do NOT drop libclang-dev/llvm blindly: other -sys crates in the build may
# still invoke bindgen (e.g. V4L bindings). Verify the cross build without them
# before removing.
sudo apt install -y libclang-dev libv4l-dev llvm gcc-aarch64-linux-gnu libssl-dev
rustup target add aarch64-unknown-linux-gnu

# build binary
cargo build --release --target $TARGET
