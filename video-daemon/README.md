# Video Daemon Rust Client

## Setup

### Dependencies

```sh
sudo apt install build-essential pkg-config libclang-dev libvpx-dev
```

## Running locally
We recommend using a linux computer with Ubuntu 22.

In our experience doing web development with a rpi is miserable due to the lack of processing power.

You can run the project locally by running:

```
RUST_LOG=info cargo run -- --user-id <user-id> --video-device-index 2 --meeting-id <meeting-id> URL

URL can be your local webtransport server or prod https://transport.rustlemania.com
```
