
# Videocall-cli Rust Client

<a href="https://crates.io/crates/videocall-cli"><img src="https://img.shields.io/crates/v/videocall-cli.svg" alt="Crates.io" height="28"></a>
<a href="https://github.com/security-union/videocall-rs"><img src="https://img.shields.io/badge/GitHub-videocall--rs-blue?logo=github" alt="GitHub" height="28"></a>

This is the official command-line client for [videocall.rs](https://github.com/security-union/videocall-rs), the open-source, ultra-low-latency video conferencing platform.

## ✨ Features
- Stream video effortlessly from the CLI on your desktop, robot, or Raspberry Pi.
- Works seamlessly with [videocall.rs](https://videocall.rs).
- Currently Supports Chrome, Safari (both mobile and desktop), Chromium and Edge.
- Compatible with **local servers** or **production environments**.

---

## 🛠️ Setup

### System Requirements
We recommend using a **Linux machine running Ubuntu 24** for the best experience.

### 1. Install Rust 🦀
Every install path below (`cargo install`, `cargo run`, `cargo deb`) needs the Rust toolchain, so do this **first**. The easiest way is via [rustup](https://rustup.rs):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then restart your shell (or run `source "$HOME/.cargo/env"`) so `cargo` is on your `PATH`. See the [official install guide](https://www.rust-lang.org/tools/install) for alternatives.

### 2. Install Dependencies

#### Linux
Make sure you have the required libraries installed:

```sh
sudo apt install build-essential pkg-config libclang-dev libvpx-dev libasound2-dev libv4l-dev cmake libssl-dev
```

#### macOS (experimental ⚠️)
On macOS the native build pulls audio/video from CoreAudio and AVFoundation, so the Linux-only ALSA (`libasound2`) and V4L (`libv4l`) packages aren't needed. Install the rest with [Homebrew](https://brew.sh):

```sh
brew install pkg-config llvm libvpx cmake openssl
```

---

## 🚀 Quick Start

### Install via Cargo

1. Skip the hassle! Install the client directly with:

```sh
cargo install videocall-cli
```

2. List the cameras in your system:
```sh
videocall-cli info --list-cameras

There are 2 available cameras.
Name: NexiGo HD Webcam: NexiGo HD Web, Description: Video4Linux Device @ /dev/video4, Extra: , Index: 0
```
3. Print the available resolutions and formats for your camera:
```sh
videocall-cli info --list-formats 0

Name: NexiGo HD Webcam: NexiGo HD Web, Description: uvcvideo, Extra: usb-0000:00:03.0-5 (6, 8, 12), Index: 0
YUYV:
 - 864x480: [10]
 - 1600x896: [5]
 - 1920x1080: [5]
NV12:
 - 640x480: [60, 30]
 - 1280x720: [60, 30]
 - 1920x1080: [60, 30]
 ```

4. Start streaming:

```
videocall-cli \
  stream \
  --user-id <your-user-id> \
  --video-device-index 0 \
  --meeting-id <meeting-id> \
  --resolution 1280x720 \
  --fps 30 \
  --frame-format NV12 \
  --bitrate-kbps 500
```

## 🌐 See Your Stream Live! using Chrome or Safari
This system integrates directly with [videocall.rs](https://videocall.rs). Simply navigate to the following URL to watch your stream live:

```
https://app.videocall.rs/meeting/<meeting-id>
```

Replace `<your-username>` and `<meeting-id>` with the appropriate values.

---

## 🖥️ Supported Platforms

| Platform          | Supported | Tested         |
|--------------------|-----------|----------------|
| Ubuntu 24 (Linux) | ✅        | ✅             |
| Ubuntu 22 (Linux) | ✅        | ✅             |
| MacOS 15.3.1+     | ⚠️(exp)  | ✅             |
| Debian            | ✅        | ❌             |
| Alpine Linux      | ✅        | ❌             |
| Windows           | ❌        | ❌             |

---

### Run Locally
Stream your video to a meeting in seconds:

```sh
RUST_LOG=info cargo run --release -- ...
```

## 📦 Build a `.deb` Package

Want to create a Debian package? Easy! 

1. Install the necessary tool:  
   ```sh
   cargo install cargo-deb
   ```
2. Build the `.deb` package:  
   ```sh
   cargo deb
   ```
   The package will be generated at: `target/debian/videocall-cli...deb`.
3. Verify dependencies (optional):  
   ```sh
   dpkg-deb -I <path_to_deb_file>
   ```
4. Install the package:  
   ```sh
   sudo dpkg -i <path_to_deb_file>
   ```

---


## 🎉 Ready to Stream?  
Whether you're testing locally or connecting to production, **Videocall-cli Rust Client** is here to elevate your video streaming experience. Install it today and see the difference!

---
💡 *Have questions or issues? Drop us a line! We're here to help.*
