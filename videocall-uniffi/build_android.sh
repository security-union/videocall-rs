#!/bin/bash
set -e

echo "Building for Android..."

# Get the directory of this script
SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
ROOT_DIR="$( cd "$SCRIPT_DIR/.." && pwd )"

# Install Android targets if not already installed
echo "Installing Android targets..."
rustup target add aarch64-linux-android
rustup target add armv7-linux-androideabi
rustup target add x86_64-linux-android

# Ensure target directories exist
mkdir -p "$SCRIPT_DIR/target/kotlin"
mkdir -p "$SCRIPT_DIR/target/kotlin/include"

# Set environment variables for Android
# Try to find Android NDK in common locations
if [ -z "$ANDROID_NDK_HOME" ]; then
    # Check common locations
    if [ -d "$HOME/Library/Android/sdk/ndk" ]; then
        # Find the latest NDK version
        NDK_VERSIONS=($(ls -d "$HOME/Library/Android/sdk/ndk/"* 2>/dev/null | grep -E '[0-9]+\.[0-9]+\.[0-9]+' | sort -V))
        if [ ${#NDK_VERSIONS[@]} -gt 0 ]; then
            export ANDROID_NDK_HOME="${NDK_VERSIONS[${#NDK_VERSIONS[@]}-1]}"
        else
            echo "Error: No NDK versions found in $HOME/Library/Android/sdk/ndk/"
            echo "Please install Android NDK via Android Studio or download from https://developer.android.com/ndk/downloads"
            exit 1
        fi
    elif [ -d "$HOME/Android/Sdk/ndk" ]; then
        # Find the latest NDK version
        NDK_VERSIONS=($(ls -d "$HOME/Android/Sdk/ndk/"* 2>/dev/null | grep -E '[0-9]+\.[0-9]+\.[0-9]+' | sort -V))
        if [ ${#NDK_VERSIONS[@]} -gt 0 ]; then
            export ANDROID_NDK_HOME="${NDK_VERSIONS[${#NDK_VERSIONS[@]}-1]}"
        else
            echo "Error: No NDK versions found in $HOME/Android/Sdk/ndk/"
            echo "Please install Android NDK via Android Studio or download from https://developer.android.com/ndk/downloads"
            exit 1
        fi
    fi
fi

# Check if Android NDK is installed
if [ ! -d "$ANDROID_NDK_HOME" ]; then
    echo "Error: Android NDK not found."
    echo "Please install Android NDK and set ANDROID_NDK_HOME environment variable"
    echo "You can install it via Android Studio or download from https://developer.android.com/ndk/downloads"
    echo ""
    echo "After installing, set the environment variable:"
    echo "export ANDROID_NDK_HOME=/path/to/your/android-ndk"
    echo ""
    echo "Then run this script again."
    exit 1
fi

echo "Using Android NDK at: $ANDROID_NDK_HOME"

# Set up environment variables for the NDK
export ANDROID_API_LEVEL=21
export ANDROID_NDK_ROOT=$ANDROID_NDK_HOME
export ANDROID_NDK=$ANDROID_NDK_HOME
export ANDROID_TOOLCHAIN=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/darwin-x86_64

# Add NDK tools to PATH
export PATH=$ANDROID_TOOLCHAIN/bin:$PATH

# Set up the compiler and linker for each target
export AR=$ANDROID_TOOLCHAIN/bin/llvm-ar
export AS=$ANDROID_TOOLCHAIN/bin/llvm-as

# aarch64 (arm64-v8a)
export CC_aarch64_linux_android=$ANDROID_TOOLCHAIN/bin/aarch64-linux-android$ANDROID_API_LEVEL-clang
export CXX_aarch64_linux_android=$ANDROID_TOOLCHAIN/bin/aarch64-linux-android$ANDROID_API_LEVEL-clang++
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$CC_aarch64_linux_android

# armv7 (armeabi-v7a)
export CC_armv7_linux_androideabi=$ANDROID_TOOLCHAIN/bin/armv7a-linux-androideabi$ANDROID_API_LEVEL-clang
export CXX_armv7_linux_androideabi=$ANDROID_TOOLCHAIN/bin/armv7a-linux-androideabi$ANDROID_API_LEVEL-clang++
export CARGO_TARGET_ARMV7_LINUX_ANDROIDEABI_LINKER=$CC_armv7_linux_androideabi

# x86_64
export CC_x86_64_linux_android=$ANDROID_TOOLCHAIN/bin/x86_64-linux-android$ANDROID_API_LEVEL-clang
export CXX_x86_64_linux_android=$ANDROID_TOOLCHAIN/bin/x86_64-linux-android$ANDROID_API_LEVEL-clang++
export CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER=$CC_x86_64_linux_android

# Set up CMake toolchain file
export CMAKE_TOOLCHAIN_FILE=$ANDROID_NDK_HOME/build/cmake/android.toolchain.cmake

# Verify that the tools exist
for tool in $CC_aarch64_linux_android $CC_armv7_linux_androideabi $CC_x86_64_linux_android $AR $AS; do
    if [ ! -f "$tool" ]; then
        echo "Error: Tool not found: $tool"
        echo "Please check your Android NDK installation."
        exit 1
    fi
done

echo "Android NDK setup complete. Proceeding with build..."

# Build for Android arm64-v8a
echo "Building for Android arm64-v8a..."
cd "$ROOT_DIR"
RUSTFLAGS="-C link-arg=-landroid" cargo build -p videocall-uniffi --release --target aarch64-linux-android

# Build for Android armeabi-v7a
echo "Building for Android armeabi-v7a..."
RUSTFLAGS="-C link-arg=-landroid" cargo build -p videocall-uniffi --release --target armv7-linux-androideabi

# Build for Android x86_64
echo "Building for Android x86_64..."
RUSTFLAGS="-C link-arg=-landroid" cargo build -p videocall-uniffi --release --target x86_64-linux-android

# Generate Kotlin bindings
echo "Generating Kotlin bindings..."
cd "$SCRIPT_DIR"
mkdir -p target/kotlin
cargo run -p videocall-uniffi --bin uniffi-bindgen -- generate --library "$ROOT_DIR/target/aarch64-linux-android/release/libvideocall_uniffi.so" --language kotlin --out-dir target/kotlin

# Verify Kotlin bindings were generated
if [ ! -f "target/kotlin/com/videocall/uniffi/videocall.kt" ]; then
    echo "Error: Kotlin bindings generation failed. videocall.kt not found."
    exit 1
fi

# Create AAR structure
echo "Creating AAR structure..."
mkdir -p target/aar/jni
mkdir -p target/aar/jni/arm64-v8a
mkdir -p target/aar/jni/armeabi-v7a
mkdir -p target/aar/jni/x86_64

# Copy libraries to AAR structure
cp "$ROOT_DIR/target/aarch64-linux-android/release/libvideocall_uniffi.so" target/aar/jni/arm64-v8a/
cp "$ROOT_DIR/target/armv7-linux-androideabi/release/libvideocall_uniffi.so" target/aar/jni/armeabi-v7a/
cp "$ROOT_DIR/target/x86_64-linux-android/release/libvideocall_uniffi.so" target/aar/jni/x86_64/

# Copy Kotlin bindings with error checking
if [ -f "target/kotlin/com/videocall/uniffi/videocall.kt" ]; then
    cp target/kotlin/com/videocall/uniffi/videocall.kt target/aar/
else
    echo "Error: Failed to copy Kotlin bindings. videocall.kt not found."
    exit 1
fi

# Create AndroidManifest.xml
cat > target/aar/AndroidManifest.xml << EOF
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android"
    package="com.videocall.uniffi">
</manifest>
EOF

# Create build.gradle
cat > target/aar/build.gradle << EOF
apply plugin: 'com.android.library'
apply plugin: 'kotlin-android'

android {
    compileSdkVersion 33
    
    defaultConfig {
        minSdkVersion 21
        targetSdkVersion 33
    }
    
    buildTypes {
        release {
            minifyEnabled false
            proguardFiles getDefaultProguardFile('proguard-android-optimize.txt'), 'proguard-rules.pro'
        }
    }
    
    sourceSets {
        main {
            jniLibs.srcDirs = ['jni']
        }
    }
}

dependencies {
    implementation "org.jetbrains.kotlin:kotlin-stdlib-jdk8:1.8.0"
}
EOF

# Package as AAR
echo "Packaging as AAR..."
cd target/aar
zip -r ../videocall-uniffi.aar *

echo "Build completed successfully!"

echo ""
echo "=== Build completed successfully ==="
echo "AAR created at: $SCRIPT_DIR/target/videocall-uniffi.aar"
echo "Kotlin bindings file: $SCRIPT_DIR/target/kotlin/com/videocall/uniffi/videocall.kt"
echo ""
echo "To use in your Android project:"
echo "1. Add the AAR to your project's libs directory"
echo "2. Add the videocall.kt file to your project"
echo "3. Import the VideoCallUniffi module in your Kotlin files"
echo "" 