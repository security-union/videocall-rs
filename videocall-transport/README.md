# videocall-transport

Framework-agnostic WebSocket and WebTransport wrappers for [videocall.rs](https://videocall.rs).

This crate provides low-level transport implementations that work with any Rust/WASM front-end framework (Yew, Dioxus, Leptos, etc.) by depending only on `web-sys` and `videocall-types::Callback` instead of framework-specific primitives.

## Main Repo

If you are new to videocall you should start at our repo [videocall-rs](https://github.com/security-union/videocall-rs).

## Transports

| Module         | Description                                                     |
|----------------|-----------------------------------------------------------------|
| `websocket`    | WebSocket client built on `web_sys::WebSocket`                  |
| `webtransport` | WebTransport client built on `web_sys::WebTransport` (HTTP/3)   |

Both transports expose a callback-driven API using `videocall_types::Callback`, making them easy to integrate into any framework.

## Usage

Because this crate uses unstable `web-sys` APIs (WebTransport), you must compile with:

```bash
RUSTFLAGS='--cfg web_sys_unstable_apis' cargo build --target wasm32-unknown-unknown
```

### WebSocket

```rust
use videocall_transport::websocket::{WebSocketService, WebSocketStatus};
use videocall_types::Callback;

let on_message = Callback::from(|data| { /* handle binary message */ });
let on_status  = Callback::from(|status: WebSocketStatus| { /* connection status */ });

let task = WebSocketService::connect("wss://example.com/ws", on_message, on_status)
    .expect("failed to connect");
```

### WebTransport

```rust
use videocall_transport::webtransport::{WebTransportService, WebTransportStatus};
use videocall_types::Callback;

let on_message = Callback::from(|data| { /* handle binary message */ });
let on_status  = Callback::from(|status: WebTransportStatus| { /* connection status */ });

let task = WebTransportService::connect("https://example.com:4433", on_message, on_status)
    .expect("failed to connect");
```

## License

Licensed under either of

- [Apache License, Version 2.0](http://www.apache.org/licenses/LICENSE-2.0)
- [MIT License](http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
