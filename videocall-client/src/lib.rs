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
//!     on_diagnostics_update: None,
//!     on_sender_stats_update: None,
//!     enable_diagnostics: false,
//!     diagnostics_update_interval_ms: None,
//!     enable_health_reporting: false,
//!     health_reporting_interval_ms: None,
//!     on_encoder_settings_update: None,
//!     rtt_testing_period_ms: 3000,
//!     rtt_probe_interval_ms: None,
//! };
//! let mut client = VideoCallClient::new(options);
//!
//! client.connect().unwrap();
//! ```
//!
//! ## Encoder creation:
//! ```no_run
//! use videocall_client::{VideoCallClient, CameraEncoder, ScreenEncoder, create_microphone_encoder};
//! use yew::Callback;
//!
//! # use videocall_client::VideoCallClientOptions;
//! # let options = VideoCallClientOptions {
//! #     enable_e2ee: false, enable_webtransport: false, on_peer_added: Callback::noop(),
//! #     on_peer_first_frame: Callback::noop(), get_peer_video_canvas_id: Callback::from(|_| "video".to_string()),
//! #     get_peer_screen_canvas_id: Callback::from(|_| "screen".to_string()), userid: "user".to_string(),
//! #     meeting_id: "room".to_string(), websocket_urls: vec![], webtransport_urls: vec![],
//! #     on_connected: Callback::noop(), on_connection_lost: Callback::noop(), on_diagnostics_update: None,
//! #     on_sender_stats_update: None, enable_diagnostics: false, diagnostics_update_interval_ms: None,
//! #     enable_health_reporting: false, health_reporting_interval_ms: None, on_encoder_settings_update: None,
//! #     rtt_testing_period_ms: 3000, rtt_probe_interval_ms: None,
//! # };
//! # let client = VideoCallClient::new(options);
//! let mut camera = CameraEncoder::new(
//!     client.clone(),
//!     "video-element",
//!     1000000, // 1 Mbps initial bitrate
//!     Callback::noop()
//! );
//! let mut microphone = create_microphone_encoder(
//!     client.clone(),
//!     128, // 128 kbps bitrate
//!     Callback::noop()
//! );
//! let mut screen = ScreenEncoder::new(
//!     client,
//!     2000, // 2 Mbps bitrate
//!     Callback::noop()
//! );
//!
//! // Select devices and start/stop encoding
//! camera.select("camera-device-id".to_string());
//! camera.start();
//! camera.stop();
//! microphone.select("microphone-device-id".to_string());
//! microphone.start();
//! microphone.stop();
//! screen.set_enabled(true);
//! screen.set_enabled(false);
//! ```
//!
//! ## Device access permission:
//!
//! ```no_run
//! use videocall_client::MediaDeviceAccess;
//! use yew::Callback;
//!
//! let mut media_device_access = MediaDeviceAccess::new();
//! media_device_access.on_granted = Callback::from(|_| {
//!     web_sys::console::log_1(&"Access granted!".into());
//! });
//! media_device_access.on_denied = Callback::from(|error| {
//!     web_sys::console::log_2(&"Access denied:".into(), &error);
//! });
//! media_device_access.request();
//! ```
//!
//! ### Device query and listing:
//! ```no_run
//! use videocall_client::MediaDeviceList;
//! use yew::Callback;
//!
//! let mut media_device_list = MediaDeviceList::new();
//! media_device_list.audio_inputs.on_selected = Callback::from(|device_id: String| {
//!     web_sys::console::log_2(&"Audio device selected:".into(), &device_id.into());
//! });
//! media_device_list.video_inputs.on_selected = Callback::from(|device_id: String| {
//!     web_sys::console::log_2(&"Video device selected:".into(), &device_id.into());
//! });
//!
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
//!
//! ```

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
pub use client::{VideoCallClient, VideoCallClientOptions};
pub use decode::{
    create_audio_peer_decoder, AudioPeerDecoderTrait, PeerDecodeManager, VideoPeerDecoder,
};
pub use encode::{create_microphone_encoder, CameraEncoder, MicrophoneEncoderTrait, ScreenEncoder};
pub use media_devices::{MediaDeviceAccess, MediaDeviceList, SelectableDevices};
