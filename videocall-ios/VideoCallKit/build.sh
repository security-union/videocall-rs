#!/bin/bash

# Exit on error
set -e

echo "Building VideoCallKit..."

# Get the directory of this script
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
PARENT_DIR="$( cd "$SCRIPT_DIR/.." && pwd )"

# Build the Rust library
echo "Building Rust library..."
cd "$PARENT_DIR"
./build_ios.sh

# Build the Swift package
echo "Building Swift package..."
cd "$SCRIPT_DIR"
swift build

echo "Build completed successfully!"
echo "The package is ready to be used in your Swift projects."
echo "To use it in your project, add the following to your Package.swift:"
echo ""
echo "dependencies: ["
echo "    .package(path: \"$SCRIPT_DIR\")"
echo "]" 