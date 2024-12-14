
# VideoCall Daemon Rust Client

## ✨ Features
- Stream video effortlessly from the CLI on your desktop, robot, or Raspberry Pi.
- Works seamlessly with [videocall.rs](https://videocall.rs).
- Currently Supports Chrome, Chromium and Edge.
- Compatible with **local servers** or **production environments**.

---

## 🛠️ Setup

### System Requirements
We recommend using a **Linux machine running Ubuntu 24** for the best experience.

### Install Dependencies (Linux)
Make sure you have the required libraries installed:

```sh
sudo apt install build-essential pkg-config libclang-dev libvpx-dev libasound2-dev libv4l-dev cmake libssl-dev
```

---

## 🚀 Quick Start

### Install via Cargo
Skip the hassle! Install the client directly with:

```sh
cargo install videocall-daemon
videocall-daemon \
  --user-id <your-user-id> \
  --video-device-index <your-camera-index> \
  --meeting-id <meeting-id> \
  --resolution 1280x720 \
  --fps 30
```

## 🌐 See Your Stream Live! using Chrome
This system integrates directly with [videocall.rs](https://videocall.rs). Simply navigate to the following URL to watch your stream live:

```
https://app.videocall.rs/meeting/<your-username>/<meeting-id>
```

Replace `<your-username>` and `<meeting-id>` with the appropriate values.

---

## 🖥️ Supported Platforms

| Platform          | Supported | Tested         |
|--------------------|-----------|----------------|
| macOS             | ❌        | ❌             |
| Ubuntu 24 (Linux) | ✅        | ✅             |
| Ubuntu 22 (Linux) | ✅        | ✅             |
| Debian            | ✅        | ❌             |
| Alpine Linux      | ✅        | ❌             |
| Windows           | ❌        | ❌             |

### Note
Only **Ubuntu 24** and **Ubuntu 22** have been fully tested. Other platforms may work, but support is not guaranteed.

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
   The package will be generated at: `target/debian/videocall-daemon...deb`.
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
Whether you're testing locally or connecting to production, **Video Call Daemon Rust Client** is here to elevate your video streaming experience. Install it today and see the difference!

---
💡 *Have questions or issues? Drop us a line! We're here to help.*
