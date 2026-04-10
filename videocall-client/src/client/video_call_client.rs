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

use super::super::connection::{ConnectionController, ConnectionManagerOptions, ConnectionState};
use super::super::decode::{PeerDecodeManager, PeerStatus};
use crate::crypto::aes::Aes128State;
use crate::crypto::rsa::RsaWrapper;
use crate::decode::peer_decode_manager::PeerDecodeError;
use crate::diagnostics::{DiagnosticManager, SenderDiagnosticManager};
use crate::health_reporter::HealthReporter;
use anyhow::{anyhow, Result};
use futures::channel::mpsc::UnboundedSender;
use videocall_diagnostics::{subscribe as subscribe_global_diagnostics, DiagEvent};

use log::{debug, error, info, warn};
use protobuf::Message;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::RsaPublicKey;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use videocall_types::protos::aes_packet::AesPacket;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use web_time::{SystemTime, UNIX_EPOCH};

use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::rsa_packet::RsaPacket;
use videocall_types::Callback;
use videocall_types::SYSTEM_USER_ID;
use wasm_bindgen::JsValue;

/// Generate a cryptographically random instance ID for correlating reconnections.
/// Uses `crypto.getRandomValues()` for unpredictability since the instance_id
/// is used for session eviction (a predictable ID could allow targeted eviction).
fn generate_instance_id() -> String {
    let mut buf = [0u8; 16];
    if let Some(crypto) = web_sys::window().and_then(|w| w.crypto().ok()) {
        let _ = crypto.get_random_values_with_u8_array(&mut buf);
    } else {
        // Fallback for environments without window.crypto (e.g., workers).
        let rand = || (js_sys::Math::random() * 0xFFFF_FFFF_u32 as f64) as u32;
        buf[0..4].copy_from_slice(&rand().to_be_bytes());
        buf[4..8].copy_from_slice(&rand().to_be_bytes());
        buf[8..12].copy_from_slice(&rand().to_be_bytes());
        buf[12..16].copy_from_slice(&rand().to_be_bytes());
    }
    format!(
        "{:08x}-{:08x}-{:08x}-{:08x}",
        u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
        u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
    )
}

/// Configuration options for creating a [`VideoCallClient`].
///
/// Contains all the callbacks, server URLs, and feature flags needed to
/// initialise the client.  Pass an instance of this struct to
/// [`VideoCallClient::new()`].
#[derive(Clone, Debug, PartialEq)]
pub struct VideoCallClientOptions {
    pub enable_e2ee: bool,
    pub enable_webtransport: bool,
    pub on_peer_added: Callback<String>,
    pub on_peer_first_frame: Callback<(String, MediaType)>,
    pub on_peer_removed: Option<Callback<String>>,
    pub get_peer_video_canvas_id: Callback<String, String>,
    pub get_peer_screen_canvas_id: Callback<String, String>,
    pub user_id: String,
    pub display_name: String,
    pub meeting_id: String,
    pub websocket_urls: Vec<String>,
    pub webtransport_urls: Vec<String>,
    pub on_connected: Callback<()>,
    pub on_connection_lost: Callback<JsValue>,
    pub enable_diagnostics: bool,
    pub diagnostics_update_interval_ms: Option<u64>,
    pub enable_health_reporting: bool,
    pub health_reporting_interval_ms: Option<u64>,
    pub on_encoder_settings_update: Option<Callback<String>>,
    pub rtt_testing_period_ms: u64,
    pub rtt_probe_interval_ms: Option<u64>,
    pub on_meeting_info: Option<Callback<f64>>,
    pub on_meeting_ended: Option<Callback<(f64, String)>>,

    /// Callback fired when the local user's speaking state changes (from
    /// encoder-side VAD).  The UI can use this to highlight the local
    /// participant's tile.
    pub on_speaking_changed: Option<Callback<bool>>,

    /// Callback fired with the local user's normalized audio level (0.0–1.0)
    /// from encoder-side VAD.  Fires when the level changes by more than 0.02.
    pub on_audio_level_changed: Option<Callback<f32>>,

    /// RMS threshold for voice activity detection.  Values typically range
    /// from 0.0 to 1.0; the default is 0.02.  Lower values are more
    /// sensitive; higher values filter out more background noise.
    pub vad_threshold: Option<f32>,

    /// Callback triggered when the meeting is activated by the host (optional)
    pub on_meeting_activated: Option<Callback<()>>,

    /// Callback triggered when this participant is admitted from the waiting room (optional).
    /// The client should fetch the room_token via HTTP after receiving this notification.
    pub on_participant_admitted: Option<Callback<()>>,

    /// Callback triggered when this participant is rejected from the waiting room (optional)
    pub on_participant_rejected: Option<Callback<()>>,

    /// Callback triggered when the waiting room participant list changes (optional)
    pub on_waiting_room_updated: Option<Callback<()>>,

    /// Callback triggered when a remote participant leaves the meeting.
    /// Emits `(display_name, user_id)` from the PARTICIPANT_LEFT meeting event.
    pub on_peer_left: Option<Callback<(String, String)>>,

    /// Callback triggered when a participant changes their display name.
    /// Emits `(user_id, new_display_name)`.
    pub on_display_name_changed: Option<Callback<(String, String)>>,

    /// Callback triggered when a remote participant joins the meeting.
    /// Emits `(display_name, user_id)` from the PARTICIPANT_JOINED meeting event.
    pub on_peer_joined: Option<Callback<(String, String)>>,
}

#[derive(Debug)]
struct InnerOptions {
    enable_e2ee: bool,
    user_id: String,
    on_peer_added: Callback<String>,
    on_meeting_info: Option<Callback<f64>>,
    on_meeting_ended: Option<Callback<(f64, String)>>,
    on_meeting_activated: Option<Callback<()>>,
    on_participant_admitted: Option<Callback<()>>,
    on_participant_rejected: Option<Callback<()>>,
    on_waiting_room_updated: Option<Callback<()>>,
    on_peer_left: Option<Callback<(String, String)>>,
    on_peer_joined: Option<Callback<(String, String)>>,
    on_display_name_changed: Option<Callback<(String, String)>>,
}

#[derive(Debug)]
struct Inner {
    options: InnerOptions,
    connection_controller: Rc<RefCell<Option<Rc<ConnectionController>>>>,
    connection_state: ConnectionState,
    aes: Rc<Aes128State>,
    rsa: Rc<RsaWrapper>,
    peer_decode_manager: PeerDecodeManager,
    _diagnostics: Option<Rc<DiagnosticManager>>,
    sender_diagnostics: Option<Rc<SenderDiagnosticManager>>,
    health_reporter: Option<Rc<RefCell<HealthReporter>>>,
    own_session_id: Option<u64>,
    /// Recently processed peer events for deduplication.
    /// Both WebSocket and WebTransport connections receive the same NATS system
    /// messages, so we deduplicate by (event_type, target_user_id) within a
    /// short time window to avoid firing duplicate toast notifications.
    /// Key: (event_type_str, target_user_id), Value: timestamp_ms
    recent_peer_events: HashMap<(String, String), f64>,
    /// Flag set by incoming KEYFRAME_REQUEST for camera video. The
    /// `CameraEncoder` checks this flag each frame and forces a keyframe.
    force_camera_keyframe: Arc<AtomicBool>,
    /// Flag set by incoming KEYFRAME_REQUEST for screen share.
    force_screen_keyframe: Arc<AtomicBool>,
    /// Flag set when a CONGESTION signal is received from the server.
    /// The camera encoder's diagnostics loop checks this flag and calls
    /// `force_video_step_down()` on the `EncoderBitrateController`.
    congestion_step_down_requested: Arc<AtomicBool>,
}

/// The main client handle for a video call session.
///
/// `VideoCallClient` is cheaply cloneable (`Rc`-based interior mutability)
/// and is passed to encoders and other subsystems so they can send packets
/// and query connection state.
#[derive(Clone, Debug)]
pub struct VideoCallClient {
    options: VideoCallClientOptions,
    inner: Rc<RefCell<Inner>>,
    connection_controller: Rc<RefCell<Option<Rc<ConnectionController>>>>,
    aes: Rc<Aes128State>,
    _diagnostics: Option<Rc<DiagnosticManager>>,
}

impl PartialEq for VideoCallClient {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
            && Rc::ptr_eq(&self.connection_controller, &other.connection_controller)
            && self.options == other.options
    }
}

impl VideoCallClient {
    /// Create a new `VideoCallClient` from the given options.
    ///
    /// This does **not** establish a connection; call [`connect()`](Self::connect)
    /// afterwards to begin the RTT election and connect to a server.
    pub fn new(options: VideoCallClientOptions) -> Self {
        let aes = Rc::new(Aes128State::new(options.enable_e2ee));

        let diagnostics = if options.enable_diagnostics {
            let diagnostics = Rc::new(DiagnosticManager::new(options.user_id.clone()));

            if let Some(interval) = options.diagnostics_update_interval_ms {
                let mut diag = DiagnosticManager::new(options.user_id.clone());
                diag.set_reporting_interval(interval);
                let diagnostics = Rc::new(diag);

                Some(diagnostics)
            } else {
                Some(diagnostics)
            }
        } else {
            None
        };

        let sender_diagnostics = if options.enable_diagnostics {
            let sender_diagnostics = Rc::new(SenderDiagnosticManager::new(options.user_id.clone()));

            if let Some(interval) = options.diagnostics_update_interval_ms {
                sender_diagnostics.set_reporting_interval(interval);
            }

            Some(sender_diagnostics)
        } else {
            None
        };

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
                options.user_id.clone(),
                options.health_reporting_interval_ms.unwrap_or(5000),
            );

            reporter.set_meeting_id(options.meeting_id.clone());
            reporter.set_display_name(options.display_name.clone());

            if let Some(interval) = options.health_reporting_interval_ms {
                reporter.set_health_interval(interval);
            }

            Some(Rc::new(RefCell::new(reporter)))
        } else {
            None
        };

        let connection_controller: Rc<RefCell<Option<Rc<ConnectionController>>>> =
            Rc::new(RefCell::new(None));

        let force_camera_keyframe = Arc::new(AtomicBool::new(false));
        let force_screen_keyframe = Arc::new(AtomicBool::new(false));
        let congestion_step_down_requested = Arc::new(AtomicBool::new(false));

        let client = Self {
            options: options.clone(),
            inner: Rc::new(RefCell::new(Inner {
                options: InnerOptions {
                    enable_e2ee: options.enable_e2ee,
                    user_id: options.user_id.clone(),
                    on_peer_added: options.on_peer_added.clone(),
                    on_meeting_ended: options.on_meeting_ended.clone(),
                    on_meeting_info: options.on_meeting_info.clone(),
                    on_meeting_activated: options.on_meeting_activated.clone(),
                    on_participant_admitted: options.on_participant_admitted.clone(),
                    on_participant_rejected: options.on_participant_rejected.clone(),
                    on_waiting_room_updated: options.on_waiting_room_updated.clone(),
                    on_display_name_changed: options.on_display_name_changed.clone(),
                    on_peer_left: options.on_peer_left.clone(),
                    on_peer_joined: options.on_peer_joined.clone(),
                },
                connection_controller: connection_controller.clone(),
                connection_state: ConnectionState::Failed {
                    error: "Not connected".to_string(),
                    last_known_server: None,
                },
                own_session_id: None,
                aes: aes.clone(),
                rsa: Rc::new(RsaWrapper::new(options.enable_e2ee)),
                peer_decode_manager: Self::create_peer_decoder_manager(
                    &options,
                    diagnostics.clone(),
                ),
                _diagnostics: diagnostics.clone(),
                sender_diagnostics: sender_diagnostics.clone(),
                health_reporter: health_reporter.clone(),
                recent_peer_events: HashMap::new(),
                force_camera_keyframe: force_camera_keyframe.clone(),
                force_screen_keyframe: force_screen_keyframe.clone(),
                congestion_step_down_requested: congestion_step_down_requested.clone(),
            })),
            connection_controller,
            aes,
            _diagnostics: diagnostics.clone(),
        };

        // Wire up the send-packet callback on PeerDecodeManager so it can
        // send KEYFRAME_REQUEST packets back through the connection.
        {
            let client_for_pli = client.clone();
            if let Ok(mut inner) = client.inner.try_borrow_mut() {
                inner.peer_decode_manager.set_send_packet_callback(
                    Callback::from(move |packet: PacketWrapper| {
                        client_for_pli.send_packet(packet);
                    }),
                    options.user_id.clone(),
                );
            }
        }

        if let Some(diagnostics) = &diagnostics {
            let client_clone = client.clone();
            diagnostics.set_packet_handler(Callback::from(move |packet| {
                client_clone.send_diagnostic_packet(packet);
            }));
        }

        if let Some(health_reporter) = &health_reporter {
            if let Ok(mut reporter) = health_reporter.try_borrow_mut() {
                let client_clone = client.clone();
                reporter.set_send_packet_callback(Callback::from(move |packet| {
                    client_clone.send_packet(packet);
                }));

                reporter.start_diagnostics_subscription();

                reporter.start_health_reporting();
                debug!("Health reporting started with real diagnostics subscription");
            }
        }

        client
    }

    pub fn connect_with_rtt_testing(&mut self) -> anyhow::Result<()> {
        // Idempotency guard: if a ConnectionController already exists we need
        // to decide whether to skip (actively connecting/connected) or tear
        // down a stale controller (failed state) before reconnecting.
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                let state = controller.get_connection_state();
                match state {
                    // Election running, connection active, or manager is
                    // already handling its own reconnection — skip.
                    ConnectionState::Testing { .. }
                    | ConnectionState::Connected { .. }
                    | ConnectionState::Reconnecting { .. } => {
                        info!(
                            "connect() called but ConnectionController is in {state:?} state — skipping duplicate connection"
                        );
                        return Ok(());
                    }
                    // Connection permanently failed — tear down the stale
                    // controller and create a fresh one below.
                    ConnectionState::Failed { .. } => {
                        drop(cc);
                        info!("connect() called with failed ConnectionController — disconnecting before reconnect");
                        let _ = self.disconnect();
                    }
                }
            }
        }

        let websocket_count = self.options.websocket_urls.len();
        let webtransport_count = if self.options.enable_webtransport {
            self.options.webtransport_urls.len()
        } else {
            0
        };
        let total_servers = websocket_count + webtransport_count;

        info!(
            "Starting RTT testing for {total_servers} servers (WebSocket: {websocket_count}, WebTransport: {webtransport_count})"
        );

        if total_servers == 0 {
            return Err(anyhow!("No servers provided for RTT testing"));
        }

        let election_period_ms = self.options.rtt_testing_period_ms;

        info!("RTT testing period: {election_period_ms}ms");

        let manager_options = ConnectionManagerOptions {
            websocket_urls: self.options.websocket_urls.clone(),
            webtransport_urls: if self.options.enable_webtransport {
                self.options.webtransport_urls.clone()
            } else {
                Vec::new()
            },
            userid: self.options.user_id.clone(),
            on_inbound_media: {
                let inner = Rc::downgrade(&self.inner);
                Callback::from(move |packet| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(mut inner) = inner.try_borrow_mut() {
                            inner.on_inbound_media(packet);
                        } else {
                            warn!("on_inbound_media: transient borrow conflict, dropping packet");
                        }
                    }
                })
            },
            on_state_changed: {
                let on_connected = self.options.on_connected.clone();
                let on_connection_lost = self.options.on_connection_lost.clone();
                let inner = Rc::downgrade(&self.inner);
                Callback::from(move |state: ConnectionState| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        if let Ok(mut inner) = inner.try_borrow_mut() {
                            inner.connection_state = state.clone();

                            // On connection failure, immediately terminate all
                            // decoder workers so stale WASM instances don't
                            // accumulate memory during reconnection.
                            if matches!(state, ConnectionState::Failed { .. }) {
                                inner.peer_decode_manager.clear_all_peers();
                            }
                        }
                    }
                    info!("Connection state changed: {state:?} in video call client");

                    match state {
                        ConnectionState::Connected { .. } => {
                            on_connected.emit(());
                        }
                        ConnectionState::Failed { error, .. } => {
                            on_connection_lost.emit(JsValue::from_str(&error));
                        }
                        _ => {}
                    }
                })
            },
            peer_monitor: {
                let inner = Rc::downgrade(&self.inner);
                Callback::from(move |_| {
                    if let Some(inner) = Weak::upgrade(&inner) {
                        match inner.try_borrow_mut() {
                            Ok(mut inner) => {
                                let removed = inner.peer_decode_manager.run_peer_monitor();
                                if !removed.is_empty() {
                                    if let Some(hr) = &inner.health_reporter {
                                        if let Ok(reporter) = hr.try_borrow() {
                                            for peer_id in &removed {
                                                reporter.remove_peer(peer_id);
                                            }
                                        }
                                    }
                                }
                            }
                            Err(_) => {
                                // Transient borrow conflict — another callback
                                // (e.g. on_inbound_media) currently holds the
                                // mutable borrow.  Skip this cycle; the next
                                // 5-second interval will retry.  This must NOT
                                // emit on_connection_lost which would trigger a
                                // full reconnect.
                                warn!(
                                    "peer_monitor: transient borrow conflict, skipping this cycle"
                                );
                            }
                        }
                    }
                })
            },
            election_period_ms,
            instance_id: generate_instance_id(),
        };

        let connection_controller = ConnectionController::new(manager_options, self.aes.clone())?;

        // Store the controller as an Rc so we can share it with the health reporter
        let controller_rc = Rc::new(connection_controller);
        *self.connection_controller.try_borrow_mut()? = Some(controller_rc.clone());

        // Pass the connection controller to the health reporter for communication metrics
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(hr) = &inner.health_reporter {
                if let Ok(hrb) = hr.try_borrow() {
                    hrb.set_connection_controller(controller_rc);
                }
            }
        }

        info!("ConnectionManager created with RTT testing and 1Hz diagnostics reporting");
        Ok(())
    }

    /// Open connections to all configured servers, run RTT-based election,
    /// and start media flow on the winner.
    pub fn connect(&mut self) -> anyhow::Result<()> {
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

    fn create_peer_decoder_manager(
        opts: &VideoCallClientOptions,
        diagnostics: Option<Rc<DiagnosticManager>>,
    ) -> PeerDecodeManager {
        let mut peer_decode_manager = match diagnostics {
            Some(diagnostics) => PeerDecodeManager::new_with_diagnostics(diagnostics),
            None => PeerDecodeManager::new(),
        };
        peer_decode_manager.on_first_frame = opts.on_peer_first_frame.clone();
        peer_decode_manager.get_video_canvas_id = opts.get_peer_video_canvas_id.clone();
        peer_decode_manager.get_screen_canvas_id = opts.get_peer_screen_canvas_id.clone();
        if let Some(cb) = opts.on_peer_removed.as_ref() {
            peer_decode_manager.on_peer_removed = cb.clone();
        }
        peer_decode_manager.set_vad_threshold(opts.vad_threshold);
        peer_decode_manager
    }

    pub(crate) fn send_packet(&self, media: PacketWrapper) {
        let packet_type = media.packet_type.enum_value();
        match self.connection_controller.try_borrow() {
            Ok(cc) => {
                if let Some(controller) = cc.as_ref() {
                    if let Err(e) = controller.send_packet(media) {
                        debug!("Failed to send {packet_type:?} packet: {e}");
                    }
                } else {
                    error!("No connection manager available for {packet_type:?} packet");
                }
            }
            Err(_) => {
                error!("Unable to borrow connection_controller -- dropping {packet_type:?} packet")
            }
        }
    }

    /// Send a media packet via reliable stream.
    ///
    /// Used for VIDEO, AUDIO, and SCREEN packets where reliable delivery is
    /// required to avoid visual/audio artifacts from packet loss. Control
    /// packets (heartbeats, RTT probes, diagnostics) use datagrams instead
    /// since they are periodic and expendable.
    pub(crate) fn send_media_packet(&self, media: PacketWrapper) {
        let packet_type = media.packet_type.enum_value();
        match self.connection_controller.try_borrow() {
            Ok(cc) => {
                if let Some(controller) = cc.as_ref() {
                    if let Err(e) = controller.send_packet(media) {
                        debug!("Failed to send {packet_type:?} media packet: {e}");
                    }
                } else {
                    error!("No connection manager available for {packet_type:?} media packet");
                }
            }
            Err(_) => {
                error!(
                    "Unable to borrow connection_controller -- dropping {packet_type:?} media packet"
                )
            }
        }
    }

    /// Returns `true` if the client has an active, elected connection.
    pub fn is_connected(&self) -> bool {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                return controller.is_connected();
            }
        }
        false
    }

    /// Disconnect from the current session, tearing down the connection
    /// controller and clearing peer state.
    pub fn disconnect(&self) -> anyhow::Result<()> {
        // Disconnect and clear the connection controller via its own RefCell
        if let Ok(mut cc) = self.connection_controller.try_borrow_mut() {
            if let Some(controller) = cc.as_mut() {
                let _ = controller.disconnect();
            }
            *cc = None;
        } else {
            return Err(anyhow::anyhow!(
                "Unable to borrow connection_controller for disconnect"
            ));
        }

        // Update connection state via inner
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.connection_state = ConnectionState::Failed {
                error: "Disconnected".to_string(),
                last_known_server: None,
            };
        }

        Ok(())
    }

    pub fn sorted_peer_keys(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner
                .peer_decode_manager
                .sorted_keys()
                .iter()
                .map(|k| k.to_string())
                .collect(),
            Err(_) => Vec::<String>::new(),
        }
    }

    /// Get the local session ID assigned by the server, if available.
    pub fn get_own_session_id(&self) -> Option<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.own_session_id.map(|sid| sid.to_string()),
            Err(_) => None,
        }
    }

    pub fn get_peer_user_id(&self, session_id: &str) -> Option<String> {
        let sid: u64 = session_id.parse().ok()?;
        match self.inner.try_borrow() {
            Ok(inner) => inner
                .peer_decode_manager
                .get(&sid)
                .map(|peer| peer.user_id.clone()),
            Err(_) => {
                warn!(
                    "Failed to borrow inner in get_peer_user_id for session_id: {}",
                    session_id
                );
                None
            }
        }
    }

    /// Get the display name for a peer by session_id string.
    /// Returns `None` if the peer doesn't exist or no display name has been set.
    pub fn get_peer_display_name(&self, session_id: &str) -> Option<String> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.get_peer_display_name(session_id),
            Err(_) => {
                warn!(
                    "Failed to borrow inner in get_peer_display_name for session_id: {}",
                    session_id
                );
                None
            }
        }
    }

    /// Hacky function that returns true if the given peer has yet to send a frame of screen share.
    ///
    /// No reason for this function to exist, it should be deducible from the
    /// [`options.on_peer_first_frame(key, MediaType::Screen)`](VideoCallClientOptions::on_peer_first_frame)
    /// callback.   Or if polling is really necessary, instead of being hardwired for screen, it'd
    /// be more elegant to at least pass a `MediaType`.
    ///
    pub fn is_awaiting_peer_screen_frame(&self, key: &str) -> bool {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(&sid) {
                return peer.screen.is_waiting_for_keyframe();
            }
        }
        false
    }

    pub fn is_video_enabled_for_peer(&self, key: &str) -> bool {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(&sid) {
                return peer.video_enabled;
            }
        }
        false
    }

    pub fn is_screen_share_enabled_for_peer(&self, key: &str) -> bool {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(&sid) {
                return peer.screen_enabled;
            }
        }
        false
    }

    pub fn is_audio_enabled_for_peer(&self, key: &str) -> bool {
        let sid: u64 = match key.parse() {
            Ok(v) => v,
            Err(_) => return false,
        };
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(peer) = inner.peer_decode_manager.get(&sid) {
                return peer.audio_enabled;
            }
        }
        false
    }

    pub fn is_speaking_for_peer(&self, key: &str) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            return inner.peer_decode_manager.is_peer_speaking(key);
        }
        false
    }

    pub fn audio_level_for_peer(&self, key: &str) -> f32 {
        if let Ok(inner) = self.inner.try_borrow() {
            return inner.peer_decode_manager.peer_audio_level(key);
        }
        0.0
    }

    /// Returns a shared reference to the camera force-keyframe flag.
    ///
    /// Pass this to `CameraEncoder` so that incoming KEYFRAME_REQUEST packets
    /// can force the encoder to produce an immediate keyframe.
    pub fn force_camera_keyframe_flag(&self) -> Arc<AtomicBool> {
        self.inner.borrow().force_camera_keyframe.clone()
    }

    /// Returns a shared reference to the screen force-keyframe flag.
    ///
    /// Pass this to `ScreenEncoder` so that incoming KEYFRAME_REQUEST packets
    /// can force the encoder to produce an immediate keyframe.
    pub fn force_screen_keyframe_flag(&self) -> Arc<AtomicBool> {
        self.inner.borrow().force_screen_keyframe.clone()
    }

    /// Returns a shared reference to the congestion step-down flag.
    ///
    /// Pass this to `CameraEncoder` so that incoming CONGESTION signals from
    /// the server trigger an immediate quality tier step-down via the
    /// `EncoderBitrateController`.
    pub fn congestion_step_down_flag(&self) -> Arc<AtomicBool> {
        self.inner.borrow().congestion_step_down_requested.clone()
    }

    pub(crate) fn aes(&self) -> Rc<Aes128State> {
        self.aes.clone()
    }

    pub fn user_id(&self) -> &String {
        &self.options.user_id
    }

    pub fn get_connection_state(&self) -> Option<ConnectionState> {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                return Some(controller.get_connection_state());
            }
        }
        None
    }

    /// Returns `true` if the client is currently in a reconnecting state.
    ///
    /// During reconnection, the server replays the full participant list as
    /// PARTICIPANT_JOINED events.  The UI can use this to suppress toast
    /// notifications for these replayed events.
    pub fn is_reconnecting(&self) -> bool {
        matches!(
            self.get_connection_state(),
            Some(ConnectionState::Reconnecting { .. })
        )
    }

    /// Returns `true` if any peer with the given `user_id` is currently
    /// tracked in the peer decode manager.
    ///
    /// This is useful for the UI to decide whether a PARTICIPANT_JOINED
    /// event represents a genuinely new participant or a reconnection of
    /// an already-known participant.
    pub fn has_peer_with_user_id(&self, user_id: &str) -> bool {
        match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.sorted_keys().iter().any(|sid| {
                inner
                    .peer_decode_manager
                    .get(sid)
                    .is_some_and(|peer| peer.user_id == user_id)
            }),
            Err(_) => false,
        }
    }

    pub fn get_rtt_measurements(&self) -> Option<HashMap<String, f64>> {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                let measurements = controller.get_rtt_measurements_clone();
                let mut result = HashMap::new();
                for (connection_id, measurement) in measurements {
                    if let Some(avg_rtt) = measurement.average_rtt {
                        result.insert(connection_id.clone(), avg_rtt);
                    }
                }
                return Some(result);
            }
        }
        None
    }

    pub fn send_rtt_probes(&self) -> anyhow::Result<()> {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if cc.is_some() {
                // RTT probes are now handled automatically by ConnectionController timers
                return Ok(());
            }
        }
        Err(anyhow!("No connection controller available"))
    }

    pub fn check_election_completion(&self) {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if cc.is_some() {
                // Election completion is now handled automatically by ConnectionController timers
            }
        }
    }

    pub fn get_diagnostics(&self) -> Option<String> {
        self.inner.borrow().peer_decode_manager.get_all_fps_stats()
    }

    pub fn set_peer_video_canvas(
        &self,
        peer_id: &str,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        let sid: u64 = peer_id
            .parse()
            .map_err(|_| JsValue::from_str("Invalid peer_id"))?;
        if let Ok(inner) = self.inner.try_borrow() {
            inner.peer_decode_manager.set_peer_video_canvas(sid, canvas)
        } else {
            Err(JsValue::from_str("Failed to borrow inner state"))
        }
    }

    pub fn set_peer_screen_canvas(
        &self,
        peer_id: &str,
        canvas: web_sys::HtmlCanvasElement,
    ) -> Result<(), JsValue> {
        let sid: u64 = peer_id
            .parse()
            .map_err(|_| JsValue::from_str("Invalid peer_id"))?;
        if let Ok(inner) = self.inner.try_borrow() {
            inner
                .peer_decode_manager
                .set_peer_screen_canvas(sid, canvas)
        } else {
            Err(JsValue::from_str("Failed to borrow inner state"))
        }
    }

    /// Update the visibility state for a peer. When `visible` is `false`,
    /// video and screen decoding is paused to save CPU. Audio is always
    /// decoded regardless of visibility.
    ///
    /// Called by the UI layer when an `IntersectionObserver` detects that a
    /// peer's canvas element has scrolled in or out of the viewport.
    pub fn set_peer_visibility(&self, peer_id: &str, visible: bool) {
        let sid: u64 = match peer_id.parse() {
            Ok(v) => v,
            Err(_) => return,
        };
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.peer_decode_manager.set_peer_visibility(sid, visible);
        }
    }

    pub fn get_peer_fps(&self, peer_id: &str, media_type: MediaType) -> f64 {
        self.inner
            .borrow()
            .peer_decode_manager
            .get_fps(peer_id, media_type)
    }

    pub fn send_diagnostic_packet(&self, packet: DiagnosticsPacket) {
        let wrapper = PacketWrapper {
            packet_type: PacketType::DIAGNOSTICS.into(),
            user_id: self.options.user_id.as_bytes().to_vec(),
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

    pub fn subscribe_global_diagnostics(&self) -> async_broadcast::Receiver<DiagEvent> {
        subscribe_global_diagnostics()
    }

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

    pub fn set_video_enabled(&self, enabled: bool) {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                if let Err(e) = controller.set_video_enabled(enabled) {
                    debug!("Failed to set video enabled {enabled}: {e}");
                } else {
                    debug!("Successfully set video enabled: {enabled}");
                    if let Ok(inner) = self.inner.try_borrow() {
                        if let Some(hr) = &inner.health_reporter {
                            if let Ok(hrb) = hr.try_borrow() {
                                hrb.set_reporting_video_enabled(enabled);
                            }
                        }
                    }
                }
            } else {
                debug!("No connection controller available for set_video_enabled({enabled})");
            }
        } else {
            error!("Unable to borrow connection_controller for set_video_enabled({enabled})");
        }
    }

    pub fn set_audio_enabled(&self, enabled: bool) {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                if let Err(e) = controller.set_audio_enabled(enabled) {
                    debug!("Failed to set audio enabled {enabled}: {e}");
                } else {
                    debug!("Successfully set audio enabled: {enabled}");
                    if let Ok(inner) = self.inner.try_borrow() {
                        if let Some(hr) = &inner.health_reporter {
                            if let Ok(hrb) = hr.try_borrow() {
                                hrb.set_reporting_audio_enabled(enabled);
                            }
                        }
                    }
                }
            } else {
                debug!("No connection controller available for set_audio_enabled({enabled})");
            }
        } else {
            error!("Unable to borrow connection_controller for set_audio_enabled({enabled})");
        }
    }

    pub fn set_screen_enabled(&self, enabled: bool) {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                if let Err(e) = controller.set_screen_enabled(enabled) {
                    debug!("Failed to set screen enabled {enabled}: {e}");
                } else {
                    debug!("Successfully set screen enabled: {enabled}");
                }
            } else {
                debug!("No connection controller available for set_screen_enabled({enabled})");
            }
        } else {
            error!("Unable to borrow connection_controller for set_screen_enabled({enabled})");
        }
    }

    pub fn set_speaking(&self, speaking: bool) {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                controller.set_speaking(speaking);
            }
        }

        if let Some(callback) = &self.options.on_speaking_changed {
            callback.emit(speaking);
        }
    }

    pub fn set_audio_level(&self, level: f32) {
        if let Some(callback) = &self.options.on_audio_level_changed {
            callback.emit(level);
        }
    }

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
    /// Returns `true` if this peer event was already seen recently (within 30 s).
    ///
    /// Both WebSocket and WebTransport connections receive the same NATS system
    /// messages, so the same PARTICIPANT_JOINED / PARTICIPANT_LEFT event can
    /// arrive twice.  This helper deduplicates them so the UI only fires one
    /// toast notification per actual event.
    ///
    /// The 30-second window is chosen to outlast the reconnection backoff
    /// schedule (which can exceed 5 seconds).  A shorter window would allow
    /// stale "existing member" PARTICIPANT_JOINED events to slip through
    /// after a reconnect because the dedup entry had already expired.
    fn is_duplicate_peer_event(&mut self, event_type: &str, target_user_id: &str) -> bool {
        let now = js_sys::Date::now();
        let key = (event_type.to_string(), target_user_id.to_string());

        // Evict stale entries (older than 30 seconds).
        self.recent_peer_events.retain(|_, ts| now - *ts < 30_000.0);

        if let std::collections::hash_map::Entry::Vacant(e) = self.recent_peer_events.entry(key) {
            e.insert(now);
            false // first occurrence
        } else {
            true // duplicate
        }
    }

    /// Try to handle the packet as a KEYFRAME_REQUEST. Returns `true` if it
    /// was a keyframe request and was handled, `false` otherwise.
    ///
    /// A KEYFRAME_REQUEST is a MEDIA packet whose inner `MediaPacket` has
    /// `media_type == KEYFRAME_REQUEST`. The `data` field contains the stream
    /// type (`"VIDEO"` or `"SCREEN"`) that needs the keyframe.
    fn try_handle_keyframe_request(&self, response: &PacketWrapper) -> bool {
        // Parse the inner MediaPacket to check its media_type.
        let media_packet = match MediaPacket::parse_from_bytes(&response.data) {
            Ok(mp) => mp,
            Err(_) => return false,
        };

        if media_packet.media_type.enum_value() != Ok(MediaType::KEYFRAME_REQUEST) {
            return false;
        }

        let requested_stream = String::from_utf8_lossy(&media_packet.data);
        info!(
            "Received KEYFRAME_REQUEST from {} for {}",
            String::from_utf8_lossy(&response.user_id),
            requested_stream,
        );

        match requested_stream.as_ref() {
            "VIDEO" => {
                self.force_camera_keyframe.store(true, Ordering::Release);
            }
            "SCREEN" => {
                self.force_screen_keyframe.store(true, Ordering::Release);
            }
            other => {
                warn!("Unknown KEYFRAME_REQUEST stream type: {other}");
            }
        }

        true
    }

    fn on_inbound_media(&mut self, response: PacketWrapper) {
        debug!(
            "<< Received {:?} from {} (session: {})",
            response.packet_type.enum_value(),
            String::from_utf8_lossy(&response.user_id),
            response.session_id
        );
        // Skip creating peers for system messages (meeting info, meeting started/ended)
        // and for session_id 0 (reserved; MEETING packets and unassigned packets use 0)
        let peer_status =
            if response.user_id == SYSTEM_USER_ID.as_bytes() || response.session_id == 0 {
                PeerStatus::NoChange
            } else {
                let peer_user_id = String::from_utf8_lossy(&response.user_id);
                self.peer_decode_manager
                    .ensure_peer(response.session_id, &peer_user_id)
            };
        match response.packet_type.enum_value() {
            Ok(PacketType::AES_KEY) => {
                if !self.options.enable_e2ee {
                    return;
                }
                if let Ok(bytes) = self.rsa.decrypt(&response.data) {
                    debug!(
                        "Decrypted AES_KEY from {}",
                        String::from_utf8_lossy(&response.user_id)
                    );
                    match AesPacket::parse_from_bytes(&bytes) {
                        Ok(aes_packet) => {
                            if let Err(e) = self.peer_decode_manager.set_peer_aes(
                                response.session_id,
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
                    Ok(data) => {
                        debug!(">> {} sending AES key", self.options.user_id);

                        // Send AES key packet via ConnectionController
                        if let Ok(cc) = self.connection_controller.try_borrow() {
                            if let Some(controller) = cc.as_ref() {
                                let packet = PacketWrapper {
                                    packet_type: PacketType::AES_KEY.into(),
                                    user_id: self.options.user_id.as_bytes().to_vec(),
                                    data,
                                    ..Default::default()
                                };

                                if let Err(e) = controller.send_packet(packet) {
                                    error!("Failed to send AES key packet: {e}");
                                }
                            } else {
                                error!("No connection controller available for AES key");
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to send AES_KEY to peer: {e}");
                    }
                }
            }
            Ok(PacketType::MEDIA) => {
                // Check if this is a KEYFRAME_REQUEST targeted at us (the sender).
                // These arrive as MEDIA packets; we intercept them here before
                // they reach the peer decode manager which would just skip them.
                if self.try_handle_keyframe_request(&response) {
                    // Handled -- do not forward to peer_decode_manager.
                    return;
                }

                let peer_session_id = response.session_id;

                if let Err(e) = self
                    .peer_decode_manager
                    .decode(response, &self.options.user_id)
                {
                    error!("error decoding packet: {e}");
                    match e {
                        PeerDecodeError::SameUserPacket(session_id) => {
                            debug!("Rejecting packet from same user: {session_id}");
                        }
                        _ => {
                            self.peer_decode_manager.delete_peer(peer_session_id);
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
                    String::from_utf8_lossy(&response.user_id)
                );
            }
            Ok(PacketType::SESSION_ASSIGNED) => {
                info!(
                    "Received SESSION_ASSIGNED: session_id={}",
                    response.session_id
                );
                self.own_session_id = Some(response.session_id);

                if let Ok(cc) = self.connection_controller.try_borrow() {
                    if let Some(controller) = cc.as_ref() {
                        if let Err(e) = controller.set_own_session_id(response.session_id) {
                            warn!("Failed to set own_session_id in ConnectionManager: {e}");
                        }
                    }
                }

                // Update health reporter with the server-assigned session_id so that
                // HealthPacket.session_id matches PacketWrapper.session_id for room traffic.
                if let Some(hr) = &self.health_reporter {
                    if let Ok(mut reporter) = hr.try_borrow_mut() {
                        reporter.set_session_id(response.session_id.to_string());
                    }
                }
            }
            Ok(PacketType::MEETING) => match MeetingPacket::parse_from_bytes(&response.data) {
                Ok(meeting_packet) => {
                    info!(
                        "Received MEETING packet: event_type={:?}, room={}, target={}, creator={}, display_name={}, session={}",
                        meeting_packet.event_type.enum_value(),
                        meeting_packet.room_id,
                        String::from_utf8_lossy(&meeting_packet.target_user_id),
                        String::from_utf8_lossy(&meeting_packet.creator_id),
                        String::from_utf8_lossy(&meeting_packet.display_name),
                        meeting_packet.session_id,
                    );
                    match meeting_packet.event_type.enum_value() {
                        Ok(MeetingEventType::MEETING_STARTED) => {
                            info!(
                                "Received MEETING_STARTED: room={}, start_time={}ms, creator={}",
                                meeting_packet.room_id,
                                meeting_packet.start_time_ms,
                                String::from_utf8_lossy(&meeting_packet.creator_id),
                            );

                            if let Some(callback) = &self.options.on_meeting_info {
                                callback.emit(meeting_packet.start_time_ms as f64);
                            }
                        }
                        Ok(MeetingEventType::MEETING_ENDED) => {
                            info!(
                                "Received MEETING_ENDED: room={}, message={}",
                                meeting_packet.room_id, meeting_packet.message
                            );
                            if let Some(callback) = &self.options.on_meeting_ended {
                                let end_time_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as f64)
                                    .unwrap_or(0.0);
                                callback.emit((end_time_ms, meeting_packet.message));
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_JOINED) => {
                            let target_str =
                                String::from_utf8_lossy(&meeting_packet.target_user_id).to_string();
                            let display_name = if meeting_packet.display_name.is_empty() {
                                target_str.clone()
                            } else {
                                String::from_utf8_lossy(&meeting_packet.display_name).to_string()
                            };

                            if meeting_packet.session_id != 0 {
                                self.peer_decode_manager.set_peer_display_name(
                                    meeting_packet.session_id,
                                    display_name.clone(),
                                );
                            }

                            // NOTE: Do NOT emit on_display_name_changed here.
                            // PARTICIPANT_JOINED carries the initial display name for bookkeeping
                            // (set_peer_display_name above), but it is NOT a name-change
                            // event.  Emitting the callback here would confuse the UI into treating
                            // every peer join as a display-name mutation — and would spuriously
                            // update the local user's own name signal on reconnect.
                            // on_display_name_changed is reserved for PARTICIPANT_DISPLAY_NAME_CHANGED.

                            let should_emit = !meeting_packet.target_user_id.is_empty()
                                && meeting_packet.target_user_id[..]
                                    != *self.options.user_id.as_bytes()
                                && !self.is_duplicate_peer_event("joined", &target_str);

                            if should_emit {
                                info!("Peer joined: {}", target_str);
                                if let Some(ref cb) = self.options.on_peer_joined {
                                    cb.emit((display_name, target_str));
                                }
                            } else {
                                debug!("Suppressed PARTICIPANT_JOINED for target={}", target_str);
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_LEFT) => {
                            if meeting_packet.session_id != 0 {
                                self.peer_decode_manager
                                    .delete_peer(meeting_packet.session_id);
                                // Also remove from health reporter — delete_peer
                                // cleans connected_peers and fps_trackers, but
                                // peer_health_data is maintained separately by
                                // the health reporter and must be cleaned
                                // explicitly. Without this, departed peers
                                // persist in the health packet's peer_stats,
                                // inflating the peer count indefinitely.
                                if let Some(hr) = &self.health_reporter {
                                    if let Ok(reporter) = hr.try_borrow() {
                                        reporter
                                            .remove_peer(&meeting_packet.session_id.to_string());
                                    }
                                }
                            }
                            let target_str =
                                String::from_utf8_lossy(&meeting_packet.target_user_id).to_string();
                            let should_emit = !meeting_packet.target_user_id.is_empty()
                                && meeting_packet.target_user_id[..]
                                    != *self.options.user_id.as_bytes()
                                && !self.is_duplicate_peer_event("left", &target_str);
                            if should_emit {
                                info!("Peer left: {}", target_str);
                                if let Some(ref cb) = self.options.on_peer_left {
                                    let display_name = if meeting_packet.display_name.is_empty() {
                                        target_str.clone()
                                    } else {
                                        String::from_utf8_lossy(&meeting_packet.display_name)
                                            .to_string()
                                    };
                                    cb.emit((display_name, target_str));
                                }
                            }
                        }
                        Ok(MeetingEventType::MEETING_ACTIVATED) => {
                            info!(
                                "Received MEETING_ACTIVATED: room={}",
                                meeting_packet.room_id
                            );
                            if let Some(callback) = &self.options.on_meeting_activated {
                                callback.emit(());
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_ADMITTED) => {
                            info!(
                                "Received PARTICIPANT_ADMITTED: room={}, target={}",
                                meeting_packet.room_id,
                                String::from_utf8_lossy(&meeting_packet.target_user_id)
                            );
                            // Only fire callback if this event is targeted at us
                            if meeting_packet.target_user_id[..] == *self.options.user_id.as_bytes()
                            {
                                if let Some(callback) = &self.options.on_participant_admitted {
                                    callback.emit(());
                                }
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_REJECTED) => {
                            info!(
                                "Received PARTICIPANT_REJECTED: room={}, target={}",
                                meeting_packet.room_id,
                                String::from_utf8_lossy(&meeting_packet.target_user_id)
                            );
                            // Only fire callback if this event is targeted at us
                            if meeting_packet.target_user_id[..] == *self.options.user_id.as_bytes()
                            {
                                if let Some(callback) = &self.options.on_participant_rejected {
                                    callback.emit(());
                                }
                            }
                        }
                        Ok(MeetingEventType::WAITING_ROOM_UPDATED) => {
                            info!(
                                "Received WAITING_ROOM_UPDATED: room={}",
                                meeting_packet.room_id
                            );
                            if let Some(callback) = &self.options.on_waiting_room_updated {
                                callback.emit(());
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED) => {
                            let target_str =
                                String::from_utf8_lossy(&meeting_packet.target_user_id).to_string();
                            let new_display_name = if meeting_packet.display_name.is_empty() {
                                target_str.clone()
                            } else {
                                String::from_utf8_lossy(&meeting_packet.display_name).to_string()
                            };

                            info!(
                                "Received PARTICIPANT_DISPLAY_NAME_CHANGED: user={} new_name=\"{}\" (local_user={})",
                                target_str, new_display_name, self.options.user_id
                            );

                            if meeting_packet.session_id != 0 {
                                self.peer_decode_manager.set_peer_display_name(
                                    meeting_packet.session_id,
                                    new_display_name.clone(),
                                );
                            } else {
                                // Server does not populate session_id for display
                                // name changes — fall back to updating all sessions
                                // belonging to this user_id. A rename logically
                                // applies to every session of the same account.
                                self.peer_decode_manager.set_peer_display_name_by_user_id(
                                    &target_str,
                                    new_display_name.clone(),
                                );
                            }

                            if let Some(cb) = &self.options.on_display_name_changed {
                                debug!(
                                    "Emitting on_display_name_changed callback for {}",
                                    target_str
                                );
                                cb.emit((target_str, new_display_name));
                                debug!("on_display_name_changed callback returned");
                            }
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
            },
            Ok(PacketType::CONGESTION) => {
                // Server-side congestion feedback: the server is dropping
                // packets destined for a receiver because the outbound channel
                // is full. Only act on it if the target session matches ours.
                if self.own_session_id == Some(response.session_id) {
                    warn!(
                        "Received CONGESTION signal from server (receiver: {}), requesting quality step-down",
                        String::from_utf8_lossy(&response.user_id),
                    );
                    self.congestion_step_down_requested
                        .store(true, Ordering::Release);
                } else {
                    debug!(
                        "Ignoring CONGESTION signal targeted at session {} (our session: {:?})",
                        response.session_id, self.own_session_id,
                    );
                }
            }
            Ok(PacketType::PACKET_TYPE_UNKNOWN) => {
                error!(
                    "Received packet with unknown packet type from {}",
                    String::from_utf8_lossy(&response.user_id)
                );
            }
            Err(e) => {
                error!("Failed to parse packet type: {e}");
            }
        }
        if let PeerStatus::Added(peer_session_id) = peer_status {
            self.options.on_peer_added.emit(peer_session_id.to_string());
            self.send_public_key();
        }
    }

    fn send_public_key(&self) {
        if !self.options.enable_e2ee {
            return;
        }
        let userid = self.options.user_id.clone();
        let rsa = &*self.rsa;
        match rsa.pub_key.to_public_key_der() {
            Ok(public_key_der) => {
                let packet = RsaPacket {
                    user_id: userid.as_bytes().to_vec(),
                    public_key_der: public_key_der.to_vec(),
                    ..Default::default()
                };
                match packet.write_to_bytes() {
                    Ok(data) => {
                        debug!(">> {userid} sending public key");

                        // Send RSA public key packet via ConnectionController
                        if let Ok(cc) = self.connection_controller.try_borrow() {
                            if let Some(controller) = cc.as_ref() {
                                let packet = PacketWrapper {
                                    packet_type: PacketType::RSA_PUB_KEY.into(),
                                    user_id: userid.as_bytes().to_vec(),
                                    data,
                                    ..Default::default()
                                };

                                if let Err(e) = controller.send_packet(packet) {
                                    error!("Failed to send RSA public key packet: {e}");
                                }
                            } else {
                                error!("No connection controller available for RSA public key");
                            }
                        }
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
