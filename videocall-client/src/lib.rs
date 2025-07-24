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

//! This crate provides a client-side (browser) interface to the videocall protocol.  The purpose is to
//! take care of the media I/O both for the encoding the current participant and for rendering the
//! media from the remote peers.  It also provides tools for listing available media devices and
//! granting access.
//!
//! This crate intends to make no assumptions about the UI or the HTML of the client app.
//! The only DOM data it needs is the ID of the `HtmlVideoElement` for the participant's own video
//! display and the ID's of the `HtmlCanvasElement`s into which remote peer video should be renderered.
//!
//! In addition to its use by Rust UI apps (e.g. via yew), it is intended that this crate be
//! compiled to npm module that could be called from javascript, e.g. in an electron app.
//!
//! Currently, only the Chrome browser is supported, due to some of the Web APIs that are used.
//!
//! **NOTE:** This initial version is a slightly frankenstein result of piecemeal refactoring bits
//! from the original app and stitching them together.   It could use cleaning up both the API the
//! internal design.
//!
//! # Outline of usage
//!
//! For more detailed documentation see the doc for each struct.
//!
//! ## Client creation and connection:
//! ```no_run
//! # use videocall_client::{VideoCallClient, VideoCallClientOptions};
//! # use yew::Callback;
//! # use videocall_types::protos::media_packet::media_packet::MediaType;
//! # use wasm_bindgen::JsValue;
//! 
//! // Create client options with required callbacks
//! let options = VideoCallClientOptions {
//!     // Required fields
//!     enable_e2ee: false,  // Enable end-to-end encryption
//!     enable_webtransport: false,  // Use WebSocket instead of WebTransport
//!     
//!     // Required callbacks
//!     on_peer_added: Callback::from(|_peer_id: String| {
//!         // Handle new peer added
//!     }),
//!     on_peer_first_frame: Callback::from(|(_peer_id, _media_type): (String, MediaType)| {
//!         // Handle first frame received from peer
//!         // media_type can be Audio, Video, or Screen
//!     }),
//!     
//!     // Required function to get video canvas ID for a peer
//!     get_peer_video_canvas_id: Callback::from(|peer_id: String| {
//!         format!("video-{peer_id}")
//!     }),
//!     
//!     // Required function to get screen canvas ID for a peer
//!     get_peer_screen_canvas_id: Callback::from(|peer_id: String| {
//!         format!("screen-{peer_id}")
//!     }),
//!     
//!     // Required user ID
//!     userid: "example_user".to_string(),
//!     
//!     // Required server URLs
//!     websocket_urls: vec!["wss://example.com/ws".to_string()],
//!     webtransport_urls: vec!["https://example.com/wt".to_string()],
//!     
//!     // Required callbacks
//!     on_connected: Callback::from(|_| {
//!         // Handle connection established
//!     }),
//!     on_connection_lost: Callback::from(|_error: JsValue| {
//!         // Handle connection lost
//!     }),
//!     
//!     // Optional callbacks
//!     on_diagnostics_update: None,  // No diagnostics updates by default
//!     on_sender_stats_update: None,  // No sender stats updates by default
//!     
//!     // Optional configuration
//!     enable_diagnostics: false,  // Disable diagnostics by default
//!     diagnostics_update_interval_ms: None,  // Use default interval
//!     on_encoder_settings_update: None,  // No encoder settings updates
//!     rtt_testing_period_ms: 3000,  // Default RTT testing period
//!     rtt_probe_interval_ms: None,  // Use default probe interval
//! };
//! 
//! // Create a client with the specified options
//! let mut client = VideoCallClient::new(options);
//! 
//! // Connect to the server
//! client.connect();
//! ```
//!
//! ## Requesting device access:
//! ```no_run
//! # use videocall_client::MediaDeviceAccess;
//! # use yew::Callback;
//! 
//! let mut media_device_access = MediaDeviceAccess::new();
//! // Request access to media devices
//! media_device_access.request();
//! ```
//!
//! ### Device query and listing:
//! ```no_run
//! # use videocall_client::MediaDeviceList;
//! 
//! // Create a new device list
//! let media_device_list = MediaDeviceList::new();
//! // Get available audio input devices
//! let microphones = media_device_list.audio_inputs.devices();
//! // Get available video input devices
//! let cameras = media_device_list.video_inputs.devices();
//! ```


pub mod audio_worklet_codec;
mod client;
mod connection;
pub mod constants;
pub mod crypto;
pub mod decode;
pub mod diagnostics;
pub mod encode;
mod media_devices;
pub mod utils;
mod wrappers;
pub use client::{VideoCallClient, VideoCallClientOptions};
pub use decode::{
    create_audio_peer_decoder, AudioPeerDecoderTrait, PeerDecodeManager, VideoPeerDecoder,
};
pub use encode::{create_microphone_encoder, CameraEncoder, MicrophoneEncoderTrait, ScreenEncoder};
pub use media_devices::{MediaDeviceAccess, MediaDeviceList, SelectableDevices};
