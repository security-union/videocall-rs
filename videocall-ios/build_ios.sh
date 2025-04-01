#!/bin/bash
set -e

echo "Building for iOS and macOS..."

# Ensure target directories exist
mkdir -p target/swift
mkdir -p target/swift/include

# Build for iOS device (arm64)
echo "Building for iOS device (arm64)..."
cargo build -p videocall-ios --release --target aarch64-apple-ios

# Build for iOS simulator (arm64)
echo "Building for iOS simulator (arm64)..."
cargo build -p videocall-ios --release --target aarch64-apple-ios-sim

# Build for macOS (arm64)
echo "Building for macOS (arm64)..."
cargo build -p videocall-ios --release --target aarch64-apple-darwin

# Generate Swift bindings
echo "Generating Swift bindings..."
cargo run -p videocall-ios --bin uniffi-bindgen -- generate --library target/aarch64-apple-ios-sim/release/libvideocall_ios.a --language swift --out-dir target/swift

# Copy header file to include directory
cp target/swift/videocallFFI.h target/swift/include/
mv target/swift/videocallFFI.modulemap target/swift/include/module.modulemap

# Ad-hoc sign the libraries
codesign -s - target/aarch64-apple-ios/release/libvideocall_ios.a
codesign -s - target/aarch64-apple-ios-sim/release/libvideocall_ios.a
codesign -s - target/aarch64-apple-darwin/release/libvideocall_ios.a

# Create XCFramework
echo "Creating XCFramework..."
rm -rf target/VideoCallIOS.xcframework
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libvideocall_ios.a \
  -headers target/swift/include \
  -library target/aarch64-apple-ios-sim/release/libvideocall_ios.a \
  -headers target/swift/include \
  -library target/aarch64-apple-darwin/release/libvideocall_ios.a \
  -headers target/swift/include \
  -output target/VideoCallIOS.xcframework

echo "Build completed successfully!"

echo ""
echo "=== Build completed successfully ==="
echo "XCFramework created at: ${ROOT_DIR}/target/VideoCallIOS.xcframework"
echo "Swift bindings file: ${ROOT_DIR}/target/swift/videocall.swift"
echo ""
echo "To use in your Swift project:"
echo "1. Add the XCFramework to your Xcode project"
echo "2. Add the videocall.swift file to your project"
echo "3. Import the VideoCallIOS module in your Swift files"
echo "" 