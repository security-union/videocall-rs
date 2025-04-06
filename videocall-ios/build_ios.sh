#!/bin/bash
set -e

echo "Building for iOS and macOS..."

# Get the directory of this script
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR/.." && pwd )"

# Change to the root directory where Cargo.toml is
cd "$ROOT_DIR"

# Ensure target directories exist
mkdir -p target/swift
mkdir -p target/swift/include

# Set environment variables for iOS version
export IPHONEOS_DEPLOYMENT_TARGET=18.0
export MACOSX_DEPLOYMENT_TARGET=14.0

# Set environment variables for aws-lc-sys
export AWS_LC_SYS_EXTERNAL_BINDGEN=1
export BINDGEN_EXTRA_CLANG_ARGS="-isysroot $(xcrun --sdk iphonesimulator --show-sdk-path)"

# Build for iOS device (arm64)
echo "Building for iOS device (arm64)..."
RUSTFLAGS="-C link-arg=-mios-version-min=18.0" cargo build -p videocall-ios --release --target aarch64-apple-ios

# Build for iOS simulator (arm64)
# echo "Building for iOS simulator (arm64)..."
# RUSTFLAGS="-C link-arg=-mios-simulator-version-min=18.0" cargo build -p videocall-ios --release --target aarch64-apple-ios-sim

# Build for macOS (arm64)
echo "Building for macOS (arm64)..."
RUSTFLAGS="-C link-arg=-mmacosx-version-min=14.0" cargo build -p videocall-ios --release --target aarch64-apple-darwin

# Generate Swift bindings
echo "Generating Swift bindings..."
cargo run -p videocall-ios --bin uniffi-bindgen -- generate --library target/aarch64-apple-ios/release/libvideocall_ios.a --language swift --out-dir target/swift

# Copy header file to include directory
cp target/swift/videocallFFI.h target/swift/include/
mv target/swift/videocallFFI.modulemap target/swift/include/module.modulemap

# Ad-hoc sign the libraries
codesign -s - target/aarch64-apple-ios/release/libvideocall_ios.a
# codesign -s - target/aarch64-apple-ios-sim/release/libvideocall_ios.a
codesign -s - target/aarch64-apple-darwin/release/libvideocall_ios.a

# Create XCFramework
echo "Creating XCFramework..."
rm -rf target/VideoCallIOS.xcframework
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libvideocall_ios.a \
  -headers target/swift/include \
  -library target/aarch64-apple-darwin/release/libvideocall_ios.a \
  -headers target/swift/include \
  -output target/VideoCallIOS.xcframework
# xcodebuild -create-xcframework \
#   -library target/aarch64-apple-ios/release/libvideocall_ios.a \
#   -headers target/swift/include \
#     -library target/aarch64-apple-ios-sim/release/libvideocall_ios.a \
#     -headers target/swift/include \
#   -library target/aarch64-apple-darwin/release/libvideocall_ios.a \
#     -headers target/swift/include \
#   -output target/VideoCallIOS.xcframework

# Copy XCFramework and Swift bindings to VideoCallKit package
echo "Copying XCFramework and Swift bindings to VideoCallKit package..."
mkdir -p "$SCRIPT_DIR/VideoCallKit/Frameworks"
cp -R target/VideoCallIOS.xcframework "$SCRIPT_DIR/VideoCallKit/Frameworks/"
cp target/swift/videocall.swift "$SCRIPT_DIR/VideoCallKit/Sources/VideoCallKit/"

# Build Swift package
echo "Building Swift package..."
cd "$SCRIPT_DIR/VideoCallKit"
swift package resolve
swift build

echo "Build completed successfully!"

echo ""
echo "=== Build completed successfully ==="
echo "XCFramework created at: $ROOT_DIR/target/VideoCallIOS.xcframework"
echo "Swift bindings file: $ROOT_DIR/target/swift/videocall.swift"
echo ""
echo "To use in your Swift project:"
echo "1. Add the XCFramework to your Xcode project"
echo "2. Add the videocall.swift file to your project"
echo "3. Import the VideoCallIOS module in your Swift files"
echo "" 