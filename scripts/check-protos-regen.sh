#!/usr/bin/env bash
# Regenerates Rust protobuf bindings in the pinned Docker build environment and
# fails if committed generated files drift from protobuf/types/*.proto.
#
# Usage: bash scripts/check-protos-regen.sh
set -euo pipefail
export LC_ALL=C

require_tool() {
  local tool="$1"
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "ERROR: check-protos-regen.sh requires '$tool' but it is not on PATH." >&2
    echo "       Proto drift cannot be validated without it; failing closed." >&2
    exit 2
  fi
}

require_tool docker
require_tool git
require_tool id

ROOT_DIR="$(git rev-parse --show-toplevel)"
PROTOBUF_DIR="$ROOT_DIR/protobuf"
PROTO_DST="$ROOT_DIR/videocall-types/src/protos"
IMAGE_TAG_SUFFIX="$(printf '%s-%s-%s' "${GITHUB_RUN_ID:-local}" "${GITHUB_RUN_ATTEMPT:-0}" "$$" | tr '[:upper:]' '[:lower:]' | tr -c 'a-z0-9_.-' '-')"
IMAGE_RUST="protobuf-types-build-env-rust:${IMAGE_TAG_SUFFIX}"
IMAGE_IID_FILE="$(mktemp "${TMPDIR:-/tmp}/check-protos-regen-image.XXXXXX")"
DOCKER_NETWORK="${DOCKER_NETWORK:-bridge}"
BUILD_RUST_DIR="$PROTOBUF_DIR/build/rust"

cleanup() {
  docker image rm "$IMAGE_RUST" >/dev/null 2>&1 || :
  rm -f "$IMAGE_IID_FILE"
}
trap cleanup EXIT

if [[ ! -f "$PROTOBUF_DIR/build-env-rust.Dockerfile" ]]; then
  echo "ERROR: protobuf Rust build Dockerfile not found: $PROTOBUF_DIR/build-env-rust.Dockerfile" >&2
  exit 2
fi
if [[ ! -d "$PROTO_DST" ]]; then
  echo "ERROR: generated proto destination not found: $PROTO_DST" >&2
  exit 2
fi

mkdir -p "$BUILD_RUST_DIR"
find "$BUILD_RUST_DIR" -maxdepth 1 -type f -name '*.rs' -delete

echo "Building pinned protobuf Rust generator image..."
docker build \
  -t "$IMAGE_RUST" \
  --iidfile "$IMAGE_IID_FILE" \
  --build-arg USER="$(id -un)" \
  --build-arg UID="$(id -u)" \
  -f "$PROTOBUF_DIR/build-env-rust.Dockerfile" \
  "$PROTOBUF_DIR"

if [[ ! -s "$IMAGE_IID_FILE" ]]; then
  echo "ERROR: docker build did not write an image ID to $IMAGE_IID_FILE." >&2
  echo "       Refusing to run an ambiguous protobuf generator image." >&2
  exit 2
fi
IMAGE_ID="$(<"$IMAGE_IID_FILE")"
if [[ -z "$IMAGE_ID" ]]; then
  echo "ERROR: docker build wrote an empty image ID to $IMAGE_IID_FILE." >&2
  echo "       Refusing to run an ambiguous protobuf generator image." >&2
  exit 2
fi

echo "Regenerating Rust protobuf bindings in Docker..."
docker run --rm \
  -v "$PROTOBUF_DIR":"$PROTOBUF_DIR" \
  -w "$PROTOBUF_DIR" \
  -u "$(id -un)":"$(id -un)" \
  --network "$DOCKER_NETWORK" \
  "$IMAGE_ID" \
  make generate_rust

generated_count="$(find "$BUILD_RUST_DIR" -maxdepth 1 -type f -name '*.rs' | wc -l | tr -d '[:space:]')"
if [[ "$generated_count" == "0" ]]; then
  echo "ERROR: proto regeneration produced no Rust files in $BUILD_RUST_DIR." >&2
  echo "       Refusing to report success on empty output." >&2
  exit 2
fi

find "$PROTO_DST" -maxdepth 1 -type f -name '*.rs' ! -name 'mod.rs' -delete
cp "$BUILD_RUST_DIR"/*.rs "$PROTO_DST"/

has_tracked_diff=0
if ! git diff --exit-code --quiet -- "$PROTO_DST"; then
  has_tracked_diff=1
fi
untracked_files="$(git ls-files --others --exclude-standard -- "$PROTO_DST")"

if [[ "$has_tracked_diff" == "1" || -n "$untracked_files" ]]; then
  echo "ERROR: generated protobuf Rust files are out of date." >&2
  echo "       Regenerate them and commit the resulting files:" >&2
  echo >&2
  echo "P=\$(git rev-parse --show-toplevel)/protobuf" >&2
  echo "docker build -t protobuf-types-build-env-rust \\" >&2
  echo "  --build-arg USER=\"\$(id -un)\" --build-arg UID=\"\$(id -u)\" \\" >&2
  echo "  -f \"\$P/build-env-rust.Dockerfile\" \"\$P\"" >&2
  echo "docker run --rm -v \"\$P\":\"\$P\" -w \"\$P\" -u \"\$(id -un)\":\"\$(id -un)\" \\" >&2
  echo "  protobuf-types-build-env-rust make generate_rust" >&2
  echo "cp \"\$P\"/build/rust/*.rs videocall-types/src/protos/" >&2
  echo >&2
  echo "Note: do not use 'make build-env-generate-rust' in non-interactive CI;" >&2
  echo "      the Makefile's docker command includes '-it', which fails without a TTY." >&2
  echo >&2
  echo "Changed generated files:" >&2
  {
    git diff --name-only -- "$PROTO_DST"
    if [[ -n "$untracked_files" ]]; then
      printf '%s\n' "$untracked_files"
    fi
  } | sort -u >&2
  echo >&2
  git diff --stat -- "$PROTO_DST" >&2
  if [[ -n "$untracked_files" ]]; then
    echo "Untracked generated files:" >&2
    printf '%s\n' "$untracked_files" >&2
  fi
  exit 1
fi

echo "Generated protobuf Rust files are up to date."
