# CLAUDE.md

## Project Overview

`videocall-rs` is a Rust-based video calling platform. The main crates are:

- **videocall-client** - Client library targeting `wasm32-unknown-unknown`. Supports two modes via the `yew-compat` cargo feature (enabled by default).
- **yew-ui** - Yew-based frontend (uses `videocall-client` with `yew-compat`)
- **dioxus-ui** - Dioxus-based frontend (uses `videocall-client` without `yew-compat`)
- **videocall-types** - Shared protobuf types
- **videocall-codecs** - Audio/video codec wrappers

## Build Commands

```bash
# Check framework-agnostic mode (no yew)
cargo check --target wasm32-unknown-unknown --no-default-features -p videocall-client

# Check yew mode (default)
cargo check --target wasm32-unknown-unknown -p videocall-client

# Full integration tests
make yew-tests-docker
```

## Architecture: Yew Separation Pattern

The `videocall-client` crate uses a companion file pattern to separate yew-specific code from framework-agnostic code. All yew code is gated behind the `yew-compat` cargo feature.

### Pattern

Each file with yew-specific code has a companion `*_yew.rs` file declared at the bottom:

```rust
// At the bottom of camera_encoder.rs:
#[cfg(feature = "yew-compat")]
#[path = "camera_encoder_yew.rs"]
mod yew_compat;
```

Companion files use `use super::*;` to access parent types and do NOT need individual `#[cfg(feature = "yew-compat")]` guards since the entire module is conditionally compiled.

### What stays in the main file
- Struct definitions (with `#[cfg]` on fields that differ between modes)
- `#[cfg(not(feature = "yew-compat"))]` impl blocks (framework-agnostic)
- Shared/ungated impl blocks and functions

### What goes in the `_yew.rs` companion file
- All `#[cfg(feature = "yew-compat")]` impl blocks
- Yew-specific imports (`use yew::Callback;`)

### Companion files

| Main File | Companion File |
|---|---|
| `src/encode/camera_encoder.rs` | `camera_encoder_yew.rs` |
| `src/encode/microphone_encoder.rs` | `microphone_encoder_yew.rs` |
| `src/encode/screen_encoder.rs` | `screen_encoder_yew.rs` |
| `src/encode/mod.rs` | `yew_compat.rs` (re-exports `MicrophoneEncoderTrait`, `create_microphone_encoder`) |
| `src/media_devices/media_device_access.rs` | `media_device_access_yew.rs` |
| `src/media_devices/media_device_list.rs` | `media_device_list_yew.rs` |
| `src/decode/peer_decode_manager.rs` | `peer_decode_manager_yew.rs` |
| `src/health_reporter.rs` | `health_reporter_yew.rs` |
| `src/client/video_call_client.rs` | `video_call_client_yew.rs` |

The `connection/` module was already properly separated before this refactoring.

### Key difference: yew vs non-yew
- Yew mode uses `yew::Callback<T>` for event callbacks
- Non-yew mode uses `Rc<dyn Fn(T)>` or `Box<dyn Fn(T)>` closures
- Non-yew mode uses `CanvasIdProvider` trait instead of yew `Callback` for canvas IDs
- Non-yew mode uses `emit_client_event()` / `ClientEvent` event bus for framework-agnostic eventing
