SHELL := /bin/bash
UID := $(shell id -u)
USER := $(shell id -un)

ifeq ($(UID), 0)
$(error Do not run as root)
endif

SRC_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))
IMAGE := protobuf-types-build-env
IMAGE_RUST := protobuf-types-build-env-rust
DOCKER_NETWORK ?= bridge
DOCKER_RUN_CMD := docker run -it -v $(SRC_DIR):$(SRC_DIR) -w $(SRC_DIR) -u $(USER):$(USER) --network $(DOCKER_NETWORK) $(IMAGE)
DOCKER_RUN_RUST_CMD := docker run -it -v $(SRC_DIR):$(SRC_DIR) -w $(SRC_DIR) -u $(USER):$(USER) --network $(DOCKER_NETWORK) $(IMAGE_RUST)

export DOCKER_BUILDKIT = 1

build/rust:
	mkdir -p build/rust

.PHONY: generate_rust
generate_rust: build/rust
	protoc --rust_out=build/rust types/*.proto

.PHONY: build-env-rust
build-env-rust:
	# We don't tag so that we can use generate `latest` tag which makes
	# referencing easier
	docker build -t $(IMAGE_RUST) --build-arg USER=$(USER) --build-arg UID=$(UID) -f build-env-rust.Dockerfile .

build-env-generate-rust:
	$(DOCKER_RUN_RUST_CMD) make generate_rust
	mv build/rust/* ../types/src/protos/rust/

.PHONY: build
build:
	make build-env-rust
	make build-env-generate-rust