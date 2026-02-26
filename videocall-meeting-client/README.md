
# videocall-meeting-client

`videocall-meeting-client` is a cross-platform REST client for the [videocall.rs](https://videocall.rs) meeting API. Built on `reqwest`, it works on WASM (browser), desktop, and mobile targets.

## Main Repo

If you are new to videocall you should start at our repo [videocall](https://github.com/security-union/videocall-rs)

## Features

- **Cross-Platform**: Works on WASM, desktop, and mobile via `reqwest`
- **Two Auth Modes**: `AuthMode::Cookie` for browsers, `AuthMode::Bearer` for native/mobile/CLI
- **Full API Coverage**: Typed methods for all meeting-api endpoints (auth, meetings CRUD, participants, waiting room)
- **Strongly Typed**: Returns response types from `videocall-meeting-types` -- no raw JSON parsing

## Usage

```toml
[dependencies]
videocall-meeting-client = { path = "../videocall-meeting-client" }
```

```rust
use videocall_meeting_client::{MeetingApiClient, AuthMode};

// Browser (WASM): cookies sent automatically
let client = MeetingApiClient::new("http://localhost:8081", AuthMode::Cookie);

// Native / mobile: use a bearer token
let client = MeetingApiClient::new(
    "http://localhost:8081",
    AuthMode::Bearer("eyJ...".to_string()),
);

// All endpoints are typed methods
let profile = client.get_profile().await?;
let status = client.join_meeting("my-room", Some("Alice")).await?;
let token = client.refresh_room_token("my-room").await?;
```

## About `videocall.rs`

The `videocall.rs` system is an open-source, real-time teleconferencing platform built with Rust, WebTransport, and HTTP/3, designed for high-performance and low-latency communication.

## License

This project is dual-licensed under [MIT](../LICENSE-MIT) or [Apache-2.0](../LICENSE-APACHE).
