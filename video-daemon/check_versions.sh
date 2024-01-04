#!/bin/bash

# List of packages to check
packages=(
    "build-essential"
    "pkg-config"
    "libclang-dev"
    "libvpx-dev"
    "libasound2-dev"
    "cmake"
)

# Function to check the version of a package
check_package_version() {
    package=$1
    version=$(dpkg-query -W -f='${Version}\n' $package 2>/dev/null)
    if [ -z "$version" ]; then
        echo "$package is not installed."
    else
        echo "$package version: $version"
    fi
}

# Check the version of each package
for package in "${packages[@]}"; do
    check_package_version $package
done
