# Video Daemon Rust Client

## Setup

### Dependencies

```sh
sudo apt install build-essential pkg-config libclang-dev libvpx-dev libasound2-dev libv4l-dev cmake
```

## Running locally
We recommend using a linux computer with Ubuntu 22.

In our experience doing web development with a rpi is miserable due to the lack of processing power.

You can run the project locally by running:

```
RUST_LOG=info cargo run -- --user-id <user-id> --video-device-index 2 --meeting-id <meeting-id> URL

URL can be your local webtransport server or prod https://transport.rustlemania.com
```

# Compile deb

1. Install `cargo-deb` with `cargo install cargo-deb`
2. run `cargo deb` this  will generate the deb file at `target/debian/video-daemon...deb`
3. Verify dependencies: `dpkg-deb -I <path_to_deb_file>`
4. Install deb file: `sudo dpkg -i <path_to_deb_file>`
