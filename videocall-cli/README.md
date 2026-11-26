
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

4. Log in. The CLI makes a key for this machine and opens your browser:
```sh
videocall-cli auth login
```
Sign in to videocall.rs, check that the fingerprint on the page matches the one in your terminal, and approve. The browser hands the result back to the CLI and you're done. The CLI then joins meetings as the account that approved it, so other people see your name. Check a machine with `videocall-cli auth status`.

5. Create a meeting, or pick one you're invited to, in the web app at [app.videocall.rs](https://app.videocall.rs). The CLI only joins meetings that already exist, and it can't start them; the host does that from a browser.

6. Start streaming:
```sh
videocall-cli stream \
  --meeting-id <meeting-id> \
  --video-device-index 0 \
  --resolution 1280x720 \
  --fps 30 \
  --frame-format NV12 \
  --bitrate-kbps 500
```
If the meeting hasn't started, the CLI waits for the host. If it has a waiting room, the host admits you like any other participant. Pass `--display-name "Garage Pi"` to show a different name than your account's.

If the relay connection drops mid-stream, the CLI exits with an error instead of quietly streaming to nobody. Run it under systemd with `Restart=on-failure` (or just run it again) and it joins from scratch. Ctrl-C or `systemctl stop` closes the connection cleanly and exits 0.

## 🍓 Logging in on a Raspberry Pi or over SSH

A headless box has no browser to open, so use `--remote` (the CLI switches to it on its own over SSH, or on Linux without a desktop session):
```sh
videocall-cli auth login --remote --name "Garage Pi"
```
It prints a URL. Open it on any device where you can sign in (your laptop, your phone), check the fingerprint, approve, and paste the code the page shows back into the terminal. The code works once, for a few minutes, and only for this login, so don't paste it anywhere else.

Every machine has its own key, so logging in on the Pi doesn't touch your laptop. `videocall-cli auth logout` revokes this machine on the server and deletes its key, even if the server can't be reached. Running `auth login` again replaces the key, and revokes the old one once the new login has worked. To see or revoke all your machines, open [api.videocall.rs/devices](https://api.videocall.rs/devices); a revoked machine stops working on its next request.

A Pi has no clock battery, so right after boot its time can be off until NTP catches up. The CLI notices from the server's time, signs with that instead, and tells you to fix NTP.

## 🔑 Where the key lives

The config directory is `~/.config/videocall` (`$XDG_CONFIG_HOME/videocall` if that's set, `%APPDATA%\videocall` on Windows). Set `VIDEOCALL_CONFIG_DIR` to put it somewhere else.

On macOS and Windows the private key goes in the OS keychain. On Linux it's a plain-text `credentials.toml` with mode 0600 in the config directory; set `VIDEOCALL_CREDENTIAL_STORE=file` to use the file on macOS or Windows too. `hosts.toml` next to it holds only the device id, name, fingerprint and account. The key never leaves the machine: the CLI signs a token that lives two minutes for each request. With the server's default settings, a machine that goes unused for 90 days expires, and every machine expires a year after approval. Then you run `auth login` again.

## ⬆️ Upgrading from a version with `--user-id`

Older versions joined with `--user-id` and no login. The relays don't accept that anymore: every connection needs a room token from the meeting API, so older CLI versions can't connect at all. Install the latest, run `videocall-cli auth login` once, and drop `--user-id` from your command. Your identity comes from the account now.

## 🌐 See Your Stream Live! using Chrome or Safari
Open the meeting in a browser to watch your stream live:

```
https://app.videocall.rs/meeting/<meeting-id>
```

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
To stream into the local dev stack (`make dev` at the repo root), point the CLI at your meeting-api and relay. Keys are stored per server, so log in to the local one first:

```sh
RUST_LOG=info cargo run --release -- auth login --api-url http://localhost:8081
RUST_LOG=info cargo run --release -- stream --api-url http://localhost:8081 \
  --url https://127.0.0.1:4433 --insecure-skip-verify --meeting-id <meeting-id> ...
```

The login step needs sign-in (OAuth) configured on your local meeting-api, because approving the key means signing in through it. The default dev stack has sign-in off, so `auth login` won't get past the browser page until you set it up (see the main README).

`--insecure-skip-verify` exists for the dev stack's self-signed certificate. The CLI refuses it unless `--url` is localhost, a loopback address or a private-range IP. Plain `http://` for `--api-url` has the same limit. You can also set `VIDEOCALL_API_URL` instead of passing `--api-url` each time.

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
