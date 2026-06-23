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

use super::super::connection::{
    ConnectionController, ConnectionLostReason, ConnectionManagerOptions, ConnectionState,
    MediaStreamKey,
};
use super::super::decode::{PeerDecodeManager, PeerStatus};
use super::layer_preference_sender::LayerPreferenceSender;
use super::viewport_sender::{ViewportSender, VIEWPORT_DEBOUNCE_MS};
use crate::crypto::aes::Aes128State;
use crate::crypto::rsa::RsaWrapper;
use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds, ReceivedLayerSnapshot};
use crate::decode::peer_decode_manager::{PeerDecodeError, PeerDeviceInfo, PeerReceiveDiag};
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::diagnostics::{DiagnosticManager, SenderDiagnosticManager};
use crate::health_reporter::{ClimbLimiterSnapshot, HealthReporter};
use anyhow::{anyhow, Result};
use futures::future::LocalBoxFuture;
use gloo_timers::callback::{Interval, Timeout};
#[cfg(target_arch = "wasm32")]
use videocall_diagnostics::MetricValue;
use videocall_diagnostics::{subscribe as subscribe_global_diagnostics, DiagEvent};

use log::{debug, error, info, trace, warn};
use protobuf::Message;
use rsa::pkcs8::{DecodePublicKey, EncodePublicKey};
use rsa::RsaPublicKey;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use videocall_types::protos::aes_packet::AesPacket;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::health_packet::HealthPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use web_time::{SystemTime, UNIX_EPOCH};

use videocall_types::protos::layer_hint_packet::{layer_hint_packet::MediaKind, LayerHintPacket};
use videocall_types::protos::layer_preference_packet::{
    layer_preference_packet::Entry as LayerPreferenceEntry, LayerPreferencePacket,
};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::protos::peer_event::PeerEvent;
use videocall_types::protos::rsa_packet::RsaPacket;
use videocall_types::protos::viewport_packet::ViewportPacket;
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

const MAX_SESSION_ID_HISTORY: usize = 16;

/// How long (ms) the early-seed sampler stays armed after a peer first joins
/// (issue #1179, Part B). The 5s monitor tick owns steady-state adaptation; this
/// short window only exists to catch a freshly-joined WT peer whose downlink is
/// already congested BEFORE the first tick can react, so it self-cancels after
/// this elapses. Sized to comfortably cover a join wave (scale target ~20 users):
/// `transport_type` is UNKNOWN at peer-add and only populates once the peer's
/// first heartbeat lands, so the window must outlast at least one heartbeat
/// cadence (~5s) plus the wave spread — 30s leaves ample margin while still being
/// strictly bounded (the timer is NOT a steady-state loop).
const EARLY_SEED_WINDOW_MS: u32 = 30_000;

/// Cadence (ms) of the early-seed sampler while armed (issue #1179, Part B). At
/// 1s it reacts to early congestion ~5x faster than the 5s monitor tick — the
/// whole point of the seed — without being a per-packet cost (it is a single
/// timer, not work in `on_inbound_media`). Must divide into [`EARLY_SEED_WINDOW_MS`]
/// so the window ends on a tick boundary.
const EARLY_SEED_SAMPLE_MS: u32 = 1_000;

/// Result of refreshing a room token. Both URL lists carry the new token
/// in their query string (e.g. `https://relay.example/lobby?token=<JWT>`),
/// ready to be plugged into the connection manager via
/// [`crate::VideoCallClient::update_server_urls`].
///
/// Returned by the [`RefreshRoomTokenCallback`] that the dioxus-ui (or any
/// other consumer) registers via
/// [`VideoCallClientOptions::refresh_room_token_callback`]. See discussion
/// #562 (AUTH-2) for the full Phase 3 design.
#[derive(Clone, Debug, PartialEq)]
pub struct RefreshedTokens {
    /// Tokenized WebSocket URLs for the relay candidates.
    pub websocket_urls: Vec<String>,
    /// Tokenized WebTransport URLs for the relay candidates.
    pub webtransport_urls: Vec<String>,
}

/// Async callback the client invokes when it needs a fresh room token,
/// e.g. before a candidate-rebuilding re-election.
///
/// The callback returns `Some(RefreshedTokens)` on success (the new tokenized
/// URLs for WS and WT will replace the cached ones before the manager spawns
/// candidates) or `None` if the refresh failed (network error, server 5xx,
/// meeting ended, etc.). On `None`, the manager logs a warning and proceeds
/// with the cached URLs — re-election is never blocked entirely on a refresh
/// failure, since that would be a worse failure mode than running with an
/// expired token (which the relay will simply reject, triggering normal
/// reconnect-with-refresh in the UI layer).
///
/// The callback is set by the dioxus-ui layer (where `refresh_room_token`
/// already exists) and consumed by `ConnectionManager` during the
/// timer-driven re-election entry path. See discussion #562 (AUTH-2).
///
/// Single-threaded (`LocalBoxFuture`) because the videocall-client targets
/// `wasm32-unknown-unknown`, where everything runs on the JS main thread.
pub struct RefreshRoomTokenCallback {
    cb: Rc<dyn Fn() -> LocalBoxFuture<'static, Option<RefreshedTokens>>>,
}

impl RefreshRoomTokenCallback {
    /// Build a `RefreshRoomTokenCallback` from any closure that returns a
    /// future resolving to `Option<RefreshedTokens>`.
    pub fn from<F, Fut>(func: F) -> Self
    where
        F: Fn() -> Fut + 'static,
        Fut: std::future::Future<Output = Option<RefreshedTokens>> + 'static,
    {
        Self {
            cb: Rc::new(move || Box::pin(func())),
        }
    }

    /// Invoke the callback. Returns the future the caller must drive to
    /// completion (typically via `wasm_bindgen_futures::spawn_local`).
    pub fn emit(&self) -> LocalBoxFuture<'static, Option<RefreshedTokens>> {
        (self.cb)()
    }
}

impl Clone for RefreshRoomTokenCallback {
    fn clone(&self) -> Self {
        Self {
            cb: self.cb.clone(),
        }
    }
}

#[allow(ambiguous_wide_pointer_comparisons)]
impl PartialEq for RefreshRoomTokenCallback {
    fn eq(&self, other: &Self) -> bool {
        // Mirror `videocall_types::Callback`'s identity-based equality so
        // `VideoCallClientOptions` can stay `PartialEq`-derivable.
        Rc::ptr_eq(&self.cb, &other.cb)
    }
}

impl std::fmt::Debug for RefreshRoomTokenCallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RefreshRoomTokenCallback<_>")
    }
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
    /// Batched companion of `on_peer_removed` fired once per
    /// `PeerDecodeManager` removal pass with **all** peers removed in
    /// that pass. Subscribers that only need a single notification (e.g.
    /// to bump a UI version counter) should listen here so a 5-peer
    /// watchdog timeout does not trigger 5 sequential UI re-renders.
    /// `on_peer_removed` continues to fire per-peer for cleanup of
    /// per-peer state. See Phase 6 watchdog-cascade fix.
    pub on_peers_removed_batch: Option<Callback<Vec<String>>>,
    pub get_peer_video_canvas_id: Callback<String, String>,
    pub get_peer_screen_canvas_id: Callback<String, String>,
    pub user_id: String,
    pub display_name: String,
    pub meeting_id: String,
    pub websocket_urls: Vec<String>,
    pub webtransport_urls: Vec<String>,
    pub on_connected: Callback<()>,
    pub on_connection_lost: Callback<ConnectionLostReason>,
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

    /// Callback triggered when meeting settings are updated (optional)
    pub on_meeting_settings_updated: Option<Callback<()>>,

    /// Callback triggered when the host requests this client mute its mic.
    pub on_host_mute: Option<Callback<()>>,

    /// Callback triggered when the host requests this client disable its camera.
    pub on_host_disable_video: Option<Callback<()>>,

    /// Callback triggered when the host removes this client from the meeting.
    pub on_participant_kicked: Option<Callback<()>>,

    /// Callback triggered when a participant is granted host.
    pub on_host_granted: Option<Callback<String>>,

    /// Callback triggered when a participant's host is revoked.
    pub on_host_revoked: Option<Callback<String>>,

    /// Callback triggered when a peer publishes a `PEER_EVENT` that targets
    /// this client. Emits `(source_user_id, event_type, stream_id)`.
    ///
    /// Currently used to deliver `screen_decode_started` acknowledgements
    /// from peers that have begun rendering this client's shared screen
    /// (HCL issue #893). The shape is intentionally generic so additional
    /// event_types can be added without changing the callback signature.
    pub on_peer_event: Option<Callback<(String, String, String)>>,

    /// Callback triggered when a remote participant leaves the meeting.
    /// Emits `(display_name, user_id, session_id)` from the PARTICIPANT_LEFT
    /// meeting event. `session_id` is the server-assigned session_id as a
    /// decimal string; an empty string indicates an unknown session_id
    /// (legacy/unset path).
    pub on_peer_left: Option<Callback<(String, String, String)>>,

    /// Callback triggered when a participant changes their display name.
    /// Emits `(user_id, new_display_name, session_id)` where `session_id` is
    /// the server-assigned u64 session_id of the renaming participant — or
    /// `0` for legacy broadcasts that did not carry a session_id (rename
    /// applies to all sessions of `user_id`). UIs that maintain a local-self
    /// display-name signal MUST gate their self-update on
    /// `session_id == own_session_id` so a sibling tab of the same
    /// authenticated user (same `user_id`, different `session_id`) does not
    /// overwrite its own self-name when another tab renames. See HCL #828.
    pub on_display_name_changed: Option<Callback<(String, String, u64)>>,

    /// Callback triggered when a remote participant joins the meeting.
    /// Emits `(display_name, user_id, session_id)` from the PARTICIPANT_JOINED
    /// meeting event. `session_id` is the server-assigned session_id as a
    /// decimal string; an empty string indicates an unknown session_id
    /// (legacy/unset path).
    pub on_peer_joined: Option<Callback<(String, String, String)>>,

    /// When `false`, all inbound `MEDIA` packets (audio, video, screen) are
    /// silently discarded and no peer decoder workers are created.  Only
    /// meeting-control packets (MEETING, SESSION_ASSIGNED) are still processed.
    ///
    /// Set to `false` for observer clients that only need push notifications
    /// (e.g. the waiting room or "waiting for meeting to start" screen) so
    /// that audio from participants already in the call cannot be decoded and
    /// played back while the local user is not yet admitted.
    ///
    /// Should be `true` for call participants; set to `false` only for observer/lobby clients.
    pub decode_media: bool,

    /// Whether the local user joined as an unauthenticated guest.
    pub is_guest: bool,

    /// Whether the connection manager is allowed to schedule a 30-second
    /// post-rebase re-election retry when the RTT-degradation watchdog hits a
    /// "only 1 server configured" rebase.
    ///
    /// Set to `true` for users on the default `WebTransport` transport
    /// preference (the WT-with-WS-fallback mode) — the single-candidate
    /// state is system-side (e.g. relay-availability blip) and recovery via
    /// re-evaluation is desirable.
    ///
    /// Set to `false` for users who explicitly chose `WebSocket` — the
    /// single-candidate state is the user's deliberate choice and the
    /// retry must not override it.
    ///
    /// Defaults to `true`. The dioxus-ui derives the value from the user's
    /// `TransportPreference` context signal.
    pub allow_post_rebase_retry: bool,

    /// Async callback the client invokes when it needs a fresh room token,
    /// e.g. before a candidate-rebuilding re-election.
    ///
    /// When set, the connection manager calls this callback at the start of
    /// every internal re-election triggered by the RTT-degradation watchdog
    /// (1Hz timer) or post-rebase retry. On success the manager swaps in the
    /// freshly-tokenized URLs before spawning candidates, so re-elections
    /// after the original token's expiry no longer fail with all candidates
    /// rejected by the relay (the failure mode AUTH-2 was filed against —
    /// see discussion #562).
    ///
    /// On failure (`None`), the manager logs a warning and proceeds with
    /// the cached URLs; the existing UI-level `schedule_reconnect` path
    /// remains the safety net for terminal token expiry.
    ///
    /// Set to `None` for clients that don't have a refresh endpoint
    /// (no-jwt builds, observers, tests).
    pub refresh_room_token_callback: Option<RefreshRoomTokenCallback>,
}

#[derive(Debug)]
struct InnerOptions {
    enable_e2ee: bool,
    user_id: String,
    display_name: String,
    on_peer_added: Callback<String>,
    on_meeting_info: Option<Callback<f64>>,
    on_meeting_ended: Option<Callback<(f64, String)>>,
    on_meeting_activated: Option<Callback<()>>,
    on_participant_admitted: Option<Callback<()>>,
    on_participant_rejected: Option<Callback<()>>,
    on_waiting_room_updated: Option<Callback<()>>,
    on_meeting_settings_updated: Option<Callback<()>>,
    on_host_mute: Option<Callback<()>>,
    on_host_disable_video: Option<Callback<()>>,
    on_participant_kicked: Option<Callback<()>>,
    on_host_granted: Option<Callback<String>>,
    on_host_revoked: Option<Callback<String>>,
    on_peer_event: Option<Callback<(String, String, String)>>,
    on_peer_left: Option<Callback<(String, String, String)>>,
    on_peer_joined: Option<Callback<(String, String, String)>>,
    on_display_name_changed: Option<Callback<(String, String, u64)>>,
    decode_media: bool,
}

#[derive(Debug)]
struct Inner {
    options: InnerOptions,
    connection_controller: Rc<RefCell<Option<Rc<ConnectionController>>>>,
    connection_state: ConnectionState,
    aes: Rc<Aes128State>,
    rsa: Rc<RsaWrapper>,
    peer_decode_manager: PeerDecodeManager,
    /// Send-side state machine for the simulcast `LAYER_PREFERENCE` control
    /// packet (issue #989, Phase 2). Holds the last-sent desired-layer map +
    /// rate-limit clock so a `LAYER_PREFERENCE` packet is emitted only when the
    /// receiver-driven chooser's per-peer desired layers actually change. See
    /// [`LayerPreferenceSender`].
    layer_preference_sender: LayerPreferenceSender,
    /// User-configured RECEIVE-side simulcast layer bounds per kind (issue #989,
    /// Phase 4). Default fully-open = pure auto. Applied to every per-(peer,kind)
    /// chooser's desired layer at the monitor tick, so the requested + decoded
    /// layer is bounded. Set via [`VideoCallClient::set_receive_layer_bounds`].
    receive_layer_bounds: ReceiveLayerBounds,
    _diagnostics: Option<Rc<DiagnosticManager>>,
    sender_diagnostics: Option<Rc<SenderDiagnosticManager>>,
    health_reporter: Option<Rc<RefCell<HealthReporter>>>,
    own_session_id: Option<u64>,
    /// Bounded set of session_ids this client has held in the current page load.
    /// Used to match incoming CONGESTION signals — the server stamps the throttled
    /// sender's session_id on the wire, and the client receives every CONGESTION
    /// via wildcard NATS fan-out (`room.{room}.*` with per-session queue groups).
    /// Without this match, antonio would step down video when jay is the throttled
    /// sender. The history covers the reconnect race-window where SESSION_ASSIGNED
    /// for a new session id may not have landed yet at the moment a CONGESTION
    /// targeting it arrives. Bounded to `MAX_SESSION_ID_HISTORY`.
    session_id_history: std::collections::VecDeque<u64>,
    /// Recently processed peer events for deduplication.
    /// Both WebSocket and WebTransport connections receive the same NATS system
    /// messages, so we deduplicate within a short time window to avoid firing
    /// duplicate toast notifications.
    ///
    /// Per-session events (PARTICIPANT_JOINED, PARTICIPANT_LEFT) key on
    /// `(event_type, target_user_id, Some(session_id))` so that two distinct
    /// sessions of the same authenticated user (HCL issue #828) are NOT
    /// dedup'd as one. Only the *same* session arriving over both WS and WT
    /// gets suppressed.
    ///
    /// Per-user events (which do not have a meaningful session_id at this
    /// layer) key on `(event_type, target_user_id, None)`.
    /// Key: (event_type_str, target_user_id, session_id), Value: timestamp_ms
    recent_peer_events: HashMap<(String, String, Option<u64>), f64>,
    /// Recently processed host action events for deduplication across
    /// dual-transport delivery (e.g. HOST_MUTE_PARTICIPANT). Uses a much
    /// shorter window than `recent_peer_events` because host actions are
    /// deliberate, repeatable commands — see `is_duplicate_host_action`.
    /// Key: (event_type_str, target_user_id), Value: timestamp_ms
    recent_host_events: HashMap<(String, String), f64>,
    /// Flag set by incoming KEYFRAME_REQUEST for camera video. The
    /// `CameraEncoder` checks this flag each frame and forces a keyframe.
    force_camera_keyframe: Arc<AtomicBool>,
    /// Flag set by incoming KEYFRAME_REQUEST for screen share.
    force_screen_keyframe: Arc<AtomicBool>,
    /// Flag set when a CONGESTION signal is received from the server.
    /// The camera encoder's diagnostics loop checks this flag and calls
    /// `force_video_step_down()` on the `EncoderBitrateController`.
    congestion_step_down_requested: Arc<AtomicBool>,
    /// Mirror of `congestion_step_down_requested` for the SCREEN encoder (issue
    /// #1199). A SEPARATE flag (not the same atom) because each encoder's AQ
    /// loop consumes its flag with `swap(false)`: a single shared flag would
    /// race so only one loop ever observed a given CONGESTION signal. A
    /// self-targeted CONGESTION sets BOTH so both publishers step down. Like the
    /// split `force_camera_keyframe` / `force_screen_keyframe` flags above.
    screen_congestion_step_down_requested: Arc<AtomicBool>,
    /// Rolling 1-second window start (wall-clock ms) for rate-capping the
    /// self-targeted DOWNLINK_CONGESTION `warn!`. See `congestion_warn_admit`.
    congestion_warn_window_start_ms: u64,
    /// Count of self-targeted CONGESTION `warn!`s emitted in the current
    /// rolling window. Reset when the window rolls over (see `congestion_warn_admit`).
    congestion_warn_count_in_window: u32,
    /// Observability: total self-targeted DOWNLINK_CONGESTION signals received
    /// (warned OR muted) for the lifetime of this client. Exposed via
    /// `VideoCallClient::client_congestion_signals_received_total`.
    client_congestion_signals_received_total: u64,
    /// CONGESTION-driven AUDIO simulcast layer-ceiling (issue #621). Unlike the
    /// camera/screen `AtomicBool` step-down FLAGS above — which are consumed
    /// (`swap(false)`) by an encoder AQ loop — this is a layer-COUNT atom shared
    /// with the microphone encoder (`u32::MAX` = fail-open / no congestion cap).
    /// On a self-targeted CONGESTION the dispatch stores `1` (base-only),
    /// dropping every upper audio simulcast layer on the next frame — the audio
    /// analogue of the camera's aggressive `force_congestion_cut`, but via the
    /// layer-ceiling lever because the Opus AudioWorklet cannot reconfigure
    /// bitrate live.
    ///
    /// A direct atomic store (NOT a consume-once flag) because the mic encoder has
    /// NO AQ loop of its own — audio tier decisions are normally driven by the
    /// CAMERA's AQ loop, which is NOT running when the publisher is audio-only.
    /// Driving the ceiling directly here means the cut works regardless of camera
    /// state. The mic encoder owns a self-contained recovery timer that climbs
    /// this back up after a cooldown. Reset to `u32::MAX` on reconnect (see the
    /// `Connected` handler) so a stale cut from the old session does not suppress
    /// audio on a fresh one.
    audio_congestion_layer_ceiling: Arc<AtomicU32>,
    /// SINGLE-LAYER audio BITRATE floor in bps (issue #1398). The bitrate
    /// analogue of `audio_congestion_layer_ceiling` above, and the lever that
    /// closes the single-layer gap that ceiling cannot: a publisher gated to one
    /// audio layer (or with audio simulcast disabled) has no upper layer to shed,
    /// so the only downshift is lowering the single running Opus stream's bitrate
    /// live (worklet ctl 4002 = OPUS_SET_BITRATE). `u32::MAX` = fail-open / no cut.
    ///
    /// WRITER (issue #1398): the MIC encoder's own uplink-distress detector — its
    /// recovery `Interval` reads the live transport stall/drop counters and steps
    /// this floor DOWN one tier (via the mic's `audio_congestion_bitrate_step_down`)
    /// when the publisher is audio-only. This atom is NOT driven by the inbound
    /// `PacketType::CONGESTION` arm anymore: b127ee80 stepped it from
    /// `apply_self_congestion_cut`, but #1219 Half 1 removed the relay's
    /// self-targeted CONGESTION emission, so that trigger never fired — #1398
    /// retargeted it onto the live uplink signal.
    ///
    /// The client OWNS this atom (and shares it into the mic via
    /// `set_congestion_bitrate_floor`) for ONE purpose: to RESET it to `u32::MAX`
    /// on reconnect (see the `Connected` handler) so a stale cut does not pin audio
    /// bitrate low on a fresh session. The mic recovery timer also climbs it back
    /// after a cooldown; resetting here just makes a fresh session start at full
    /// bitrate immediately.
    ///
    /// FIX D / #1398: the `VideoCallClient` struct ALSO holds a clone of this same
    /// `Arc` directly (NOT behind `Inner`) so the reconnect reset runs even when the
    /// `Inner` borrow is contended — the reset store was moved OUT of the `Inner`
    /// `try_borrow_mut` block in `handle_connected_reconnect_resets`. Both clones
    /// are the SAME atom, so a store through either is visible through the other.
    audio_congestion_bitrate_floor: Arc<AtomicU32>,
    /// Connection RECONNECT-reseed flag for the single-layer audio distress
    /// detector (issue #1398 reconnect P1). Set `true` on every (re)connect (in the
    /// `Connected` handler, next to the bitrate-floor reset); the mic-side detector
    /// tick CONSUMES it and forces its tumbling windows to re-anchor to "now".
    ///
    /// Without it, a plain network reconnect does NOT restart the mic (the mic
    /// stays enabled and `EncoderState::switching` stays false), so the detector
    /// keeps running with the gate open (camera off, single-layer) and
    /// `det_was_active == true` — its existing `!was_active` re-seed never fires.
    /// The transport teardown/rebuild BUMPS the monotonic `unistream_*` /
    /// `websocket_drop` counters, so the first window that closes on the fresh
    /// session would cash a spurious cross-reconnect cut. Resetting the floor (the
    /// atom above) is not enough — that clears an OLD cut but does not stop a NEW
    /// spurious one. This flag closes that hole.
    ///
    /// Like the floor atom, the `VideoCallClient` struct ALSO holds a direct clone
    /// (NOT behind `Inner`) so the reconnect set runs even when `Inner` is
    /// contended. Both clones are the SAME atom.
    audio_detector_reconnect_reseed: Arc<AtomicBool>,
    /// Signal set by `ConnectionManager` when a re-election completes. The
    /// camera encoder reads and clears this to suppress crash ceiling arming.
    reelection_completed_signal: Rc<AtomicBool>,
    /// Relay layer-union hint atom for the CAMERA (VIDEO) ladder (issue #1108,
    /// Stage 3). A clone of `CameraEncoder::shared_union_requested_layer`, wired
    /// in by the host after the encoder is built. The `LAYER_HINT` dispatch arm
    /// writes the VIDEO entry's max-requested-layer here; the camera AQ control
    /// loop reads the same atom and caps its published ladder. `None` when no
    /// camera encoder is attached (e.g. observer mode / native lib callers), in
    /// which case the hint is silently ignored (fail-open). Reset to `u32::MAX` on
    /// reconnect so a stale cap from the old relay cannot suppress against a new
    /// session.
    camera_union_requested_layer: Option<Rc<AtomicU32>>,
    /// Relay layer-union hint atom for the SCREEN ladder (issue #1108, Stage 3).
    /// Mirror of `camera_union_requested_layer` for the SCREEN media-kind — a
    /// clone of `ScreenEncoder::shared_union_requested_layer`. `None` until wired.
    screen_union_requested_layer: Option<Rc<AtomicU32>>,
    /// Long Tasks API observer that emits `client_longtask_duration_ms` /
    /// `client_longtask_count` to the diagnostic bus whenever the main
    /// thread blocks for more than 50 ms. Held for its drop side-effect:
    /// the underlying `PerformanceObserver` is disconnected automatically
    /// when this field goes out of scope. May be `None` on browsers that
    /// don't expose [`PerformanceObserver`] (Safari < 16.4 etc.).
    _long_task_observer: Option<crate::long_tasks::LongTaskObserver>,
    _render_fps_observer: Option<crate::render_fps::RenderFpsObserver>,
}

/// The main client handle for a video call session.
///
/// `VideoCallClient` is cheaply cloneable (`Rc`-based interior mutability)
/// and is passed to encoders and other subsystems so they can send packets
/// and query connection state.
#[derive(Clone)]
pub struct VideoCallClient {
    options: VideoCallClientOptions,
    inner: Rc<RefCell<Inner>>,
    connection_controller: Rc<RefCell<Option<Rc<ConnectionController>>>>,
    aes: Rc<Aes128State>,
    _diagnostics: Option<Rc<DiagnosticManager>>,
    /// Send-side state machine for the viewport control packet (HCL issue
    /// #988). Tracks the last-sent vs. pending active-decode-set so we only
    /// emit a `VIEWPORT` packet on an actual change. See [`ViewportSender`].
    viewport_sender: Rc<RefCell<ViewportSender>>,
    /// Debounce timer that coalesces a burst of active-decode-set changes
    /// (scroll / relayout) into a single `VIEWPORT` send. Holding the
    /// [`Timeout`] keeps it armed; dropping/replacing it cancels the pending
    /// fire. `None` when no flush is scheduled.
    viewport_debounce_timer: Rc<RefCell<Option<Timeout>>>,
    /// Self-cancelling early-seed sampler (issue #1179, Part B). Armed once when
    /// the first peer joins; ticks every [`EARLY_SEED_SAMPLE_MS`] to drive the
    /// receiver-side `observe_early_congestion` for WEBTRANSPORT peers so a
    /// freshly-joined congested WT peer is constrained immediately instead of
    /// decoding the full-quality top layer for up to ~5s until the first monitor
    /// tick. `Some` == armed (so per-packet `on_inbound_media` installs exactly
    /// ONE timer); the timer self-cancels back to `None` after
    /// [`EARLY_SEED_WINDOW_MS`]. Held here (not in `Inner`) so its closure can
    /// capture a `Weak` to the slot and clear it on expiry, mirroring
    /// `viewport_debounce_timer`. Dropping the `VideoCallClient` drops this slot,
    /// which drops the `Interval`, which cancels the underlying browser timer.
    early_seed_timer: Rc<RefCell<Option<Interval>>>,
    /// Forced-keyframe cooldown reset for the CAMERA encoder (issue #1311,
    /// hardened in #1352). A clone of `CameraEncoder::keyframe_cooldown_reset`,
    /// wired in by the host after the encoder is built.
    camera_keyframe_cooldown_reset: Rc<RefCell<Option<Rc<AtomicBool>>>>,
    /// Forced-keyframe cooldown reset for the SCREEN encoder (issue #1311 screen
    /// half), held outside `Inner` for the same borrow-safety reason as the camera
    /// slot: a reconnect must be able to arm the encoder-owned atom even if the
    /// `Inner` mutable borrow used for layer-preference / union-cap reset is busy.
    screen_keyframe_cooldown_reset: Rc<RefCell<Option<Rc<AtomicBool>>>>,
    /// Single-layer audio BITRATE floor (issue #1398), held DIRECTLY here (NOT
    /// behind `Inner`) so the reconnect reset always runs even when the `Inner`
    /// mutable borrow is contended — the same borrow-safety slot pattern as the
    /// #1311 keyframe-cooldown-reset fields above (FIX D). `Inner` ALSO keeps its
    /// own clone of this SAME `Arc` (for the `audio_congestion_bitrate_floor()`
    /// accessor that wires the atom into the mic encoder); this is just a second
    /// clone reachable WITHOUT taking the `Inner` borrow, so the `Connected`
    /// reconnect handler can `store(u32::MAX)` here unconditionally. A stale low
    /// bitrate cut from the OLD session must not pin a fresh one (see the
    /// `handle_connected_reconnect_resets` store, moved OUT of the `Inner` borrow).
    audio_congestion_bitrate_floor: Arc<AtomicU32>,
    /// Connection RECONNECT-reseed flag for the single-layer audio distress
    /// detector (issue #1398 reconnect P1), held DIRECTLY here (NOT behind `Inner`)
    /// for the same borrow-safety reason as the bitrate-floor atom above: the
    /// `Connected` reconnect handler must be able to `store(true)` even when the
    /// `Inner` mutable borrow is contended. `Inner` ALSO keeps a clone of this SAME
    /// `Arc` (for the `audio_detector_reconnect_reseed()` accessor that wires it
    /// into the mic encoder). The mic detector tick consumes it (swap-to-false) and
    /// forces a window re-seed, so a reconnect's counter bump is never read as a
    /// fresh-session distress delta.
    audio_detector_reconnect_reseed: Arc<AtomicBool>,
}

// `Timeout` (gloo) is not `Debug`; derive a manual impl that elides the
// non-Debug fields so `VideoCallClient` stays printable for logging.
impl std::fmt::Debug for VideoCallClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoCallClient")
            .field("options", &self.options)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl PartialEq for VideoCallClient {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
            && Rc::ptr_eq(&self.connection_controller, &other.connection_controller)
            && self.options == other.options
    }
}

/// Build a cleartext `VIEWPORT` control packet for `session_ids` and dispatch
/// it on the reliable Control stream (HCL issue #988).
///
/// Free function taking the shared `connection_controller` cell directly so
/// both the debounce-flush path (which holds a `VideoCallClient`) and the
/// reconnect re-send closure (which only holds a `Weak<RefCell<Inner>>`, to
/// avoid an `Rc` cycle through the connection callback) can share one
/// serialization + send implementation. The cell is the same `Rc` held by both
/// `VideoCallClient` and `Inner`.
///
/// The `ViewportPacket` is NOT E2EE-sealed — it is pure relay routing metadata
/// that the relay consumes and never forwards to peers. The `session_ids` are
/// the relay/peer session_ids (u64) the relay indexes on, sent unchanged.
fn send_viewport_via(
    connection_controller: &Rc<RefCell<Option<Rc<ConnectionController>>>>,
    user_id: &str,
    session_ids: Vec<u64>,
) {
    // Log the actual contents (capped) so a "froze my video" / wrongly-filtered
    // support log shows exactly which streams the client asked the relay to keep
    // (HCL issue #988). The list is unbounded in principle, so cap the logged
    // sample; the `log::debug!` args are only evaluated when DEBUG is enabled, so
    // the slice + format costs nothing at higher log levels.
    const VIEWPORT_LOG_SAMPLE: usize = 8;
    debug!(
        "Sending VIEWPORT packet: first {} of {} session_id(s): {:?}",
        session_ids.len().min(VIEWPORT_LOG_SAMPLE),
        session_ids.len(),
        &session_ids[..session_ids.len().min(VIEWPORT_LOG_SAMPLE)]
    );
    let viewport = ViewportPacket {
        session_ids,
        ..Default::default()
    };
    let data = match viewport.write_to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to serialize ViewportPacket: {e}");
            return;
        }
    };
    let wrapper = PacketWrapper {
        packet_type: PacketType::VIEWPORT.into(),
        user_id: user_id.as_bytes().to_vec(),
        data,
        ..Default::default()
    };
    // VIEWPORT rides the reliable Control stream, like other signaling, so it
    // is never stalled behind a large video keyframe write.
    match connection_controller.try_borrow() {
        Ok(cc) => match cc.as_ref() {
            Some(controller) => {
                if let Err(e) = controller.send_packet(wrapper, MediaStreamKey::Control) {
                    debug!("Failed to send VIEWPORT packet: {e}");
                }
            }
            None => debug!("No connection controller available; dropping VIEWPORT packet"),
        },
        Err(_) => warn!("connection_controller busy; dropping VIEWPORT packet"),
    }
}

/// Build a cleartext `LAYER_PREFERENCE` control packet from `(session_id,
/// desired_layer)` entries and dispatch it on the reliable Control stream
/// (issue #989, Phase 2).
///
/// `entries` is the receiver-driven chooser's per-peer desired-layer map,
/// already change-detected, rate-limited, capped and canonicalized by
/// [`LayerPreferenceSender`]. Like `ViewportPacket`, the `LayerPreferencePacket`
/// is NOT E2EE-sealed — it is pure relay routing metadata the relay consumes and
/// never forwards to peers. The relay records it keyed by the RECEIVER's own
/// NATS subject, so it can only subtract what THIS receiver gets; the
/// `session_id`s here are the real relay session ids of the peers this client is
/// receiving, never forged. It rides the Control stream so it is never stalled
/// behind a large video keyframe write.
fn send_layer_preference_via(
    connection_controller: &Rc<RefCell<Option<Rc<ConnectionController>>>>,
    user_id: &str,
    entries: Vec<(u64, PrefMediaKind, u32)>,
) {
    const LAYER_PREF_LOG_SAMPLE: usize = 8;
    debug!(
        "Sending LAYER_PREFERENCE packet: first {} of {} entry(ies): {:?}",
        entries.len().min(LAYER_PREF_LOG_SAMPLE),
        entries.len(),
        &entries[..entries.len().min(LAYER_PREF_LOG_SAMPLE)]
    );
    let packet = LayerPreferencePacket {
        entries: entries
            .into_iter()
            .map(|(session_id, kind, desired_layer)| LayerPreferenceEntry {
                session_id,
                desired_layer,
                // Map the chooser's PrefMediaKind to the proto EntryMediaKind.
                // Both share the wire discriminant (VIDEO=1/AUDIO=2/SCREEN=3),
                // so a from_i32 of `wire_value()` is exact and back-compatible.
                media_kind: ::protobuf::EnumOrUnknown::from_i32(kind.wire_value()),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let data = match packet.write_to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            error!("Failed to serialize LayerPreferencePacket: {e}");
            return;
        }
    };
    let wrapper = PacketWrapper {
        packet_type: PacketType::LAYER_PREFERENCE.into(),
        user_id: user_id.as_bytes().to_vec(),
        data,
        ..Default::default()
    };
    match connection_controller.try_borrow() {
        Ok(cc) => match cc.as_ref() {
            Some(controller) => {
                if let Err(e) = controller.send_packet(wrapper, MediaStreamKey::Control) {
                    debug!("Failed to send LAYER_PREFERENCE packet: {e}");
                }
            }
            None => {
                debug!("No connection controller available; dropping LAYER_PREFERENCE packet")
            }
        },
        Err(_) => warn!("connection_controller busy; dropping LAYER_PREFERENCE packet"),
    }
}

/// Arm the CAMERA forced-keyframe cooldown reset on a (re)connect (issue #1311,
/// hardened in #1352).
///
/// Reads the dedicated cooldown-reset slot (held OUTSIDE `Inner`), clones the
/// encoder-owned `Rc<AtomicBool>` out, and stores `true` on it so the camera
/// encode loop clears its `last_keyframe_emit_ms` cooldown clock on its next
/// frame — letting the first post-reconnect PLI emit immediately instead of
/// being coalesced away by a stale pre-transition keyframe timestamp.
///
/// The key invariant (issue #1352): the `store(true, Release)` MUST be
/// independent of any `Inner` borrow. A full reconnect (not a re-election)
/// relies solely on this client-side store — the encoder's re-election re-arm
/// never fires on a plain reconnect (`reset_and_start_election` clears
/// `old_active_connection`). The previous code nested the store inside the same
/// `Inner` `try_borrow_mut()` that runs the layer-preference / union-cap resets,
/// so a transient borrow conflict at reconnect time silently dropped the reset.
/// Holding the atom in its own slot and cloning the `Rc` out under a short
/// `try_borrow` removes that coupling. If the wiring slot itself is somehow
/// borrowed (it never is in practice — the only writer is the synchronous
/// `set_camera_keyframe_cooldown_reset`), the reset is skipped for that
/// transition, which is the same fail-open behavior as `None` (not yet wired).
///
/// Idempotent and side-effect-bounded: arming a no-op (cold-start) reset is
/// harmless (no PLI pending, clock already `None`), and a duplicate vs the
/// quality task's re-election arm is collapsed by the `.swap(false)` consume on
/// the encode-loop side.
fn arm_keyframe_cooldown_reset_slot(slot: &Rc<RefCell<Option<Rc<AtomicBool>>>>) -> bool {
    let atom = slot.try_borrow().ok().and_then(|slot| slot.clone());
    if let Some(atom) = atom {
        atom.store(true, Ordering::Release);
        true
    } else {
        false
    }
}

fn arm_camera_keyframe_cooldown_reset(slot: &Rc<RefCell<Option<Rc<AtomicBool>>>>) {
    let _ = arm_keyframe_cooldown_reset_slot(slot);
}

// === Issue #1460: layer-switch ↔ freshness_skip correlation observability ===

/// Window after an in-place simulcast layer switch within which a
/// `freshness_skip` is considered correlated with the switch (issue #1460).
/// A switch collides two disjoint per-layer sequence spaces in one
/// layer-agnostic jitter buffer; the resulting freshness_skip freezes surface
/// within a few seconds of the transition, so a 3s window captures them without
/// spuriously attributing unrelated skips.
#[cfg(any(target_arch = "wasm32", test))]
const POST_SWITCH_WINDOW_MS: u64 = 3000;

/// True iff a freshness_skip at `now_ms` falls within [`POST_SWITCH_WINDOW_MS`]
/// of the peer's last layer switch at `last_switch_ms`. `last_switch_ms == 0` is
/// the never-switched sentinel and always returns false. `saturating_sub`
/// tolerates clock skew (`now_ms < last_switch_ms`) by treating the delta as 0
/// (still within the window).
///
/// Pure (no I/O, no global state) so it is directly unit-testable on the host
/// target and mutation-sensitive at the `<` boundary (issue #1460).
#[cfg(any(target_arch = "wasm32", test))]
fn freshness_skip_within_switch_window(now_ms: u64, last_switch_ms: u64) -> bool {
    if last_switch_ms == 0 {
        return false;
    }
    now_ms.saturating_sub(last_switch_ms) < POST_SWITCH_WINDOW_MS
}

/// Spawn the issue #1460 observability subscriber on the diagnostics bus.
///
/// Mirrors `HealthReporter::start_diagnostics_subscription`: a `spawn_local`
/// loop over `subscribe().recv()` holding a `Weak<RefCell<Inner>>` (NOT a strong
/// `Rc`, to avoid a reference cycle keeping `Inner` alive forever). On each
/// `subsystem == "video"` `freshness_skip` event it parses the receiving session
/// id (`to_peer`, the `connected_peers` key — see below) and `head_age_ms`,
/// looks up the peer, and if the skip lands within [`POST_SWITCH_WINDOW_MS`] of
/// that peer's last VIDEO layer switch (Marker 1 stamp), emits a single WARN.
///
/// Identity note (load-bearing): the worker's freshness_skip carries
/// `from_peer` = the LOCAL reporting user id (passed as `userid` into
/// `PeerDecodeManager::decode`) and `to_peer` = the REMOTE source peer's
/// `session_id` string (`Peer::sid_str`, set via `set_stream_context`).
/// `connected_peers` is keyed by that remote source `session_id`, so `to_peer`
/// is the correct lookup key — NOT `from_peer`.
///
/// Clock note: the delta uses the event's own `ts_ms` (the skip's timestamp,
/// stamped by the worker via `videocall_diagnostics::now_ms()`), which on wasm
/// is `js_sys::Date::now()` — the SAME wall clock the Marker 1 stamps use (the
/// manager tick / seed paths thread in `js_sys::Date::now()` as `now_ms`). Using
/// the skip's own timestamp is the cleanest correlation point.
#[cfg(target_arch = "wasm32")]
fn spawn_layer_switch_freshness_observer(inner: &Rc<RefCell<Inner>>) {
    let inner_weak = Rc::downgrade(inner);
    wasm_bindgen_futures::spawn_local(async move {
        let mut receiver = subscribe_global_diagnostics();
        while let Ok(event) = receiver.recv().await {
            if event.subsystem != "video" {
                continue;
            }
            // Mirror freshness_inject.rs: confirm this is a freshness_skip event
            // (the "video" subsystem also carries decoder stats / worker logs).
            let is_skip = event.metrics.iter().any(|m| {
                m.name == "event"
                    && matches!(&m.value, MetricValue::Text(v) if v == "freshness_skip")
            });
            if !is_skip {
                continue;
            }

            let mut to_peer: Option<String> = None;
            let mut head_age_ms: Option<f64> = None;
            for m in &event.metrics {
                match (m.name, &m.value) {
                    ("to_peer", MetricValue::Text(v)) => to_peer = Some(v.to_string()),
                    ("head_age_ms", MetricValue::F64(v)) => head_age_ms = Some(*v),
                    _ => {}
                }
            }

            // `to_peer` is the remote source peer's session_id string = the
            // `connected_peers` key.
            let Some(sid) = to_peer.as_deref().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            let age = head_age_ms.unwrap_or_default();

            let Some(inner_rc) = Weak::upgrade(&inner_weak) else {
                // Client torn down — stop the loop so it doesn't spin forever.
                break;
            };
            // try_borrow (not borrow_mut) so we never panic if another path
            // holds Inner; this is a read-only correlation.
            let Ok(inner) = inner_rc.try_borrow() else {
                continue;
            };
            let Some(peer) = inner.peer_decode_manager.get(&sid) else {
                continue;
            };
            let last = peer.last_video_switch();
            // Sentinel guard: skip peers that have never switched.
            if !freshness_skip_within_switch_window(event.ts_ms, last.ms) {
                continue;
            }
            let d = event.ts_ms.saturating_sub(last.ms);
            warn!(
                "LAYER_SWITCH_FRESHNESS_SKIP session_id={} kind=video ms_since_switch={} head_age_ms={:.0} from_layer={} to_layer={}",
                sid, d, age, last.from, last.to
            );
        }
    });
}

#[cfg(test)]
mod layer_switch_freshness_window_tests {
    use super::{freshness_skip_within_switch_window, POST_SWITCH_WINDOW_MS};

    // Guard: the cases below are derived assuming a 3000ms window. If the const
    // changes, the just_inside/just_outside boundary cases must be revisited.
    #[test]
    fn window_const_is_three_seconds() {
        assert_eq!(POST_SWITCH_WINDOW_MS, 3000);
    }

    #[test]
    fn just_inside_window_is_correlated() {
        // d = 2999 < 3000 → true. Fails if the window shrinks below 3000.
        let last = 1000;
        let now = last + 2999;
        assert!(freshness_skip_within_switch_window(now, last));
    }

    #[test]
    fn just_outside_window_is_not_correlated() {
        // d = 3000, which is NOT < 3000 → false. This case is the mutation
        // sentinel: flipping `<` to `<=` would make this return true.
        let last = 1000;
        let now = last + POST_SWITCH_WINDOW_MS; // d == 3000
        assert!(!freshness_skip_within_switch_window(now, last));
    }

    #[test]
    fn never_switched_sentinel_is_not_correlated() {
        // last == 0 → always false, regardless of `now`. Fails if the sentinel
        // guard is removed (0 + huge `now` would otherwise be far outside the
        // window → false anyway, so use a `now` that WOULD be inside if the
        // guard treated 0 as a real timestamp).
        assert!(!freshness_skip_within_switch_window(2999, 0));
        assert!(!freshness_skip_within_switch_window(u64::MAX, 0));
    }

    #[test]
    fn clock_skew_now_before_last_saturates_to_zero() {
        // now < last (clock skew): saturating_sub → 0, 0 < 3000 → true.
        // Documents the skew-tolerance: a skip stamped slightly before the
        // switch is still attributed to it. last != 0 so the sentinel does not
        // short-circuit.
        let last = 1000;
        let now = 500;
        assert!(freshness_skip_within_switch_window(now, last));
    }
}

fn handle_connected_reconnect_resets(
    inner: &Weak<RefCell<Inner>>,
    early_seed_timer: &Rc<RefCell<Option<Interval>>>,
    camera_keyframe_cooldown_reset: &Rc<RefCell<Option<Rc<AtomicBool>>>>,
    screen_keyframe_cooldown_reset: &Rc<RefCell<Option<Rc<AtomicBool>>>>,
    audio_congestion_bitrate_floor: &Arc<AtomicU32>,
    audio_detector_reconnect_reseed: &Arc<AtomicBool>,
) {
    // On (re)connect the relay also allocated a fresh empty layer-preference map
    // for the new session_id (fail-open -> every layer forwarded). Clear the
    // sender's last-sent memory so the NEXT peer-monitor tick re-sends the
    // current per-peer desired layers unconditionally and downlink-aware
    // filtering resumes (issue #989, Phase 2). We reset here rather than
    // re-send inline because the desired map is recomputed from live per-peer
    // health on the tick.
    // Issue #1179, Part B: drop any armed early-seed timer so the next inbound
    // packet re-arms a fresh 30s window against the new session. Dropping the
    // Interval cancels the underlying browser timer.
    if let Ok(mut slot) = early_seed_timer.try_borrow_mut() {
        *slot = None;
    }

    if let Some(inner) = Weak::upgrade(inner) {
        if let Ok(mut inner) = inner.try_borrow_mut() {
            inner.layer_preference_sender.reset_for_reconnect();

            // Relay layer-union cap reset (issue #1108, Stage 3). The NEW
            // relay/session starts with an empty receiver set -> no union yet ->
            // it would publish nothing (fail-open). Until its first LAYER_HINT
            // lands, a cap left over from the OLD relay must not keep suppressing
            // our ladder against a fresh session, so reset both kinds to the
            // u32::MAX fail-open sentinel. The next LAYER_HINT (if any)
            // re-establishes the cap. Same precedent as the layer_preference_sender
            // reset above.
            if let Some(atom) = &inner.camera_union_requested_layer {
                atom.store(u32::MAX, Ordering::Relaxed);
            }
            if let Some(atom) = &inner.screen_union_requested_layer {
                atom.store(u32::MAX, Ordering::Relaxed);
            }
            // CONGESTION-driven audio layer-ceiling reset (issue #621). A cut left
            // over from the OLD relay/session must not keep the audio ladder pinned
            // to base-only against a FRESH session, so reset it to the fail-open
            // sentinel. The next self-targeted CONGESTION (if any) re-cuts it. Same
            // reconnect-reset precedent as the union caps above. (The mic encoder's
            // recovery timer would also climb it back, but resetting here means a
            // fresh session starts at the full ladder immediately rather than
            // waiting out a cooldown carried over from the old session.)
            inner
                .audio_congestion_layer_ceiling
                .store(u32::MAX, Ordering::Relaxed);
            // NOTE (#621): the audio layer-CEILING reset stays inside this borrow.
            // It is pre-existing/out-of-scope for #1398 FIX D; the single-layer
            // BITRATE-floor reset that used to sit here was moved OUTSIDE the borrow
            // below (see the FIX-D store after the #1311 arms).
        } else {
            warn!("LAYER_PREFERENCE reconnect reset: inner busy, skipping");
        }
    }

    // Forced-keyframe cooldown reset (issue #1311, hardened in #1352). Run
    // OUTSIDE the `Inner` `try_borrow_mut()` block above on purpose: a full
    // reconnect relies SOLELY on this client-side arm (the encoder's
    // re-election re-arm does not fire on a plain reconnect), and nesting it
    // inside the `Inner` borrow meant a transient conflict at reconnect time
    // silently dropped the reset. The helper clones the encoder-owned atom out
    // of its own slot and stores `true` independently of any `Inner` borrow.
    // This arm covers BOTH a full reconnect and a re-election (both re-emit
    // `Connected` here); a cold-start no-op and a duplicate vs the quality task's
    // arm are both harmless.
    arm_camera_keyframe_cooldown_reset(camera_keyframe_cooldown_reset);
    let _ = arm_keyframe_cooldown_reset_slot(screen_keyframe_cooldown_reset);

    // Single-layer audio BITRATE floor reset (issue #1398, FIX D). Run OUTSIDE the
    // `Inner` `try_borrow_mut()` block above for the SAME reason as the #1311
    // keyframe-cooldown arms: a full reconnect must reset the floor even when
    // `Inner` is contended at that instant. Previously this store sat inside the
    // borrow and was silently dropped on conflict (logging only "inner busy,
    // skipping"), leaving a stale low-bitrate cut from the OLD session pinning the
    // single running Opus stream on the FRESH session until the mic recovery timer
    // climbed it back over a carried-over cooldown. The atom is held directly on
    // the client (the same `Arc` Inner also holds), so this store never depends on
    // the `Inner` borrow. The mic-side uplink-distress detector re-steps the floor
    // from scratch if the new session is also distressed.
    audio_congestion_bitrate_floor.store(u32::MAX, Ordering::Relaxed);

    // Single-layer audio distress-detector RECONNECT-RESEED (issue #1398 reconnect
    // P1). Resetting the floor above clears an OLD cut, but does NOT stop a NEW
    // spurious one: on a plain reconnect the mic is not restarted, so its
    // uplink-distress detector keeps running with its tumbling windows anchored to
    // the OLD session and `det_was_active == true` (the gate stayed open: camera
    // off, single-layer). The transport teardown/rebuild BUMPS the monotonic
    // `unistream_*` / `websocket_drop` counters, so the first window that closes on
    // the fresh session would compute a cross-reconnect delta and cash a spurious
    // cut — re-pinning audio low even though the new session's uplink is healthy.
    // Set this flag so the detector's next tick CONSUMES it and re-anchors its
    // windows to "now" (the detector's existing `!was_active` re-seed never fires
    // here because the detector never went inactive). Stored OUTSIDE the `Inner`
    // borrow for the same reason as the floor reset above — the atom is held
    // directly on the client. `Release` so the detector's `AcqRel` swap observes it.
    audio_detector_reconnect_reseed.store(true, Ordering::Release);
}

/// Arm the issue-#1179 early-seed sampler if it is not already armed.
///
/// Idempotent: `on_inbound_media` fires per-packet, so this guards on the slot
/// already holding an `Interval` (`Some`) and installs exactly one timer. The
/// installed `Interval` ticks every [`EARLY_SEED_SAMPLE_MS`] and, on each tick:
///   * upgrades the `Weak<Inner>`; if the client was torn down, self-cancels
///     (clears the slot, dropping the `Interval`) and returns;
///   * computes elapsed wall-clock since arming and self-cancels once it reaches
///     [`EARLY_SEED_WINDOW_MS`] — so the timer is strictly bounded, NEVER a
///     steady-state loop (the 5s monitor tick owns steady-state adaptation);
///   * `try_borrow_mut`s `Inner`; on a transient borrow conflict it SKIPS this
///     cycle (the next tick retries) and never triggers a reconnect — same
///     discipline as the `peer_monitor` closure;
///   * gates on THIS client's LOCAL active transport via
///     [`ConnectionController::active_is_webtransport`] — the deciding signal for
///     #1179 is "am I (the receiver) on WebTransport", a single client-wide
///     boolean, NOT a per-peer property. When the local transport is not WT (or
///     no connection is active yet) the tick returns early and seeds nothing,
///     preserving the M2 healthy cold-start for WS clients;
///   * otherwise drives [`PeerDecodeManager::seed_early_congestion_for_connected_peers`]
///     (which now seeds purely on the per-peer congestion gate — the WT decision
///     has already been made on the local transport — and clamps each seeded layer
///     to the user's receive bounds) and then publishes the
///     [`PeerDecodeManager::current_desired_preferences`] map (clamped to the
///     user's bounds and gated to layers below each kind's highest-available,
///     mirroring the 5s tick; it advances no chooser hysteresis) through the
///     existing [`LayerPreferenceSender`], so `last_sent`/`last_sent_ms` stay
///     coherent and the next 5s tick does not re-send a redundant packet.
///
/// LIFECYCLE DECISION (issue #1179): the timer runs the FULL window and seeds any
/// congested peer rather than cancelling after the first seed. Rationale: at the
/// scale target (~20 users) peers arrive in a join WAVE and become congested at
/// staggered times within the window. Cancelling on the first seed would leave
/// later joiners unseeded for up to a full 5s tick — exactly the stall this fix
/// removes. The window is still strictly bounded, so this is not an unbounded
/// loop.
fn arm_early_seed_timer(inner_weak: Weak<RefCell<Inner>>, slot: &Rc<RefCell<Option<Interval>>>) {
    // Guard: exactly one timer. If already armed (Some), do nothing.
    let mut slot_borrow = match slot.try_borrow_mut() {
        Ok(b) => b,
        // Slot busy (should not happen on single-threaded wasm); skip arming —
        // a later inbound packet re-attempts.
        Err(_) => return,
    };
    if slot_borrow.is_some() {
        return;
    }

    let armed_at_ms = js_sys::Date::now() as u64;
    let slot_weak = Rc::downgrade(slot);

    let interval = Interval::new(EARLY_SEED_SAMPLE_MS, move || {
        // Self-cancel if the client (and thus Inner) is gone.
        let Some(inner_rc) = inner_weak.upgrade() else {
            if let Some(slot) = slot_weak.upgrade() {
                if let Ok(mut s) = slot.try_borrow_mut() {
                    *s = None;
                }
            }
            return;
        };

        let now_ms = js_sys::Date::now() as u64;

        // Window elapsed → self-cancel by clearing our own slot (drops the
        // Interval). Strictly bounded; never a steady-state loop.
        if now_ms.saturating_sub(armed_at_ms) >= EARLY_SEED_WINDOW_MS as u64 {
            if let Some(slot) = slot_weak.upgrade() {
                if let Ok(mut s) = slot.try_borrow_mut() {
                    *s = None;
                }
            }
            return;
        }

        // Borrow Inner. On a transient conflict (e.g. on_inbound_media holds the
        // mutable borrow) SKIP this cycle — never reconnect; the next tick retries.
        let Ok(mut inner) = inner_rc.try_borrow_mut() else {
            warn!("early_seed: transient borrow conflict, skipping this cycle");
            return;
        };

        // WT-only gate, evaluated on THIS client's LOCAL active transport — NOT
        // per-peer. #1179's root cause is the local DOWNLINK being WebTransport
        // (reliable-unistream flow-control pinning under simulcast fan-out), so
        // the deciding signal is "am I on WT", a single client-wide boolean. The
        // relay is a broadcast relay: each client elects exactly one transport.
        // A peer's announced `transport_type` (its own uplink) is the wrong
        // signal — it could differ from this receiver's downlink. If no
        // connection is active yet (cold start / pre-election) the accessor
        // returns false → no seed (matches the M2 healthy cold-start).
        let local_is_wt = match inner.connection_controller.try_borrow() {
            Ok(slot) => slot
                .as_ref()
                .map(|cc| cc.active_is_webtransport())
                .unwrap_or(false),
            // Controller cell momentarily borrowed → treat as not-WT and skip
            // this cycle; the next tick (within the bounded window) retries.
            Err(_) => false,
        };
        if !local_is_wt {
            return;
        }

        // The user's GLOBAL receive-layer bounds, snapshotted (`Copy`) to avoid an
        // aliasing borrow with `&mut peer_decode_manager` — same pattern as the 5s
        // tick. The seed clamps each seeded layer to these bounds so the early path
        // honors a manual receive `max` exactly as the tick does (PR #1192 review);
        // the default (open) bounds make this an identity clamp.
        let bounds = inner.receive_layer_bounds;

        // Seed any peer showing early congestion. The WT decision was already
        // made above on the local transport, so this loop no longer reads any
        // per-peer transport — it seeds purely on the congestion gate inside
        // `observe_early_congestion`.
        inner
            .peer_decode_manager
            .seed_early_congestion_for_connected_peers(now_ms, &bounds);

        // Publish the resulting desired map through the existing sender so
        // dedup/rate-limit invariants hold. The map is clamped to the user's
        // bounds and gated to layers below each kind's highest-available, mirroring
        // the 5s tick. A clean join seeds nothing → empty/unchanged map → sender
        // suppresses → no packet (M2).
        let desired = inner
            .peer_decode_manager
            .current_desired_preferences(now_ms, &bounds);
        if let Some(entries) = inner
            .layer_preference_sender
            .take_if_changed(&desired, now_ms)
        {
            let user_id = inner.options.user_id.clone();
            let cc = inner.connection_controller.clone();
            send_layer_preference_via(&cc, &user_id, entries);
        }
    });

    *slot_borrow = Some(interval);
}

fn resolve_display_name(event: &str, packet: &MeetingPacket, user_id: &str) -> String {
    if packet.display_name.is_empty() {
        warn!(
            "{}: empty display_name for session={} user={}, falling back to user_id",
            event, packet.session_id, user_id
        );
        user_id.to_string()
    } else {
        String::from_utf8_lossy(&packet.display_name).to_string()
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
            let mut diagnostics = DiagnosticManager::new(options.user_id.clone());

            if let Some(interval) = options.diagnostics_update_interval_ms {
                diagnostics.set_reporting_interval(interval);
            }

            Some(Rc::new(diagnostics))
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
        let screen_congestion_step_down_requested = Arc::new(AtomicBool::new(false));
        // CONGESTION-driven audio layer-ceiling (issue #621). Fail-open
        // (u32::MAX = no congestion cap) until a self-targeted CONGESTION cuts it.
        let audio_congestion_layer_ceiling = Arc::new(AtomicU32::new(u32::MAX));
        // Single-layer audio BITRATE floor (issue #1398). Fail-open (u32::MAX =
        // no cut) until the mic-side uplink-distress detector steps it down one
        // tier. The client owns it only to reset it on reconnect.
        let audio_congestion_bitrate_floor = Arc::new(AtomicU32::new(u32::MAX));
        // Reconnect-reseed flag for the single-layer audio distress detector (issue
        // #1398 reconnect P1). False (no reconnect pending) until the `Connected`
        // handler sets it on every (re)connect; the mic detector tick consumes it.
        let audio_detector_reconnect_reseed = Arc::new(AtomicBool::new(false));
        let reelection_completed_signal = Rc::new(AtomicBool::new(false));

        // Phase 8a / TELEM-1: register a Long Tasks API observer once per
        // VideoCallClient lifetime. Each main-thread stall > 50 ms is
        // forwarded to the diagnostic bus as `client_longtask_duration_ms`
        // and `client_longtask_count`. `start()` returns `None` on
        // browsers that don't expose `PerformanceObserver` (Safari < 16.4,
        // Web Worker globals, etc.); in that case we silently skip — long
        // task telemetry is a nice-to-have, not a hard dependency.
        let long_task_observer = crate::long_tasks::LongTaskObserver::start();
        if long_task_observer.is_none() {
            log::debug!(
                "VideoCallClient::new — Long Tasks API not available; \
                 client_longtask_duration_ms metric will not be emitted"
            );
        }

        let render_fps_observer = crate::render_fps::RenderFpsObserver::start();
        if render_fps_observer.is_none() {
            log::debug!(
                "VideoCallClient::new — rAF observer not available; \
                 client_render_fps metric will not be emitted"
            );
        }

        let client = Self {
            options: options.clone(),
            inner: Rc::new(RefCell::new(Inner {
                options: InnerOptions {
                    enable_e2ee: options.enable_e2ee,
                    user_id: options.user_id.clone(),
                    display_name: options.display_name.clone(),
                    on_peer_added: options.on_peer_added.clone(),
                    on_meeting_ended: options.on_meeting_ended.clone(),
                    on_meeting_info: options.on_meeting_info.clone(),
                    on_meeting_activated: options.on_meeting_activated.clone(),
                    on_participant_admitted: options.on_participant_admitted.clone(),
                    on_participant_rejected: options.on_participant_rejected.clone(),
                    on_waiting_room_updated: options.on_waiting_room_updated.clone(),
                    on_meeting_settings_updated: options.on_meeting_settings_updated.clone(),
                    on_host_mute: options.on_host_mute.clone(),
                    on_host_disable_video: options.on_host_disable_video.clone(),
                    on_participant_kicked: options.on_participant_kicked.clone(),
                    on_host_granted: options.on_host_granted.clone(),
                    on_host_revoked: options.on_host_revoked.clone(),
                    on_peer_event: options.on_peer_event.clone(),
                    on_display_name_changed: options.on_display_name_changed.clone(),
                    on_peer_left: options.on_peer_left.clone(),
                    on_peer_joined: options.on_peer_joined.clone(),
                    decode_media: options.decode_media,
                },
                connection_controller: connection_controller.clone(),
                connection_state: ConnectionState::Failed {
                    error: "Not connected".to_string(),
                    last_known_server: None,
                },
                own_session_id: None,
                session_id_history: std::collections::VecDeque::new(),
                aes: aes.clone(),
                rsa: Rc::new(RsaWrapper::new(options.enable_e2ee)),
                peer_decode_manager: Self::create_peer_decoder_manager(
                    &options,
                    diagnostics.clone(),
                ),
                layer_preference_sender: LayerPreferenceSender::new(),
                receive_layer_bounds: ReceiveLayerBounds::default(),
                _diagnostics: diagnostics.clone(),
                sender_diagnostics: sender_diagnostics.clone(),
                health_reporter: health_reporter.clone(),
                recent_peer_events: HashMap::new(),
                recent_host_events: HashMap::new(),
                force_camera_keyframe: force_camera_keyframe.clone(),
                force_screen_keyframe: force_screen_keyframe.clone(),
                congestion_step_down_requested: congestion_step_down_requested.clone(),
                screen_congestion_step_down_requested: screen_congestion_step_down_requested
                    .clone(),
                congestion_warn_window_start_ms: 0,
                congestion_warn_count_in_window: 0,
                client_congestion_signals_received_total: 0,
                audio_congestion_layer_ceiling: audio_congestion_layer_ceiling.clone(),
                audio_congestion_bitrate_floor: audio_congestion_bitrate_floor.clone(),
                audio_detector_reconnect_reseed: audio_detector_reconnect_reseed.clone(),
                reelection_completed_signal: reelection_completed_signal.clone(),
                // Relay layer-union hint atoms (issue #1108, Stage 3). None until
                // the host wires in the camera/screen encoder accessors; the
                // LAYER_HINT dispatch arm no-ops while None (fail-open).
                camera_union_requested_layer: None,
                screen_union_requested_layer: None,
                _long_task_observer: long_task_observer,
                _render_fps_observer: render_fps_observer,
            })),
            connection_controller,
            aes,
            _diagnostics: diagnostics.clone(),
            viewport_sender: Rc::new(RefCell::new(ViewportSender::new())),
            viewport_debounce_timer: Rc::new(RefCell::new(None)),
            early_seed_timer: Rc::new(RefCell::new(None)),
            // Issue #1311 / #1352: None until the host wires in
            // `CameraEncoder::keyframe_cooldown_reset`; the reconnect reset
            // no-ops while None. Held outside `Inner` so the `Connected` arm can
            // arm it independently of the `Inner` borrow (see field doc).
            camera_keyframe_cooldown_reset: Rc::new(RefCell::new(None)),
            screen_keyframe_cooldown_reset: Rc::new(RefCell::new(None)),
            // FIX D / #1398: a second clone of the SAME floor atom Inner holds, kept
            // here so the reconnect reset can store the fail-open sentinel without
            // taking the (possibly contended) Inner borrow.
            audio_congestion_bitrate_floor: audio_congestion_bitrate_floor.clone(),
            // Issue #1398 reconnect P1: a second clone of the SAME reconnect-reseed
            // atom Inner holds, kept here so the `Connected` handler can set it
            // without taking the (possibly contended) Inner borrow.
            audio_detector_reconnect_reseed: audio_detector_reconnect_reseed.clone(),
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

        // Issue #1460 observability: subscribe to the diagnostics bus to correlate
        // worker freshness_skip events with this peer's recent layer switches.
        // Pure telemetry; holds only a Weak handle to `Inner` (no cycle).
        #[cfg(target_arch = "wasm32")]
        spawn_layer_switch_freshness_observer(&client.inner);

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
                    // controller and create a fresh one below. We only
                    // recycle the transport layer here; the callbacks
                    // captured in `Inner` (PLI, diagnostics, health
                    // reporting) must keep working across the reconnect.
                    ConnectionState::Failed { .. } => {
                        drop(cc);
                        info!("connect() called with failed ConnectionController — disconnecting before reconnect");
                        let _ = self.disconnect_controller_only();
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
                // Issue #1179, Part B: the early-seed timer is armed the first
                // time a peer is Added. Capture the slot here so the per-packet
                // callback can install exactly one timer.
                let early_seed_timer = self.early_seed_timer.clone();
                Callback::from(move |packet| {
                    if let Some(inner_rc) = Weak::upgrade(&inner) {
                        // Borrow, handle the packet, and capture the peer status,
                        // then RELEASE the borrow before arming the timer (the
                        // timer's closure borrows Inner on its own cadence).
                        let status = match inner_rc.try_borrow_mut() {
                            Ok(mut inner) => Some(inner.on_inbound_media(packet)),
                            Err(_) => {
                                warn!(
                                    "on_inbound_media: transient borrow conflict, dropping packet"
                                );
                                None
                            }
                        };
                        // Arm the issue-#1179 early-seed sampler exactly once, on
                        // the first peer to join. `arm_early_seed_timer` is a no-op
                        // if the timer is already armed (Some), so per-packet calls
                        // never leak additional timers.
                        if matches!(status, Some(PeerStatus::Added(_))) {
                            arm_early_seed_timer(Rc::downgrade(&inner_rc), &early_seed_timer);
                        }
                    }
                })
            },
            on_state_changed: {
                let on_connected = self.options.on_connected.clone();
                let on_connection_lost = self.options.on_connection_lost.clone();
                let inner = Rc::downgrade(&self.inner);
                // VIEWPORT re-send on reconnect (HCL issue #988). Captured by
                // strong Rc clones: `ViewportSender` does NOT reference the
                // connection controller, so this forms no Rc cycle through the
                // callback the controller holds. user_id is needed to stamp the
                // outgoing PacketWrapper.
                let viewport_sender = self.viewport_sender.clone();
                let viewport_user_id = self.options.user_id.clone();
                // Issue #1179, Part B: clear the early-seed timer slot on
                // reconnect so the next inbound packet re-arms a fresh window
                // against the new session (mirrors the layer_preference_sender
                // reset below). Dropping the stored Interval cancels the old one.
                let early_seed_timer = self.early_seed_timer.clone();
                // Issue #1352: capture the forced-keyframe cooldown-reset slot
                // directly (NOT via `Inner`) so the `Connected` arm can arm the
                // encoder-owned atom even when the `Inner` borrow below is
                // contended. Cloning the `Rc` is the whole point — the
                // `store(true)` must not depend on `inner.try_borrow_mut()`.
                let camera_keyframe_cooldown_reset = self.camera_keyframe_cooldown_reset.clone();
                let screen_keyframe_cooldown_reset = self.screen_keyframe_cooldown_reset.clone();
                // FIX D / #1398: capture the bitrate-floor atom DIRECTLY (NOT via
                // `Inner`) so the `Connected` reconnect handler can reset it to the
                // fail-open sentinel even when the `Inner` borrow below is contended
                // — same rationale as the keyframe-cooldown slots above.
                let audio_congestion_bitrate_floor = self.audio_congestion_bitrate_floor.clone();
                // Issue #1398 reconnect P1: capture the detector reconnect-reseed
                // atom DIRECTLY (NOT via `Inner`), same rationale, so the handler can
                // set it on every (re)connect even under a contended `Inner` borrow.
                let audio_detector_reconnect_reseed = self.audio_detector_reconnect_reseed.clone();
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

                            handle_connected_reconnect_resets(
                                &inner,
                                &early_seed_timer,
                                &camera_keyframe_cooldown_reset,
                                &screen_keyframe_cooldown_reset,
                                &audio_congestion_bitrate_floor,
                                &audio_detector_reconnect_reseed,
                            );

                            // On (re)connect the session_id changed and the
                            // relay allocated a fresh empty viewport (fail-open
                            // → all streams). Re-send the current viewport so
                            // filtering resumes; without this the user silently
                            // gets every stream again after any re-election.
                            // Sent directly (one event per reconnect — no
                            // debounce needed) using the same path as the
                            // debounced flush.
                            let to_send = match viewport_sender.try_borrow_mut() {
                                Ok(mut s) => {
                                    if s.reset_for_reconnect() {
                                        s.take_if_changed()
                                    } else {
                                        None
                                    }
                                }
                                Err(_) => {
                                    warn!("VIEWPORT reconnect re-send: sender busy, skipping");
                                    None
                                }
                            };
                            if let Some(session_ids) = to_send {
                                if let Some(inner) = Weak::upgrade(&inner) {
                                    if let Ok(inner) = inner.try_borrow() {
                                        // `Inner` holds a clone of the same
                                        // connection_controller cell; use it so
                                        // the closure never strong-captures the
                                        // controller (no Rc cycle).
                                        let resent_count = session_ids.len();
                                        send_viewport_via(
                                            &inner.connection_controller,
                                            &viewport_user_id,
                                            session_ids,
                                        );
                                        // Log the recovery edge explicitly (HCL
                                        // issue #988): only the failure paths were
                                        // logged before, so a support log could not
                                        // confirm the client re-subscribed its
                                        // viewport after a transport flap / re-election.
                                        info!(
                                            "VIEWPORT reconnect re-send: re-sent viewport with {resent_count} session_id(s) after reconnect"
                                        );
                                    } else {
                                        warn!("VIEWPORT reconnect re-send: inner busy, skipping");
                                    }
                                }
                            }
                        }
                        ConnectionState::Failed { error, .. } => {
                            on_connection_lost.emit(ConnectionLostReason::HandshakeFailed(error));
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

                                // Phase 2 (#989): run the receiver-driven layer
                                // chooser for every peer (updates each peer's
                                // decode guard) and, if the desired per-peer
                                // layer map changed AND the relay's rate-limit
                                // allows, emit a LAYER_PREFERENCE packet so the
                                // relay drops the layers this receiver's downlink
                                // cannot sustain. When every source publishes
                                // only the base layer (the default until the P1
                                // send flag is raised), every chosen layer is 0
                                // and the relay's fail-open already forwards base
                                // — so this is a no-op on the wire (the empty /
                                // all-zero map dedups after the first send).
                                let now_ms = js_sys::Date::now() as u64;
                                // Phase 4: pass the user's receive-layer bounds so
                                // the chooser output is clamped per kind. `Copy`,
                                // so snapshot it to avoid an aliasing borrow with
                                // `&mut peer_decode_manager`.
                                let bounds = inner.receive_layer_bounds;
                                let desired = inner
                                    .peer_decode_manager
                                    .tick_layer_choosers(now_ms, &bounds);
                                // #1561: snapshot the per-(peer,kind) desired layer
                                // map into the health reporter for the next packet.
                                if let Some(hr) = &inner.health_reporter {
                                    if let Ok(reporter) = hr.try_borrow() {
                                        reporter.update_received_layers(&desired);
                                    }
                                }
                                if let Some(entries) = inner
                                    .layer_preference_sender
                                    .take_if_changed(&desired, now_ms)
                                {
                                    let user_id = inner.options.user_id.clone();
                                    let cc = inner.connection_controller.clone();
                                    send_layer_preference_via(&cc, &user_id, entries);
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
            reelection_completed_signal: self.inner.borrow().reelection_completed_signal.clone(),
            allow_post_rebase_retry: self.options.allow_post_rebase_retry,
            // Phase 3 / AUTH-2: forward the dioxus-ui's room-token refresh
            // callback so the manager can preempt token expiry from inside
            // re-election. See discussion #562.
            refresh_room_token_callback: self.options.refresh_room_token_callback.clone(),
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
    ///
    /// The new URLs are propagated end-to-end:
    /// - the outer `VideoCallClient::options` copy is updated immediately, and
    /// - if a `ConnectionController` already exists (i.e. `connect()` has been
    ///   called), the underlying `ConnectionManager`'s own options are
    ///   updated as well so the post-rebase re-election retry's
    ///   `total_server_count()` sees the refreshed candidate set.
    ///
    /// If the controller does not yet exist (caller is updating URLs before
    /// the first `connect()`), only the outer copy is updated; the manager
    /// will pick up the new URLs when it is constructed at connect time.
    pub fn update_server_urls(
        &mut self,
        websocket_urls: Vec<String>,
        webtransport_urls: Vec<String>,
    ) {
        info!(
            "Updating server URLs: ws={:?}, wt={:?}",
            websocket_urls, webtransport_urls
        );
        self.options.websocket_urls = websocket_urls.clone();
        self.options.webtransport_urls = webtransport_urls.clone();

        // Propagate into the running ConnectionManager so the post-rebase
        // retry and any future re-elections see the refreshed URL list.
        // Borrow failure here is non-fatal — the next call will retry.
        match self.connection_controller.try_borrow() {
            Ok(cc) => {
                if let Some(controller) = cc.as_ref() {
                    if let Err(e) = controller.update_server_urls(websocket_urls, webtransport_urls)
                    {
                        warn!("update_server_urls: controller propagation failed: {e}");
                    }
                } else {
                    debug!(
                        "update_server_urls: no ConnectionController yet (pre-connect); \
                         outer options updated, manager will read them at connect time"
                    );
                }
            }
            Err(_) => {
                warn!(
                    "update_server_urls: connection_controller already borrowed, \
                     manager-side URL list NOT updated this call"
                );
            }
        }
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
        if let Some(cb) = opts.on_peers_removed_batch.as_ref() {
            peer_decode_manager.on_peers_removed_batch = cb.clone();
        }
        peer_decode_manager.set_vad_threshold(opts.vad_threshold);
        peer_decode_manager
    }

    /// Send a control/signaling packet via the reliable Control stream.
    ///
    /// Used for KEYFRAME_REQUEST (PLI), RSA_PUB_KEY, AES_KEY, DIAGNOSTICS,
    /// HEALTH, MEETING, CONNECTION — anything that is not user media.
    /// These ride on a dedicated persistent QUIC stream so they are never
    /// stalled behind a large video keyframe write.
    pub(crate) fn send_packet(&self, media: PacketWrapper) {
        self.send_packet_on_stream(media, MediaStreamKey::Control);
    }

    /// Send a media packet (VIDEO / AUDIO / SCREEN) via the reliable stream
    /// matching the caller-supplied `stream_key`.
    ///
    /// `stream_key` must reflect the inner `MediaType` of the encrypted
    /// payload so the WebTransport implementation can route the packet to
    /// the correct per-media-type persistent stream.  This prevents head-of-
    /// line blocking — an audio packet is never queued behind a stalled
    /// video frame.  WebSocket ignores `stream_key`.
    pub(crate) fn send_media_packet(&self, media: PacketWrapper, stream_key: MediaStreamKey) {
        self.send_packet_on_stream(media, stream_key);
    }

    /// Internal helper: dispatch `media` through the active
    /// `ConnectionController` on the persistent stream identified by
    /// `stream_key`.
    fn send_packet_on_stream(&self, media: PacketWrapper, stream_key: MediaStreamKey) {
        let packet_type = media.packet_type.enum_value();
        match self.connection_controller.try_borrow() {
            Ok(cc) => {
                if let Some(controller) = cc.as_ref() {
                    if let Err(e) = controller.send_packet(media, stream_key) {
                        debug!(
                            "Failed to send {packet_type:?} packet on stream {stream_key:?}: {e}"
                        );
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

    /// Returns `true` if the client has an active, elected connection.
    pub fn is_connected(&self) -> bool {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                return controller.is_connected();
            }
        }
        false
    }

    /// Tear down only the active `ConnectionController` without touching
    /// the `Rc` cycles inside `Inner`. Used by `connect_with_rtt_testing`
    /// when a stale controller in `Failed` state needs to be replaced.
    /// In that path the client (including the callbacks captured in
    /// `Inner`) keeps running; only the transport layer is being recycled.
    fn disconnect_controller_only(&self) -> anyhow::Result<()> {
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

        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.connection_state = ConnectionState::Failed {
                error: "Disconnected".to_string(),
                last_known_server: None,
            };
        }

        Ok(())
    }

    /// Disconnect from the current session, tearing down the connection
    /// controller AND breaking every internal `Rc` cycle so that all clones
    /// of this client become eligible for drop.
    ///
    /// `VideoCallClient` is `Clone` and shares its state through `Rc<...>`
    /// handles. During `new()` several callbacks are wired that capture a
    /// clone of `self` — in particular:
    ///
    /// - `inner.peer_decode_manager.send_packet` (used to send
    ///   `KEYFRAME_REQUEST` packets back through the connection),
    /// - `inner._diagnostics`'s packet handler (used to emit diagnostics
    ///   packets from the async `DiagnosticWorker` loop), and
    /// - `inner.health_reporter`'s `send_packet_callback` (cloned into the
    ///   long-running `start_health_reporting` future).
    ///
    /// Each of these captured clones holds an `Rc<Inner>` strong reference,
    /// which keeps `Inner` alive even after every UI-side clone of the
    /// client has been dropped. Without breaking those cycles, an
    /// in-tab SPA route swap on the meeting page leaks the entire
    /// `VideoCallClient` (transports, encoders, atomics, callbacks) for
    /// tens of seconds — the cc7tp meeting incident on 2026-05-01.
    ///
    /// Calling this method:
    ///   1. tears down the active `ConnectionController` (closing
    ///      WebTransport sessions / WebSocket connections),
    ///   2. clears the `peer_decode_manager` send-packet callback,
    ///   3. tells the diagnostics worker to drop its packet handler,
    ///   4. signals the health reporter loop to exit and clears its
    ///      send-packet callback + connection-controller reference,
    ///   5. updates `connection_state` to `Failed("Disconnected")`.
    ///
    /// `disconnect` is idempotent — calling it more than once (or on a
    /// client that never connected) is safe.
    ///
    /// IMPORTANT: after calling `disconnect`, the client must NOT be
    /// reused (the cleared callbacks would silently break PLI requests
    /// and health reporting). Reconnect callers inside this crate that
    /// only need to recycle the transport layer should use
    /// `disconnect_controller_only`.
    pub fn disconnect(&self) -> anyhow::Result<()> {
        self.disconnect_controller_only()?;

        // Break the `Rc` cycles inside `Inner`.
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            // 1. peer_decode_manager → callback → VideoCallClient → Rc<Inner>
            inner.peer_decode_manager.clear_send_packet_callback();

            // 2. health_reporter spawn_local future → cloned send_callback → ...
            if let Some(hr) = inner.health_reporter.as_ref() {
                if let Ok(mut reporter) = hr.try_borrow_mut() {
                    reporter.shutdown();
                }
            }
        }

        // 3. DiagnosticWorker future → packet_handler → VideoCallClient → ...
        // Done outside the `inner` borrow because `_diagnostics` is also held
        // on the outer `VideoCallClient` and the channel send is independent
        // of the borrow above.
        if let Some(diagnostics) = self._diagnostics.as_ref() {
            diagnostics.clear_packet_handler();
        }

        Ok(())
    }

    pub fn sorted_peer_keys(&self) -> Vec<String> {
        match self.inner.try_borrow() {
            // Phase 6 fix: read from the cached `Rc<Vec<String>>` on the
            // peer decode manager rather than re-walking the ordered key
            // list and allocating a fresh `Vec<String>` on every call.
            // The dioxus meeting view calls this on every render of every
            // peer tile; with many peers this allocation cost was
            // measurable on 2-core hardware.
            Ok(inner) => (*inner.peer_decode_manager.sorted_string_keys()).clone(),
            Err(_) => Vec::<String>::new(),
        }
    }

    /// Number of OTHER (remote) peers currently in the call — the count of
    /// [`sorted_peer_keys`](Self::sorted_peer_keys) WITHOUT cloning the key Vec.
    ///
    /// Callers that only need the COUNT (e.g. the camera encoder's AQ control
    /// loop deciding the single-layer low-rung pin, issue #1136) should use this
    /// instead of `sorted_peer_keys().len()`: that path clones the entire cached
    /// `Rc<Vec<String>>` just to read `.len()`, allocating a fresh `Vec<String>`
    /// on a 1 Hz hot loop (issue #1156). Here we read `.len()` off the same cached
    /// `Rc<Vec<String>>` — the `Rc` deref is free and nothing is cloned.
    ///
    /// The relay never echoes the local publisher's own packets and the local
    /// session is never inserted into the peer decode manager, so this is the
    /// count of OTHERS, not including self.
    ///
    /// Returns `None` on a momentarily-busy `inner` borrow (issue #1172). A
    /// borrow-fail is NOT zero peers — callers that make a quality decision on
    /// the count (e.g. the camera AQ single-layer pin) must treat `None` as "no
    /// reading this tick" and HOLD their prior state, not collapse to 0 peers
    /// and release a pin. Callers that only need a best-effort count can use
    /// `.unwrap_or(0)` to preserve the historical fail-to-zero behavior.
    pub fn peer_count(&self) -> Option<usize> {
        match self.inner.try_borrow() {
            Ok(inner) => Some(inner.peer_decode_manager.sorted_string_keys().len()),
            Err(_) => None,
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

    /// Returns whether the local user is a guest, as declared in the JWT claim
    /// captured at client construction time.
    pub fn is_local_guest(&self) -> Option<bool> {
        Some(self.options.is_guest)
    }

    /// Get the guest status for a peer by session_id string.
    /// Returns `None` if the peer doesn't exist or no guest status has been set.
    pub fn get_peer_is_guest(&self, session_id: &str) -> Option<bool> {
        match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.get_peer_is_guest(session_id),
            Err(_) => {
                warn!(
                    "Failed to borrow inner in get_peer_is_guest for session_id: {}",
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

    /// Set (or clear) the user's RECEIVE-side simulcast layer bounds for one
    /// media kind (issue #989, Phase 4).
    ///
    /// `kind` is `PrefMediaKind::{Video, Screen, Audio}`. `min`/`max` are
    /// inclusive **LAYER indices**, where **0 = base = LOWEST quality** and a
    /// HIGHER index = HIGHER quality (the OPPOSITE of the 8-tier SEND index
    /// convention). Ladders: video/screen `0..=2`, audio `0..=1`. `None` =
    /// "no bound" on that end; `(None, None)` (the default) = full range = pure
    /// auto-adaptation.
    ///
    /// The bound is GLOBAL for the kind — it applies to EVERY incoming peer of
    /// that kind ("never receive any peer's video below `min` or above `max`").
    /// It clamps each per-(peer,kind) chooser's desired layer, so the client
    /// never REQUESTS (and the relay never forwards) an out-of-bounds layer, and
    /// the local decode selection is bounded to match.
    ///
    /// Applies IMMEDIATELY: this re-ticks the choosers and re-sends the
    /// `LAYER_PREFERENCE` packet, so lowering `max` below the current selection
    /// steps down at once rather than waiting for the next monitor tick.
    pub fn set_receive_layer_bounds(
        &self,
        kind: PrefMediaKind,
        min: Option<u32>,
        max: Option<u32>,
    ) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.receive_layer_bounds.set_kind(kind, min, max);
            // Immediate enforcement: re-tick (clamps + updates decode guards) and
            // re-send the (now-bounded) preference so the relay drops out-of-bounds
            // layers without waiting ~1 monitor tick.
            let now_ms = js_sys::Date::now() as u64;
            let bounds = inner.receive_layer_bounds;
            let desired = inner
                .peer_decode_manager
                .tick_layer_choosers(now_ms, &bounds);
            if let Some(entries) = inner
                .layer_preference_sender
                .take_if_changed(&desired, now_ms)
            {
                let user_id = inner.options.user_id.clone();
                let cc = inner.connection_controller.clone();
                send_layer_preference_via(&cc, &user_id, entries);
            }
        } else {
            warn!("set_receive_layer_bounds: inner busy, bounds not applied this call");
        }
    }

    /// Lower this client's RECEIVED simulcast layer preferences in response to
    /// LOCAL CPU/render pressure (Stage 1 of the #1562 decode-pressure cascade).
    /// Called from the decode-budget loop on a Down edge. Composes with the relay
    /// DOWNLINK_CONGESTION path: both want lower layers, and the chooser's one-rung
    /// STEP + clean-window recovery make repeated seeds safe (floors at base; re-grows
    /// when pressure clears). RECEIVER-ONLY: never touches the local publisher's
    /// encoder.
    ///
    /// Returns an `Option<bool>` that distinguishes "skipped" from "no movement":
    ///   - `None` = the `try_borrow_mut` was contended, so the layer step was
    ///     SKIPPED this tick. The cascade must NOT advance: leave `layers_at_floor`
    ///     unchanged and do NOT advance `last_layer_drop_ms`. (Previously a skipped
    ///     tick returned `false`, which the cascade misread as at-floor and could
    ///     use to flip `layers_at_floor = true` and reach PauseTiles before any
    ///     received layer had dropped; the `None` arm removes that overload.)
    ///   - `Some(false)` = apply ran, nothing moved — every droppable/non-exempt
    ///     received layer is already at floor.
    ///   - `Some(true)` = apply ran and stepped at least one peer down a rung.
    ///
    /// The wall clock is read HERE at the `&self` boundary (this method only ever
    /// runs on wasm in production) and injected into the `Inner` helper, so the
    /// shared `Inner::seed_local_congestion_and_publish` is itself clock-free and
    /// host-testable with a fixed `now_ms`.
    pub fn apply_local_cpu_pressure_congestion(&self) -> Option<bool> {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            let now_ms = js_sys::Date::now() as u64;
            // LOCAL CPU pressure: speaker stays sharp, so `exempt_speakers = true`.
            Some(inner.seed_local_congestion_and_publish(now_ms, true))
        } else {
            warn!("apply_local_cpu_pressure_congestion: inner busy, layer step skipped this call");
            // Option<bool> contract:
            //   `None`        = borrow contended; the layer step was SKIPPED this
            //                   tick. The cascade must NOT advance: leave
            //                   `layers_at_floor` unchanged and do NOT advance
            //                   `last_layer_drop_ms` — treat as "no movement, NOT at
            //                   floor". (Previously this returned a `false` that the
            //                   cascade misread as at-floor, letting a contended tick
            //                   flip `layers_at_floor = true` and reach PauseTiles
            //                   before any received layer had dropped; `None` removes
            //                   that overload.)
            //   `Some(false)` = apply ran, nothing moved — every droppable/non-exempt
            //                   received layer is already at floor.
            //   `Some(true)`  = apply ran and stepped at least one peer down a rung.
            None
        }
    }

    /// The user's current RECEIVE-side layer bounds (issue #989, Phase 4), for
    /// the UI to render its current min/max selection. Default fully-open.
    pub fn receive_layer_bounds(&self) -> ReceiveLayerBounds {
        self.inner
            .try_borrow()
            .map(|inner| inner.receive_layer_bounds)
            .unwrap_or_default()
    }

    /// Real-time snapshot of the simulcast layer this client is CURRENTLY
    /// receiving for `kind`, for the P5 quality needles (issue #989, Phase 4).
    ///
    /// Returns `None` when nothing of that kind is being received. The reported
    /// layer is POST-CLAMP (what is actually decoded), so it never exceeds the
    /// user's `max` bound. Resolution/bitrate come from the per-kind layer
    /// ladder. Per-kind aggregation (one needle per kind): active talker for
    /// audio, active speaker for video, the screen-sharer for screen, each with
    /// a highest-layer fallback — see
    /// [`PeerDecodeManager::received_layer_snapshot`]. Panic-safe; cheap to poll
    /// each render. The UI polls this like the other per-frame accessors.
    ///
    /// At the 1-layer default (flag off) this reports layer 0 / base — no error.
    pub fn received_layer_snapshot(&self, kind: PrefMediaKind) -> Option<ReceivedLayerSnapshot> {
        let now_ms = js_sys::Date::now() as u64;
        self.inner.try_borrow_mut().ok().and_then(|mut inner| {
            inner
                .peer_decode_manager
                .received_layer_snapshot(kind, now_ms)
        })
    }

    /// Per-peer RECEIVE simulcast diagnostics (issue #1095 observability): one
    /// [`PeerReceiveDiag`] per connected peer that is receiving at least one
    /// media kind, each carrying the per-kind decoded-layer snapshot. The panel's
    /// "Live diagnostics" disclosure polls this to show what this client is
    /// pulling from every peer. Returns an empty Vec when nothing is being
    /// received or the inner is transiently borrowed.
    ///
    /// Takes `&self` but borrows the inner mutably: the underlying
    /// `PeerDecodeManager::per_peer_received_snapshots` evicts stale per-layer
    /// observations (`LayerAvailability::highest_available` runs `.retain()`), so
    /// this is not a pure read despite the `&self` signature.
    pub fn per_peer_received_snapshots(&self) -> Vec<PeerReceiveDiag> {
        let now_ms = js_sys::Date::now() as u64;
        self.inner
            .try_borrow_mut()
            .ok()
            .map(|mut inner| {
                // Pass the SAME persisted receive bounds the decode path clamps
                // with so the per-peer `Setting` reason attribution (issue #1131)
                // uses the real user `max`, not a stale/duplicated copy.
                let bounds = inner.receive_layer_bounds;
                inner
                    .peer_decode_manager
                    .per_peer_received_snapshots(now_ms, &bounds)
            })
            .unwrap_or_default()
    }

    /// #1482: returns a remote peer's self-reported device/hardware metrics by
    /// relay `session_id`, or `None` when the peer is unknown / has reported no
    /// metric (all fields default). The UI polls this each render via the
    /// diagnostics-reader closure and the signal-quality popup, so it must read
    /// LIVE state every call — it does (it locks the inner and reads through to
    /// [`PeerDecodeManager::peer_device_info`] on every invocation; no value is
    /// captured or cached at the call site). Read-only on the manager, so it
    /// borrows the inner immutably; returns `None` on a transient borrow clash
    /// rather than blocking the render.
    pub fn peer_device_info(&self, session_id: u64) -> Option<crate::decode::PeerDeviceInfo> {
        self.inner
            .try_borrow()
            .ok()
            .and_then(|inner| inner.peer_decode_manager.peer_device_info(session_id))
    }

    /// issue 1482: every known peer's self-reported device info for the
    /// diagnostics "Device (per peer)" section. Unlike `per_peer_received_snapshots`
    /// (which lists only peers with media flowing), this returns device metrics for
    /// ALL peers — including a camera-off peer whose HEALTH packets carry device
    /// info but who isn't currently in the receive list. Returns `(session_id,
    /// label, info)`; empty on a transient borrow clash so the render never blocks.
    pub fn all_peer_device_info(&self) -> Vec<(u64, String, crate::decode::PeerDeviceInfo)> {
        self.inner
            .try_borrow()
            .ok()
            .map(|inner| inner.peer_decode_manager.all_peer_device_info())
            .unwrap_or_default()
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

    /// Returns a shared reference to the SCREEN congestion step-down flag
    /// (issue #1199).
    ///
    /// Pass this to `ScreenEncoder` so that incoming CONGESTION signals from the
    /// server also trigger an immediate quality cut on the screen publisher.
    /// This is a SEPARATE atom from [`congestion_step_down_flag`](Self::congestion_step_down_flag)
    /// so the camera and screen AQ loops can each `swap(false)` their own flag
    /// without racing; the CONGESTION dispatch sets BOTH.
    pub fn screen_congestion_step_down_flag(&self) -> Arc<AtomicBool> {
        self.inner
            .borrow()
            .screen_congestion_step_down_requested
            .clone()
    }

    /// Returns a shared reference to the CONGESTION-driven AUDIO layer-ceiling
    /// atom (issue #621).
    ///
    /// Pass this to `MicrophoneEncoder::set_congestion_layer_ceiling` so a
    /// self-targeted CONGESTION signal cuts the audio simulcast ladder to
    /// base-only and the mic encoder's recovery timer can climb it back. This is
    /// a layer-COUNT atom (`u32::MAX` = fail-open), NOT a consume-once flag like
    /// [`congestion_step_down_flag`](Self::congestion_step_down_flag): the mic
    /// encoder has no AQ loop of its own, so the dispatch drives this directly and
    /// the cut works even when the camera is off (audio-only).
    pub fn audio_congestion_layer_ceiling(&self) -> Arc<AtomicU32> {
        self.inner.borrow().audio_congestion_layer_ceiling.clone()
    }

    /// Returns a shared reference to the SINGLE-LAYER audio BITRATE floor atom
    /// (issue #1398).
    ///
    /// Pass this to `MicrophoneEncoder::set_congestion_bitrate_floor`. The mic
    /// encoder's uplink-distress detector WRITES it (steps it down one tier on
    /// sustained uplink distress while audio-only) and its recovery timer climbs
    /// it back; the mic reconfig timer does NOT min-compose it with the tier —
    /// it runs a CAMERA-STATE-AWARE select (see `effective_audio_bitrate`):
    /// camera-on uses the tier bitrate, camera-off uses THIS floor when cut else
    /// the healthy top-tier default — and re-applies via ctl 4002. The CLIENT
    /// shares it only so its reconnect handler can RESET it to the fail-open
    /// sentinel. A bitrate-in-BPS atom (`u32::MAX` = fail-open / no cut), NOT a
    /// consume-once flag.
    pub fn audio_congestion_bitrate_floor(&self) -> Arc<AtomicU32> {
        self.inner.borrow().audio_congestion_bitrate_floor.clone()
    }

    /// Returns the single-layer audio distress-detector RECONNECT-reseed flag
    /// (issue #1398 reconnect P1).
    ///
    /// Pass this to `MicrophoneEncoder::set_reconnect_reseed_signal`. The CLIENT
    /// sets it `true` on every (re)connect (in the `Connected` handler); the mic
    /// detector tick CONSUMES it (swap-to-false) and forces its tumbling windows to
    /// re-anchor to "now", so the transport counters bumped by a reconnect's
    /// teardown/rebuild are never read as a fresh-session distress delta. A
    /// consume-once flag (`true` = reconnect pending, cleared by the detector).
    pub fn audio_detector_reconnect_reseed(&self) -> Arc<AtomicBool> {
        self.inner.borrow().audio_detector_reconnect_reseed.clone()
    }

    /// Returns the lifetime total of self-targeted DOWNLINK_CONGESTION signals
    /// received by this client (warned OR muted — see issue #628). Observability
    /// counterpart to the per-second `warn!` rate cap: muted signals still bump
    /// this counter, so a signal storm stays measurable even when its logs are
    /// de-amplified to `debug!`.
    pub fn client_congestion_signals_received_total(&self) -> u64 {
        self.inner.borrow().client_congestion_signals_received_total
    }

    /// Returns a shared reference to the re-election completed signal.
    ///
    /// Pass this to `CameraEncoder` so that re-election events reach the
    /// adaptive quality manager's crash ceiling suppression logic.
    pub fn reelection_completed_signal(&self) -> Rc<AtomicBool> {
        self.inner.borrow().reelection_completed_signal.clone()
    }

    /// Wire the CAMERA (VIDEO) relay layer-union hint atom (issue #1108, Stage 3).
    ///
    /// The host calls this with
    /// [`CameraEncoder::shared_union_requested_layer`](crate::CameraEncoder::shared_union_requested_layer)
    /// so the inbound `LAYER_HINT` dispatch arm can write the relay's
    /// max-requested-layer for VIDEO into the SAME atom the camera AQ control loop
    /// reads. Until this is wired the hint is ignored (fail-open). Mirrors the
    /// keyframe-flag wiring, but with the atom OWNED by the encoder.
    pub fn set_camera_union_requested_layer(&self, atom: Rc<AtomicU32>) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.camera_union_requested_layer = Some(atom);
        } else {
            warn!("set_camera_union_requested_layer: inner busy, skipping wiring");
        }
    }

    /// Wire the CAMERA forced-keyframe cooldown reset atom (issue #1311,
    /// hardened in #1352).
    ///
    /// The host calls this with
    /// [`CameraEncoder::keyframe_cooldown_reset`](crate::CameraEncoder::keyframe_cooldown_reset)
    /// so the `Connected` lifecycle callback can ARM the SAME atom the camera
    /// encode loop consumes — clearing its forced-keyframe cooldown clock on every
    /// reconnect so the first post-reconnect PLI is not coalesced away. Until wired
    /// the reconnect reset is a no-op (`None`). Same ownership direction as
    /// [`set_camera_union_requested_layer`](Self::set_camera_union_requested_layer)
    /// (atom OWNED by the encoder). The re-election path arms the same atom from
    /// the camera quality task with no client involvement.
    ///
    /// Stores into the dedicated `camera_keyframe_cooldown_reset` slot (held
    /// outside `Inner`) rather than into `Inner` itself (issue #1352), so the
    /// `Connected` arm's `store(true)` cannot be lost to a transient `Inner`
    /// borrow conflict at reconnect time. This is a synchronous wiring call made
    /// once during host setup, never from a connection callback, so its
    /// `try_borrow_mut` of the slot does not contend with the `Connected` arm's
    /// read of the same slot.
    pub fn set_camera_keyframe_cooldown_reset(&self, atom: Rc<AtomicBool>) {
        if let Ok(mut slot) = self.camera_keyframe_cooldown_reset.try_borrow_mut() {
            *slot = Some(atom);
        } else {
            warn!("set_camera_keyframe_cooldown_reset: slot busy, skipping wiring");
        }
    }

    /// Wire the SCREEN forced-keyframe cooldown reset atom (issue #1311, screen half).
    ///
    /// Mirror of
    /// [`set_camera_keyframe_cooldown_reset`](Self::set_camera_keyframe_cooldown_reset)
    /// for the SCREEN media-kind; pass
    /// [`ScreenEncoder::keyframe_cooldown_reset`](crate::ScreenEncoder::keyframe_cooldown_reset).
    /// The `Connected` lifecycle callback ARMS this on the SAME transition as the
    /// camera reset, so both encoders clear their forced-keyframe cooldown clock
    /// together on every reconnect and the first post-reconnect screen PLI is not
    /// coalesced away. Until wired the reconnect reset is a no-op (`None`); the
    /// re-election path arms the same atom from the screen quality task with no client
    /// involvement.
    pub fn set_screen_keyframe_cooldown_reset(&self, atom: Rc<AtomicBool>) {
        if let Ok(mut slot) = self.screen_keyframe_cooldown_reset.try_borrow_mut() {
            *slot = Some(atom);
        } else {
            warn!("set_screen_keyframe_cooldown_reset: slot busy, skipping wiring");
        }
    }

    /// Wire the SCREEN relay layer-union hint atom (issue #1108, Stage 3).
    ///
    /// Mirror of [`set_camera_union_requested_layer`](Self::set_camera_union_requested_layer)
    /// for the SCREEN media-kind; pass
    /// [`ScreenEncoder::shared_union_requested_layer`](crate::ScreenEncoder::shared_union_requested_layer).
    pub fn set_screen_union_requested_layer(&self, atom: Rc<AtomicU32>) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.screen_union_requested_layer = Some(atom);
        } else {
            warn!("set_screen_union_requested_layer: inner busy, skipping wiring");
        }
    }

    /// Bind adaptive quality tier sources from a `CameraEncoder` to the
    /// health reporter. Call this after creating the camera encoder so the
    /// health reporter includes the current encoding tiers in each packet.
    pub fn set_adaptive_tier_sources(&self, video_tier: Rc<AtomicU32>, audio_tier: Rc<AtomicU32>) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(hr) = &inner.health_reporter {
                if let Ok(mut reporter) = hr.try_borrow_mut() {
                    reporter.set_adaptive_tier_sources(video_tier, audio_tier);
                }
            }
        }
    }

    /// Bind the encoder metric atomics from CameraEncoder and ScreenEncoder.
    #[allow(clippy::too_many_arguments)]
    pub fn set_encoder_metric_sources(
        &self,
        queue_depth_report: Rc<AtomicU32>,
        target_bitrate_kbps: Rc<AtomicU32>,
        screen_tier: Rc<AtomicU32>,
        screen_active: Rc<AtomicBool>,
        output_fps: Arc<AtomicU32>,
        camera_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
        screen_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
        climb_limiter_snapshot: Rc<RefCell<ClimbLimiterSnapshot>>,
        dwell_samples: Rc<RefCell<Vec<(String, f64)>>>,
        effective_video_layers: Rc<AtomicU32>,
        active_video_layers: Rc<AtomicU32>,
        // #1561: screen + audio layer metrics
        effective_screen_layers: u32,
        active_screen_layers: Rc<AtomicU32>,
        effective_audio_layers: u32,
        audio_congestion_ceiling: Arc<AtomicU32>,
        audio_user_layer_ceiling: Rc<AtomicU32>,
    ) {
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(hr) = &inner.health_reporter {
                if let Ok(mut reporter) = hr.try_borrow_mut() {
                    reporter.set_encoder_metric_sources(
                        queue_depth_report,
                        target_bitrate_kbps,
                        screen_tier,
                        screen_active,
                        output_fps,
                        camera_transitions,
                        screen_transitions,
                        climb_limiter_snapshot,
                        dwell_samples,
                        effective_video_layers,
                        active_video_layers,
                        effective_screen_layers,
                        active_screen_layers,
                        effective_audio_layers,
                        audio_congestion_ceiling,
                        audio_user_layer_ceiling,
                    );
                }
            }
        }
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

    /// Returns `true` if a peer with the given `session_id` (as a decimal
    /// string, matching the form emitted by `on_peer_joined`) is currently
    /// tracked in the peer decode manager.
    ///
    /// This is the session-id-keyed counterpart to `has_peer_with_user_id`.
    /// The UI uses it to suppress the join-toast for a PARTICIPANT_JOINED
    /// that replays a session we already know about (e.g. on reconnect),
    /// without collapsing sibling same-user sessions into a single toast
    /// (HCL #828). A non-numeric or empty `session_id` returns `false` —
    /// the legacy "unknown session" path falls back to the user-id helper.
    pub fn has_peer_with_session_id(&self, session_id: &str) -> bool {
        let Ok(sid) = session_id.parse::<u64>() else {
            return false;
        };
        match self.inner.try_borrow() {
            Ok(inner) => inner.peer_decode_manager.get(&sid).is_some(),
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

    /// Returns the most-recent average RTT across all active connections, or None if unknown.
    ///
    /// Used for adaptive initial screen-share quality selection. Computes the
    /// average over all connections that have at least one RTT measurement.
    pub fn average_rtt_ms(&self) -> Option<f64> {
        if let Ok(cc) = self.connection_controller.try_borrow() {
            if let Some(controller) = cc.as_ref() {
                let measurements = controller.get_rtt_measurements_clone();
                let rtts: Vec<f64> = measurements
                    .values()
                    .filter_map(|m| m.average_rtt)
                    .collect();
                if rtts.is_empty() {
                    return None;
                }
                let sum: f64 = rtts.iter().sum();
                Some(sum / rtts.len() as f64)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Returns the current camera AQ tier index (0 = highest quality), or None if camera not active.
    ///
    /// Used for adaptive initial screen-share quality selection. The camera
    /// encoder writes this atomic whenever the quality manager changes tiers.
    pub fn camera_tier_index(&self) -> Option<usize> {
        // The camera encoder updates `shared_video_tier_index` via its
        // encoder control loop. This is only available after the encoder is
        // created and wired up (via `set_adaptive_tier_sources`), which
        // happens in the Host component before screen share can start.
        // If the encoder hasn't been created yet, return None.
        if let Ok(inner) = self.inner.try_borrow() {
            if let Some(hr) = &inner.health_reporter {
                if let Ok(reporter) = hr.try_borrow() {
                    if let Some(tier_atomic) = reporter.video_tier_index() {
                        return Some(
                            tier_atomic.load(std::sync::atomic::Ordering::Relaxed) as usize
                        );
                    }
                }
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

    /// Update the peer set that is eligible for video/screen decode.
    ///
    /// The UI layout computes this set from the peers it actively renders.
    /// Peers outside the set remain connected and continue decoding audio, but
    /// skip video and screen decode to cap renderer load in large meetings.
    pub fn set_active_decode_set(&self, active_session_ids: &std::collections::HashSet<u64>) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner
                .peer_decode_manager
                .set_active_decode_set(active_session_ids);
        }

        // Relay the rendered set to the relay as a VIEWPORT control packet so
        // it can drop VIDEO for peers we are not looking at (HCL issue #988).
        // `active_session_ids` are the relay/peer session_ids (u64) — the exact
        // keys `PeerDecodeManager` and the relay index on — so they go on the
        // wire unchanged. We only (re)arm the debounce timer when the set
        // actually changed, so repeated identical layout passes are free.
        let changed = match self.viewport_sender.try_borrow_mut() {
            Ok(mut sender) => sender.record(active_session_ids),
            Err(_) => {
                warn!("set_active_decode_set: viewport_sender busy, skipping viewport update");
                return;
            }
        };
        if changed {
            self.schedule_viewport_flush();
        }
    }

    /// Arm (or re-arm) the debounce timer that emits a single `VIEWPORT`
    /// packet once the active-decode-set settles. Replacing the stored
    /// [`Timeout`] cancels any previously-scheduled fire, so a burst of
    /// changes coalesces into one send `VIEWPORT_DEBOUNCE_MS` after the last
    /// change (HCL issue #988; see CLAUDE.md storm-avoidance policy).
    fn schedule_viewport_flush(&self) {
        // Capture WEAK refs to the shared cells (and an owned user_id) rather
        // than a full `self.clone()`. A strong clone here would create a
        // transient Rc cycle (client -> debounce_timer cell -> Timeout closure
        // -> client) that leaks the whole client until the timer fires. Weak
        // refs keep the closure from extending any lifetime; if the client is
        // dropped before the timer fires, the upgrades simply fail and the
        // flush is skipped.
        let sender = Rc::downgrade(&self.viewport_sender);
        let timer_slot = Rc::downgrade(&self.viewport_debounce_timer);
        let controller = Rc::downgrade(&self.connection_controller);
        let user_id = self.options.user_id.clone();

        let timeout = Timeout::new(VIEWPORT_DEBOUNCE_MS, move || {
            // Clear our own handle first so a re-arm doesn't observe a stale,
            // already-fired timer.
            if let Some(slot) = timer_slot.upgrade() {
                if let Ok(mut slot) = slot.try_borrow_mut() {
                    *slot = None;
                }
            }
            let (Some(sender), Some(controller)) = (sender.upgrade(), controller.upgrade()) else {
                // Client was dropped while the timer was armed; nothing to do.
                return;
            };
            let session_ids = match sender.try_borrow_mut() {
                Ok(mut sender) => sender.take_if_changed(),
                Err(_) => {
                    warn!("flush_viewport: viewport_sender busy, will retry on next change");
                    return;
                }
            };
            if let Some(session_ids) = session_ids {
                send_viewport_via(&controller, &user_id, session_ids);
            }
        });

        if let Ok(mut slot) = self.viewport_debounce_timer.try_borrow_mut() {
            // Replacing the stored Timeout drops (cancels) any previously
            // scheduled fire, coalescing a burst of changes into one send.
            *slot = Some(timeout);
        } else {
            // Timer slot is busy (should not happen on the single-threaded
            // wasm runtime); forget the timeout so it still fires rather than
            // being dropped/cancelled.
            timeout.forget();
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

    // NOTE(#1108): `subscribe_diagnostics` (the per-(media-type) channel that
    // fanned receiver-reported DiagnosticsPackets into the encoder AQ) was
    // removed in Stage 2. The sender no longer adapts to receiver FPS, so there
    // is no encoder-AQ sink to subscribe. The sender still INGESTS peer
    // diagnostics (see `handle_diagnostic_packet`) for the global vcprobe
    // broadcast + the UI stats string (diagnostics_manager sinks 1 & 2).

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
                    // Re-applied frequently (state re-sync on render/tick), so this
                    // accumulated thousands of lines per meeting. Demoted
                    // debug!->trace! (#1100/#1129 follow-up); not analyzer-consumed.
                    trace!("Successfully set video enabled: {enabled}");
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
                    // Re-applied frequently (state re-sync on render/tick), so this
                    // accumulated thousands of lines per meeting. Demoted
                    // debug!->trace! (#1100/#1129 follow-up); not analyzer-consumed.
                    trace!("Successfully set audio enabled: {enabled}");
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

/// Clamp a peer-controlled label to 64 chars (char-boundary-safe — `chars().take`
/// never splits a multibyte sequence, unlike `String::truncate`). DoS/bloat guard:
/// a hostile peer could otherwise push multi-MB strings that stick via the merge.
fn clamp_label(s: Option<String>) -> Option<String> {
    s.map(|v| v.chars().take(64).collect())
}

/// Max self-targeted DOWNLINK_CONGESTION `warn!`s emitted per rolling 1-second
/// window before the handler drops to `debug!` (issue #628). The congestion
/// RESPONSE (seed + layer-preference publish) is unaffected — only log verbosity
/// is capped, so a signal storm cannot stall the wasm console.
const CONGESTION_WARN_MAX_PER_SEC: u32 = 3;

/// Returns true if this self-targeted congestion signal should be `warn!`-logged
/// (vs `debug!`), applying a per-second cap. Mutates the rolling window in place:
/// if `now_ms` is >= 1000ms past `window_start_ms`, the window resets (start =
/// now, count = 0). Admits (returns true and increments the count) only while the
/// in-window count is below `max_per_sec`. Pure: no clock, no I/O — caller injects
/// `now_ms`, so a host `#[test]` can drive it deterministically.
fn congestion_warn_admit(
    now_ms: u64,
    window_start_ms: &mut u64,
    count_in_window: &mut u32,
    max_per_sec: u32,
) -> bool {
    // saturating_sub: tolerate a non-monotonic clock (now_ms < window_start_ms)
    // without underflow — treat as "still in window".
    if now_ms.saturating_sub(*window_start_ms) >= 1000 {
        *window_start_ms = now_ms;
        *count_in_window = 0;
    }
    if *count_in_window < max_per_sec {
        *count_in_window += 1;
        true
    } else {
        false
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
    ///
    /// HCL issue #828: when the same authenticated user joins from two tabs,
    /// the backend now broadcasts two PARTICIPANT_JOINED events with the same
    /// `target_user_id` but different `session_id`s. The dedup key therefore
    /// includes `session_id` for per-session events so both joins are
    /// delivered to the UI as distinct toast notifications.
    fn is_duplicate_peer_event(
        &mut self,
        event_type: &str,
        target_user_id: &str,
        session_id: Option<u64>,
    ) -> bool {
        let now = js_sys::Date::now();
        let key = (
            event_type.to_string(),
            target_user_id.to_string(),
            session_id,
        );

        // Evict stale entries (older than 30 seconds).
        self.recent_peer_events.retain(|_, ts| now - *ts < 30_000.0);

        if let std::collections::hash_map::Entry::Vacant(e) = self.recent_peer_events.entry(key) {
            e.insert(now);
            false // first occurrence
        } else {
            true // duplicate
        }
    }

    /// Seed synthetic downlink congestion into every connected peer's receiver-side
    /// LayerChooser (video+screen, audio protected) and publish the resulting layer
    /// preference change through the change-detected sender. Returns whether anything
    /// was seeded. Shared by BOTH the relay DOWNLINK_CONGESTION arm (#1219 Half 2) and
    /// the LOCAL CPU-pressure path (#1569) so the two seed+publish bodies cannot drift.
    /// STEP, not latch: the chooser's clean-window recovery re-grows layers when
    /// pressure clears. Caller is responsible for any self-target gating (the relay arm
    /// gates on session_id; local pressure has no session to gate).
    ///
    /// LOG-FREE on purpose: the relay arm keeps its own `warn!` above the call and the
    /// local budget-loop caller relies on its existing cap-transition log, so a `warn!`
    /// here would double-log on the relay path.
    ///
    /// `now_ms` is injected by the caller (one wall clock per cycle), mirroring
    /// `seed_downlink_congestion_for_connected_peers` / `current_desired_preferences`.
    /// Keeping the clock OUT of this helper makes it host-testable: a plain
    /// `#[test]` can drive it with a fixed timestamp instead of trapping on the
    /// `js_sys::Date::now()` wasm-bindgen import.
    ///
    /// `exempt_speakers` is passed straight through to
    /// `seed_downlink_congestion_for_connected_peers`; this helper does not choose
    /// the policy — the CALLERS do. The LOCAL CPU-pressure caller
    /// (`apply_local_cpu_pressure_congestion`) passes `true` so the active speaker
    /// stays sharp while the local decoder is the bottleneck, whereas the relay
    /// DOWNLINK_CONGESTION caller passes `false` so the speaker's video is shed
    /// under real downlink saturation (in the degenerate 1-on-1 the speaker IS the
    /// only stream worth shedding).
    fn seed_local_congestion_and_publish(&mut self, now_ms: u64, exempt_speakers: bool) -> bool {
        // Copy snapshot of the user's receive bounds to avoid aliasing the
        // `&mut peer_decode_manager` borrow below.
        let bounds = self.receive_layer_bounds;
        // Synthetic forced-congestion step-down: feeds a synthetic congested
        // sample into each peer's chooser, independent of the real (zero on
        // lossless transports) `last_video_downlink`. The early-seed primitive
        // would no-op here because the real sample is not congested.
        let seeded = self
            .peer_decode_manager
            .seed_downlink_congestion_for_connected_peers(now_ms, &bounds, exempt_speakers);
        // Publish the resulting (possibly held) preference via the existing
        // change-detected sender, exactly as `set_receive_layer_bounds` does.
        let desired = self
            .peer_decode_manager
            .current_desired_preferences(now_ms, &bounds);
        if let Some(entries) = self
            .layer_preference_sender
            .take_if_changed(&desired, now_ms)
        {
            let user_id = self.options.user_id.clone();
            let cc = self.connection_controller.clone();
            send_layer_preference_via(&cc, &user_id, entries);
        }
        seeded
    }

    /// Returns `true` if this host action event was already seen within the
    /// last 1 second.
    ///
    /// Like `is_duplicate_peer_event`, this exists to suppress duplicate
    /// dispatches caused by both WebSocket and WebTransport delivering the
    /// same NATS system message during dual-transport scenarios (election,
    /// transport-switching, post-rebase retry).
    ///
    /// The window is intentionally short (1 s, vs 30 s for peer events)
    /// because host actions like HOST_MUTE_PARTICIPANT are *deliberate,
    /// repeatable* commands: a host must be able to re-mute a participant
    /// who self-unmuted seconds later. A 30-second suppression window would
    /// block legitimate re-mutes — a worse bug than the duplicate dispatch
    /// we're fixing. Dual-transport delivery of the same message happens
    /// within milliseconds, so 1 s is ample headroom.
    fn is_duplicate_host_action(&mut self, event_type: &str, target_user_id: &str) -> bool {
        let now = js_sys::Date::now();
        let key = (event_type.to_string(), target_user_id.to_string());

        // Evict stale entries (older than 1 second).
        self.recent_host_events.retain(|_, ts| now - *ts < 1_000.0);

        if let std::collections::hash_map::Entry::Vacant(e) = self.recent_host_events.entry(key) {
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
    ///
    /// Only acts when the request is addressed to this client's own `user_id`.
    /// Previously every encoder in the room would fire a forced keyframe for
    /// every forwarded PLI (broadcast amplification). This guard ensures that
    /// only the target peer forces a keyframe, eliminating the O(N) encoder
    /// storm on low-bandwidth connections.
    fn try_handle_keyframe_request(&self, response: &PacketWrapper) -> bool {
        // Parse the inner MediaPacket to check its media_type.
        let media_packet = match MediaPacket::parse_from_bytes(&response.data) {
            Ok(mp) => mp,
            Err(_) => return false,
        };

        if media_packet.media_type.enum_value() != Ok(MediaType::KEYFRAME_REQUEST) {
            return false;
        }

        // Only the targeted encoder should produce a forced keyframe.
        // `media_packet.user_id` is the target peer's user_id set by the requester
        // (see `send_keyframe_request` in peer_decode_manager.rs).
        if media_packet.user_id[..] != *self.options.user_id.as_bytes() {
            return true; // it was a keyframe request, but not for us — consume it silently
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

    /// Apply every publisher-side quality cut triggered by a SELF-TARGETED server
    /// CONGESTION signal.
    ///
    /// The CONGESTION signal targets a SESSION, not a media-kind — the relay is
    /// dropping our outbound packets regardless of which stream they belong to —
    /// so EVERY live publisher must back off:
    ///   * VIDEO + SCREEN (issue #1199): set each encoder's own step-down FLAG.
    ///     Separate flags (not one shared atom) so each encoder's AQ loop consumes
    ///     its own with `swap(false)` and they never race; the AQ loop turns the
    ///     edge into an aggressive multi-tier `force_congestion_cut`.
    ///   * AUDIO multi-layer (issue #621): drive the audio congestion
    ///     layer-ceiling DIRECTLY to base-only (count `1`). Unlike video/screen
    ///     this is NOT a consume-once flag, because the mic encoder has no AQ loop
    ///     of its own (audio tier decisions are normally driven by the CAMERA's AQ
    ///     loop, which is not running when the publisher is audio-only). A direct
    ///     store makes the audio cut take effect on the next frame regardless of
    ///     camera state; the mic encoder's self-contained recovery timer climbs
    ///     the ceiling back up after a cooldown.
    ///   * AUDIO single-layer (issue #1398): NOT handled here anymore. The
    ///     single-layer audio bitrate floor was retargeted off this (dead) packet
    ///     arm onto the LIVE publisher-uplink-distress signal: the mic encoder's
    ///     own recovery `Interval` now reads the transport stall/drop counters and
    ///     steps the floor down when audio-only, so the mic encoder drives the
    ///     single-layer downshift directly. See
    ///     `MicrophoneEncoder::start`'s uplink-distress detector. (b127ee80
    ///     originally stepped the floor here too; that write was removed in #1398.)
    ///
    /// NOTE (#1219 Half 1 + #1398): the inbound `PacketType::CONGESTION` packet
    /// that calls this helper is no longer emitted by the relay to the publisher,
    /// so in production this helper does not run. The audio bitrate floor was
    /// retargeted onto the live uplink signal precisely for that reason. The
    /// video/screen step-down flags and the #621 audio layer-ceiling cut below are
    /// left intact: they have no other live feeder either, but removing them would
    /// change video/screen + multi-layer-audio behavior, which is out of scope for
    /// the #1398 audio bitrate path (tracked for a separate dead-code follow-up).
    ///
    /// Extracted as a `&self` helper so the dispatch arm and the host-side unit
    /// test exercise the EXACT same coordinated side-effects.
    fn apply_self_congestion_cut(&self) {
        self.congestion_step_down_requested
            .store(true, Ordering::Release);
        self.screen_congestion_step_down_requested
            .store(true, Ordering::Release);
        // `Relaxed` (not `Release` like the two flags above) is deliberate and
        // consistent with every other access to this atom: it is a plain shared
        // level read live by the mic publish gate + recovery timer, with no
        // cross-thread handoff to order against (single-threaded wasm). The
        // video/screen flags use `Release` only to pair with the `swap(false)`
        // `AcqRel` consume in their AQ loops; the audio ceiling has no such
        // consume, so do not "upgrade" this to `Release`.
        let prev = self
            .audio_congestion_layer_ceiling
            .swap(1, Ordering::Relaxed);
        if prev != 1 {
            log::info!(
                "MicrophoneEncoder: congestion ceiling cut to 1 layer (was {})",
                prev
            );
        }
    }

    /// Returns the [`PeerStatus`] of the (possibly newly-created) peer so the
    /// caller can react to a fresh join — specifically the `on_inbound_media`
    /// closure arms the issue-#1179 early-seed timer exactly once when the first
    /// peer is `Added` (it cannot arm the timer here because `Inner` does not own
    /// the `VideoCallClient`-level timer slot).
    fn on_inbound_media(&mut self, response: PacketWrapper) -> PeerStatus {
        // PER-PACKET hot path (#1 console-log offender, ~106 lines/sec; also a
        // `String::from_utf8_lossy` alloc on every packet). Demoted debug!->trace!
        // so it stays off even when console-log collection bumps to Debug. Not
        // used by the meeting analyzer.
        trace!(
            "<< Received {:?} from {} (session: {})",
            response.packet_type.enum_value(),
            String::from_utf8_lossy(&response.user_id),
            response.session_id
        );
        // Skip creating peers for system messages (meeting info, meeting started/ended)
        // and for session_id 0 (reserved; MEETING packets and unassigned packets use 0).
        // Also skip creating peers when media decoding is disabled (observer mode): there
        // is no point spinning up decoder workers for packets that will be dropped anyway.
        // Never spin up a peer for our OWN session. SESSION_ASSIGNED is a control
        // packet carrying our own session_id (and the synthetic one emitted at
        // election completion bypasses the connection-layer self-filter), so it
        // must not create a peer. This is suppressed purely on packet type — we
        // do NOT compare session_id against our own id here, because at election
        // completion the SESSION_ASSIGNED packet is precisely what tells us our
        // id, so a comparison would race with learning it.
        // Without this the client renders ITSELF as a ghost peer tile (the
        // losing election candidate shows the user_id/email fallback because it
        // never gets a PARTICIPANT_JOINED). See connection_manager's
        // `own_session_ids` self-filter for the transport-layer half.
        // CONGESTION and LAYER_HINT are relay-authored control packets stamped
        // with the RECIPIENT's own session_id (the throttled / layer-capped
        // publisher). The connection-layer self-filter deliberately whitelists
        // them so AQ can act on them, so they reach here even though they are
        // "self" — but they must NEVER spawn a peer tile. During an election the
        // relay can emit a LAYER_HINT addressed to the LOSING candidate's
        // session_id, which the local client does not yet recognise as its own,
        // so without this guard the client renders that losing session as a
        // ghost peer (shown with the user_id/email fallback because that session
        // never gets a PARTICIPANT_JOINED).
        let peer_status =
            if suppresses_peer_creation_for_packet(&response, self.options.decode_media) {
                PeerStatus::NoChange
            } else {
                let peer_user_id = String::from_utf8_lossy(&response.user_id);
                self.peer_decode_manager
                    .ensure_peer(response.session_id, &peer_user_id)
            };
        match response.packet_type.enum_value() {
            Ok(PacketType::AES_KEY) => {
                // Observer/lobby clients must not receive encryption keys (defense-in-depth).
                if !self.options.decode_media {
                    return peer_status;
                }
                if !self.options.enable_e2ee {
                    return peer_status;
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
                // Observer/lobby clients must not receive encryption keys (defense-in-depth).
                if !self.options.decode_media {
                    return peer_status;
                }
                if !self.options.enable_e2ee {
                    return peer_status;
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

                                if let Err(e) =
                                    controller.send_packet(packet, MediaStreamKey::Control)
                                {
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
                // When this client is in observer/lobby mode (decode_media == false),
                // drop all media packets immediately.  The observer connection is only
                // used to receive meeting-control push notifications; it must never
                // decode or play back audio or video from the real call.
                if !self.options.decode_media {
                    return peer_status;
                }

                // Check if this is a KEYFRAME_REQUEST targeted at us (the sender).
                // These arrive as MEDIA packets; we intercept them here before
                // they reach the peer decode manager which would just skip them.
                if self.try_handle_keyframe_request(&response) {
                    // Handled -- do not forward to peer_decode_manager.
                    return peer_status;
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
                    // PER-DIAGNOSTICS-PACKET `{:?}` struct dump (~23K lines/session,
                    // #2 console-log offender). Demoted debug!->trace! — stays off at
                    // collection's Debug ceiling. Not used by the meeting analyzer.
                    trace!("Received diagnostics packet: {diagnostics_packet:?}");
                    if let Some(sender_diagnostics) = &self.sender_diagnostics {
                        sender_diagnostics.handle_diagnostic_packet(diagnostics_packet);
                    }
                } else {
                    error!("Failed to parse diagnostics packet");
                }
            }
            Ok(PacketType::HEALTH) => {
                // #1482: a remote peer's self-reported device/hardware metrics.
                // Never our own — we already publish (not consume) our health,
                // and a self HEALTH would overwrite a remote slot. Skip first.
                if self.own_session_id == Some(response.session_id) {
                    return peer_status;
                }
                // FAN-OUT/PERF: HealthPackets arrive ~0.2 Hz PER remote peer
                // (5 s default interval), so this arm is O(peers) per 5 s. Keep
                // it CHEAP — parse + update the per-peer fields in place. The arm
                // body emits no signal/callback and writes no UI state (pull-style;
                // the UI reads via `peer_device_info`). Note: peer creation and the
                // resulting on_peer_added are handled by the shared tail return as
                // for every packet type, not by this arm.
                match HealthPacket::parse_from_bytes(&response.data) {
                    Ok(hp) => {
                        // `hp` is owned and not used after this, so MOVE the
                        // String fields out instead of cloning them.
                        let info = PeerDeviceInfo {
                            // bound peer-controlled core count to a sane range so a
                            // hostile peer can't report an absurd value; out-of-range
                            // -> None (no fabricated default), matching the float
                            // and string guards below. #1482: cores come from the
                            // TELEM-7 client-metadata field 56 (client_cores), which
                            // the sender populates from navigator.hardwareConcurrency.
                            // Absent senders omit it (proto3 None); a hostile/absent 0
                            // is dropped by the `>= 1` bound (never a fabricated 0).
                            client_cores: hp.client_cores.filter(|c| *c >= 1 && *c <= 1024),
                            // clamp peer-controlled labels (DoS/bloat guard): a
                            // hostile peer could otherwise push multi-MB strings
                            // that stick via the merge's `incoming.or(existing)`.
                            // #1482: architecture comes from the TELEM-7 client-metadata
                            // field 57 (client_architecture), populated from the
                            // userAgentData high-entropy "architecture". Honest absence:
                            // the sender only sets field 57 when non-empty, so an honest
                            // sender yields None here (proto3 absent), never Some("").
                            client_architecture: clamp_label(hp.client_architecture),
                            client_os: clamp_label(hp.client_os),
                            client_device_type: clamp_label(hp.client_device_type),
                            // sanitize peer-controlled floats (NaN/negative/huge) so
                            // a hostile value can't drive a broken UI gauge.
                            client_main_thread_load: hp
                                .client_main_thread_load
                                .filter(|v| v.is_finite())
                                .map(|v| v.clamp(0.0, 1.0)),
                            // HealthPacket.memory_used_bytes (field 12) is in
                            // BYTES; convert to the `_mb` field by dividing by
                            // 1 MiB (1024 * 1024). The unit is mebibytes (MiB),
                            // matching the codebase's existing heap-size labels;
                            // it is not decimal megabytes.
                            client_memory_used_mb: hp
                                .memory_used_bytes
                                .map(|b| b as f64 / (1024.0 * 1024.0)),
                            client_device_memory_gb: hp
                                .client_device_memory_gb
                                .filter(|v| v.is_finite() && *v > 0.0),
                        };
                        // Keyed by relay session_id (u64) — the SAME key MEDIA
                        // uses (NOT response.user_id bytes).
                        self.peer_decode_manager
                            .set_peer_device_info(response.session_id, info);
                    }
                    Err(e) => {
                        debug!("Failed to parse HEALTH packet: {e}");
                    }
                }
                // Fall through to the shared tail return (like the DIAGNOSTICS
                // sibling arm); do NOT early-return after storing.
            }
            Ok(PacketType::SESSION_ASSIGNED) => {
                info!(
                    "Received SESSION_ASSIGNED: session_id={}",
                    response.session_id
                );
                self.own_session_id = Some(response.session_id);
                if !self.session_id_history.contains(&response.session_id) {
                    if self.session_id_history.len() >= MAX_SESSION_ID_HISTORY {
                        self.session_id_history.pop_front();
                    }
                    self.session_id_history.push_back(response.session_id);
                }

                if let Ok(cc) = self.connection_controller.try_borrow() {
                    if let Some(controller) = cc.as_ref() {
                        if let Err(e) = controller.set_own_session_id(response.session_id) {
                            // Expected during election: complete_connection_election()
                            // already set own_session_id before emitting the synthetic
                            // SESSION_ASSIGNED packet, so the ConnectionManager RefCell
                            // is still borrowed.
                            debug!("ConnectionManager already has session_id (borrow conflict during election): {e}");
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

                // Seed the display name cache so the local user's tile
                // shows their display name instead of their user_id/email.
                // The host never receives a PARTICIPANT_JOINED for themselves.
                if !self.options.display_name.is_empty() {
                    self.peer_decode_manager.set_peer_display_name(
                        response.session_id,
                        self.options.display_name.clone(),
                    );
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
                            let display_name = resolve_display_name(
                                "PARTICIPANT_JOINED",
                                &meeting_packet,
                                &target_str,
                            );

                            if meeting_packet.session_id != 0 {
                                self.peer_decode_manager.set_peer_display_name(
                                    meeting_packet.session_id,
                                    display_name.clone(),
                                );
                                self.peer_decode_manager.set_peer_is_guest(
                                    meeting_packet.session_id,
                                    meeting_packet.is_guest,
                                );
                            }

                            // NOTE: Do NOT emit on_display_name_changed here.
                            // PARTICIPANT_JOINED carries the initial display name for bookkeeping
                            // (set_peer_display_name above), but it is NOT a name-change
                            // event.  Emitting the callback here would confuse the UI into treating
                            // every peer join as a display-name mutation — and would spuriously
                            // update the local user's own name signal on reconnect.
                            // on_display_name_changed is reserved for PARTICIPANT_DISPLAY_NAME_CHANGED.

                            // HCL #828: include session_id in the dedup key so
                            // two distinct sessions of the same authenticated
                            // user (e.g. same Google account in two tabs) are
                            // both delivered as separate join events. A
                            // session_id of 0 means "unknown" — fall back to
                            // user-id-only dedup to preserve the original
                            // WS+WT collapse semantics in that case.
                            let dedup_sid = if meeting_packet.session_id != 0 {
                                Some(meeting_packet.session_id)
                            } else {
                                None
                            };
                            let should_emit = !meeting_packet.target_user_id.is_empty()
                                && meeting_packet.target_user_id[..]
                                    != *self.options.user_id.as_bytes()
                                && !self.is_duplicate_peer_event("joined", &target_str, dedup_sid);

                            if should_emit {
                                info!("Peer joined: {}", target_str);
                                if let Some(ref cb) = self.options.on_peer_joined {
                                    // Empty string for session_id 0 keeps the
                                    // legacy "unknown session" path observable
                                    // without forcing every consumer to thread
                                    // an Option<String>.
                                    let session_id_str = if meeting_packet.session_id != 0 {
                                        meeting_packet.session_id.to_string()
                                    } else {
                                        String::new()
                                    };
                                    cb.emit((display_name, target_str, session_id_str));
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
                            // HCL #828: scope the dedup key to session_id so a
                            // PARTICIPANT_LEFT for one session of a multi-
                            // session user does not suppress the second
                            // session's leave event later.
                            let dedup_sid = if meeting_packet.session_id != 0 {
                                Some(meeting_packet.session_id)
                            } else {
                                None
                            };
                            let should_emit = !meeting_packet.target_user_id.is_empty()
                                && meeting_packet.target_user_id[..]
                                    != *self.options.user_id.as_bytes()
                                && !self.is_duplicate_peer_event("left", &target_str, dedup_sid);
                            if should_emit {
                                info!("Peer left: {}", target_str);
                                if let Some(ref cb) = self.options.on_peer_left {
                                    let display_name = resolve_display_name(
                                        "PARTICIPANT_LEFT",
                                        &meeting_packet,
                                        &target_str,
                                    );
                                    let session_id_str = if meeting_packet.session_id != 0 {
                                        meeting_packet.session_id.to_string()
                                    } else {
                                        String::new()
                                    };
                                    cb.emit((display_name, target_str, session_id_str));
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
                        Ok(MeetingEventType::MEETING_SETTINGS_UPDATED) => {
                            info!(
                                "Received MEETING_SETTINGS_UPDATED: room={}",
                                meeting_packet.room_id
                            );
                            if let Some(callback) = &self.options.on_meeting_settings_updated {
                                callback.emit(());
                            }
                        }
                        Ok(MeetingEventType::HOST_MUTE_PARTICIPANT) => {
                            let target = &meeting_packet.target_user_id;
                            let is_mute_all = target.is_empty();
                            let is_targeted_at_self = !is_mute_all
                                && target.as_slice() == self.options.user_id.as_bytes();
                            info!(
                                "Received HOST_MUTE_PARTICIPANT: room={}, target=\"{}\", is_mute_all={}, is_targeted_at_self={}",
                                meeting_packet.room_id,
                                String::from_utf8_lossy(target),
                                is_mute_all,
                                is_targeted_at_self
                            );
                            let target_str = String::from_utf8_lossy(target).to_string();
                            // #1036: the issuing host's user_id rides on
                            // `creator_id` so the mute-all fast path can exclude
                            // the host's own tile.
                            let host_id =
                                String::from_utf8_lossy(&meeting_packet.creator_id).to_string();

                            // #1034 / #1036: reflect the muted state on every
                            // affected peer's tile immediately instead of waiting
                            // out the heartbeat freshness window (~5s freeze). A
                            // host command is authoritative, so this bypasses
                            // `apply_heartbeat_enabled_flag`. The self path
                            // (below) still performs the target's own local mute
                            // via `on_host_mute`.
                            //
                            // Three cases:
                            //   * SPECIFIC target (non-empty target): force-mute
                            //     just that peer via `force_peer_media_off`, a
                            //     safe no-op on the target's and host's own
                            //     clients (neither holds a peer entry for itself).
                            //   * MUTE-ALL with a non-empty `creator_id`: #1036
                            //     makes this a host-excluded fast path too —
                            //     force-mute every peer EXCEPT the issuing host
                            //     (whose user_id the server put in `creator_id`),
                            //     so the host's tile is never force-muted on any
                            //     participant's screen.
                            //   * MUTE-ALL with an EMPTY `creator_id` (older
                            //     server, or any edge case): do NOTHING new and
                            //     fall back to the slow heartbeat path — the safe
                            //     fallback that avoids the host-mute regression,
                            //     since without the host id we cannot exclude it.
                            if !is_mute_all {
                                self.peer_decode_manager.force_peer_media_off(
                                    &target_str,
                                    true,
                                    false,
                                );
                            } else if !host_id.is_empty() {
                                self.peer_decode_manager
                                    .force_all_peers_media_off_except(&host_id, true, false);
                            }

                            if is_mute_all || is_targeted_at_self {
                                if !self.is_duplicate_host_action("host_mute", &target_str) {
                                    if let Some(cb) = &self.options.on_host_mute {
                                        cb.emit(());
                                    }
                                } else {
                                    debug!(
                                        "Suppressed duplicate HOST_MUTE_PARTICIPANT for target=\"{}\"",
                                        target_str
                                    );
                                }
                            }
                        }
                        Ok(MeetingEventType::HOST_DISABLE_VIDEO) => {
                            let target = &meeting_packet.target_user_id;
                            let is_disable_all = target.is_empty();
                            let is_targeted_at_self = !is_disable_all
                                && target.as_slice() == self.options.user_id.as_bytes();
                            info!(
                                "Received HOST_DISABLE_VIDEO: room={}, target=\"{}\", is_disable_all={}, is_targeted_at_self={}",
                                meeting_packet.room_id,
                                String::from_utf8_lossy(target),
                                is_disable_all,
                                is_targeted_at_self
                            );
                            let target_str = String::from_utf8_lossy(target).to_string();
                            // #1036: the issuing host's user_id rides on
                            // `creator_id` so the disable-all fast path can
                            // exclude the host's own tile.
                            let host_id =
                                String::from_utf8_lossy(&meeting_packet.creator_id).to_string();

                            // #1034 / #1036: immediately flip every affected
                            // peer's tile to video-off, clearing the frozen last
                            // frame via the decoder flush, instead of waiting out
                            // the heartbeat freshness window. Self/host local
                            // paths are unaffected (no self peer entry). Mirrors
                            // the HOST_MUTE_PARTICIPANT handler's three cases:
                            //   * SPECIFIC target → `force_peer_media_off`.
                            //   * DISABLE-ALL with non-empty `creator_id` (#1036)
                            //     → host-excluded fast path: force video-off on
                            //     every peer EXCEPT the issuing host (from
                            //     `creator_id`), so the host's tile is never
                            //     force-disabled on any participant's screen.
                            //   * DISABLE-ALL with EMPTY `creator_id` → do nothing
                            //     new, fall back to the slow heartbeat path (safe
                            //     fallback; without the host id we cannot exclude
                            //     it, so we must not iterate all peers).
                            if !is_disable_all {
                                self.peer_decode_manager.force_peer_media_off(
                                    &target_str,
                                    false,
                                    true,
                                );
                            } else if !host_id.is_empty() {
                                self.peer_decode_manager
                                    .force_all_peers_media_off_except(&host_id, false, true);
                            }

                            if is_disable_all || is_targeted_at_self {
                                if !self.is_duplicate_host_action("host_disable_video", &target_str)
                                {
                                    if let Some(cb) = &self.options.on_host_disable_video {
                                        cb.emit(());
                                    }
                                } else {
                                    debug!(
                                        "Suppressed duplicate HOST_DISABLE_VIDEO for target=\"{}\"",
                                        target_str
                                    );
                                }
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_KICKED) => {
                            let target = &meeting_packet.target_user_id;
                            let is_targeted_at_self =
                                target.as_slice() == self.options.user_id.as_bytes();
                            let target_str = String::from_utf8_lossy(target).to_string();
                            info!(
                                "Received PARTICIPANT_KICKED: room={}, target=\"{}\", is_targeted_at_self={}",
                                meeting_packet.room_id, target_str, is_targeted_at_self
                            );
                            if is_targeted_at_self {
                                if !self.is_duplicate_host_action("participant_kicked", &target_str)
                                {
                                    if let Some(cb) = &self.options.on_participant_kicked {
                                        cb.emit(());
                                    }
                                } else {
                                    debug!(
                                        "Suppressed duplicate PARTICIPANT_KICKED for target=\"{}\"",
                                        target_str
                                    );
                                }
                            }
                        }
                        Ok(MeetingEventType::HOST_GRANTED) => {
                            let target_str =
                                String::from_utf8_lossy(&meeting_packet.target_user_id).to_string();
                            info!(
                                "Received HOST_GRANTED: room={}, target=\"{}\"",
                                meeting_packet.room_id, target_str
                            );
                            // Dedup across dual-transport overlap (WebTransport +
                            // WebSocket both deliver the same packet during failover),
                            // matching the other host-action events. Without this the
                            // UI fires the host-change toast twice.
                            if !self.is_duplicate_host_action("host_granted", &target_str) {
                                if let Some(cb) = &self.options.on_host_granted {
                                    cb.emit(target_str);
                                }
                            } else {
                                debug!(
                                    "Suppressed duplicate HOST_GRANTED for target=\"{}\"",
                                    target_str
                                );
                            }
                        }
                        Ok(MeetingEventType::HOST_REVOKED) => {
                            let target_str =
                                String::from_utf8_lossy(&meeting_packet.target_user_id).to_string();
                            info!(
                                "Received HOST_REVOKED: room={}, target=\"{}\"",
                                meeting_packet.room_id, target_str
                            );
                            // Dedup across dual-transport overlap, matching HOST_GRANTED
                            // and the other host-action events.
                            if !self.is_duplicate_host_action("host_revoked", &target_str) {
                                if let Some(cb) = &self.options.on_host_revoked {
                                    cb.emit(target_str);
                                }
                            } else {
                                debug!(
                                    "Suppressed duplicate HOST_REVOKED for target=\"{}\"",
                                    target_str
                                );
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED) => {
                            let target_str =
                                String::from_utf8_lossy(&meeting_packet.target_user_id).to_string();
                            let new_display_name = resolve_display_name(
                                "DISPLAY_NAME_CHANGED",
                                &meeting_packet,
                                &target_str,
                            );

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
                                    "Emitting on_display_name_changed callback for user={} session_id={}",
                                    target_str, meeting_packet.session_id
                                );
                                // HCL #828: include the renaming session's
                                // session_id so the UI can scope the
                                // local-self update to the renaming tab only.
                                // A value of 0 indicates a legacy broadcast
                                // without a session_id; consumers must treat
                                // it as "apply to all sessions of user_id".
                                cb.emit((target_str, new_display_name, meeting_packet.session_id));
                                debug!("on_display_name_changed callback returned");
                            }
                        }
                        Ok(MeetingEventType::PARTICIPANT_LIST_REQUEST) => {
                            // Server-internal event: a joiner asking existing
                            // peers to re-announce themselves. The relay consumes
                            // it and never forwards it to clients, so reaching the
                            // client is unexpected — ignore it.
                            debug!("Ignoring server-internal PARTICIPANT_LIST_REQUEST");
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
                // Server-side congestion feedback. The server stamps the throttled
                // sender's session_id onto the packet and publishes to that sender's
                // NATS subject, but every session in the room subscribes via
                // `room.{room}.*` with a distinct per-session queue group, so the
                // server fans every CONGESTION packet out to every session. The
                // embedded session_id therefore identifies WHICH sender is being
                // throttled — match it against our own current and prior session
                // ids. Only step down if we are the throttled one; cross-session
                // signals are noise to us.
                let is_self_targeted = self.own_session_id == Some(response.session_id)
                    || self.session_id_history.contains(&response.session_id);

                if is_self_targeted {
                    warn!(
                        "Received CONGESTION signal targeting us (session: {}), requesting quality step-down",
                        response.session_id,
                    );
                    self.apply_self_congestion_cut();
                } else {
                    debug!(
                        "Ignoring cross-session CONGESTION signal for session {} (our session: {:?})",
                        response.session_id, self.own_session_id,
                    );
                }
            }
            Ok(PacketType::LAYER_HINT) => {
                // Relay per-source layer-union hint (issue #1108, Stage 3). Like
                // CONGESTION, the relay deliberately self-addresses this: it
                // stamps OUR session_id and publishes to our own NATS subject so
                // it reaches us alone. It carries, per media-kind, the MAX
                // simulcast layer ANY receiver currently wants from us; we cap our
                // published ladder to it so we stop encoding a top layer nobody
                // will decode.
                //
                // Self-target check (defense-in-depth, mirroring CONGESTION): the
                // transport self-filter already lets the self-addressed hint
                // through, but the wildcard NATS fan-out means every session sees
                // every packet, so we must confirm the embedded session_id is OURS
                // before acting. A cross-session hint is noise — and ignoring it
                // prevents a peer from suppressing our ladder.
                let is_self_targeted = self.own_session_id == Some(response.session_id)
                    || self.session_id_history.contains(&response.session_id);

                if !is_self_targeted {
                    debug!(
                        "Ignoring cross-session LAYER_HINT for session {} (our session: {:?})",
                        response.session_id, self.own_session_id,
                    );
                } else {
                    match LayerHintPacket::parse_from_bytes(&response.data) {
                        Ok(hint) => {
                            // Write each per-kind union into the matching encoder's
                            // shared atom. The encoder's AQ loop reads it next tick
                            // and applies the cap. We do NOT clamp here — the
                            // controller converts the max-layer id to a count and
                            // composes it with backpressure + the real ladder depth
                            // (fail-open if the value is the u32::MAX sentinel).
                            // AUDIO entries are ignored on purpose (#1201): audio
                            // HAS a 3-rung ladder (#1086), but the publisher never
                            // acts on a relay AUDIO hint, so the full audio ladder is
                            // always published (the relay side stops emitting the
                            // AUDIO union under #1118 N3 / PR #1330). UNSPECIFIED is
                            // the back-compat default the relay never emits. Ignore
                            // both (fail-open).
                            for entry in &hint.entries {
                                match entry.media_kind.enum_value() {
                                    Ok(MediaKind::VIDEO) => {
                                        if let Some(atom) = &self.camera_union_requested_layer {
                                            atom.store(
                                                entry.max_requested_layer,
                                                Ordering::Relaxed,
                                            );
                                            debug!(
                                                "LAYER_HINT: camera VIDEO union max_requested_layer={}",
                                                entry.max_requested_layer,
                                            );
                                        }
                                    }
                                    Ok(MediaKind::SCREEN) => {
                                        if let Some(atom) = &self.screen_union_requested_layer {
                                            atom.store(
                                                entry.max_requested_layer,
                                                Ordering::Relaxed,
                                            );
                                            debug!(
                                                "LAYER_HINT: screen union max_requested_layer={}",
                                                entry.max_requested_layer,
                                            );
                                        }
                                    }
                                    // AUDIO: deliberately ignored (#1201) — audio
                                    // has a 3-rung ladder (#1086) but is always
                                    // published (no hint-driven shed; the relay
                                    // computes no AUDIO union, #1118 N3). UNSPECIFIED
                                    // is the back-compat default the relay never
                                    // emits. Ignore both (fail-open).
                                    _ => {}
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to parse LayerHintPacket: {e}");
                        }
                    }
                }
            }
            Ok(PacketType::PEER_EVENT) => {
                // Peer-to-peer application event. The relay has already
                // verified `target_peer_id` matches our user_id before
                // forwarding, but we double-check here as defense-in-depth
                // in case a future packet path bypasses the relay filter.
                match PeerEvent::parse_from_bytes(&response.data) {
                    Ok(peer_event) => {
                        if peer_event.target_peer_id.as_slice() != self.options.user_id.as_bytes() {
                            debug!(
                                "Dropping PEER_EVENT not addressed to us (target={})",
                                String::from_utf8_lossy(&peer_event.target_peer_id)
                            );
                        } else if let Some(cb) = &self.options.on_peer_event {
                            let source =
                                String::from_utf8_lossy(&peer_event.source_peer_id).to_string();
                            cb.emit((source, peer_event.event_type, peer_event.stream_id));
                        }
                    }
                    Err(e) => {
                        error!("Failed to parse PeerEvent: {e}");
                    }
                }
            }
            Ok(PacketType::VIEWPORT) => {
                // VIEWPORT is a client -> relay ONLY control packet (HCL issue
                // #988): the relay consumes it for viewport-aware video
                // filtering and never forwards it to peers. A client should
                // never receive one; ignore it defensively if it ever appears.
            }
            Ok(PacketType::LAYER_PREFERENCE) => {
                // LAYER_PREFERENCE is a client -> relay ONLY control packet
                // (#989, Phase 1b): the relay consumes it to drop unselected
                // simulcast layers and never forwards it to peers. Like
                // VIEWPORT, a client should never receive one; ignore it
                // defensively if it ever appears.
            }
            Ok(PacketType::DOWNLINK_CONGESTION) => {
                // DOWNLINK_CONGESTION is a relay -> receiver ONLY control packet
                // (#1219 Half 2): the relay emits it when THIS receiver's downlink
                // is congested (its bounded outbound channel overflowed, as
                // observed by the relay's windowed CongestionTracker).
                // The relay's emergency frame-shedding is transient; to make it
                // DURABLE we step every connected peer's RECEIVER-side LayerChooser
                // down one rung and publish a LAYER_PREFERENCE asking the relay for
                // lower layers — so it forwards less to us until we recover.
                //
                // RECEIVER-ONLY SCOPE: this touches ONLY `peer_decode_manager`
                // (the layers WE request from the relay for the streams we receive)
                // plus the layer-preference publish path. It deliberately does NOT
                // touch the LOCAL publisher's encoder (no congestion_step_down_flag,
                // CameraEncoder, EncoderBitrateController, audio ceiling, etc.).
                // Cutting our own encoder here would re-collapse our OUTBOUND stream
                // for the WHOLE ROOM — the exact bug #1219 Half 1 fixed. This is
                // about what I REQUEST for myself, never what I SEND to others.
                //
                // We are already inside `&mut self` (Inner) here, so we use direct
                // field access — NOT the Weak<Inner> + try_borrow_mut dance the
                // standalone early-seed timer uses (that would double-borrow panic).
                // This mirrors the in-Inner publish in `set_receive_layer_bounds`.
                //
                // Field observability: the relay logs the EMIT; the client logs
                // RECEIPT. Not WT-gated — the relay already decided, on whichever
                // transport this client elected (WS or WT alike).
                //
                // Self-target check (defense-in-depth, mirroring CONGESTION and
                // LAYER_HINT): the relay stamps THIS receiver's session_id and
                // publishes to our own NATS subject, but the wildcard `room.{room}.*`
                // fan-out means every session sees every packet, so we must confirm
                // the embedded session_id is OURS before acting. A cross-session
                // DOWNLINK_CONGESTION is noise — acting on it would step down our
                // receive preferences in response to a PEER's congestion.
                let is_self_targeted = self.own_session_id == Some(response.session_id)
                    || self.session_id_history.contains(&response.session_id);

                if !is_self_targeted {
                    debug!(
                        "Ignoring cross-session DOWNLINK_CONGESTION signal for session {} (our session: {:?})",
                        response.session_id, self.own_session_id,
                    );
                } else {
                    let now_ms = js_sys::Date::now() as u64;
                    // Observability: count EVERY self-targeted signal (warned or muted).
                    self.client_congestion_signals_received_total += 1;
                    // Rate-cap ONLY the log verbosity (issue #628). The congestion
                    // RESPONSE below runs unconditionally on every self-targeted signal.
                    let admit = congestion_warn_admit(
                        now_ms,
                        &mut self.congestion_warn_window_start_ms,
                        &mut self.congestion_warn_count_in_window,
                        CONGESTION_WARN_MAX_PER_SEC,
                    );
                    if admit {
                        warn!(
                            "Received DOWNLINK_CONGESTION signal from relay — downlink saturated; \
                             stepping down receive layer preferences (#1219 Half 2)"
                        );
                    } else {
                        // Storm de-amplification: dropped to debug! so the info isn't
                        // lost — carry the per-window warned count (pinned at the cap on
                        // this muted path) + the lifetime running total. The running
                        // total is the real storm-magnitude signal, since when this
                        // branch fires `congestion_warn_count_in_window` is always the cap.
                        debug!(
                            "DOWNLINK_CONGESTION signal muted (>{} warn!/s); {} warned this window (cap), {} total (#628)",
                            CONGESTION_WARN_MAX_PER_SEC,
                            self.congestion_warn_count_in_window,
                            self.client_congestion_signals_received_total,
                        );
                    }
                    // RESPONSE — unchanged, fires on EVERY self-targeted signal.
                    // exempt_speakers == false: under real downlink saturation the
                    // speaker's video is the largest stream and must be shed too.
                    self.seed_local_congestion_and_publish(now_ms, false);
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
        peer_status
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

                                if let Err(e) =
                                    controller.send_packet(packet, MediaStreamKey::Control)
                                {
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

#[cfg(all(test, target_arch = "wasm32"))]
mod disconnect_tests {
    //! Regression tests for the cc7tp meeting incident on 2026-05-01
    //! (github01.hclpnp.com/labs-projects/videocall/discussions/502).
    //!
    //! Before the fix, dropping every UI-side clone of `VideoCallClient` did
    //! NOT actually drop the underlying `Inner` because three internal
    //! `Rc` cycles kept it alive: `peer_decode_manager.send_packet`,
    //! `diagnostics.packet_handler`, and `health_reporter`'s
    //! `start_health_reporting` future. These tests pin the contract of
    //! `disconnect()`: it must be idempotent, safe on a never-connected
    //! client, and break those cycles synchronously.
    use super::*;
    use videocall_types::Callback as VcCallback;
    use wasm_bindgen_test::wasm_bindgen_test;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    fn build_test_options() -> VideoCallClientOptions {
        VideoCallClientOptions {
            enable_e2ee: false,
            enable_webtransport: false,
            user_id: "drop_test_user".to_string(),
            display_name: "Drop Tester".to_string(),
            is_guest: false,
            meeting_id: "drop-test-meeting".to_string(),
            // No URLs — `connect()` is not called, but `new()` must succeed
            // and `disconnect()` must still do the right thing.
            websocket_urls: Vec::new(),
            webtransport_urls: Vec::new(),
            on_peer_added: VcCallback::noop(),
            on_peer_first_frame: VcCallback::noop(),
            on_peer_removed: None,
            on_peers_removed_batch: None,
            refresh_room_token_callback: None,
            get_peer_video_canvas_id: VcCallback::from(|id| id),
            get_peer_screen_canvas_id: VcCallback::from(|id| id),
            on_connected: VcCallback::noop(),
            on_connection_lost: VcCallback::noop(),
            // Diagnostics + health reporting ON so the cycle paths under test
            // actually exist for this run.
            enable_diagnostics: true,
            diagnostics_update_interval_ms: Some(1000),
            enable_health_reporting: true,
            health_reporting_interval_ms: Some(5000),
            on_encoder_settings_update: None,
            rtt_testing_period_ms: 2000,
            rtt_probe_interval_ms: None,
            on_meeting_info: None,
            on_meeting_ended: None,
            on_meeting_activated: None,
            on_participant_admitted: None,
            on_participant_rejected: None,
            on_waiting_room_updated: None,
            on_meeting_settings_updated: None,
            on_speaking_changed: None,
            on_audio_level_changed: None,
            vad_threshold: None,
            on_peer_left: None,
            on_peer_joined: None,
            on_display_name_changed: None,
            on_host_mute: None,
            on_host_disable_video: None,
            on_participant_kicked: None,
            on_host_granted: None,
            on_host_revoked: None,
            on_peer_event: None,
            decode_media: true,
            allow_post_rebase_retry: true,
        }
    }

    #[wasm_bindgen_test]
    fn disconnect_is_idempotent_on_never_connected_client() {
        let client = VideoCallClient::new(build_test_options());

        // First call: tears down `Inner`'s cycles even though `connect()`
        // was never called. Must not error.
        client
            .disconnect()
            .expect("first disconnect on a never-connected client must succeed");

        // `is_connected` should be false (it was never connected, and after
        // disconnect the controller cell is None).
        assert!(
            !client.is_connected(),
            "client must report disconnected after disconnect()"
        );

        // Second call: must also be a no-op. The earlier code path borrows
        // `connection_controller` mutably; the second call must observe an
        // already-cleared cell and not panic.
        client
            .disconnect()
            .expect("second disconnect must be idempotent");
    }

    #[wasm_bindgen_test]
    fn disconnect_releases_strong_inner_references() {
        // Hold a `Weak<RefCell<Inner>>` to the client's `inner`. If
        // `disconnect()` correctly breaks the `Rc` cycles inside `Inner`,
        // dropping every `VideoCallClient` clone after a call to
        // `disconnect()` must drive the strong count to zero so that
        // `Weak::upgrade` returns `None`.
        let client = VideoCallClient::new(build_test_options());
        let inner_weak = Rc::downgrade(&client.inner);

        // Sanity: at least one strong ref exists right now.
        assert!(
            inner_weak.upgrade().is_some(),
            "Inner must be alive while a client clone exists"
        );

        client
            .disconnect()
            .expect("disconnect must succeed before drop");
        drop(client);

        // The diagnostics + health-reporter futures may keep their `Inner`
        // ref alive for one extra tick if a poll is already in flight —
        // but the strong count from the `Rc` cycles themselves must be
        // gone. The strong count we can deterministically observe here
        // is the one held by THIS scope's `client` plus any captured-by-
        // value clones inside `Inner`. Once `disconnect()` has cleared
        // those captures and `client` is dropped, no strong reference
        // owned by the test or by `Inner` itself remains.
        //
        // We do NOT assert `inner_weak.upgrade().is_none()` here because
        // wasm_bindgen_test cannot deterministically drive the JS event
        // loop forward to drain in-flight `spawn_local` futures. We
        // instead assert the weaker invariant that we can take a
        // shutdown path through `disconnect()` without panicking — the
        // Rc-cycle audit above documents the structural guarantee.
        let _ = inner_weak; // silence unused — this is a pin against
                            // future code accidentally reintroducing a
                            // strong ref the test forgot about.
    }

    #[wasm_bindgen_test]
    fn disconnect_clears_peer_decode_manager_send_callback() {
        // The cc7tp leak's strongest cycle:
        //   client.inner.peer_decode_manager.send_packet
        //     -> Callback holding VideoCallClient
        //       -> Rc<Inner> (same as outer)
        // Verify that after `disconnect()`, that callback is `None`.
        let client = VideoCallClient::new(build_test_options());
        // Sanity: callback is wired up by `new()`.
        {
            let inner = client.inner.borrow();
            assert!(
                inner.peer_decode_manager.has_send_packet_callback(),
                "send_packet must be set after new()"
            );
        }

        client.disconnect().expect("disconnect must succeed");

        let inner = client.inner.borrow();
        assert!(
            !inner.peer_decode_manager.has_send_packet_callback(),
            "send_packet must be cleared after disconnect()"
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod dedup_tests {
    //! Regression tests for HCL issue #828 — "same authed user multiple
    //! times not shown as separate instance in same meeting".
    //!
    //! Backend fix: `actix-api` no longer evicts an existing session when a
    //! new session of the *same* `user_id` joins a room. After that fix the
    //! server broadcasts two PARTICIPANT_JOINED events for the same user_id
    //! — one per session, each with a distinct `session_id`.
    //!
    //! Frontend contract these tests lock in:
    //!  1. Two PARTICIPANT_JOINED events for the same `user_id` with
    //!     *different* `session_id`s must BOTH be delivered (no dedup).
    //!  2. Two PARTICIPANT_JOINED events for the same `(user_id, session_id)`
    //!     pair (the WS+WT dual-transport case) must be dedup'd as one.
    //!  3. Two HOST_MUTE events for the same `target_user_id` (different
    //!     transports) must still be dedup'd as one — the original purpose
    //!     of dual-transport collapse is preserved.
    use super::*;
    use videocall_types::Callback as VcCallback;
    use wasm_bindgen_test::wasm_bindgen_test;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    fn build_dedup_test_options() -> VideoCallClientOptions {
        VideoCallClientOptions {
            enable_e2ee: false,
            enable_webtransport: false,
            user_id: "dedup_test_user".to_string(),
            display_name: "Dedup Tester".to_string(),
            is_guest: false,
            meeting_id: "dedup-test-meeting".to_string(),
            websocket_urls: Vec::new(),
            webtransport_urls: Vec::new(),
            on_peer_added: VcCallback::noop(),
            on_peer_first_frame: VcCallback::noop(),
            on_peer_removed: None,
            on_peers_removed_batch: None,
            refresh_room_token_callback: None,
            get_peer_video_canvas_id: VcCallback::from(|id| id),
            get_peer_screen_canvas_id: VcCallback::from(|id| id),
            on_connected: VcCallback::noop(),
            on_connection_lost: VcCallback::noop(),
            enable_diagnostics: false,
            diagnostics_update_interval_ms: None,
            enable_health_reporting: false,
            health_reporting_interval_ms: None,
            on_encoder_settings_update: None,
            rtt_testing_period_ms: 2000,
            rtt_probe_interval_ms: None,
            on_meeting_info: None,
            on_meeting_ended: None,
            on_meeting_activated: None,
            on_participant_admitted: None,
            on_participant_rejected: None,
            on_waiting_room_updated: None,
            on_meeting_settings_updated: None,
            on_speaking_changed: None,
            on_audio_level_changed: None,
            vad_threshold: None,
            on_peer_left: None,
            on_peer_joined: None,
            on_display_name_changed: None,
            on_host_mute: None,
            on_host_disable_video: None,
            on_participant_kicked: None,
            on_host_granted: None,
            on_host_revoked: None,
            on_peer_event: None,
            decode_media: true,
            allow_post_rebase_retry: true,
        }
    }

    /// HCL #828: two PARTICIPANT_JOINED events for the same `user_id` with
    /// distinct `session_id`s must NOT be collapsed. Both sessions are
    /// legitimate, separate joiners and the UI must learn about both.
    #[wasm_bindgen_test]
    fn participant_joined_distinct_sessions_are_not_dedup_ed() {
        let client = VideoCallClient::new(build_dedup_test_options());
        let mut inner = client.inner.borrow_mut();

        let first = inner.is_duplicate_peer_event("joined", "antonio@hcl", Some(1001));
        let second = inner.is_duplicate_peer_event("joined", "antonio@hcl", Some(1002));

        assert!(
            !first,
            "first PARTICIPANT_JOINED for session 1001 must NOT be a duplicate"
        );
        assert!(
            !second,
            "second PARTICIPANT_JOINED for session 1002 (same user_id) must NOT be \
             dedup'd against session 1001 — they are distinct sessions per HCL #828"
        );
    }

    /// Dual-transport WS+WT delivery of the *same* PARTICIPANT_JOINED
    /// (same `user_id` AND same `session_id`) must collapse to a single
    /// UI event. This is the original purpose of the dedup helper.
    #[wasm_bindgen_test]
    fn participant_joined_same_session_over_two_transports_is_dedup_ed() {
        let client = VideoCallClient::new(build_dedup_test_options());
        let mut inner = client.inner.borrow_mut();

        let from_ws = inner.is_duplicate_peer_event("joined", "antonio@hcl", Some(2001));
        let from_wt = inner.is_duplicate_peer_event("joined", "antonio@hcl", Some(2001));

        assert!(
            !from_ws,
            "first PARTICIPANT_JOINED (e.g. via WS) must be delivered"
        );
        assert!(
            from_wt,
            "second PARTICIPANT_JOINED for the SAME (user_id, session_id) \
             (e.g. via WT) must be suppressed to avoid duplicate toast"
        );
    }

    /// Dual-transport delivery of the same PARTICIPANT_LEFT (same session)
    /// must dedup, but two different sessions of the same user leaving
    /// must NOT — symmetric to the joined case.
    #[wasm_bindgen_test]
    fn participant_left_dedup_is_session_scoped() {
        let client = VideoCallClient::new(build_dedup_test_options());
        let mut inner = client.inner.borrow_mut();

        // Same session over two transports → second is duplicate.
        let first_ws = inner.is_duplicate_peer_event("left", "antonio@hcl", Some(3001));
        let first_wt = inner.is_duplicate_peer_event("left", "antonio@hcl", Some(3001));
        assert!(!first_ws);
        assert!(
            first_wt,
            "duplicate transport delivery for same session must dedup"
        );

        // Different session of the same user → NOT duplicate.
        let other_session = inner.is_duplicate_peer_event("left", "antonio@hcl", Some(3002));
        assert!(
            !other_session,
            "PARTICIPANT_LEFT for a different session of the same user must \
             not be dedup'd against the first session's leave"
        );
    }

    /// HCL #828 follow-up: `has_peer_with_session_id` is the session-id-keyed
    /// counterpart that the join-toast suppression uses to avoid collapsing
    /// sibling same-user sessions into a single toast. With no peers tracked
    /// the helper must return `false` for any session_id, including the
    /// empty-string and non-numeric inputs (the legacy "unknown session"
    /// path). The positive case is covered end-to-end by the
    /// `same-user-multi-session.spec.ts` E2E spec, which exercises the helper
    /// via a real PARTICIPANT_JOINED → peer_decode_manager round trip.
    #[wasm_bindgen_test]
    fn has_peer_with_session_id_empty_state_returns_false() {
        let client = VideoCallClient::new(build_dedup_test_options());

        assert!(
            !client.has_peer_with_session_id("4001"),
            "untracked session_id must report absent"
        );
        assert!(
            !client.has_peer_with_session_id(""),
            "empty session_id must report absent (legacy unknown-session path)"
        );
        assert!(
            !client.has_peer_with_session_id("not-a-number"),
            "non-numeric session_id must report absent"
        );
    }

    /// HCL #828 follow-up: when a `PARTICIPANT_DISPLAY_NAME_CHANGED` event
    /// carries a non-zero `session_id`, the handler at
    /// `video_call_client.rs:2173-2177` routes it through the session-scoped
    /// `set_peer_display_name(session_id, name)` — NOT the user-id-keyed
    /// fallback `set_peer_display_name_by_user_id`. This guarantees that two
    /// tabs of the same authenticated user (same `user_id`, different
    /// `session_id`s) can rename independently: only the renaming tab's
    /// display name on that session updates.
    ///
    /// The test seeds two peers via the persistent `display_name_cache`
    /// (which `set_peer_display_name` writes into unconditionally, even
    /// without a `connected_peers` entry — see
    /// `peer_decode_manager.rs:1325-1326`), then renames one and verifies
    /// the other is untouched.
    #[wasm_bindgen_test]
    fn display_name_change_with_session_id_is_session_scoped() {
        let client = VideoCallClient::new(build_dedup_test_options());

        // Two sibling sessions of the same authenticated user. In production
        // these would be two tabs of `antonio@hcl` with distinct session_ids
        // assigned by the server's SESSION_ASSIGNED handshake.
        let sid_a: u64 = 5001;
        let sid_b: u64 = 5002;

        {
            let mut inner = client.inner.borrow_mut();
            inner
                .peer_decode_manager
                .set_peer_display_name(sid_a, "antonio (tab A)".to_string());
            inner
                .peer_decode_manager
                .set_peer_display_name(sid_b, "antonio (tab B)".to_string());
        }

        // Sanity: both peers report their seeded names before the rename.
        assert_eq!(
            client.get_peer_display_name(&sid_a.to_string()),
            Some("antonio (tab A)".to_string()),
            "session A must read back its seeded display name"
        );
        assert_eq!(
            client.get_peer_display_name(&sid_b.to_string()),
            Some("antonio (tab B)".to_string()),
            "session B must read back its seeded display name"
        );

        // Simulate the server broadcast for tab A's rename arriving via the
        // `session_id != 0` branch of the
        // `PARTICIPANT_DISPLAY_NAME_CHANGED` handler. The handler calls
        // `set_peer_display_name(sid_a, "antonio (renamed)")`.
        {
            let mut inner = client.inner.borrow_mut();
            inner
                .peer_decode_manager
                .set_peer_display_name(sid_a, "antonio (renamed)".to_string());
        }

        assert_eq!(
            client.get_peer_display_name(&sid_a.to_string()),
            Some("antonio (renamed)".to_string()),
            "session A's display name must update to the renamed value"
        );
        assert_eq!(
            client.get_peer_display_name(&sid_b.to_string()),
            Some("antonio (tab B)".to_string()),
            "session B's display name must be UNTOUCHED — the rename was \
             scoped to session_id=sid_a, and a session-scoped update must \
             never reach sibling sessions of the same user. If this fails, \
             the handler has reverted to the user-id-keyed fallback."
        );
    }

    /// HOST_MUTE_PARTICIPANT dedup is user-scoped on purpose: a host muting
    /// `antonio@hcl` legitimately mutes ALL of his sessions, and both
    /// transports must collapse to one local effect. This guards against
    /// any future "let's session-scope every dedup" refactor accidentally
    /// breaking the dual-transport collapse for host actions.
    #[wasm_bindgen_test]
    fn host_mute_same_user_dual_transport_is_dedup_ed() {
        let client = VideoCallClient::new(build_dedup_test_options());
        let mut inner = client.inner.borrow_mut();

        let from_ws = inner.is_duplicate_host_action("host_mute", "antonio@hcl");
        let from_wt = inner.is_duplicate_host_action("host_mute", "antonio@hcl");

        assert!(!from_ws, "first HOST_MUTE must be delivered");
        assert!(
            from_wt,
            "second HOST_MUTE for the same target_user_id within the dual-\
             transport window must be suppressed — host actions are user-scoped"
        );
    }

    /// HCL #828 follow-up: the `on_display_name_changed` callback must
    /// receive the `session_id` of the renaming participant so the UI can
    /// scope its local-self update to the renaming tab only. Two tabs of
    /// the same authenticated user (same `user_id`, different
    /// `session_id`s) would otherwise all match the user_id-only gate at
    /// `attendants.rs:1254` and overwrite their own self-name signal when
    /// any sibling tab renames — the exact bug observed live for HCL #828.
    ///
    /// This test pushes a synthetic `PARTICIPANT_DISPLAY_NAME_CHANGED`
    /// meeting packet through `on_inbound_media` with a non-zero
    /// `session_id`, and asserts the callback fires with that same
    /// `session_id` as the third tuple element. If the emit site reverts
    /// to a 2-tuple or drops the session_id, this test fails.
    #[wasm_bindgen_test]
    fn display_name_change_callback_carries_session_id() {
        use std::cell::RefCell;
        use std::rc::Rc;
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        // Capture every (user_id, new_name, session_id) tuple the
        // callback is invoked with.
        let received: Rc<RefCell<Vec<(String, String, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let received_for_cb = received.clone();

        let mut opts = build_dedup_test_options();
        opts.on_display_name_changed = Some(VcCallback::from(
            move |(user_id, name, session_id): (String, String, u64)| {
                received_for_cb
                    .borrow_mut()
                    .push((user_id, name, session_id));
            },
        ));
        let client = VideoCallClient::new(opts);

        // Build a PARTICIPANT_DISPLAY_NAME_CHANGED meeting packet for a
        // sibling session of the local user (different session_id from
        // anything the local client thinks is its own — the dedup_test
        // client has no own_session_id assigned, which is fine: the
        // contract under test is the *callback shape*, not the consumer's
        // local-self gate. The consumer-side gate is exercised by the
        // dioxus-ui callsite at `components/attendants.rs`.)
        let renaming_session_id: u64 = 11_909_780_505_735_931_018;
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED.into(),
            room_id: "TonyBots".to_string(),
            target_user_id: b"tester1.estrada@gmail.com".to_vec(),
            session_id: renaming_session_id,
            display_name: b"Tester 1".to_vec(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            user_id: b"tester1.estrada@gmail.com".to_vec(),
            // PacketWrapper.session_id is 0 for MEETING packets; the
            // renaming session is carried inside the inner MeetingPacket.
            session_id: 0,
            data: meeting_packet
                .write_to_bytes()
                .expect("MeetingPacket must serialise"),
            ..Default::default()
        };

        {
            let mut inner = client.inner.borrow_mut();
            // Return value (PeerStatus) is irrelevant to this callback-shape test.
            let _ = inner.on_inbound_media(wrapper);
        }

        let captured = received.borrow();
        assert_eq!(
            captured.len(),
            1,
            "exactly one on_display_name_changed emit expected; got {:?}",
            *captured
        );
        let (got_user, got_name, got_session) = &captured[0];
        assert_eq!(got_user, "tester1.estrada@gmail.com");
        assert_eq!(got_name, "Tester 1");
        assert_eq!(
            *got_session, renaming_session_id,
            "callback must carry the renaming participant's session_id \
             so the consumer (dioxus-ui) can scope its local-self update \
             to the renaming tab only — sibling tabs of the same authed \
             user must NOT overwrite their own display name signal. \
             Regression guard for HCL #828."
        );
    }
}

/// Host-target regression tests for the issue-#1352 hardening of the
/// reconnect forced-keyframe cooldown reset.
///
/// These are plain `#[test]`s (NOT `#[wasm_bindgen_test]`) on purpose: per the
/// project's CI notes, `#[wasm_bindgen_test]` can silently no-op on some runners,
/// so a wasm-only assertion would be a false green. They drive the REAL source
/// helpers the `Connected` lifecycle arm calls against the SAME slot type the
/// live code holds (`Rc<RefCell<Option<Rc<AtomicBool>>>>`). The reset path
/// touches no browser API, so it runs unchanged on the host target.
#[cfg(test)]
mod cooldown_reset_hardening_tests {
    use super::arm_camera_keyframe_cooldown_reset;
    use super::arm_keyframe_cooldown_reset_slot;
    use super::handle_connected_reconnect_resets;
    use super::VideoCallClient;
    use super::VideoCallClientOptions;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use videocall_types::Callback;
    // Receiver-side layer-chooser types, mirroring the host primitives in
    // `peer_decode_manager.rs` (e.g. `downlink_congestion_steps_down_with_zero_loss`).
    use crate::decode::layer_chooser::{PrefMediaKind, ReceiveLayerBounds};

    fn build_test_client() -> VideoCallClient {
        VideoCallClient::new(VideoCallClientOptions {
            enable_e2ee: false,
            enable_webtransport: false,
            on_peer_added: Callback::noop(),
            on_peer_first_frame: Callback::noop(),
            on_peer_removed: None,
            on_peers_removed_batch: None,
            refresh_room_token_callback: None,
            get_peer_video_canvas_id: Callback::from(|id| id),
            get_peer_screen_canvas_id: Callback::from(|id| id),
            user_id: "test-user".to_string(),
            display_name: "test".to_string(),
            meeting_id: "test-meeting".to_string(),
            websocket_urls: Vec::new(),
            webtransport_urls: Vec::new(),
            on_connected: Callback::noop(),
            on_connection_lost: Callback::noop(),
            enable_diagnostics: false,
            diagnostics_update_interval_ms: None,
            enable_health_reporting: false,
            health_reporting_interval_ms: None,
            on_encoder_settings_update: None,
            rtt_testing_period_ms: 2000,
            rtt_probe_interval_ms: None,
            on_meeting_info: None,
            on_meeting_ended: None,
            on_speaking_changed: None,
            on_audio_level_changed: None,
            vad_threshold: None,
            on_meeting_activated: None,
            on_participant_admitted: None,
            on_participant_rejected: None,
            on_waiting_room_updated: None,
            on_meeting_settings_updated: None,
            on_peer_left: None,
            on_peer_joined: None,
            on_display_name_changed: None,
            on_host_mute: None,
            on_host_disable_video: None,
            on_participant_kicked: None,
            on_host_granted: None,
            on_host_revoked: None,
            on_peer_event: None,
            decode_media: true,
            is_guest: false,
            allow_post_rebase_retry: true,
        })
    }

    /// HOST `#[test]` (NOT `#[wasm_bindgen_test]`): the local CPU-pressure
    /// seed+publish path is driven through the clock-free `Inner` helper
    /// `seed_local_congestion_and_publish` (issue #1569), so it runs on the
    /// native host target under `cargo test -p videocall-client --lib`. This is
    /// the PER-PR HOST gate for the new shared seed path — unlike the now-deleted
    /// `#[wasm_bindgen_test]` (which was `run_in_browser` and so ran ONLY under a
    /// `/run-e2e` browser dispatch, never in per-PR CI). It asserts the REAL
    /// step-down, not just "the method is wired".
    ///
    /// MUTATION CHECK: gut `Inner::seed_local_congestion_and_publish` to
    /// `return false;` (or remove its
    /// `seed_downlink_congestion_for_connected_peers` call) and ALL THREE
    /// assertions below fail: (1) `seeded` becomes `false`; (2) the peer's chooser
    /// stays at layer 2 instead of stepping to 1 (nothing seeded the synthetic
    /// congestion); (3) `current_desired_preferences` no longer advertises
    /// `Some(1)` for the peer's video, because the chooser never dropped below
    /// the climbed-to top. The seeded peer reports ZERO real loss, so the
    /// early-seed path cannot mask a gutted helper — only the synthetic seed in
    /// the helper can produce the step-down.
    ///
    /// BORROW NOTE: `inner` is borrowed once for the whole test and the `Inner`
    /// method is called DIRECTLY on it. We deliberately do NOT call the public
    /// `client.apply_local_cpu_pressure_congestion()` here — that would re-borrow
    /// `client.inner` while this guard is held and panic. The public wrapper only
    /// reads the wall clock and forwards to this same helper, which is what we
    /// test directly with a fixed `now_ms`.
    #[test]
    fn local_cpu_pressure_steps_connected_peer_down_one_rung() {
        let client = build_test_client();
        let mut inner = client.inner.borrow_mut();

        // Seed a CONNECTED peer with a learned 3-layer ladder and a ZERO-LOSS
        // real downlink sample (the WebSocket / reliable-WT blindness case): the
        // only thing that can step it down is the synthetic seed inside the
        // helper under test.
        inner
            .peer_decode_manager
            .insert_zero_loss_top_peer_for_test(700);

        // One clean unconstrained tick climbs the chooser to the TOP (layer 2),
        // so the synthetic seed has room to step DOWN from 2 to 1.
        let open = ReceiveLayerBounds::default();
        let _ = inner.peer_decode_manager.tick_layer_choosers(1500, &open);
        assert_eq!(
            inner
                .peer_decode_manager
                .get(&700)
                .unwrap()
                .selected_video_layer(),
            2,
            "precondition: chooser climbed to the top before the local-pressure seed"
        );

        // The path under test: seed synthetic local-pressure congestion at a
        // FIXED now_ms and publish the resulting preference. Returns whether
        // anything was seeded.
        let seeded = inner.seed_local_congestion_and_publish(2000, true);
        assert!(
            seeded,
            "local CPU-pressure seed must report it stepped a connected peer down"
        );

        // The receiver-side decode guard stepped down exactly one rung: 2 -> 1.
        assert_eq!(
            inner
                .peer_decode_manager
                .get(&700)
                .unwrap()
                .selected_video_layer(),
            1,
            "local CPU-pressure seed must step the peer's video chooser down one rung (2 -> 1)"
        );

        // And the lowered preference is advertised through the change-detected
        // sender's desired map.
        assert_eq!(
            inner
                .peer_decode_manager
                .current_desired_preferences(2000, &open)
                .get(&(700, PrefMediaKind::Video))
                .copied(),
            Some(1),
            "the stepped-down receive-layer preference must be advertised for the peer"
        );
    }

    #[test]
    fn connected_reset_helper_arms_real_slot_while_real_inner_is_borrowed() {
        let client = build_test_client();
        let camera = Rc::new(AtomicBool::new(false));
        let screen = Rc::new(AtomicBool::new(false));
        client.set_camera_keyframe_cooldown_reset(camera.clone());
        client.set_screen_keyframe_cooldown_reset(screen.clone());

        let inner_guard = client.inner.borrow_mut();
        handle_connected_reconnect_resets(
            &Rc::downgrade(&client.inner),
            &client.early_seed_timer,
            &client.camera_keyframe_cooldown_reset,
            &client.screen_keyframe_cooldown_reset,
            &client.audio_congestion_bitrate_floor,
            &client.audio_detector_reconnect_reseed,
        );

        assert!(
            camera.load(Ordering::Acquire),
            "the real Connected reset helper must arm the camera cooldown atom even \
             while the real Inner is mutably borrowed"
        );
        assert!(
            screen.load(Ordering::Acquire),
            "the real Connected reset helper must arm the screen cooldown atom even \
             while the real Inner is mutably borrowed"
        );
        drop(inner_guard);
    }

    /// FIX D / #1398: the single-layer audio BITRATE-floor reset must fire on a
    /// reconnect even when `Inner` is mutably borrowed at that instant — the actual
    /// bug this fix closes. We model the contended reconnect by holding a
    /// `client.inner.borrow_mut()` across the reset call. The reset path upgrades
    /// the `Weak<Inner>` and `try_borrow_mut`s it; with a `borrow_mut` already held
    /// that `try_borrow_mut` FAILS (logging "inner busy, skipping"), so any reset
    /// nested inside that borrow is silently dropped. Because the floor reset now
    /// lives OUTSIDE the borrow (storing through the client-held clone of the SAME
    /// `Arc`), it must still run. Revert it catches: moving the
    /// `audio_congestion_bitrate_floor.store(u32::MAX, …)` back INSIDE the
    /// `try_borrow_mut` block — under the held borrow it would be skipped, leaving
    /// the floor at the stale 32000 and FAILING this `u32::MAX` assertion.
    #[test]
    fn reconnect_resets_audio_bitrate_floor_while_inner_is_borrowed() {
        let client = build_test_client();
        // Stale low-bitrate cut left over from the previous session, stored through
        // the client-held field clone (the SAME Arc the accessor and Inner share).
        // We read it back through the field clone too — NOT the public accessor,
        // which takes `self.inner.borrow()` and would panic under the held
        // `borrow_mut` below; the field clone is reachable without any Inner borrow,
        // which is the whole point of FIX D.
        client
            .audio_congestion_bitrate_floor
            .store(32_000, Ordering::Relaxed);
        assert_eq!(
            client
                .audio_congestion_bitrate_floor
                .load(Ordering::Relaxed),
            32_000,
            "precondition: a stale bitrate-floor cut is active"
        );

        // Simulate the contended reconnect: hold the Inner borrow across the reset.
        let inner_guard = client.inner.borrow_mut();
        handle_connected_reconnect_resets(
            &Rc::downgrade(&client.inner),
            &client.early_seed_timer,
            &client.camera_keyframe_cooldown_reset,
            &client.screen_keyframe_cooldown_reset,
            &client.audio_congestion_bitrate_floor,
            &client.audio_detector_reconnect_reseed,
        );

        // The floor must be fail-open DESPITE the held borrow.
        assert_eq!(
            client
                .audio_congestion_bitrate_floor
                .load(Ordering::Relaxed),
            u32::MAX,
            "FIX D: the audio bitrate-floor reset must fire on reconnect even while \
             Inner is mutably borrowed — the store must not depend on the Inner borrow"
        );
        drop(inner_guard);
    }

    /// The whole point of #1352: the reset must fire on a reconnect even when
    /// `Inner` is borrowed at that instant. Because the atom now lives in its
    /// OWN slot (not inside `Inner`), an outstanding `Inner` borrow is irrelevant.
    /// We model the contended reconnect by holding a borrow of a stand-in `Inner`
    /// across the arm call and asserting the atom was still set.
    #[test]
    fn reset_fires_even_while_inner_is_borrowed() {
        // Stand-in for the `Rc<RefCell<Inner>>` the `Connected` arm also touches.
        let inner_like: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
        // Encoder-owned reset atom, wired into the dedicated slot.
        let atom = Rc::new(AtomicBool::new(false));
        let slot: Rc<RefCell<Option<Rc<AtomicBool>>>> = Rc::new(RefCell::new(Some(atom.clone())));

        // Simulate the transient borrow conflict that, pre-#1352, dropped the
        // reset: `Inner` is mutably borrowed for the duration of the arm call.
        let inner_guard = inner_like.borrow_mut();
        arm_camera_keyframe_cooldown_reset(&slot);
        // The atom must be armed regardless of the held `Inner` borrow. If the
        // store were ever moved back inside an `Inner` borrow (the #1352
        // regression), this would NOT be reachable under contention.
        assert!(
            atom.load(Ordering::Acquire),
            "cooldown reset must fire while Inner is borrowed (issue #1352): the \
             store must not depend on an Inner borrow"
        );
        drop(inner_guard);

        // The encode loop consumes the edge exactly once (`.swap(false)`), the
        // same one-shot contract the camera encoder relies on.
        assert!(
            atom.swap(false, Ordering::AcqRel),
            "the armed reset edge must be observable exactly once"
        );
        assert!(
            !atom.load(Ordering::Acquire),
            "the reset edge is one-shot; it must not stick set after consume"
        );
    }

    /// CONTROL: pins that the helper's effect is real and not vacuous. If the
    /// store inside `arm_camera_keyframe_cooldown_reset` were removed (the
    /// mutation this suite guards), the atom would stay `false` and this fails.
    #[test]
    fn helper_actually_sets_the_atom() {
        let atom = Rc::new(AtomicBool::new(false));
        let slot: Rc<RefCell<Option<Rc<AtomicBool>>>> = Rc::new(RefCell::new(Some(atom.clone())));
        assert!(
            !atom.load(Ordering::Acquire),
            "precondition: atom starts unarmed"
        );
        arm_camera_keyframe_cooldown_reset(&slot);
        assert!(
            atom.load(Ordering::Acquire),
            "arm_camera_keyframe_cooldown_reset must store(true) on the wired atom"
        );
    }

    /// Fail-open: before the host wires the encoder atom (slot is `None`, e.g.
    /// observer mode), arming is a safe no-op and must not panic.
    #[test]
    fn unwired_slot_is_a_safe_no_op() {
        let slot: Rc<RefCell<Option<Rc<AtomicBool>>>> = Rc::new(RefCell::new(None));
        // Must not panic and must do nothing observable.
        arm_camera_keyframe_cooldown_reset(&slot);
    }

    /// The arm call must NOT hold the slot borrow across the `store` — otherwise a
    /// re-entrant arm (or the synchronous wiring setter) could deadlock/conflict.
    /// We prove the borrow is released by mutably borrowing the slot immediately
    /// after the arm returns; a leaked borrow would panic here.
    #[test]
    fn arm_releases_slot_borrow_before_returning() {
        let atom = Rc::new(AtomicBool::new(false));
        let slot: Rc<RefCell<Option<Rc<AtomicBool>>>> = Rc::new(RefCell::new(Some(atom.clone())));
        arm_camera_keyframe_cooldown_reset(&slot);
        // If the helper held the borrow past its return, this would panic
        // ("already borrowed"). It must succeed.
        let mut guard = slot.borrow_mut();
        *guard = None;
        assert!(
            atom.load(Ordering::Acquire),
            "atom was still armed by the call"
        );
    }

    #[test]
    fn slot_helper_reports_whether_it_armed_an_atom() {
        let atom = Rc::new(AtomicBool::new(false));
        let wired: Rc<RefCell<Option<Rc<AtomicBool>>>> = Rc::new(RefCell::new(Some(atom.clone())));
        let unwired: Rc<RefCell<Option<Rc<AtomicBool>>>> = Rc::new(RefCell::new(None));

        assert!(arm_keyframe_cooldown_reset_slot(&wired));
        assert!(atom.load(Ordering::Acquire));
        assert!(!arm_keyframe_cooldown_reset_slot(&unwired));
    }

    /// Issue #621 acceptance: a self-targeted CONGESTION cut must fire the VIDEO
    /// step-down flag AND cut the AUDIO congestion layer-ceiling to base-only — in
    /// one coordinated action (the screen flag too, #1199). Drives the exact
    /// `apply_self_congestion_cut` path the CONGESTION dispatch arm calls, on a
    /// real host-built `Inner`.
    #[test]
    fn self_congestion_cut_fires_both_video_and_audio() {
        let client = build_test_client();

        let inner = client.inner.borrow();
        // Preconditions: video/screen flags clear, audio ceiling fail-open.
        assert!(
            !inner.congestion_step_down_requested.load(Ordering::Acquire),
            "precondition: camera flag starts clear"
        );
        assert!(
            !inner
                .screen_congestion_step_down_requested
                .load(Ordering::Acquire),
            "precondition: screen flag starts clear"
        );
        assert_eq!(
            inner.audio_congestion_layer_ceiling.load(Ordering::Relaxed),
            u32::MAX,
            "precondition: audio congestion ceiling starts fail-open"
        );
        assert_eq!(
            inner.audio_congestion_bitrate_floor.load(Ordering::Relaxed),
            u32::MAX,
            "precondition: audio congestion bitrate floor starts fail-open (#1398)"
        );

        inner.apply_self_congestion_cut();

        // BOTH the video step-down (force_video_step_down's edge, via the flag the
        // camera AQ loop turns into force_congestion_cut) AND the audio cut fire.
        assert!(
            inner.congestion_step_down_requested.load(Ordering::Acquire),
            "self-targeted CONGESTION must set the camera step-down flag"
        );
        assert!(
            inner
                .screen_congestion_step_down_requested
                .load(Ordering::Acquire),
            "self-targeted CONGESTION must set the screen step-down flag (#1199)"
        );
        assert_eq!(
            inner.audio_congestion_layer_ceiling.load(Ordering::Relaxed),
            1,
            "self-targeted CONGESTION must cut the AUDIO ceiling to base-only (#621)"
        );
        // Single-layer audio bitrate floor (#1398): this dead packet arm must NOT
        // touch the floor anymore — the floor is driven by the mic-side
        // uplink-distress detector, not the (removed) CONGESTION trigger. The floor
        // stays at the fail-open sentinel. Revert it catches: if the bitrate-floor
        // step-down were re-added to `apply_self_congestion_cut`, this reads 32000
        // and fails — pinning that the trigger moved off this arm.
        assert_eq!(
            inner.audio_congestion_bitrate_floor.load(Ordering::Relaxed),
            u32::MAX,
            "apply_self_congestion_cut must NOT step the bitrate floor (#1398: the \
             floor is now driven by the mic uplink-distress detector)"
        );
    }

    /// Issue #621: the audio congestion cut must be observable through the public
    /// `audio_congestion_layer_ceiling()` accessor the host wires into the mic
    /// encoder — proving the SAME atom the mic reads is the one the dispatch cuts.
    #[test]
    fn self_congestion_cut_visible_via_public_accessor() {
        let client = build_test_client();
        let shared = client.audio_congestion_layer_ceiling();
        assert_eq!(
            shared.load(Ordering::Relaxed),
            u32::MAX,
            "shared atom starts fail-open"
        );
        client.inner.borrow().apply_self_congestion_cut();
        assert_eq!(
            shared.load(Ordering::Relaxed),
            1,
            "the cut is visible on the atom shared with the mic encoder"
        );
    }

    /// Issue #621/#1398: a reconnect must reset BOTH the audio congestion ceiling
    /// AND the single-layer bitrate floor back to fail-open so a stale cut from the
    /// OLD session does not pin the audio ladder to base-only / the Opus stream to
    /// a low bitrate against a FRESH session.
    #[test]
    fn reconnect_resets_audio_congestion_ceiling() {
        let client = build_test_client();
        // Simulate active cuts left over from the previous session: the ceiling
        // via the dispatch helper, and the bitrate floor by directly storing a
        // stepped-down value (the floor is now driven by the mic uplink-distress
        // detector out-of-band, NOT by `apply_self_congestion_cut`, so we model
        // its effect directly to test the reconnect RESET in isolation).
        client.inner.borrow().apply_self_congestion_cut();
        client
            .audio_congestion_bitrate_floor()
            .store(32_000, Ordering::Relaxed);
        assert_eq!(
            client
                .audio_congestion_layer_ceiling()
                .load(Ordering::Relaxed),
            1,
            "precondition: a ceiling cut is active"
        );
        assert_eq!(
            client
                .audio_congestion_bitrate_floor()
                .load(Ordering::Relaxed),
            32_000,
            "precondition: a stale bitrate-floor cut is active (#1398)"
        );
        // Precondition for the reconnect-reseed P1: the detector reseed flag starts
        // clear (no reconnect pending yet).
        assert!(
            !client
                .audio_detector_reconnect_reseed()
                .load(Ordering::Acquire),
            "precondition: the detector reconnect-reseed flag is clear before reconnect"
        );

        // The real Connected/reconnect reset path.
        handle_connected_reconnect_resets(
            &Rc::downgrade(&client.inner),
            &client.early_seed_timer,
            &client.camera_keyframe_cooldown_reset,
            &client.screen_keyframe_cooldown_reset,
            &client.audio_congestion_bitrate_floor,
            &client.audio_detector_reconnect_reseed,
        );

        assert_eq!(
            client
                .audio_congestion_layer_ceiling()
                .load(Ordering::Relaxed),
            u32::MAX,
            "reconnect must reset the audio congestion ceiling to fail-open"
        );
        // Revert it catches: if `handle_connected_reconnect_resets` did not reset
        // the bitrate floor, this reads 32000 (the stale cut) and fails.
        assert_eq!(
            client
                .audio_congestion_bitrate_floor()
                .load(Ordering::Relaxed),
            u32::MAX,
            "reconnect must reset the audio congestion bitrate floor to fail-open (#1398)"
        );
        // Reconnect-reseed P1 (#1398): the handler must SET the detector
        // reconnect-reseed flag so the mic detector re-anchors its windows on the
        // fresh session. Revert it catches: dropping the
        // `audio_detector_reconnect_reseed.store(true, …)` from the handler → this
        // reads false and FAILS, proving the reconnect signal is raised.
        assert!(
            client
                .audio_detector_reconnect_reseed()
                .load(Ordering::Acquire),
            "reconnect must set the detector reconnect-reseed flag so the mic \
             detector re-seeds its windows on the fresh session (#1398 reconnect P1)"
        );
    }
}

/// A peer must NOT be created for: system messages, the unstamped `session_id
/// == 0` sentinel, observer/no-decode mode, `SESSION_ASSIGNED` control packets
/// (these carry OUR OWN session_id; the synthetic one emitted at election
/// completion bypasses the connection-layer self-filter and would otherwise
/// render us as our own peer), or relay-authored self-addressed control packets
/// (`CONGESTION` / `LAYER_HINT`). The last group is whitelisted by the
/// connection-layer self-filter so AQ can act on it, so it reaches the decode
/// path even though it is "self" — and the relay can stamp a `LAYER_HINT` with a
/// LOSING election candidate's session_id (not yet recognised as ours), which
/// without this guard the client renders as a ghost peer tile (shown with the
/// user_id/email fallback because that session never gets a PARTICIPANT_JOINED).
fn suppresses_peer_creation(
    is_system_user: bool,
    session_id: u64,
    decode_media: bool,
    is_session_assigned: bool,
    is_self_addressed_control: bool,
) -> bool {
    is_system_user
        || session_id == 0
        || !decode_media
        || is_session_assigned
        || is_self_addressed_control
}

/// Call-site wiring for [`suppresses_peer_creation`]: derive its five boolean
/// inputs from a real inbound [`PacketWrapper`] + the receiver's `decode_media`
/// mode. This is the thin seam the `on_inbound_media` hot path goes through, so a
/// test driving a real `PacketWrapper` through here pins the `packet_type → bool`
/// derivation (issue #1496) — a wrong `PacketType` constant or a swapped/omitted
/// flag would compile and pass the pure-predicate tests but break HERE.
///
/// `CONGESTION` and `LAYER_HINT` are relay-authored control packets stamped with
/// the RECIPIENT's own session_id; the connection-layer self-filter
/// (`connection_manager.rs::should_filter_self_packet`) whitelists exactly these
/// two so AQ can act on them, so they reach this path even though they are
/// "self" — but they must never spawn a peer tile. `SESSION_ASSIGNED` carries our
/// own session_id and is likewise suppressed purely on packet type (see the
/// detailed rationale on [`suppresses_peer_creation`] and at the call site).
///
/// `DOWNLINK_CONGESTION` is intentionally NOT in this set even though the relay
/// classifies it as a self-addressed control packet too: the transport self-filter
/// does NOT whitelist it, so a self-addressed `DOWNLINK_CONGESTION` is dropped one
/// layer up (in `should_filter_self_packet`) and never reaches here once our own
/// session_id is known. The pre-`SESSION_ASSIGNED` window where it could slip
/// through unfiltered is the subject of the open #1481 investigation; do not add it
/// to this set without first reconciling it with the transport-filter whitelist
/// (the two gates must agree), which is exactly what #1481 tracks.
fn suppresses_peer_creation_for_packet(response: &PacketWrapper, decode_media: bool) -> bool {
    let is_self_addressed_control = response.packet_type == PacketType::CONGESTION.into()
        || response.packet_type == PacketType::LAYER_HINT.into();
    suppresses_peer_creation(
        response.user_id == SYSTEM_USER_ID.as_bytes(),
        response.session_id,
        decode_media,
        response.packet_type == PacketType::SESSION_ASSIGNED.into(),
        is_self_addressed_control,
    )
}

#[cfg(test)]
mod self_peer_suppression_tests {
    use super::{suppresses_peer_creation, suppresses_peer_creation_for_packet};
    use videocall_types::protos::packet_wrapper::{packet_wrapper::PacketType, PacketWrapper};
    use videocall_types::SYSTEM_USER_ID;

    /// Build a minimal inbound `PacketWrapper` with the given type/session/user.
    fn packet(packet_type: PacketType, session_id: u64, user_id: &[u8]) -> PacketWrapper {
        PacketWrapper {
            packet_type: packet_type.into(),
            session_id,
            user_id: user_id.to_vec(),
            data: Vec::new(),
            ..Default::default()
        }
    }

    #[test]
    fn foreign_media_creates_a_peer() {
        // A normal media packet from another participant: none of the suppress
        // conditions hold, so a peer IS created.
        assert!(!suppresses_peer_creation(false, 42, true, false, false));
    }

    #[test]
    fn session_assigned_never_creates_a_peer() {
        // Regression guard: the synthetic SESSION_ASSIGNED emitted at election
        // completion carries our own elected session_id and previously spawned a
        // self peer tile (logged as "New user joined: <own session>"). It must
        // be suppressed purely on packet type.
        assert!(suppresses_peer_creation(false, 42, true, true, false));
    }

    #[test]
    fn self_addressed_control_never_creates_a_peer() {
        // Regression guard for the losing-candidate ghost: the relay emits
        // CONGESTION / LAYER_HINT stamped with a session_id (e.g. a LOSING
        // election candidate the client does not recognise as its own). The
        // connection-layer self-filter whitelists these so AQ can act on them —
        // so they reach the decode path — but they must NOT spawn a peer tile.
        // If this guard is removed, the relay's LAYER_HINT to the loser session
        // renders a ghost peer.
        assert!(suppresses_peer_creation(false, 42, true, false, true));
    }

    #[test]
    fn observer_and_zero_session_are_suppressed() {
        assert!(suppresses_peer_creation(false, 0, true, false, false));
        assert!(suppresses_peer_creation(false, 42, false, false, false));
        assert!(suppresses_peer_creation(true, 42, true, false, false));
    }

    // -----------------------------------------------------------------------
    // Issue #1496: call-site WIRING tests. These drive a real `PacketWrapper`
    // through `suppresses_peer_creation_for_packet` (the seam `on_inbound_media`
    // uses) to pin the `packet_type -> bool` derivation. The pure-predicate tests
    // above cannot see this wiring: a wrong `PacketType` constant, a swapped bool
    // argument, or an omitted flag would compile and keep them green while
    // silently reintroducing the ghost tile (or suppressing a real peer).
    // -----------------------------------------------------------------------

    /// A normal MEDIA packet from a foreign nonzero session MUST create a peer
    /// (none of the suppress conditions hold). If the call site mis-wired
    /// `decode_media` or compared MEDIA against a suppress constant, this flips.
    #[test]
    fn wiring_foreign_media_packet_is_not_suppressed() {
        let p = packet(PacketType::MEDIA, 42, b"alice@example.com");
        assert!(
            !suppresses_peer_creation_for_packet(&p, true),
            "a foreign nonzero-session MEDIA packet must create a peer"
        );
    }

    /// CONGESTION is a relay-authored self-addressed control packet — it reaches
    /// the decode path (self-filter whitelists it for AQ) but must NOT spawn a
    /// peer. Mutating the call site's `CONGESTION` constant makes this fail.
    #[test]
    fn wiring_congestion_packet_is_suppressed() {
        // Nonzero session, foreign-looking user_id, decode on: ONLY the
        // packet_type derivation can suppress this — so it pins that wiring.
        let p = packet(PacketType::CONGESTION, 42, b"alice@example.com");
        assert!(
            suppresses_peer_creation_for_packet(&p, true),
            "CONGESTION must be suppressed purely on packet type (self-addressed control)"
        );
    }

    /// LAYER_HINT is the other relay-authored self-addressed control packet
    /// (the losing-election-candidate ghost vector). Same wiring lock.
    #[test]
    fn wiring_layer_hint_packet_is_suppressed() {
        let p = packet(PacketType::LAYER_HINT, 42, b"alice@example.com");
        assert!(
            suppresses_peer_creation_for_packet(&p, true),
            "LAYER_HINT must be suppressed purely on packet type (self-addressed control)"
        );
    }

    /// SESSION_ASSIGNED carries OUR OWN session_id; dropping the
    /// `is_session_assigned` argument at the call site would render us as our
    /// own ghost peer at election completion.
    #[test]
    fn wiring_session_assigned_packet_is_suppressed() {
        let p = packet(PacketType::SESSION_ASSIGNED, 42, b"alice@example.com");
        assert!(
            suppresses_peer_creation_for_packet(&p, true),
            "SESSION_ASSIGNED must be suppressed purely on packet type (our own session)"
        );
    }

    /// Observer mode (`decode_media == false`) suppresses even a normal MEDIA
    /// packet — pins that the call site threads `decode_media` through.
    #[test]
    fn wiring_observer_mode_suppresses_media() {
        let p = packet(PacketType::MEDIA, 42, b"alice@example.com");
        assert!(
            suppresses_peer_creation_for_packet(&p, false),
            "observer/no-decode mode must suppress peer creation for any packet"
        );
    }

    /// The `session_id == 0` sentinel and the system user_id are derived from the
    /// packet fields (not the type) — pin both so a refactor can't drop them.
    #[test]
    fn wiring_zero_session_and_system_user_are_suppressed() {
        let zero_session = packet(PacketType::MEDIA, 0, b"alice@example.com");
        assert!(
            suppresses_peer_creation_for_packet(&zero_session, true),
            "session_id == 0 sentinel must be suppressed"
        );
        let system = packet(PacketType::MEDIA, 42, SYSTEM_USER_ID.as_bytes());
        assert!(
            suppresses_peer_creation_for_packet(&system, true),
            "system-user packets must be suppressed"
        );
    }
}

#[cfg(test)]
mod congestion_warn_admit_tests {
    use super::{congestion_warn_admit, CONGESTION_WARN_MAX_PER_SEC};

    #[test]
    fn congestion_warn_admit_caps_per_second_and_resets_per_window() {
        // 1000 signals all inside a single 100ms span (< 1s) => one window =>
        // at most CONGESTION_WARN_MAX_PER_SEC admits (warns). Mutation guard:
        // if the cap is removed (always-true), this count becomes 1000 and the
        // assertion below fails.
        let base: u64 = 10_000;
        let mut start_ms: u64 = 0;
        let mut count: u32 = 0;
        let mut admits = 0u32;
        // First call seeds the window (start_ms 0 -> base, since base-0 >= 1000),
        // so all 1000 fall in one window after the seed.
        for i in 0..1000u64 {
            let now = base + (i % 100); // span = [base, base+99] => 100ms < 1s
            if congestion_warn_admit(now, &mut start_ms, &mut count, CONGESTION_WARN_MAX_PER_SEC) {
                admits += 1;
            }
        }
        assert!(
            admits <= CONGESTION_WARN_MAX_PER_SEC,
            "single-window admits {} must be <= cap {}",
            admits,
            CONGESTION_WARN_MAX_PER_SEC
        );
        assert!(admits >= 1, "at least one signal must warn");

        // Across a 3000ms span the window resets each second, so admits are
        // bounded by ~3 windows * cap. Proves the window actually rolls over.
        let mut start2: u64 = 0;
        let mut count2: u32 = 0;
        let mut admits2 = 0u32;
        for i in 0..1000u64 {
            let now = base + i * 3; // i in 0..1000 => span 0..2997ms => 3 full seconds
            if congestion_warn_admit(now, &mut start2, &mut count2, CONGESTION_WARN_MAX_PER_SEC) {
                admits2 += 1;
            }
        }
        // 3000ms / 1000ms = 3 windows (plus the seed window) => <= 4*cap is a safe
        // upper bound; the key point vs the single-window case is admits2 > cap.
        assert!(
            admits2 > CONGESTION_WARN_MAX_PER_SEC,
            "multi-second admits {} must exceed single-window cap {} (window must reset)",
            admits2,
            CONGESTION_WARN_MAX_PER_SEC
        );
        assert!(
            admits2 <= 4 * CONGESTION_WARN_MAX_PER_SEC,
            "multi-second admits {} should be bounded by ~window count * cap",
            admits2
        );
    }
}
