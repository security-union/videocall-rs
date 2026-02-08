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

//! Framework-agnostic event types for the videocall client.
//!
//! These events are emitted via the event bus and can be subscribed to by any
//! frontend framework (Yew, Dioxus, Leptos, React via wasm-bindgen, etc.)

use crate::encode::ScreenShareEvent;
use videocall_types::protos::media_packet::media_packet::MediaType;

/// Events emitted by the VideoCallClient that UI frameworks can subscribe to.
#[derive(Clone, Debug)]
pub enum ClientEvent {
    // === Connection Events ===
    /// Connection to the server was established successfully
    Connected,

    /// Connection to the server was lost
    ConnectionLost(String),

    // === Peer Events ===
    /// A new peer joined the call
    PeerAdded(String),

    /// A peer left the call (e.g., heartbeat lost)
    PeerRemoved(String),

    /// First frame received from a peer for a specific media type
    PeerFirstFrame {
        peer_id: String,
        media_type: MediaType,
    },

    // === Meeting Events ===
    /// Meeting information received (start time in milliseconds since epoch)
    MeetingInfo(f64),

    /// Meeting has ended
    MeetingEnded {
        end_time_ms: f64,
        message: String,
    },

    // === Encoder Events ===
    /// Encoder settings have been updated (e.g., bitrate change)
    EncoderSettingsUpdate {
        encoder: String,
        settings: String,
    },

    /// Screen share state has changed
    ScreenShareStateChange(ScreenShareEvent),

    // === Device Events ===
    /// Media devices have been loaded/enumerated
    DevicesLoaded,

    /// Device list changed (device connected/disconnected)
    DevicesChanged,

    /// Media permission was granted
    PermissionGranted,

    /// Media permission was denied
    PermissionDenied(String),
}
