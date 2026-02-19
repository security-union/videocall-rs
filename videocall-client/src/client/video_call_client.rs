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

// Connection types - only available with yew-compat feature (requires yew-websocket/yew-webtransport)
#[cfg(feature = "yew-compat")]
use super::super::connection::{ConnectionController, ConnectionState};
use super::super::decode::{PeerDecodeManager, PeerStatus};
// Canvas provider imports - only needed for non-yew-compat mode
#[cfg(not(feature = "yew-compat"))]
use crate::canvas_provider::{CanvasIdProvider, DirectCanvasIdProvider};
use crate::crypto::aes::Aes128State;
use crate::crypto::rsa::RsaWrapper;
use crate::decode::peer_decode_manager::PeerDecodeError;
use crate::diagnostics::{DiagnosticManager, SenderDiagnosticManager};
use crate::event_bus::emit_client_event;
use crate::events::ClientEvent;
use crate::health_reporter::HealthReporter;
use anyhow::{anyhow, Result};
use futures::channel::mpsc::UnboundedSender;
use videocall_diagnostics::{subscribe as subscribe_global_diagnostics, DiagEvent};

use log::{debug, error, info};
use protobuf::Message;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::RsaPublicKey;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use videocall_types::protos::aes_packet::AesPacket;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use web_time::{SystemTime, UNIX_EPOCH};

use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::rsa_packet::RsaPacket;
use videocall_types::SYSTEM_USER_EMAIL;
use wasm_bindgen::JsValue;

// Yew callbacks - only available with yew-compat feature
#[cfg(feature = "yew-compat")]
use yew::prelude::Callback;

/// Options struct for constructing a client via [VideoCallClient::new(options)][VideoCallClient::new]
///
/// This struct supports two modes of operation:
/// 1. **Event Bus Mode** (framework-agnostic): Subscribe to events via `subscribe_client_events()`
/// 2. **Callback Mode** (Yew-compatible): Use Yew `Callback`s for event handling (requires `yew-compat` feature)
///
/// Both modes can be used simultaneously - events are emitted to both the event bus and callbacks.
#[cfg(feature = "yew-compat")]
#[derive(Clone, Debug, PartialEq)]
pub struct VideoCallClientOptions {
    /// `true` to use end-to-end encription; `false` to send data unencrypted
    pub enable_e2ee: bool,

    /// `true` to use webtransport, `false` to use websocket
    pub enable_webtransport: bool,

    /// Callback will be called as `callback(peer_userid)` when a new peer is added
    pub on_peer_added: Callback<String>,

    /// Callback will be called as `callback(peer_userid, media_type)` immediately after the first frame of a given peer & media type is decoded
    pub on_peer_first_frame: Callback<(String, MediaType)>,

    /// Optional callback called as `callback(peer_userid)` when a peer is removed (e.g., heartbeat lost)
    pub on_peer_removed: Option<Callback<String>>,

    /// Callback will be called as `callback(peer_userid)` and must return the DOM id of the
    /// `HtmlCanvasElement` into which the peer video should be rendered
    pub get_peer_video_canvas_id: Callback<String, String>,

    /// Callback will be called as `callback(peer_userid)` and must return the DOM id of the
    /// `HtmlCanvasElement` into which the peer screen image should be rendered
    pub get_peer_screen_canvas_id: Callback<String, String>,

    /// The current client's userid.  This userid will appear as this client's `peer_userid` in the
    /// remote peers' clients.
    pub userid: String,

    /// The meeting ID that this client is joining
    pub meeting_id: String,

    /// The urls to which WebSocket connections should be made (comma-separated)
    pub websocket_urls: Vec<String>,

    /// The urls to which WebTransport connections should be made (comma-separated)
    pub webtransport_urls: Vec<String>,

    /// Callback will be called as `callback(())` after a new connection is made
    pub on_connected: Callback<()>,

    /// Callback will be called as `callback(())` if a connection gets dropped
    pub on_connection_lost: Callback<JsValue>,

    /// `true` to enable diagnostics collection; `false` to disable
    pub enable_diagnostics: bool,

    /// How often to send diagnostics updates in milliseconds (default: 1000)
    pub diagnostics_update_interval_ms: Option<u64>,

    /// `true` to enable health reporting to server; `false` to disable
    pub enable_health_reporting: bool,

    /// How often to send health packets in milliseconds (default: 5000)
    pub health_reporting_interval_ms: Option<u64>,

    /// Callback for encoder settings
    pub on_encoder_settings_update: Option<Callback<String>>,

    /// RTT testing period in milliseconds (default: 3000ms)
    pub rtt_testing_period_ms: u64,

    /// Interval between RTT probes in milliseconds (default: 200ms)
    pub rtt_probe_interval_ms: Option<u64>,

    /// Callback triggered when meeting info is received (optional)
    pub on_meeting_info: Option<Callback<f64>>,

    /// Callback triggered when the meeting ends (optional)
    pub on_meeting_ended: Option<Callback<(f64, String)>>,
}

/// Options struct for constructing a client via [VideoCallClient::new(options)][VideoCallClient::new]
///
/// This is the framework-agnostic version without Yew callbacks.
/// Subscribe to events via `subscribe_client_events()`.
#[cfg(not(feature = "yew-compat"))]
#[derive(Clone, Debug)]
pub struct VideoCallClientOptions {
    /// `true` to use end-to-end encription; `false` to send data unencrypted
    pub enable_e2ee: bool,

    /// `true` to use webtransport, `false` to use websocket
    pub enable_webtransport: bool,

    /// Canvas ID provider for peer video/screen rendering.
    /// Use `DirectCanvasIdProvider` for default behavior or implement `CanvasIdProvider` trait.
    pub canvas_id_provider: Rc<dyn CanvasIdProvider>,

    /// The current client's userid.  This userid will appear as this client's `peer_userid` in the
    /// remote peers' clients.
    pub userid: String,

    /// The meeting ID that this client is joining
    pub meeting_id: String,

    /// The urls to which WebSocket connections should be made (comma-separated)
    pub websocket_urls: Vec<String>,

    /// The urls to which WebTransport connections should be made (comma-separated)
    pub webtransport_urls: Vec<String>,

    /// `true` to enable diagnostics collection; `false` to disable
    pub enable_diagnostics: bool,

    /// How often to send diagnostics updates in milliseconds (default: 1000)
    pub diagnostics_update_interval_ms: Option<u64>,

    /// `true` to enable health reporting to server; `false` to disable
    pub enable_health_reporting: bool,

    /// How often to send health packets in milliseconds (default: 5000)
    pub health_reporting_interval_ms: Option<u64>,

    /// RTT testing period in milliseconds (default: 3000ms)
    pub rtt_testing_period_ms: u64,

    /// Interval between RTT probes in milliseconds (default: 200ms)
    pub rtt_probe_interval_ms: Option<u64>,
}

#[cfg(not(feature = "yew-compat"))]
impl PartialEq for VideoCallClientOptions {
    fn eq(&self, other: &Self) -> bool {
        self.enable_e2ee == other.enable_e2ee
            && self.enable_webtransport == other.enable_webtransport
            && self.userid == other.userid
            && self.meeting_id == other.meeting_id
            && self.websocket_urls == other.websocket_urls
            && self.webtransport_urls == other.webtransport_urls
            && self.enable_diagnostics == other.enable_diagnostics
            && self.diagnostics_update_interval_ms == other.diagnostics_update_interval_ms
            && self.enable_health_reporting == other.enable_health_reporting
            && self.health_reporting_interval_ms == other.health_reporting_interval_ms
            && self.rtt_testing_period_ms == other.rtt_testing_period_ms
            && self.rtt_probe_interval_ms == other.rtt_probe_interval_ms
    }
}

#[cfg(not(feature = "yew-compat"))]
impl Default for VideoCallClientOptions {
    fn default() -> Self {
        Self {
            enable_e2ee: false,
            enable_webtransport: true,
            canvas_id_provider: Rc::new(DirectCanvasIdProvider),
            userid: String::new(),
            meeting_id: String::new(),
            websocket_urls: Vec::new(),
            webtransport_urls: Vec::new(),
            enable_diagnostics: true,
            diagnostics_update_interval_ms: Some(1000),
            enable_health_reporting: true,
            health_reporting_interval_ms: Some(5000),
            rtt_testing_period_ms: 3000,
            rtt_probe_interval_ms: Some(200),
        }
    }
}

#[cfg(feature = "yew-compat")]
#[derive(Debug)]
struct InnerOptions {
    enable_e2ee: bool,
    userid: String,
    on_peer_added: Callback<String>,
    on_meeting_info: Option<Callback<f64>>,
    on_meeting_ended: Option<Callback<(f64, String)>>,
}

#[cfg(not(feature = "yew-compat"))]
#[derive(Debug)]
struct InnerOptions {
    enable_e2ee: bool,
    userid: String,
}

#[cfg(feature = "yew-compat")]
#[derive(Debug)]
struct Inner {
    options: InnerOptions,
    connection_controller: Option<ConnectionController>,
    connection_state: ConnectionState,
    aes: Rc<Aes128State>,
    rsa: Rc<RsaWrapper>,
    peer_decode_manager: PeerDecodeManager,
    _diagnostics: Option<Rc<DiagnosticManager>>,
    sender_diagnostics: Option<Rc<SenderDiagnosticManager>>,
    health_reporter: Option<Rc<RefCell<HealthReporter>>>,
}

// Non-yew-compat Inner struct - connection functionality is not available
#[cfg(not(feature = "yew-compat"))]
#[derive(Debug)]
struct Inner {
    options: InnerOptions,
    aes: Rc<Aes128State>,
    rsa: Rc<RsaWrapper>,
    peer_decode_manager: PeerDecodeManager,
    _diagnostics: Option<Rc<DiagnosticManager>>,
    sender_diagnostics: Option<Rc<SenderDiagnosticManager>>,
    health_reporter: Option<Rc<RefCell<HealthReporter>>>,
}

/// The client struct for a video call connection.
///
/// To use it, first construct the struct using [new(options)][Self::new].  Then when/if desired,
/// create the connection using [connect()][Self::connect].  Once connected, decoding of media from
/// remote peers will start immediately.
///
#[derive(Clone, Debug)]
pub struct VideoCallClient {
    options: VideoCallClientOptions,
    inner: Rc<RefCell<Inner>>,
    aes: Rc<Aes128State>,
    _diagnostics: Option<Rc<DiagnosticManager>>,
}

impl PartialEq for VideoCallClient {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner) && self.options == other.options
    }
}

impl VideoCallClient {
    /// Constructor for the client struct (framework-agnostic version).
    ///
    /// See [VideoCallClientOptions] for description of the options.
    /// Subscribe to events via `subscribe_client_events()`.
    ///
    #[cfg(not(feature = "yew-compat"))]
    pub fn new(options: VideoCallClientOptions) -> Self {
        let aes = Rc::new(Aes128State::new(options.enable_e2ee));

        // Create diagnostics manager if enabled
        let diagnostics = if options.enable_diagnostics {
            let diagnostics = Rc::new(DiagnosticManager::new(options.userid.clone()));

            // Set update interval if provided
            if let Some(interval) = options.diagnostics_update_interval_ms {
                let mut diag = DiagnosticManager::new(options.userid.clone());
                diag.set_reporting_interval(interval);
                let diagnostics = Rc::new(diag);

                Some(diagnostics)
            } else {
                Some(diagnostics)
            }
        } else {
            None
        };

        // Create sender diagnostics manager if diagnostics are enabled
        let sender_diagnostics = if options.enable_diagnostics {
            let sender_diagnostics = Rc::new(SenderDiagnosticManager::new(options.userid.clone()));

            // Set update interval if provided
            if let Some(interval) = options.diagnostics_update_interval_ms {
                sender_diagnostics.set_reporting_interval(interval);
            }

            Some(sender_diagnostics)
        } else {
            None
        };

        // Create health reporter if enabled
        let health_reporter = if options.enable_health_reporting {
            let session_id = format!(
                "session_{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
            );

            let mut reporter = HealthReporter::new(
                session_id,
                options.userid.clone(),
                options.health_reporting_interval_ms.unwrap_or(5000),
            );

            // Set the meeting ID
            reporter.set_meeting_id(options.meeting_id.clone());

            // Set health reporting interval if provided
            if let Some(interval) = options.health_reporting_interval_ms {
                reporter.set_health_interval(interval);
            }

            Some(Rc::new(RefCell::new(reporter)))
        } else {
            None
        };

        let client = Self {
            options: options.clone(),
            inner: Rc::new(RefCell::new(Inner {
                options: InnerOptions {
                    enable_e2ee: options.enable_e2ee,
                    userid: options.userid.clone(),
                },
                aes: aes.clone(),
                rsa: Rc::new(RsaWrapper::new(options.enable_e2ee)),
                peer_decode_manager: Self::create_peer_decoder_manager(
                    &options,
                    diagnostics.clone(),
                ),
                _diagnostics: diagnostics.clone(),
                sender_diagnostics: sender_diagnostics.clone(),
                health_reporter: health_reporter.clone(),
            })),
            aes,
            _diagnostics: diagnostics.clone(),
        };

        // Note: In non-yew-compat mode, we don't set up packet forwarding since
        // send_packet is a no-op (connection functionality requires yew-compat feature).
        // Diagnostics are still collected and can be accessed via the event bus.

        client
    }

    /// Initiates a connection to a videocall server with RTT testing (framework-agnostic version).
    ///
    /// **Note**: Connection functionality requires the `yew-compat` feature since it depends on
    /// yew-websocket and yew-webtransport. Without this feature, this method returns an error.
    ///
    /// To use connection functionality, enable the `yew-compat` feature in your Cargo.toml:
    /// ```toml
    /// videocall-client = { version = "...", features = ["yew-compat"] }
    /// ```
    ///
    #[cfg(not(feature = "yew-compat"))]
    pub fn connect_with_rtt_testing(&mut self) -> anyhow::Result<()> {
        Err(anyhow!(
            "Connection functionality requires the 'yew-compat' feature. \
             Enable it in Cargo.toml: videocall-client = {{ features = [\"yew-compat\"] }}"
        ))
    }

    /// Initiates a connection to a videocall server with automatic RTT-based server selection.
    ///
    /// This method automatically tests all provided servers and connects to the one with the lowest RTT.
    /// For single server deployments, it connects immediately without testing.
    ///
    /// Note that this method's success means only that it succesfully *attempted* initiation of the
    /// connection.  The connection cannot actually be considered to have been succesful until the
    /// [`options.on_connected`](VideoCallClientOptions::on_connected) callback has been invoked.
    ///
    /// If the connection does not succeed, the
    /// [`options.on_connection_lost`](VideoCallClientOptions::on_connection_lost) callback will be
    /// invoked.
    ///
    pub fn connect(&mut self) -> anyhow::Result<()> {
        // Always use RTT testing - it handles single server case efficiently
        info!("Connecting with RTT testing");
        self.connect_with_rtt_testing()
    }

    /// Replace the WebSocket and WebTransport server URLs used for future
    /// connections.
    ///
    /// Call this before [`connect()`][Self::connect] when you have a fresh room
    /// access token and need to reconnect. The existing media pipeline
    /// (encoders, decoders, peer state) is preserved.
    pub fn update_server_urls(
        &mut self,
        websocket_urls: Vec<String>,
        webtransport_urls: Vec<String>,
    ) {
        info!(
            "Updating server URLs: ws={:?}, wt={:?}",
            websocket_urls, webtransport_urls
        );
        self.options.websocket_urls = websocket_urls;
        self.options.webtransport_urls = webtransport_urls;
    }

    #[cfg(not(feature = "yew-compat"))]
    fn create_peer_decoder_manager(
        opts: &VideoCallClientOptions,
        diagnostics: Option<Rc<DiagnosticManager>>,
    ) -> PeerDecodeManager {
        let canvas_provider = opts.canvas_id_provider.clone();
        match diagnostics {
            Some(diagnostics) => {
                PeerDecodeManager::new_with_canvas_provider_and_diagnostics(
                    canvas_provider,
                    diagnostics,
                )
            }
            None => PeerDecodeManager::new_with_canvas_provider(canvas_provider),
        }
    }

    #[cfg(not(feature = "yew-compat"))]
    pub(crate) fn send_packet(&self, media: PacketWrapper) {
        let packet_type = media.packet_type.enum_value();
        debug!("send_packet called in non-yew-compat mode for {packet_type:?} - packet dropped (no connection support)");
    }

    /// Returns `true` if the client is currently connected to a server.
    /// Note: Without yew-compat feature, connection is not supported.
    #[cfg(not(feature = "yew-compat"))]
    pub fn is_connected(&self) -> bool {
        false
    }

    /// Disconnect from the current server.
    /// Note: Without yew-compat feature, connection is not supported.
    #[cfg(not(feature = "yew-compat"))]
    pub fn disconnect(&self) -> anyhow::Result<()> {
        Ok(()) // No-op in non-yew-compat mode
    }

    /// Returns a vector of the userids of the currently connected remote peers, sorted alphabetically.
    pub fn sorted_peer_keys(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.sorted_keys().to_vec(),
            Err(_) => Vec::<String>::new(),
        }
    }

    /// Hacky function that returns true if the given peer has yet to send a frame of screen share.
    ///
    /// No reason for this function to exist, it should be deducible from the
    /// [`options.on_peer_first_frame(key, MediaType::Screen)`](VideoCallClientOptions::on_peer_first_frame)
    /// callback.   Or if polling is really necessary, instead of being hardwired for screen, it'd
    /// be more elegant to at least pass a `MediaType`.
    ///
    pub fn is_awaiting_peer_screen_frame(&self, key: &String) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(key) {
                return peer.screen.is_waiting_for_keyframe();
            }
        }
        false
    }

    pub fn is_video_enabled_for_peer(&self, key: &String) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(key) {
                return peer.video_enabled;
            }
        }
        false
    }

    pub fn is_screen_share_enabled_for_peer(&self, key: &String) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(key) {
                return peer.screen_enabled;
            }
        }
        false
    }

    pub fn is_audio_enabled_for_peer(&self, key: &String) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(key) {
                return peer.audio_enabled;
            }
        }
        false
    }

    pub(crate) fn aes(&self) -> Rc<Aes128State> {
        self.aes.clone()
    }

    /// Returns a reference to a copy of [`options.userid`](VideoCallClientOptions::userid)
    pub fn userid(&self) -> &String {
        &self.options.userid
    }

    /// Get RTT measurements (not available without yew-compat feature)
    #[cfg(not(feature = "yew-compat"))]
    pub fn get_rtt_measurements(&self) -> Option<HashMap<String, f64>> {
        None
    }

    /// Send RTT probes (not available without yew-compat feature)
    #[cfg(not(feature = "yew-compat"))]
    pub fn send_rtt_probes(&self) -> anyhow::Result<()> {
        Err(anyhow!("RTT probes require the 'yew-compat' feature"))
    }

    /// Check and complete election (no-op without yew-compat feature)
    #[cfg(not(feature = "yew-compat"))]
    pub fn check_election_completion(&self) {
        // No-op in non-yew-compat mode
    }

    /// Get diagnostics information for all peers
    ///
    /// Returns a formatted string with FPS stats for all peers if diagnostics are enabled,
    /// or None if diagnostics are disabled.
    pub fn get_diagnostics(&self) -> Option<String> {
        self.inner.borrow().peer_decode_manager.get_all_fps_stats()
    }

    /// Set the canvas element for a peer's video rendering
    ///
    /// This method allows you to pass a canvas reference directly instead of relying on DOM queries.
    /// Should be called when the canvas element is mounted in the UI.
    ///
    /// # Arguments
    /// * `peer_id` - The ID of the peer
    /// * `canvas` - The HtmlCanvasElement to render video frames to
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err(JsValue)` if the peer doesn't exist or canvas setup fails
    pub fn set_peer_video_canvas(
        &self,
        peer_id: &str,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        if let Ok(inner) = self.inner.try_borrow() {
            inner
                .peer_decode_manager
                .set_peer_video_canvas(peer_id, canvas)
        } else {
            Err(JsValue::from_str("Failed to borrow inner state"))
        }
    }

    /// Set the canvas element for a peer's screen share rendering
    ///
    /// This method allows you to pass a canvas reference directly instead of relying on DOM queries.
    /// Should be called when the canvas element is mounted in the UI.
    ///
    /// # Arguments
    /// * `peer_id` - The ID of the peer
    /// * `canvas` - The HtmlCanvasElement to render screen frames to
    ///
    /// # Returns
    /// * `Ok(())` if successful
    /// * `Err(JsValue)` if the peer doesn't exist or canvas setup fails
    pub fn set_peer_screen_canvas(
        &self,
        peer_id: &str,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        if let Ok(inner) = self.inner.try_borrow() {
            inner
                .peer_decode_manager
                .set_peer_screen_canvas(peer_id, canvas)
        } else {
            Err(JsValue::from_str("Failed to borrow inner state"))
        }
    }

    /// Get the FPS for a specific peer and media type
    ///
    /// Returns the current frames per second for the specified peer and media type,
    /// or 0.0 if diagnostics are disabled or the peer doesn't exist.
    pub fn get_peer_fps(&self, peer_id: &str, media_type: MediaType) -> f64 {
        self.inner
            .borrow()
            .peer_decode_manager
            .get_fps(peer_id, media_type)
    }

    /// Send a diagnostic packet to the server
    pub fn send_diagnostic_packet(&self, packet: DiagnosticsPacket) {
        let wrapper = PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            email: self.options.userid.clone(),
            data: packet.write_to_bytes().unwrap(),
            ..Default::default()
        };
        self.send_packet(wrapper);
    }

    pub fn subscribe_diagnostics(
        &self,
        tx: UnboundedSender<DiagnosticsPacket>,
        media_type: MediaType,
    ) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(sender_diagnostics) = &inner.sender_diagnostics {
                sender_diagnostics.add_sender_channel(tx, media_type);
            }
        }
    }

    /// Subscribe to the global diagnostics broadcast system
    ///
    /// Returns a receiver that will receive all diagnostic events from across the system.
    /// This is the new preferred way to access diagnostics data using the MPMC broadcast pattern.
    pub fn subscribe_global_diagnostics(&self) -> async_broadcast::Receiver<DiagEvent> {
        subscribe_global_diagnostics()
    }

    /// Remove a peer from health tracking
    pub fn remove_peer_health(&self, peer_id: &str) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(health_reporter) = &inner.health_reporter {
                if let Ok(reporter) = health_reporter.try_borrow() {
                    reporter.remove_peer(peer_id);
                    debug!("Removed peer from health tracking: {peer_id}");
                }
            }
        }
    }

    #[cfg(not(feature = "yew-compat"))]
    pub fn set_video_enabled(&self, enabled: bool) {
        debug!("set_video_enabled({enabled}) called in non-yew-compat mode - no-op");
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(hr) = &inner.health_reporter {
                if let Ok(hrb) = hr.try_borrow() {
                    hrb.set_reporting_video_enabled(enabled);
                }
            }
        }
    }

    #[cfg(not(feature = "yew-compat"))]
    pub fn set_audio_enabled(&self, enabled: bool) {
        debug!("set_audio_enabled({enabled}) called in non-yew-compat mode - no-op");
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(hr) = &inner.health_reporter {
                if let Ok(hrb) = hr.try_borrow() {
                    hrb.set_reporting_audio_enabled(enabled);
                }
            }
        }
    }

    #[cfg(not(feature = "yew-compat"))]
    pub fn set_screen_enabled(&self, _enabled: bool) {
        debug!("set_screen_enabled called in non-yew-compat mode - no-op");
    }

    /// Updates the speaker device for all connected peers
    ///
    /// This will recreate all audio decoders to use the specified speaker device.
    /// Pass None to use the default system speaker.
    pub fn update_speaker_device(&self, speaker_device_id: Option<String>) -> Result<(), JsValue> {
        match self.inner.try_borrow_mut() {
            Ok(mut inner) => inner
                .peer_decode_manager
                .update_speaker_device(speaker_device_id),
            Err(_) => {
                error!("Failed to borrow inner for updating speaker device");
                Err(JsValue::from_str(
                    "Failed to borrow inner for updating speaker device",
                ))
            }
        }
    }
}

impl Inner {
    #[cfg(not(feature = "yew-compat"))]
    fn on_inbound_media(&mut self, response: PacketWrapper) {
        debug!(
            "<< Received {:?} from {}",
            response.packet_type.enum_value(),
            response.email
        );
        // Skip creating peers for system messages (meeting info, meeting started/ended)
        let peer_status = if response.email == SYSTEM_USER_EMAIL {
            PeerStatus::NoChange
        } else {
            self.peer_decode_manager.ensure_peer(&response.email)
        };
        match response.packet_type.enum_value() {
            Ok(PacketType::AES_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                if let Ok(bytes) = self.rsa.decrypt(&response.data) {
                    debug!("Decrypted AES_KEY from {}", response.email);
                    match AesPacket::parse_from_bytes(&bytes) {
                        Ok(aes_packet) => {
                            if let Err(e) = self.peer_decode_manager.set_peer_aes(
                                &response.email,
                                Aes128State::from_vecs(
                                    aes_packet.key,
                                    aes_packet.iv,
                                    self.options.enable_e2ee,
                                ),
                            ) {
                                error!("Failed to set peer aes: {e}");
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse aes packet: {e}");
                        }
                    }
                }
            }
            Ok(PacketType::RSA_PUB_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                let encrypted_aes_packet = parse_rsa_packet(&response.data)
                    .and_then(parse_public_key)
                    .and_then(|pub_key| {
                        self.serialize_aes_packet()
                            .map(|aes_packet| (aes_packet, pub_key))
                    })
                    .and_then(|(aes_packet, pub_key)| {
                        self.encrypt_aes_packet(&aes_packet, &pub_key)
                    });

                match encrypted_aes_packet {
                    Ok(_data) => {
                        debug!(">> {} would send AES key (no connection in non-yew-compat mode)", self.options.userid);
                        // Note: Cannot send packets without yew-compat feature
                    }
                    Err(e) => {
                        error!("Failed to prepare AES_KEY for peer: {e}");
                    }
                }
            }
            Ok(PacketType::MEDIA) => {
                let email = response.email.clone();

                if let Err(e) = self
                    .peer_decode_manager
                    .decode(response, &self.options.userid)
                {
                    error!("error decoding packet: {e}");
                    match e {
                        PeerDecodeError::SameUserPacket(email) => {
                            debug!("Rejecting packet from same user: {email}");
                        }
                        _ => {
                            self.peer_decode_manager.delete_peer(&email);
                        }
                    }
                }
            }
            Ok(PacketType::CONNECTION) => {
                let data_str = String::from_utf8_lossy(&response.data);
                debug!("Received CONNECTION packet: {data_str}");
            }
            Ok(PacketType::DIAGNOSTICS) => {
                if let Ok(diagnostics_packet) = DiagnosticsPacket::parse_from_bytes(&response.data)
                {
                    debug!("Received diagnostics packet: {diagnostics_packet:?}");
                    if let Some(sender_diagnostics) = &self.sender_diagnostics {
                        sender_diagnostics.handle_diagnostic_packet(diagnostics_packet);
                    }
                } else {
                    error!("Failed to parse diagnostics packet");
                }
            }
            Ok(PacketType::HEALTH) => {
                debug!(
                    "Received unexpected health packet from {}, ignoring",
                    response.email
                );
            }
            Ok(PacketType::MEETING) => {
                match MeetingPacket::parse_from_bytes(&response.data) {
                    Ok(meeting_packet) => {
                        match meeting_packet.event_type.enum_value() {
                            Ok(MeetingEventType::MEETING_STARTED) => {
                                info!(
                                    "Received MEETING_STARTED: room={}, start_time={}ms, creator={}",
                                    meeting_packet.room_id,
                                    meeting_packet.start_time_ms,
                                    meeting_packet.creator_id
                                );
                                emit_client_event(ClientEvent::MeetingInfo(
                                    meeting_packet.start_time_ms as f64,
                                ));
                            }
                            Ok(MeetingEventType::MEETING_ENDED) => {
                                info!(
                                    "Received MEETING_ENDED: room={}, message={}",
                                    meeting_packet.room_id, meeting_packet.message
                                );
                                let end_time_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as f64)
                                    .unwrap_or(0.0);
                                emit_client_event(ClientEvent::MeetingEnded {
                                    end_time_ms,
                                    message: meeting_packet.message,
                                });
                            }
                            Ok(MeetingEventType::PARTICIPANT_JOINED) => {
                                info!(
                                    "Received PARTICIPANT_JOINED: room={}, count={}",
                                    meeting_packet.room_id, meeting_packet.participant_count
                                );
                            }
                            Ok(MeetingEventType::PARTICIPANT_LEFT) => {
                                info!(
                                    "Received PARTICIPANT_LEFT: room={}, count={}",
                                    meeting_packet.room_id, meeting_packet.participant_count
                                );
                            }
                            Ok(MeetingEventType::MEETING_EVENT_TYPE_UNKNOWN) => {
                                error!(
                                    "Received meeting packet with unknown event type: room={}",
                                    meeting_packet.room_id
                                );
                            }
                            Err(e) => {
                                error!("Failed to parse MeetingEventType: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse MeetingPacket: {e}");
                    }
                }
            }
            Ok(PacketType::PACKET_TYPE_UNKNOWN) => {
                error!(
                    "Received packet with unknown packet type from {}",
                    response.email
                );
            }
            Err(e) => {
                error!("Failed to parse packet type: {e}");
            }
        }
        if let PeerStatus::Added(peer_userid) = peer_status {
            if peer_userid != self.options.userid {
                emit_client_event(ClientEvent::PeerAdded(peer_userid));
                self.send_public_key();
            } else {
                log::debug!("Rejecting packet from same user: {peer_userid}");
            }
        }
    }

    fn send_public_key(&self) {
        if !self.options.enable_e2ee {
            return;
        }
        let userid = self.options.userid.clone();
        let rsa = &*self.rsa;
        match rsa.pub_key.to_public_key_der() {
            Ok(public_key_der) => {
                let packet = RsaPacket {
                    username: userid.clone(),
                    public_key_der: public_key_der.to_vec(),
                    ..Default::default()
                };
                match packet.write_to_bytes() {
                    Ok(_data) => {
                        debug!(">> {userid} would send public key (no connection in non-yew-compat mode)");
                        // Note: Cannot send packets without yew-compat feature
                    }
                    Err(e) => {
                        error!("Failed to serialize rsa packet: {e}");
                    }
                }
            }
            Err(e) => {
                error!("Failed to export rsa public key to der: {e}");
            }
        }
    }

    fn serialize_aes_packet(&self) -> Result<Vec<u8>> {
        AesPacket {
            key: self.aes.key.to_vec(),
            iv: self.aes.iv.to_vec(),
            ..Default::default()
        }
        .write_to_bytes()
        .map_err(|e| anyhow!("Failed to serialize aes packet: {e}"))
    }

    fn encrypt_aes_packet(&self, aes_packet: &[u8], pub_key: &RsaPublicKey) -> Result<Vec<u8>> {
        self.rsa
            .encrypt_with_key(aes_packet, pub_key)
            .map_err(|e| anyhow!("Failed to encrypt aes packet: {e}"))
    }
}

fn parse_rsa_packet(response_data: &[u8]) -> Result<RsaPacket> {
    RsaPacket::parse_from_bytes(response_data)
        .map_err(|e| anyhow!("Failed to parse rsa packet: {e}"))
}

fn parse_public_key(rsa_packet: RsaPacket) -> Result<RsaPublicKey> {
    RsaPublicKey::from_public_key_der(&rsa_packet.public_key_der)
        .map_err(|e| anyhow!("Failed to parse rsa public key: {e}"))
}

#[cfg(feature = "yew-compat")]
#[path = "video_call_client_yew.rs"]
mod yew_compat;
