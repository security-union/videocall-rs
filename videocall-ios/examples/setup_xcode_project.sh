#!/bin/bash
set -e

# Define paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${SCRIPT_DIR}/videocall-demo"
FRAMEWORK_DIR="${PROJECT_DIR}/Frameworks"
BINDINGS_FILE="${SCRIPT_DIR}/videocall.swift"
TARGET_DIR="/Users/darioalessandro/Documents/videocall-rs/target"

# Check if the project exists
if [ ! -d "${PROJECT_DIR}" ]; then
  echo "Error: videocall-demo project not found at ${PROJECT_DIR}"
  exit 1
fi

# Create Frameworks directory if it doesn't exist
mkdir -p "${FRAMEWORK_DIR}"

# Copy the XCFramework
echo "Copying XCFramework..."
if [ -d "${TARGET_DIR}/VideoCallIOS.xcframework" ]; then
  cp -R "${TARGET_DIR}/VideoCallIOS.xcframework" "${FRAMEWORK_DIR}/"
else
  echo "Error: VideoCallIOS.xcframework not found at ${TARGET_DIR}"
  exit 1
fi

# Copy the Swift bindings
echo "Copying Swift bindings..."
if [ -f "${TARGET_DIR}/swift/videocall.swift" ]; then
  cp "${TARGET_DIR}/swift/videocall.swift" "${PROJECT_DIR}/"
else
  echo "Error: videocall.swift not found at ${TARGET_DIR}/swift"
  exit 1
fi

echo ""
echo "Setup complete! Now you need to:"
echo "1. Open the Xcode project at ${PROJECT_DIR}"
echo "2. Add the VideoCallIOS.xcframework to your project:"
echo "   - Drag and drop the framework from ${FRAMEWORK_DIR} to your project navigator"
echo "   - When prompted, check 'Copy items if needed' and select your app target"
echo "   - In the target's 'General' tab, ensure the framework is listed under 'Frameworks, Libraries, and Embedded Content'"
echo "   - Set Embed to 'Embed & Sign'"
echo "3. Add the videocall.swift file to your project:"
echo "   - Drag and drop ${PROJECT_DIR}/videocall.swift to your project navigator"
echo "   - When prompted, check 'Copy items if needed' and select your app target"
echo "4. Make sure the VideoCallIOS module is properly imported in your Swift files"
echo ""
echo "Important: The ContentView.swift file has been updated to use the Rust functions,"
echo "but you may need to add 'import VideoCallIOS' at the top of the file." 