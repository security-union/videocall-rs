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
//!
//! With `yew-compat` feature (Yew callbacks):
//! ```ignore
//! use videocall_client::{VideoCallClient, VideoCallClientOptions};
//! use yew::Callback;
//!
//! let options = VideoCallClientOptions {
//!     enable_e2ee: true,
//!     enable_webtransport: true,
//!     on_peer_added: Callback::noop(),
//!     on_peer_first_frame: Callback::noop(),
//!     get_peer_video_canvas_id: Callback::from(|_| "video-canvas".to_string()),
//!     get_peer_screen_canvas_id: Callback::from(|_| "screen-canvas".to_string()),
//!     userid: "user123".to_string(),
//!     meeting_id: "room456".to_string(),
//!     websocket_urls: vec!["ws://localhost:8080".to_string()],
//!     webtransport_urls: vec!["https://localhost:8443".to_string()],
//!     on_connected: Callback::noop(),
//!     on_connection_lost: Callback::noop(),
//!     enable_diagnostics: false,
//!     diagnostics_update_interval_ms: None,
//!     enable_health_reporting: false,
//!     on_encoder_settings_update: None,
//!     rtt_testing_period_ms: 3000,
//!     rtt_probe_interval_ms: None,
//!     health_reporting_interval_ms: Some(5000),
//!     on_peer_removed: None,
//!     on_meeting_info: None,
//!     on_meeting_ended: None,
//! };
//! let mut client = VideoCallClient::new(options);
//! client.connect().unwrap();
//! ```
//!
//! Without `yew-compat` feature (framework-agnostic):
//! ```ignore
//! use videocall_client::{VideoCallClient, VideoCallClientOptions, DirectCanvasIdProvider};
//! use std::rc::Rc;
//!
//! let options = VideoCallClientOptions {
//!     enable_e2ee: true,
//!     enable_webtransport: true,
//!     canvas_id_provider: Rc::new(DirectCanvasIdProvider),
//!     userid: "user123".to_string(),
//!     meeting_id: "room456".to_string(),
//!     websocket_urls: vec!["ws://localhost:8080".to_string()],
//!     webtransport_urls: vec!["https://localhost:8443".to_string()],
//!     enable_diagnostics: false,
//!     diagnostics_update_interval_ms: None,
//!     enable_health_reporting: false,
//!     health_reporting_interval_ms: Some(5000),
//!     rtt_testing_period_ms: 3000,
//!     rtt_probe_interval_ms: None,
//! };
//! let client = VideoCallClient::new(options);
//! // Subscribe to events via event bus
//! let mut rx = videocall_client::subscribe_client_events();
//! ```
//!
//! ## Encoder creation:
//!
//! Note: Encoder APIs differ between `yew-compat` and framework-agnostic modes.
//! See individual encoder documentation for details.
//!
//! ```ignore
//! // Example with yew-compat feature
//! use videocall_client::{VideoCallClient, CameraEncoder, ScreenEncoder, create_microphone_encoder};
//!
//! // Create encoders with a VideoCallClient instance
//! let mut camera = CameraEncoder::new(client.clone(), "video-element", 1000000, on_settings_change);
//! let mut screen = ScreenEncoder::new(client.clone(), 2000);
//!
//! // Select devices and start/stop encoding
//! camera.select("camera-device-id".to_string());
//! camera.start();
//! camera.stop();
//! screen.set_enabled(true);
//! screen.set_enabled(false);
//! ```
//!
//! ## Device access permission:
//!
//! ```ignore
//! use videocall_client::MediaDeviceAccess;
//!
//! let mut media_device_access = MediaDeviceAccess::new();
//! // With yew-compat, set callbacks:
//! // media_device_access.on_granted = Callback::from(|_| { ... });
//! // media_device_access.on_denied = Callback::from(|error| { ... });
//! // Without yew-compat, subscribe to ClientEvent::PermissionGranted/PermissionDenied
//! media_device_access.request();
//! ```
//!
//! ### Device query and listing:
//! ```ignore
//! use videocall_client::MediaDeviceList;
//!
//! let mut media_device_list = MediaDeviceList::new();
//! // With yew-compat, set callbacks on audio_inputs/video_inputs
//! // Without yew-compat, subscribe to ClientEvent::DevicesLoaded/DevicesChanged
//! media_device_list.load();
//!
//! let microphones = media_device_list.audio_inputs.devices();
//! let cameras = media_device_list.video_inputs.devices();
//! if let Some(mic) = microphones.first() {
//!     media_device_list.audio_inputs.select(&mic.device_id());
//! }
//! if let Some(camera) = cameras.first() {
//!     media_device_list.video_inputs.select(&camera.device_id());
//! }
//! ```

pub mod audio;
pub mod audio_worklet_codec;
mod client;
mod connection;
pub mod constants;
pub mod crypto;
pub mod decode;
pub mod diagnostics;
pub mod encode;
pub mod health_reporter;
mod media_devices;
pub mod utils;
mod wrappers;

// New framework-agnostic modules
mod canvas_provider;
mod event_bus;
mod events;

pub use client::{VideoCallClient, VideoCallClientOptions};
pub use decode::{
    create_audio_peer_decoder, AudioPeerDecoderTrait, PeerDecodeManager, VideoPeerDecoder,
};
pub use encode::{
    create_microphone_encoder, CameraEncoder, MicrophoneEncoderTrait, ScreenEncoder,
    ScreenShareEvent,
};
pub use media_devices::{MediaDeviceAccess, MediaDeviceList, SelectableDevices};

// Framework-agnostic event system exports
pub use canvas_provider::{
    create_canvas_provider, CanvasIdProvider, DefaultCanvasIdProvider, DirectCanvasIdProvider,
};
pub use event_bus::{emit_client_event, global_client_sender, subscribe_client_events};
pub use events::ClientEvent;
