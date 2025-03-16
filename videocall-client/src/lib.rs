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
//! use wasm_bindgen::JsValue;
//! use videocall_types::protos::media_packet::media_packet::MediaType;
//! 
//! // Create client options
//! let options = VideoCallClientOptions {
//!     userid: "test-user".to_string(),
//!     websocket_url: "wss://example.com/ws".to_string(),
//!     webtransport_url: "https://example.com/wt".to_string(),
//!     enable_e2ee: false,
//!     enable_webtransport: false,
//!     on_peer_added: Callback::noop(),
//!     on_peer_first_frame: Callback::noop(),
//!     get_peer_video_canvas_id: Callback::from(|_| "video-canvas".to_string()),
//!     get_peer_screen_canvas_id: Callback::from(|_| "screen-canvas".to_string()),
//!     on_connected: Callback::noop(),
//!     on_connection_lost: Callback::noop(),
//! };
//! 
//! // Create the client
//! let client = VideoCallClient::new(options);
//! 
//! // Connect to the server
//! let _ = client.connect();
//! ```
//!
//! ## Encoder creation:
//! ```no_run
//! # use videocall_client::{VideoCallClient, VideoCallClientOptions, CameraEncoder, MicrophoneEncoder, ScreenEncoder};
//! # use yew::Callback;
//! # use wasm_bindgen::JsValue;
//! # use videocall_types::protos::media_packet::media_packet::MediaType;
//! # 
//! # // Create client options
//! # let options = VideoCallClientOptions {
//! #    userid: "test-user".to_string(),
//! #    websocket_url: "wss://example.com/ws".to_string(),
//! #    webtransport_url: "https://example.com/wt".to_string(),
//! #    enable_e2ee: false,
//! #    enable_webtransport: false,
//! #    on_peer_added: Callback::noop(),
//! #    on_peer_first_frame: Callback::noop(),
//! #    get_peer_video_canvas_id: Callback::from(|_| "video-canvas".to_string()),
//! #    get_peer_screen_canvas_id: Callback::from(|_| "screen-canvas".to_string()),
//! #    on_connected: Callback::noop(),
//! #    on_connection_lost: Callback::noop(),
//! # };
//! # // Create the client
//! # let client = VideoCallClient::new(options);
//! # let video_element_id = "local-video";
//! # let device_id = "camera1";
//! 
//! // Create encoders (note: these take ownership of client)
//! let mut camera = CameraEncoder::new(client.clone(), video_element_id);
//! let mut microphone = MicrophoneEncoder::new(client.clone());
//! let mut screen = ScreenEncoder::new(client);
//! 
//! // Use camera
//! camera.select(device_id.to_string());
//! camera.start();
//! camera.stop();
//! 
//! // Use microphone
//! microphone.select(device_id.to_string());
//! microphone.start();
//! microphone.stop();
//! 
//! // Use screen sharing
//! screen.start();
//! screen.stop();
//! ```
//!
//! ## Device access permission:
//!
//! ```no_run
//! # use videocall_client::MediaDeviceAccess;
//! # use yew::Callback;
//! # use wasm_bindgen::JsValue;
//! 
//! let mut media_device_access = MediaDeviceAccess::new();
//! 
//! // Set up callbacks
//! media_device_access.on_granted = Callback::from(|_: ()| {
//!     println!("Access granted!");
//! });
//! 
//! media_device_access.on_denied = Callback::from(|err: JsValue| {
//!     println!("Access denied: {:?}", err);
//! });
//! 
//! // Request access to devices
//! media_device_access.request();
//! ```
//!
//! ### Device query and listing:
//! ```no_run
//! # use videocall_client::MediaDeviceList;
//! # use yew::Callback;
//! 
//! let mut media_device_list = MediaDeviceList::new();
//! 
//! // Set up callbacks
//! media_device_list.audio_inputs.on_selected = Callback::from(|device_id: String| {
//!     println!("Audio device selected: {}", device_id);
//! });
//! 
//! media_device_list.video_inputs.on_selected = Callback::from(|device_id: String| {
//!     println!("Video device selected: {}", device_id);
//! });
//! 
//! // Load devices
//! media_device_list.load();
//! 
//! // Access devices and select them
//! let microphones = media_device_list.audio_inputs.devices();
//! let cameras = media_device_list.video_inputs.devices();
//! 
//! if !microphones.is_empty() {
//!     media_device_list.audio_inputs.select(&microphones[0].device_id());
//! }
//! 
//! if !cameras.is_empty() {
//!     media_device_list.video_inputs.select(&cameras[0].device_id());
//! }
//! ```

mod client;
mod connection;
mod constants;
mod crypto;
mod decode;
mod encode;
mod media_devices;
mod wrappers;

#[cfg(any(test, feature = "tests"))]
pub mod tests;

pub use client::{VideoCallClient, VideoCallClientOptions};
pub use encode::{CameraEncoder, MicrophoneEncoder, ScreenEncoder};
pub use media_devices::{MediaDeviceAccess, MediaDeviceList, SelectableDevices};
