#!/bin/bash
#
# Build the pure-Rust VP9 codec as an Apple .xcframework with Swift bindings.
#
# Pure Rust only: no libvpx, no C. Builds with `--features uniffi` (never the
# `native`/`libvpx` features) so the resulting static library has zero C deps.
# Minimum deployment target: iOS 15 / macOS 12.
#
# Targets: aarch64-apple-ios (device), aarch64-apple-ios-sim (simulator),
#          aarch64-apple-darwin (macOS, incl. Mac Catalyst hosts).
set -euo pipefail

echo "Building videocall-codecs for iOS and macOS (pure Rust, no libvpx)..."

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR/.." && pwd )"
cd "$ROOT_DIR"

# The static library produced by crate-type = ["staticlib", ...].
LIB_NAME="libvideocall_codecs.a"
FEATURES="uniffi"
OUT_SWIFT="target/swift-codecs"

mkdir -p "$OUT_SWIFT/include"

# Minimum OS versions (iOS 15 per the codec's baseline).
export IPHONEOS_DEPLOYMENT_TARGET=15.0
export MACOSX_DEPLOYMENT_TARGET=12.0

echo "Building for iOS device (arm64)..."
RUSTFLAGS="-C link-arg=-mios-version-min=15.0" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target aarch64-apple-ios

echo "Building for iOS simulator (arm64)..."
RUSTFLAGS="-C link-arg=-mios-simulator-version-min=15.0" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target aarch64-apple-ios-sim

echo "Building for macOS (arm64)..."
RUSTFLAGS="-C link-arg=-mmacosx-version-min=12.0" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target aarch64-apple-darwin

echo "Generating Swift bindings..."
cargo run -p videocall-codecs --no-default-features --features "$FEATURES" \
  --bin uniffi-bindgen -- generate \
  --library "target/aarch64-apple-ios/release/$LIB_NAME" \
  --language swift --out-dir "$OUT_SWIFT"

# Collect the generated FFI header(s) and modulemap into an include dir. UniFFI
# emits one `*FFI.h` + one `*FFI.modulemap` per crate; xcframework wants a
# single `module.modulemap`.
echo "Assembling module map..."
cp "$OUT_SWIFT"/*FFI.h "$OUT_SWIFT/include/"
cat "$OUT_SWIFT"/*FFI.modulemap > "$OUT_SWIFT/include/module.modulemap"

# Ad-hoc sign each static library.
for target in aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin; do
  codesign -s - "target/$target/release/$LIB_NAME"
done

echo "Creating XCFramework..."
rm -rf target/VideocallCodecs.xcframework
xcodebuild -create-xcframework \
  -library "target/aarch64-apple-ios/release/$LIB_NAME" \
  -headers "$OUT_SWIFT/include" \
  -library "target/aarch64-apple-ios-sim/release/$LIB_NAME" \
  -headers "$OUT_SWIFT/include" \
  -library "target/aarch64-apple-darwin/release/$LIB_NAME" \
  -headers "$OUT_SWIFT/include" \
  -output target/VideocallCodecs.xcframework

echo ""
echo "=== Build completed successfully ==="
echo "XCFramework:    $ROOT_DIR/target/VideocallCodecs.xcframework"
echo "Swift bindings: $ROOT_DIR/$OUT_SWIFT/videocall_codecs.swift"
echo ""
echo "To use in Swift:"
echo "  1. Drag VideocallCodecs.xcframework into your Xcode target."
echo "  2. Add $OUT_SWIFT/videocall_codecs.swift to your sources."
echo "  3. import videocall_codecs (the generated module)."
