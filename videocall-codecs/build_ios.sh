#!/bin/bash
#
# Build the pure-Rust VP9 codec as an Apple .xcframework with Swift bindings.
#
# Pure Rust only: no libvpx, no C. Builds with `--features uniffi` (never the
# `native`/`libvpx` features) so the resulting static library has zero C deps.
#
# Slices produced (see README-ios.md for the full rationale):
#   * ios-arm64               aarch64-apple-ios          device       (stable, Tier 2)
#   * ios-arm64-simulator     aarch64-apple-ios-sim      simulator    (stable, Tier 2)
#   * ios-arm64_x86_64-maccatalyst  arm64+x86_64-apple-ios-macabi  Catalyst  (stable, Tier 2)
#   * macos-arm64_x86_64      arm64+x86_64-apple-darwin  macOS        (stable, Tier 2)
#   * watchos                 arm64_32 + arm64           device (fat) (NIGHTLY, Tier 3)
#   * watchos-simulator       aarch64-apple-watchos-sim  simulator    (NIGHTLY, Tier 3)
#
# Mac Catalyst (`-macabi`) is a distinct slice from native macOS: a Catalyst app
# (SUPPORTS_MACCATALYST=YES) links the iOS-on-Mac ABI, which native macos-arm64
# does NOT provide. Both slices coexist in the xcframework.
#
# Deployment targets: iOS 15 / macOS 12 / watchOS 10.
#
# watchOS is a Tier-3 Rust target: it has no prebuilt `std`, so those slices are
# cross-compiled with a NIGHTLY toolchain and `-Z build-std`. iOS/macOS use the
# default (stable) toolchain. Set `BUILD_WATCHOS=0` to skip the watchOS slices
# (e.g. on a machine without a nightly toolchain); the remote-shutter Watch app
# needs them, so they are ON by default.
set -euo pipefail

echo "Building videocall-codecs for Apple platforms (pure Rust, no libvpx)..."

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR/.." && pwd )"
cd "$ROOT_DIR"

# The static library produced by crate-type = ["staticlib", ...].
LIB_NAME="libvideocall_codecs.a"
FEATURES="uniffi"
OUT_SWIFT="target/swift-codecs"
BUILD_WATCHOS="${BUILD_WATCHOS:-1}"
NIGHTLY="${NIGHTLY:-nightly}"

mkdir -p "$OUT_SWIFT/include"

# Minimum OS versions.
export IPHONEOS_DEPLOYMENT_TARGET=15.0
export MACOSX_DEPLOYMENT_TARGET=12.0
export WATCHOS_DEPLOYMENT_TARGET=10.0

# --- iOS / macOS: default (stable) toolchain, Tier 2 -------------------------

echo "Building for iOS device (arm64)..."
RUSTFLAGS="-C link-arg=-mios-version-min=15.0" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target aarch64-apple-ios

echo "Building for iOS simulator (arm64)..."
RUSTFLAGS="-C link-arg=-mios-simulator-version-min=15.0" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target aarch64-apple-ios-sim

# macOS is fat (arm64 + x86_64) for the same reason as Catalyst below: the
# `macosx` SDK spans both native macOS and Catalyst, so a consumer that drops
# the x86_64 arch exclusion needs Intel objects on every macosx-SDK slice.
MACOS_FAT_DIR="target/macos-fat"

echo "Building for macOS (arm64)..."
RUSTFLAGS="-C link-arg=-mmacosx-version-min=12.0" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target aarch64-apple-darwin

echo "Building for macOS (x86_64)..."
RUSTFLAGS="-C link-arg=-mmacosx-version-min=12.0" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target x86_64-apple-darwin

echo "Fusing macOS slices (arm64 + x86_64) with lipo..."
mkdir -p "$MACOS_FAT_DIR"
lipo -create \
  "target/aarch64-apple-darwin/release/$LIB_NAME" \
  "target/x86_64-apple-darwin/release/$LIB_NAME" \
  -output "$MACOS_FAT_DIR/$LIB_NAME"

# Mac Catalyst: the `-macabi` variant needs the deployment target expressed via a
# full `-target <arch>-apple-ios<min>-macabi` triple (there is no `-macabi`
# min-version flag), otherwise clang defaults the object's Catalyst minos.
#
# The Catalyst slice is fat (arm64 + x86_64): Apple Silicon *and* Intel Macs. A
# Catalyst app whose deployment target is below macOS 13 must ship x86_64 or the
# App Store rejects it (ITMS-90981) — an arm64-only slice would force the host app
# arm64-only via CocoaPods' auto-generated `EXCLUDED_ARCHS[sdk=macosx*] = x86_64`.
MACCATALYST_FAT_DIR="target/maccatalyst-fat"

echo "Building for Mac Catalyst (arm64, ios-macabi)..."
RUSTFLAGS="-C link-arg=-target -C link-arg=arm64-apple-ios15.0-macabi" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target aarch64-apple-ios-macabi

echo "Building for Mac Catalyst (x86_64, ios-macabi)..."
RUSTFLAGS="-C link-arg=-target -C link-arg=x86_64-apple-ios15.0-macabi" \
  cargo build -p videocall-codecs --no-default-features --features "$FEATURES" \
  --release --target x86_64-apple-ios-macabi

echo "Fusing Mac Catalyst slices (arm64 + x86_64) with lipo..."
mkdir -p "$MACCATALYST_FAT_DIR"
lipo -create \
  "target/aarch64-apple-ios-macabi/release/$LIB_NAME" \
  "target/x86_64-apple-ios-macabi/release/$LIB_NAME" \
  -output "$MACCATALYST_FAT_DIR/$LIB_NAME"

# --- watchOS: NIGHTLY + build-std, Tier 3 ------------------------------------
#
# watchOS device ships as a single fat static library containing both the
# 32-bit-pointer `arm64_32` slice (Series 4-8, broadest coverage) and the
# 64-bit-pointer `arm64` slice (Series 9 / Ultra 2+), combined with `lipo`.
WATCHOS_FAT_DIR="target/watchos-device-fat"

if [ "$BUILD_WATCHOS" = "1" ]; then
  # Preflight: watchOS needs a nightly toolchain WITH the rust-src component.
  if ! rustup run "$NIGHTLY" rustc --version >/dev/null 2>&1; then
    echo "ERROR: watchOS requires a working nightly toolchain '$NIGHTLY'." >&2
    echo "  rustup toolchain install nightly --component rust-src" >&2
    echo "  (or re-run with BUILD_WATCHOS=0 to skip watchOS)" >&2
    exit 1
  fi
  if ! rustup run "$NIGHTLY" rustc --print target-list 2>/dev/null | grep -q '^arm64_32-apple-watchos$'; then
    echo "ERROR: nightly '$NIGHTLY' does not know the watchOS targets." >&2
    exit 1
  fi

  echo "Building for watchOS device (arm64_32, build-std)..."
  RUSTFLAGS="-C link-arg=-mwatchos-version-min=10.0" \
    cargo "+$NIGHTLY" build -Z build-std=std,panic_abort \
    -p videocall-codecs --no-default-features --features "$FEATURES" \
    --release --target arm64_32-apple-watchos

  echo "Building for watchOS device (arm64, build-std)..."
  RUSTFLAGS="-C link-arg=-mwatchos-version-min=10.0" \
    cargo "+$NIGHTLY" build -Z build-std=std,panic_abort \
    -p videocall-codecs --no-default-features --features "$FEATURES" \
    --release --target aarch64-apple-watchos

  echo "Building for watchOS simulator (arm64, build-std)..."
  RUSTFLAGS="-C link-arg=-mwatchos-simulator-version-min=10.0" \
    cargo "+$NIGHTLY" build -Z build-std=std,panic_abort \
    -p videocall-codecs --no-default-features --features "$FEATURES" \
    --release --target aarch64-apple-watchos-sim

  echo "Fusing watchOS device slices (arm64_32 + arm64) with lipo..."
  mkdir -p "$WATCHOS_FAT_DIR"
  lipo -create \
    "target/arm64_32-apple-watchos/release/$LIB_NAME" \
    "target/aarch64-apple-watchos/release/$LIB_NAME" \
    -output "$WATCHOS_FAT_DIR/$LIB_NAME"
fi

# --- Swift bindings ----------------------------------------------------------

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

# --- Sign + package ----------------------------------------------------------

# Ad-hoc sign every static library.
SIGN_LIBS=(
  "target/aarch64-apple-ios/release/$LIB_NAME"
  "target/aarch64-apple-ios-sim/release/$LIB_NAME"
  "$MACCATALYST_FAT_DIR/$LIB_NAME"
  "$MACOS_FAT_DIR/$LIB_NAME"
)
if [ "$BUILD_WATCHOS" = "1" ]; then
  SIGN_LIBS+=(
    "$WATCHOS_FAT_DIR/$LIB_NAME"
    "target/aarch64-apple-watchos-sim/release/$LIB_NAME"
  )
fi
for lib in "${SIGN_LIBS[@]}"; do
  codesign -s - "$lib"
done

echo "Creating XCFramework..."
rm -rf target/VideocallCodecs.xcframework
XCARGS=(
  -library "target/aarch64-apple-ios/release/$LIB_NAME" -headers "$OUT_SWIFT/include"
  -library "target/aarch64-apple-ios-sim/release/$LIB_NAME" -headers "$OUT_SWIFT/include"
  -library "$MACCATALYST_FAT_DIR/$LIB_NAME" -headers "$OUT_SWIFT/include"
  -library "$MACOS_FAT_DIR/$LIB_NAME" -headers "$OUT_SWIFT/include"
)
if [ "$BUILD_WATCHOS" = "1" ]; then
  XCARGS+=(
    -library "$WATCHOS_FAT_DIR/$LIB_NAME" -headers "$OUT_SWIFT/include"
    -library "target/aarch64-apple-watchos-sim/release/$LIB_NAME" -headers "$OUT_SWIFT/include"
  )
fi
xcodebuild -create-xcframework "${XCARGS[@]}" -output target/VideocallCodecs.xcframework

echo ""
echo "=== Build completed successfully ==="
echo "XCFramework:    $ROOT_DIR/target/VideocallCodecs.xcframework"
echo "Swift bindings: $ROOT_DIR/$OUT_SWIFT/videocall_codecs.swift"
if [ "$BUILD_WATCHOS" = "1" ]; then
  echo "Slices:         ios-arm64, ios-arm64-simulator, ios-arm64_x86_64-maccatalyst, macos-arm64_x86_64, watchos (arm64_32+arm64), watchos-simulator"
else
  echo "Slices:         ios-arm64, ios-arm64-simulator, ios-arm64_x86_64-maccatalyst, macos-arm64  (watchOS skipped)"
fi
echo ""
echo "To use in Swift:"
echo "  1. Drag VideocallCodecs.xcframework into your app + Watch targets."
echo "  2. Add $OUT_SWIFT/videocall_codecs.swift to your sources."
echo "  3. import videocall_codecs (the generated module)."
