/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Cross-platform video conferencing client for videocall.rs.
//!
//! This crate provides a client-side interface to the videocall protocol that works
//! on both **WASM (browser)** and **native (desktop/server/embedded)** targets.
//!
//! # Platform support
//!
//! Enable exactly one of the following Cargo features:
//!
//! - **`wasm`** — for browser targets (`wasm32-unknown-unknown`). Provides the full
//!   media pipeline: camera/microphone/screen encoding, peer decoding, device
//!   enumeration, and the [`VideoCallClient`] API.
//!
//! - **`native`** — for desktop, server, and embedded targets. Provides the
//!   [`NativeVideoCallClient`] API for connection lifecycle, heartbeat, and packet I/O.
//!   Media capture and encoding are left to the application (see `videocall-codecs`).
//!
//! # Architecture
//!
//! The crate is layered:
//!
//! 1. **Platform primitives** ([`platform`]) — timers, spawn, timestamps
//! 2. **Transport** — WebSocket / WebTransport (via `videocall-transport`)
//! 3. **Connection protocol** — heartbeat, RTT, E2EE, state machine
//! 4. **Client API** — [`VideoCallClient`] (WASM) or [`NativeVideoCallClient`] (native)
//! 5. **Media pipeline** — encoders, decoders, device access (WASM only)

// ── Always-available modules ──────────────────────────────────────────────────

/// Platform abstraction layer (timers, spawn, timestamps).
pub mod platform;

/// Cryptographic primitives (AES-128-CBC, RSA key exchange).
pub mod crypto;

/// Constants shared across the client.
pub mod constants;

// ── WASM-only modules (browser media pipeline) ────────────────────────────────

#[cfg(target_arch = "wasm32")]
pub mod audio;

#[cfg(target_arch = "wasm32")]
pub mod audio_worklet_codec;

#[cfg(target_arch = "wasm32")]
mod connection;

#[cfg(target_arch = "wasm32")]
mod client;

#[cfg(target_arch = "wasm32")]
pub mod decode;

/// Diagnostics and metrics collection (WASM only — uses browser timers).
#[cfg(target_arch = "wasm32")]
pub mod diagnostics;

#[cfg(target_arch = "wasm32")]
pub mod encode;

/// Health reporting to the server (WASM only — uses browser APIs).
#[cfg(target_arch = "wasm32")]
pub mod health_reporter;

#[cfg(target_arch = "wasm32")]
mod media_devices;

#[cfg(target_arch = "wasm32")]
pub mod utils;

#[cfg(target_arch = "wasm32")]
mod wrappers;

// ── WASM public API ───────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
pub use client::{VideoCallClient, VideoCallClientOptions};

#[cfg(target_arch = "wasm32")]
pub use decode::{
    create_audio_peer_decoder, AudioPeerDecoderTrait, PeerDecodeManager, VideoPeerDecoder,
};

#[cfg(target_arch = "wasm32")]
pub use encode::{
    create_microphone_encoder, CameraEncoder, MicrophoneEncoderTrait, ScreenEncoder,
    ScreenShareEvent,
};

#[cfg(target_arch = "wasm32")]
pub use media_devices::{MediaDeviceAccess, MediaDeviceList, SelectableDevices};

#[cfg(target_arch = "wasm32")]
pub use videocall_types::Callback;

// ── Native-only modules ───────────────────────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
mod native_client;

#[cfg(not(target_arch = "wasm32"))]
pub use native_client::{NativeClientOptions, NativeVideoCallClient};
