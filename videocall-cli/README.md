
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
sudo apt install build-essential pkg-config libclang-dev libasound2-dev libv4l-dev cmake libssl-dev
```

#### macOS (experimental ⚠️)
First make sure the Xcode Command Line Tools are installed — they provide the C compiler and SDK headers needed to build native dependencies. (Homebrew's installer also pulls these in automatically if you don't have them yet.)

```sh
xcode-select --install
```

On macOS the native build pulls audio/video from CoreAudio and AVFoundation, so the Linux-only ALSA (`libasound2`) and V4L (`libv4l`) packages aren't needed. Install the rest with [Homebrew](https://brew.sh):

```sh
brew install pkg-config llvm cmake openssl
```

The macOS camera backend is a Swift AVFoundation layer built by `cargo` at compile time, so the Xcode command-line tools (Swift toolchain) above are required — no extra runtime dependency.

Two macOS-specific behaviors to know when streaming:

- **Select the camera by UUID, not name.** Pass the device's UUID (shown in the "Extras" field of `info --list-cameras`) as the `--video-device-index` value. See [Quick Start](#-quick-start) below.
- **The requested resolution must be exact.** The backend captures at exactly the resolution you request or fails with a clear error; it never silently substitutes a different size. Run `videocall-cli info --list-formats <camera>` to see the resolutions your camera actually supports.

> **VP9 encoding is pure Rust by default** — no C libvpx is required or linked.
> If you need the legacy C libvpx backend (for A/B comparison or rollback), build
> with `--features libvpx`, which additionally requires `libvpx-dev` (Linux) /
> `libvpx` (macOS). The public behavior is the same either way.

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

4. Log in once. The CLI makes a key for this machine and opens your browser; sign in, check the fingerprint matches your terminal, and approve:
```sh
videocall-cli auth login
```

5. Pick a meeting that already exists in [app.videocall.rs](https://app.videocall.rs). The CLI joins as your account but can't create or start meetings, and the host may have to admit it from the waiting room.

6. Start streaming:
```sh
videocall-cli stream \
  --video-device-index 0 \
  --meeting-id <meeting-id> \
  --resolution 1280x720 \
  --fps 30 \
  --frame-format NV12 \
  --bitrate-kbps 500
```

Add `--display-name "Garage cam"` to show a different name than your account's.

Ctrl-C or SIGTERM closes the relay connection and exits 0. If the relay connection drops, the CLI exits non-zero instead of reconnecting, so run it under systemd with `Restart=on-failure` and it joins again from scratch. While joining, it keeps retrying meeting-api and the relay through network trouble: relay connects back off from 1 s to 30 s, and each connect attempt gets 15 s.

## 🍓 Raspberry Pi or SSH

With no browser on the machine, use `--remote` (automatic over SSH or without a desktop session). Open the printed URL on your laptop or phone, approve, and paste the code it shows back into the terminal:
```sh
videocall-cli auth login --remote --name "Garage Pi"
```

Each machine has its own key. You can see and revoke them all at [app.videocall.rs/devices](https://app.videocall.rs/devices); `videocall-cli auth logout` revokes this one. Revoking stops new joins within five minutes but doesn't cut a relay session that's already connected.

The private key never leaves the machine. It lives in the OS keychain on macOS and Windows, and on Linux in a 0600 file under `~/.config/videocall/` (`VIDEOCALL_CONFIG_DIR` moves it). On macOS, a newly built binary has to be allowed into the keychain once: if the CLI says it's waiting for keychain access, answer the dialog on the Mac's screen. A machine expires after 90 days unused or a year after approval; then run `auth login` again.

## ⬆️ Upgrading from a version with `--user-id`

The relays now need a room token from the meeting API, so older CLI versions can't connect. Install the latest, run `videocall-cli auth login` once, and drop `--user-id`.

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
To stream into the local dev stack (`make dev`), log in to your local meeting-api first. That needs OAuth configured on it, since approving means signing in:

```sh
RUST_LOG=info cargo run --release -- auth login --api-url http://localhost:8081
RUST_LOG=info cargo run --release -- stream --api-url http://localhost:8081 \
  --url https://127.0.0.1:4433 --insecure-skip-verify --meeting-id <meeting-id> ...
```

`--insecure-skip-verify` only works for a relay on localhost, a loopback address, or a private-range IP. Anywhere else the CLI refuses it, because the room token would go to whoever answers.

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
