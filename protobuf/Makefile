SHELL := /bin/bash

SRC_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
REPO_ROOT := $(abspath $(SRC_DIR)/..)

# Rust protobuf codegen, natively via nix (no containers): protoc + the
# pinned protoc-gen-rs plugin (protobuf-codegen 3.7.1) come from the repo's
# nix shell, so regenerated files are byte-stable across machines.
build/rust:
	mkdir -p build/rust

.PHONY: generate_rust
generate_rust: build/rust
	nix-shell $(REPO_ROOT)/default.nix -A shells.protobuf \
		--run "cd $(SRC_DIR) && protoc --rs_out=build/rust types/*.proto"

.PHONY: build
build: generate_rust
	mv build/rust/* ../videocall-types/src/protos
