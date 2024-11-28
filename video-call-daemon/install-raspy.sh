#!/bin/bash -e

USER=ubuntu
PI_IP=192.168.18.39
TARGET=aarch64-unknown-linux-gnu

# upload binary
ssh-copy-id $USER@$PI_IP
scp -r ./target/$TARGET/release/video-call-daemon $USER@$PI_IP:/tmp/