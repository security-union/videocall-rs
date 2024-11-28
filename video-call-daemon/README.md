# Video Call Daemon Rust Client

This is a rust application that can be used to connect to the video call daemon and stream video to a meeting.

## Setup

### Dependencies

```sh
sudo apt install build-essential pkg-config libclang-dev libvpx-dev libasound2-dev libv4l-dev cmake libssl-dev
```
## Running using cargo

```
cargo install video-call-daemon
```

## Running locally
We recommend using a linux computer with Ubuntu 24.

You can run the project locally by running:

```
RUST_LOG=info cargo run -- --user-id <user-id> --video-device-index 2 --meeting-id <meeting-id> URL

URL can be your local webtransport server or prod https://transport.rustlemania.com
```

# Compile deb

1. Install `cargo-deb` with `cargo install cargo-deb`
2. run `cargo deb` this  will generate the deb file at `target/debian/video-call-daemon...deb`
3. Verify dependencies: `dpkg-deb -I <path_to_deb_file>`
4. Install deb file: `sudo dpkg -i <path_to_deb_file>`
