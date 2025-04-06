#!/bin/bash
set -e

# Check if version is provided
if [ -z "$1" ]; then
    echo "Usage: ./scripts/release.sh <version>"
    echo "Example: ./scripts/release.sh 1.0.0"
    exit 1
fi

VERSION=$1
TAG="videocall-ios-v$VERSION"

# Update version in Package.swift
sed -i '' "s/version: \".*\"/version: \"$VERSION\"/" VideoCallKit-Dist/Package.swift

# Create git tag
git add VideoCallKit-Dist/Package.swift
git commit -m "Release VideoCallKit $TAG"
git tag -a $TAG -m "Release VideoCallKit $TAG"

# Push changes and tag
git push origin main
git push origin $TAG

echo "Released $TAG"
echo "GitHub Actions will automatically create the release with the XCFramework" 