# Design: Decouple videocall-client from Yew

## Problem

`videocall-client` has three hard dependencies on the Yew ecosystem:

- **`yew`** crate -- used only for `Callback<IN, OUT>` (17 files, ~100+ instances)
- **`yew-websocket`** -- thin wrapper around `web_sys::WebSocket` (only uses `yew::Callback`)
- **`yew-webtransport`** -- thin wrapper around `web_sys::WebTransport` (only uses `yew::Callback` + `yew::platform::pinned::oneshot::channel`)

Additionally, **`videocall-types`** depends on `yew-websocket` for two type aliases: `Binary = Result<Vec<u8>, Error>` and `Text = Result<String, Error>`.

This prevents any non-Yew frontend (Dioxus, Leptos, vanilla JS) from using the library.

## Key Finding

Both `yew-websocket` and `yew-webtransport` contain **zero framework logic**. They are pure `web-sys`/`wasm-bindgen` code that happens to use `yew::Callback` as a notification type. Replacing `Callback` with our own identical type makes them framework-agnostic with no behavioral change.

## Architecture

```
videocall-types (shared, no framework deps)
  - Callback<IN, OUT>
  - Binary, Text type aliases
  - Proto types, PacketWrapper

videocall-transport (new crate, depends on videocall-types)
  - websocket::WebSocketTask, WebSocketService, WebSocketStatus
  - webtransport::WebTransportTask, WebTransportService, WebTransportStatus

videocall-client (depends on videocall-types + videocall-transport, NO yew)
  - connection/ (ConnectionManager, ConnectionController, etc.)
  - encode/, decode/, media_devices/, etc.

yew-ui (depends on videocall-client + yew)
  - Uses videocall_types::Callback at the API boundary
  - Uses yew::Callback for its own UI component logic

future-dioxus-ui (depends on videocall-client + dioxus)
  - Uses videocall_types::Callback at the API boundary
  - Zero yew anywhere in dependency tree
```

## Phase 1: Shared types and transport crate

### 1a. Callback + Binary/Text in videocall-types

`Callback<IN, OUT>` is defined in `videocall-types/src/callback.rs`. This is a direct copy of yew's implementation (MIT licensed), dropping the `ImplicitClone` marker trait:

```rust
pub struct Callback<IN, OUT = ()> {
    cb: Rc<dyn Fn(IN) -> OUT>,
}
```

Implements: `Clone`, `PartialEq` (via `Rc::ptr_eq`), `Debug`, `Default`, `From<F>`, plus `emit()`, `noop()`, `reform()`, `filter_reform()`.

`Binary` and `Text` are defined locally in `videocall-types/src/lib.rs`:
```rust
pub type Text = Result<String, anyhow::Error>;
pub type Binary = Result<Vec<u8>, anyhow::Error>;
```

`yew-websocket` is removed from `videocall-types/Cargo.toml`.

### 1b. Create videocall-transport crate

New workspace crate at `videocall-transport/` with two modules:

- **`src/websocket.rs`** -- forked from `yew-websocket` `src/websocket.rs` (~307 lines). Change: `use yew::callback::Callback` -> `use videocall_types::Callback`. Everything else untouched.

- **`src/webtransport.rs`** -- forked from `yew-webtransport` `src/webtransport.rs` (~458 lines). Changes:
  - `use yew::callback::Callback` -> `use videocall_types::Callback`
  - `use yew::platform::pinned::oneshot::channel` -> `use futures::channel::oneshot::channel`

Dependencies: `videocall-types`, `web-sys`, `wasm-bindgen`, `wasm-bindgen-futures`, `js-sys`, `gloo`, `gloo-console`, `futures`, `anyhow`, `thiserror`, `log`.

## Phase 2: Update consumers

### 2a. Mechanical Callback import replacement in videocall-client (17 files)

Replace all `use yew::Callback` / `use yew::prelude::Callback` with `use videocall_types::Callback`.

Files: `video_call_client.rs`, `encode/mod.rs`, `screen_encoder.rs`, `microphone_encoder.rs`, `camera_encoder.rs`, `peer_decode_manager.rs`, `connection_manager.rs`, `connection_controller.rs` (test), `connection.rs`, `webtransport.rs`, `websocket.rs`, `webmedia.rs`, `media_device_list.rs`, `media_device_access.rs`, `health_reporter.rs`, `diagnostics_manager.rs`.

### 2b. Replace transport crate deps in videocall-client

In `videocall-client/Cargo.toml`:
- Remove: `yew = { version = "0.21" }`
- Remove: `yew-websocket = "1.21.0"`
- Remove: `yew-webtransport = "0.21.1"`
- Add: `videocall-transport = { path = "../videocall-transport" }`

In source files:
- `connection/websocket.rs`: `use yew_websocket::websocket::*` -> `use videocall_transport::websocket::*`
- `connection/webtransport.rs`: `use yew_webtransport::webtransport::*` -> `use videocall_transport::webtransport::*`
- `connection/task.rs`: same import changes

### 2c. Update yew-ui

At the API boundary where yew-ui creates `VideoCallClientOptions` and encoder callbacks (`yew-ui/src/components/attendants.rs`), change `use yew::Callback` to `use videocall_client::Callback` (re-exported from videocall-types).

### 2d. Update lib.rs doc examples

Replace `use yew::Callback` with `use videocall_client::Callback` in all doc examples in `videocall-client/src/lib.rs`.

Re-export: `pub use videocall_types::Callback;` from lib.rs for ergonomics.

## Why Callback is the right pattern

Options considered:
- **`futures::channel::mpsc`** -- async channels add complexity, require polling, break synchronous notification pattern
- **Trait-based event handlers** -- more boilerplate, framework consumers need adapter structs
- **Framework signals** -- framework-specific, defeats the purpose
- **`Box<dyn Fn>`** -- not Clone, essential for passing to multiple async operations

Our `Callback<IN, OUT>` is trivially wrappable by any framework:

```rust
// Dioxus
let signal = use_signal(|| Vec::new());
let on_peer_added = Callback::from(move |peer: String| signal.write().push(peer));

// Yew
let on_peer_added = Callback::from(move |peer: String| link.send_message(Msg::PeerAdded(peer)));
```

## ConnectionManager stays in videocall-client

The ConnectionManager depends on `crate::crypto::aes::Aes128State` for heartbeat/RTT encryption. Moving it to videocall-transport would create a circular dependency. Its only yew dependency is `Callback`, which is resolved by the import replacement. A future refactor could extract a `CryptoProvider` trait to enable the move.
