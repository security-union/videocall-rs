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

//! Centralized Prometheus metrics for the videocall API

use actix_web::{HttpResponse, Responder};
use lazy_static::lazy_static;
use prometheus::{
    register_counter, register_counter_vec, register_gauge_vec, register_histogram,
    register_histogram_vec, Counter, CounterVec, Encoder, GaugeVec, Histogram, HistogramVec,
};

/// Shared Prometheus metrics HTTP handler for relay server binaries.
pub async fn metrics_responder() -> impl Responder {
    let encoder = prometheus::TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(()) => HttpResponse::Ok()
            .content_type("text/plain; version=0.0.4")
            .body(buffer),
        Err(e) => {
            HttpResponse::InternalServerError().body(format!("Failed to encode metrics: {e}"))
        }
    }
}

// =============================================================================
// Bounded relay label taxonomies (single source of truth for cardinality GC)
// =============================================================================
//
// `room` (= meeting_id) is user-provided, so every room-labeled CounterVec
// would otherwise accrue a permanent series for every meeting the process ever
// served (issue #996). We do NOT drop the `room` label — the
// meeting-investigation Grafana dashboard and the `RelayPacketDrops` alert key
// on it (`relay_packet_drops_total{room=~"$meeting"}`, `{{ $labels.room }}`),
// so removing it would be a breaking change to operational tooling. Instead we
// BOUND the live series to the set of LIVE ROOMS: `chat_server` calls
// [`forget_room_metrics`] when a room drains to empty (the same room-drain hook
// that already removed the `relay_viewport_set_size` gauge for #988), removing
// every room-labeled series for that room. The prometheus `MetricVec` removal
// API requires the FULL label tuple (it hashes every variable label — there is
// no partial-match removal), so for multi-label counters we must enumerate the
// cartesian product of the OTHER labels' fixed taxonomies. These consts are
// that authoritative enumeration; `metrics::tests` cross-checks they cover the
// labels the code actually emits so the bound cannot silently leak.

/// Every `drop_reason`/`kind` label ever passed to a relay drop counter.
///
/// This is the UNION of:
/// - `relay_packet_drops_total{drop_reason}` (`mailbox_full`, `channel_full`,
///   `priority_drop_video`, `priority_drop_audio`), and
/// - `relay_session_drops_total{kind}` / `videocall_outbound_channel_drops_total{kind}`
///   (`audio`, `video`, `screen`, `media`, `control`, `rtt`, `unknown`,
///   `priority_drop_video`, `priority_drop_audio`, `overflow_critical`).
///
/// Iterating the full union for either counter's GC is leak-proof: removing a
/// `(…, kind)` tuple that was never created is a benign `Err` (issue #1090),
/// and removing the superset guarantees no residual series regardless of which
/// subset a given counter actually emits.
pub const RELAY_DROP_KINDS: &[&str] = &[
    // relay_packet_drops_total drop_reasons
    "mailbox_full",
    "channel_full",
    // shared priority-policy reasons (both counters)
    "priority_drop_video",
    "priority_drop_audio",
    // outbound/session drop kinds (drop_kind_label + overflow_critical)
    "audio",
    "video",
    "screen",
    "media",
    "control",
    "rtt",
    "unknown",
    "overflow_critical",
];

/// Every `transport` label value carried by a room-labeled relay counter.
///
/// `relay_packet_drops_total{transport}` is emitted with the receiver's
/// transport at the per-transport `Handler<Message>` hop (`websocket` /
/// `webtransport`) AND with the publish-side identity `nats_delivery` at the
/// inbound fan-out hop in `chat_server::handle_msg`.
pub const RELAY_DROP_TRANSPORTS: &[&str] = &["websocket", "webtransport", "nats_delivery"];

/// Every `outcome` label value of `relay_viewport_updates_total{room, outcome}`
/// (#988 `try_intercept_viewport`).
pub const RELAY_VIEWPORT_UPDATE_OUTCOMES: &[&str] = &[
    "accepted",
    "rate_limited",
    "truncated",
    "ignored_other_subject",
];

/// Every `outcome` label value of
/// `relay_layer_preference_updates_total{room, outcome}` (#989, #1082
/// `try_intercept_layer_preference`). Superset of the viewport outcomes plus
/// `layer_id_out_of_bound`.
pub const RELAY_LAYER_PREFERENCE_UPDATE_OUTCOMES: &[&str] = &[
    "accepted",
    "rate_limited",
    "truncated",
    "layer_id_out_of_bound",
    "ignored_other_subject",
];

/// Every `direction` label value of
/// `relay_layer_hint_emitted_total{room, direction}` (#1108 publish-side
/// suppression).
pub const RELAY_LAYER_HINT_DIRECTIONS: &[&str] = &["suppress", "restore"];

/// Remove EVERY room-labeled relay CounterVec/GaugeVec series for `room`.
///
/// Called by `chat_server` the moment a room drains to empty (see
/// `forget_room_if_empty` / `forget_session`), bounding the live series for
/// these `room`-labeled metrics to the set of currently-live rooms (issue
/// #996). Without this, each metric accrued a permanent series per distinct
/// meeting for the process lifetime.
///
/// `remove_label_values` errors only when no such series exists, so every call
/// here is intentionally `let _ =`-discarded: a room that never tripped a given
/// (transport, drop_reason) / outcome simply has nothing to remove.
///
/// NOTE: the `room`-only GaugeVecs `relay_outbound_queue_depth` and
/// `relay_active_sessions_per_room` carry an additional `transport` label and
/// are GC'd on a per-session basis already (`relay_active_sessions_per_room` is
/// decremented in `on_stopping`); the queue-depth gauge is overwritten every
/// heartbeat for a live session and stops being written once the session ends,
/// but we still sweep its `(room, transport)` tuples here so a drained room
/// leaves no stale depth reading.
pub fn forget_room_metrics(room: &str) {
    // Single-label room counters: one tuple each.
    let _ = RELAY_VIEWPORT_FILTERED_TOTAL.remove_label_values(&[room]);
    let _ = RELAY_VIEWPORT_FORWARDED_TOTAL.remove_label_values(&[room]);
    let _ = RELAY_LAYER_FILTERED_TOTAL.remove_label_values(&[room]);
    let _ = RELAY_LAYER_FORWARDED_TOTAL.remove_label_values(&[room]);

    // relay_room_bytes_total{room, direction}.
    for direction in ["inbound", "outbound"] {
        let _ = RELAY_ROOM_BYTES_TOTAL.remove_label_values(&[room, direction]);
    }

    // relay_viewport_updates_total{room, outcome}.
    for outcome in RELAY_VIEWPORT_UPDATE_OUTCOMES {
        let _ = RELAY_VIEWPORT_UPDATES_TOTAL.remove_label_values(&[room, outcome]);
    }

    // relay_layer_preference_updates_total{room, outcome} (#989, #1082).
    for outcome in RELAY_LAYER_PREFERENCE_UPDATE_OUTCOMES {
        let _ = RELAY_LAYER_PREFERENCE_UPDATES_TOTAL.remove_label_values(&[room, outcome]);
    }

    // relay_layer_hint_emitted_total{room, direction} (#1108).
    for direction in RELAY_LAYER_HINT_DIRECTIONS {
        let _ = RELAY_LAYER_HINT_EMITTED_TOTAL.remove_label_values(&[room, direction]);
    }

    // relay_packet_drops_total{room, transport, drop_reason}: full cartesian
    // product of the two bounded taxonomies.
    for transport in RELAY_DROP_TRANSPORTS {
        for drop_reason in RELAY_DROP_KINDS {
            let _ = RELAY_PACKET_DROPS_TOTAL.remove_label_values(&[room, transport, drop_reason]);
        }
        // relay_outbound_queue_depth / relay_active_sessions_per_room
        // {room, transport} gauges.
        let _ = RELAY_OUTBOUND_QUEUE_DEPTH.remove_label_values(&[room, transport]);
        // EVICTION-PATH ORDERING RACE — benign, transient, self-healing (#1187).
        //
        // `relay_active_sessions_per_room{room,transport}` is `.inc()`'d in
        // `SessionLogic::track_connection_start` and `.dec()`'d in
        // `SessionLogic::on_stopping`. On the LEAVE path the actor's
        // `on_stopping().dec()` runs first and `forget_room_metrics` (this
        // removal) runs after the room has drained, so the series is removed at
        // its true `0.0` resting value.
        //
        // On the EVICTION path the order can INVERT. `ChatServer::forget_session`
        // (reached via `evict_stale_session` on an `EvictInstance` NATS message)
        // calls `forget_room_metrics(room)` synchronously the moment it removes
        // the evicted session from `room_members` and the room becomes empty —
        // BUT the evicted session is a *separate, still-alive* actix actor.
        // `forget_session` only tears down ChatServer's bookkeeping and aborts
        // the NATS sub task; it does NOT stop that actor synchronously. The
        // evicted actor's `on_stopping().dec()` therefore fires LATER (when its
        // connection actually closes), AFTER this `remove_label_values` already
        // deleted the `{room,transport}` series. A `.dec()` on a removed series
        // RE-CREATES it at `-1.0`.
        //
        // This is NOT the #996 permanent leak: it can only happen when the
        // evicted session was the room's LAST member (otherwise the room stays
        // populated and this removal does not run), and the -1.0 series is
        // erased again by the next `forget_room_metrics` for that room, or
        // corrected the instant any session re-joins (`track_connection_start`
        // `.inc()` brings it to 0.0). A guard here is not trivially safe — a
        // GaugeVec has no cheap "does this series exist" probe that would not
        // itself re-create the series — and the dec()/remove() ordering cannot
        // be sequenced from ChatServer because the evicted actor stops
        // asynchronously. The transient -1.0 is therefore documented as benign
        // rather than guarded (issue #1187, option a).
        let _ = RELAY_ACTIVE_SESSIONS_PER_ROOM.remove_label_values(&[room, transport]);
    }

    // relay_viewport_set_size{room} — the #988 gauge previously swept inline.
    let _ = RELAY_VIEWPORT_SET_SIZE.remove_label_values(&[room]);
}

lazy_static! {
    /// Total number of health reports received
    pub static ref HEALTH_REPORTS_TOTAL: Counter = register_counter!(
        "videocall_health_reports_total",
        "Total number of health reports received"
    )
    .expect("Failed to create health_reports_total metric");

    /// Whether peer can receive audio (1 = yes, 0 = no)
    pub static ref PEER_CAN_LISTEN: GaugeVec = register_gauge_vec!(
        "videocall_peer_can_listen",
        "Indicates if a peer can receive audio from another peer (1=yes, 0=no)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create peer_can_listen metric");

    /// Whether peer can receive video (1 = yes, 0 = no)
    pub static ref PEER_CAN_SEE: GaugeVec = register_gauge_vec!(
        "videocall_peer_can_see",
        "Indicates if a peer can receive video from another peer (1=yes, 0=no)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create peer_can_see metric");

    /// NetEQ audio buffer size in milliseconds
    pub static ref NETEQ_AUDIO_BUFFER_MS: GaugeVec = register_gauge_vec!(
        "videocall_neteq_audio_buffer_ms",
        "Audio data buffered for playback in milliseconds",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create neteq_audio_buffer_ms metric");

    /// NetEQ packets waiting for decode
    pub static ref NETEQ_PACKETS_AWAITING_DECODE: GaugeVec = register_gauge_vec!(
        "videocall_neteq_packets_awaiting_decode",
        "Number of encoded packets waiting to be decoded",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create neteq_packets_awaiting_decode metric");

    /// NetEQ packets received per second
    pub static ref NETEQ_PACKETS_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_neteq_packets_per_sec",
        "Number of audio RTP packets received per second (rolling 1s window)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create neteq_packets_per_sec metric");

    /// NetEQ normal decode operations per second
    pub static ref NETEQ_NORMAL_OPS_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_neteq_normal_ops_per_sec",
        "Normal decode operations per second",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create neteq_normal_ops_per_sec metric");

    /// NetEQ expand operations per second
    pub static ref NETEQ_EXPAND_OPS_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_neteq_expand_ops_per_sec",
        "Expand operations per second (packet loss concealment)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create neteq_expand_ops_per_sec metric");

    /// NetEQ accelerate operations per second
    pub static ref NETEQ_ACCELERATE_OPS_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_neteq_accelerate_ops_per_sec",
        "Accelerate operations per second (time compression)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create neteq_accelerate_ops_per_sec metric");

    // NOTE: Low-value NetEQ operation counters removed for cardinality reduction:
    // fast_accelerate, preemptive_expand, merge, comfort_noise, dtmf, undefined
    // These are still collected client-side and visible in vcprobe, just not in Prometheus.

    /// Total number of active sessions
    pub static ref ACTIVE_SESSIONS_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_active_sessions_total",
        "Number of active sessions",
        &["meeting_id", "session_id"]
    )
    .expect("Failed to create active_sessions_total metric");

    /// Number of participants in each meeting
    pub static ref MEETING_PARTICIPANTS: GaugeVec = register_gauge_vec!(
        "videocall_meeting_participants",
        "Number of participants in a meeting",
        &["meeting_id"]
    )
    .expect("Failed to create meeting_participants metric");

    /// Total number of peer connections
    pub static ref PEER_CONNECTIONS_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_peer_connections_total",
        "Number of peer connections",
        &["meeting_id", "peer_id"]
    )
    .expect("Failed to create peer_connections_total metric");

    /// Per-pair video framerate as observed by the receiver
    pub static ref VIDEO_FPS: GaugeVec = register_gauge_vec!(
        "videocall_video_fps",
        "Video frames per second observed by the receiver",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_fps metric");

    /// Whether receiving peer reports audio enabled for the sender
    pub static ref PEER_AUDIO_ENABLED: GaugeVec = register_gauge_vec!(
        "videocall_peer_audio_enabled",
        "Indicates if sender's audio is enabled (1=yes, 0=no)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create peer_audio_enabled metric");

    /// Whether receiving peer reports video enabled for the sender
    pub static ref PEER_VIDEO_ENABLED: GaugeVec = register_gauge_vec!(
        "videocall_peer_video_enabled",
        "Indicates if sender's camera is enabled (1=yes, 0=no)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create peer_video_enabled metric");

    /// Sender self-reported audio enabled (authoritative), per meeting and peer
    pub static ref SELF_AUDIO_ENABLED: GaugeVec = register_gauge_vec!(
        "videocall_self_audio_enabled",
        "Sender self-reported audio enabled (1=yes, 0=no)",
        &["meeting_id", "peer_id", "display_name"]
    )
    .expect("Failed to create self_audio_enabled metric");

    /// Sender self-reported video enabled (authoritative), per meeting and peer
    pub static ref SELF_VIDEO_ENABLED: GaugeVec = register_gauge_vec!(
        "videocall_self_video_enabled",
        "Sender self-reported video enabled (1=yes, 0=no)",
        &["meeting_id", "peer_id", "display_name"]
    )
    .expect("Failed to create self_video_enabled metric");

    /// Client-side measured active server RTT in milliseconds
    pub static ref CLIENT_ACTIVE_SERVER_RTT_MS: GaugeVec = register_gauge_vec!(
        "videocall_client_active_server_rtt_ms",
        "Client-side measured RTT to the elected server (ms)",
        &["meeting_id", "session_id", "peer_id", "server_url", "server_type", "display_name"]
    )
    .expect("Failed to create client_active_server_rtt_ms metric");

    /// Marker that a client is connected to a given server (value = 1)
    pub static ref CLIENT_ACTIVE_SERVER: GaugeVec = register_gauge_vec!(
        "videocall_client_active_server",
        "Indicates which server a client is connected to (1)",
        &["meeting_id", "session_id", "peer_id", "server_url", "server_type", "display_name"]
    )
    .expect("Failed to create client_active_server metric");

    // ===== SERVER-SIDE METRICS (via NATS) =====

    /// Active connections on servers by protocol and customer (now with unique session_id)
    pub static ref SERVER_CONNECTIONS_ACTIVE: GaugeVec = register_gauge_vec!(
        "videocall_server_connections_active",
        "Number of active connections on servers",
        &["session_id", "protocol", "customer_email", "meeting_id", "server_instance", "region"]
    )
    .expect("Failed to create server_connections_active metric");

    /// Active unique user connections (deduplicated by customer_email + meeting_id)
    pub static ref SERVER_UNIQUE_USERS_ACTIVE: GaugeVec = register_gauge_vec!(
        "videocall_server_unique_users_active",
        "Number of unique users active in meetings (deduplicated across protocols)",
        &["customer_email", "meeting_id", "region"]
    )
    .expect("Failed to create server_unique_users_active metric");

    /// Active protocol connections per unique user
    pub static ref SERVER_PROTOCOL_CONNECTIONS: GaugeVec = register_gauge_vec!(
        "videocall_server_protocol_connections",
        "Protocols used by active connections per user",
        &["protocol", "customer_email", "meeting_id", "region"]
    )
    .expect("Failed to create server_protocol_connections metric");

    /// Total data bytes transferred by servers per customer (now with unique session_id)
    pub static ref SERVER_DATA_BYTES_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_server_data_bytes_total",
        "Cumulative bytes transferred by servers per customer",
        &["direction", "session_id", "protocol", "customer_email", "meeting_id", "server_instance", "region"]
    )
    .expect("Failed to create server_data_bytes_total metric");

    /// Connection duration in seconds (when connection closes)
    pub static ref SERVER_CONNECTION_DURATION_SECONDS: Histogram = register_histogram!(
        "videocall_server_connection_duration_seconds",
        "Duration of server connections in seconds",
        vec![1.0, 10.0, 30.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0] // 1s to 2h buckets
    )
    .expect("Failed to create server_connection_duration_seconds metric");

    /// Connection lifecycle events counter
    pub static ref SERVER_CONNECTION_EVENTS_TOTAL: Counter = register_counter!(
        "videocall_server_connection_events_total",
        "Total connection lifecycle events on servers"
    )
    .expect("Failed to create server_connection_events_total metric");

    /// Reconnection tracking per customer and meeting
    pub static ref SERVER_RECONNECTIONS_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_server_reconnections_total",
        "Total reconnections per customer and meeting",
        &["protocol", "customer_email", "meeting_id", "server_instance", "region"]
    )
    .expect("Failed to create server_reconnections_total metric");

    // ===== PHASE 1 METRICS: Browser State and Quality Indicators =====

    /// Client tab visibility indicator (1=visible, 0=hidden/backgrounded)
    pub static ref CLIENT_TAB_VISIBLE: GaugeVec = register_gauge_vec!(
        "videocall_client_tab_visible",
        "Indicates if client browser tab is visible (1=visible, 0=hidden/throttled)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_tab_visible metric");

    /// Client JS heap memory usage in bytes (Chrome only)
    pub static ref CLIENT_MEMORY_USED_BYTES: GaugeVec = register_gauge_vec!(
        "videocall_client_memory_used_bytes",
        "JS heap memory used by client in bytes (Chrome only)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_memory_used_bytes metric");

    /// Client JS heap memory total/limit in bytes (Chrome only)
    pub static ref CLIENT_MEMORY_TOTAL_BYTES: GaugeVec = register_gauge_vec!(
        "videocall_client_memory_total_bytes",
        "JS heap memory limit for client in bytes (Chrome only)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_memory_total_bytes metric");

    /// #1032: Client WASM linear-memory size in bytes
    /// (WebAssembly.Memory.buffer.byteLength). Distinct from the JS heap above;
    /// always available. Part of the non-heap memory telemetry for freeze
    /// observability — the JS-heap gauge misses the multi-GB pressure.
    pub static ref CLIENT_WASM_MEMORY_BYTES: GaugeVec = register_gauge_vec!(
        "videocall_client_wasm_memory_bytes",
        "WASM linear memory size of client in bytes (WebAssembly.Memory.buffer.byteLength)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_wasm_memory_bytes metric");

    /// #1032: Client total agent memory in bytes from
    /// performance.measureUserAgentSpecificMemory() — includes GPU-backed and
    /// worker allocations. Chrome-only + crossOriginIsolated-gated, so this
    /// series is absent for clients where the API is unavailable.
    pub static ref CLIENT_AGENT_MEMORY_BYTES: GaugeVec = register_gauge_vec!(
        "videocall_client_agent_memory_bytes",
        "Total agent memory of client in bytes (measureUserAgentSpecificMemory; Chrome + crossOriginIsolated only)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_agent_memory_bytes metric");

    /// Video frames dropped by receiver
    pub static ref VIDEO_FRAMES_DROPPED: GaugeVec = register_gauge_vec!(
        "videocall_video_frames_dropped",
        "Number of video frames dropped by the receiver",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_frames_dropped metric");

    /// Audio quality score (0-100, absent when no audio flowing)
    pub static ref AUDIO_QUALITY_SCORE: GaugeVec = register_gauge_vec!(
        "videocall_audio_quality_score",
        "Audio quality score 0-100 (concealment + packet loss penalty)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create audio_quality_score metric");

    /// Video quality score (0-100, absent when video disabled)
    pub static ref VIDEO_QUALITY_SCORE: GaugeVec = register_gauge_vec!(
        "videocall_video_quality_score",
        "Video quality score 0-100 (FPS health + decode error penalty)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_quality_score metric");

    /// Per-peer windowed video sequence packet-loss rate (freeze indicator)
    pub static ref VIDEO_SEQ_LOSS_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_video_seq_loss_per_sec",
        "Per-peer windowed video sequence packet-loss rate (lost packets/sec) observed by the receiver; freeze indicator",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_seq_loss_per_sec metric");

    /// Per-peer windowed rate of keyframe (PLI) requests sent to the peer
    pub static ref KEYFRAME_REQUESTS_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_keyframe_requests_per_sec",
        "Per-peer windowed rate of keyframe (PLI) requests this client sent to the peer; sustained nonzero => stream cannot recover",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create keyframe_requests_per_sec metric");

    /// Call quality score (0-100, min of audio and video)
    pub static ref CALL_QUALITY_SCORE: GaugeVec = register_gauge_vec!(
        "videocall_call_quality_score",
        "Call quality score 0-100 — min(audio, video), primary alerting metric",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create call_quality_score metric");

    /// NetEQ target delay (jitter estimate) in milliseconds
    pub static ref NETEQ_TARGET_DELAY_MS: GaugeVec = register_gauge_vec!(
        "videocall_neteq_target_delay_ms",
        "NetEQ delay manager target delay (network jitter estimate) in ms",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create neteq_target_delay_ms metric");

    /// Video bitrate observed by receiver (kbps)
    pub static ref VIDEO_BITRATE_KBPS: GaugeVec = register_gauge_vec!(
        "videocall_video_bitrate_kbps",
        "Video bitrate observed by the receiver in kbps",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_bitrate_kbps metric");

    // ===== CLIENT COMMUNICATION & BROWSER STATE =====

    /// Client send queue bytes (WebSocket bufferedAmount)
    pub static ref CLIENT_SEND_QUEUE_BYTES: GaugeVec = register_gauge_vec!(
        "videocall_client_send_queue_bytes",
        "Client-side send queue buffer size in bytes",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_send_queue_bytes metric");

    /// Client total packets received per second
    pub static ref CLIENT_PACKETS_RECEIVED_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_client_packets_received_per_sec",
        "Total packets received per second by client",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_packets_received_per_sec metric");

    /// Client total packets sent per second
    pub static ref CLIENT_PACKETS_SENT_PER_SEC: GaugeVec = register_gauge_vec!(
        "videocall_client_packets_sent_per_sec",
        "Total packets sent per second by client",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_packets_sent_per_sec metric");

    /// Client tab throttled indicator (1=throttled by browser, 0=normal)
    pub static ref CLIENT_TAB_THROTTLED: GaugeVec = register_gauge_vec!(
        "videocall_client_tab_throttled",
        "Indicates if client tab is throttled by browser (1=throttled, 0=normal)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_tab_throttled metric");

    // ===== RECEIVER-SIDE QUALITY METRICS =====

    /// Adaptive video encoding tier (0=full_hd/best, 7=minimal)
    pub static ref ADAPTIVE_VIDEO_TIER: GaugeVec = register_gauge_vec!(
        "videocall_adaptive_video_tier",
        "Adaptive video encoding tier index (0=best, 7=minimal)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create adaptive_video_tier metric");

    /// Adaptive audio encoding tier (0=high, 3=emergency)
    pub static ref ADAPTIVE_AUDIO_TIER: GaugeVec = register_gauge_vec!(
        "videocall_adaptive_audio_tier",
        "Adaptive audio encoding tier index (0=high, 3=emergency)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create adaptive_audio_tier metric");

    /// Cumulative datagrams dropped as of the latest client health snapshot.
    pub static ref DATAGRAM_DROPS: GaugeVec = register_gauge_vec!(
        "videocall_datagram_drops",
        "Cumulative datagrams dropped due to locked writable stream as of the latest client health snapshot",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create datagram_drops metric");

    /// Cumulative WebSocket packet drops as of the latest client health snapshot.
    pub static ref WEBSOCKET_DROPS: GaugeVec = register_gauge_vec!(
        "videocall_websocket_drops",
        "Cumulative WebSocket packets dropped due to send buffer backpressure as of the latest client health snapshot",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create websocket_drops metric");

    /// Cumulative keyframe requests sent (PLI)
    pub static ref KEYFRAME_REQUESTS_SENT_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_keyframe_requests_sent_total",
        "Cumulative keyframe requests (PLI) sent by this client",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create keyframe_requests_sent_total metric");

    /// Cumulative transport re-election outcomes reported by the client
    /// (dashboard audit Tier B #3; discussion #562).
    ///
    /// TYPE DECISION — GaugeVec, NOT CounterVec: the client reports a CUMULATIVE
    /// total in every health packet (the same convention as DATAGRAM_DROPS /
    /// WEBSOCKET_DROPS / KEYFRAME_REQUESTS_SENT_TOTAL above, all GaugeVecs). The
    /// expander therefore `.set()`s this gauge to the client's reported
    /// cumulative value once per packet. A CounterVec `.inc()`-ed per packet
    /// would multiply-count (the same cumulative value arrives every second).
    /// Because the client value is monotonic within a page session, Grafana
    /// charts re-election RATE with `rate()`/`increase()` over this gauge
    /// exactly as it does for the sibling `*_total` gauges. (A process restart /
    /// reconnect that resets the client counter shows as a gauge drop, the same
    /// minor caveat the sibling gauges already carry — acceptable for a
    /// dashboard signal, and avoids the multiply-count bug.)
    ///
    /// CARDINALITY: `meeting_id` × `session_id` × `result` (exactly 4 bounded
    /// result values: `proceeded|aborted|preserved|failed`). Per-session series
    /// are GC'd by the metrics-server's existing stale-session cleanup
    /// (`cleanup_stale_sessions` → `remove_session_metrics`), same as every
    /// other `meeting_id,session_id`-keyed client series.
    pub static ref CLIENT_REELECTION_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_client_reelection_total",
        "Cumulative transport re-election outcomes reported by the client, by result (proceeded|aborted|preserved|failed). GaugeVec set() to the client's cumulative value; chart with rate()/increase()",
        &["meeting_id", "session_id", "result"]
    )
    .expect("Failed to create videocall_client_reelection_total metric");

    // ===== ENCODER & SCREEN SHARE METRICS (sender-side, P0/P1) =====
    // NOTE(#1184): videocall_encoder_fps_ratio / videocall_encoder_bitrate_ratio
    // removed — dead telemetry whose source proto fields no longer exist.

    /// p75 peer FPS signal driving encoder decisions.
    pub static ref ENCODER_P75_PEER_FPS: GaugeVec = register_gauge_vec!(
        "videocall_encoder_p75_peer_fps",
        "p75 peer FPS driving adaptive quality decisions",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create encoder_p75_peer_fps metric");

    /// Screen share quality tier (0=high, 1=medium, 2=low)
    pub static ref ADAPTIVE_SCREEN_TIER: GaugeVec = register_gauge_vec!(
        "videocall_adaptive_screen_tier",
        "Screen share adaptive quality tier index (0=high, 2=low)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create adaptive_screen_tier metric");

    /// Screen sharing active indicator
    pub static ref SCREEN_SHARING_ACTIVE: GaugeVec = register_gauge_vec!(
        "videocall_screen_sharing_active",
        "Whether screen sharing is active (1=active, 0=inactive)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create screen_sharing_active metric");

    /// Encoder output FPS
    pub static ref ENCODER_OUTPUT_FPS: GaugeVec = register_gauge_vec!(
        "videocall_encoder_output_fps",
        "Actual frames per second produced by the camera encoder",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create encoder_output_fps metric");

    /// Encoder target bitrate (PID controller output)
    pub static ref ENCODER_TARGET_BITRATE_KBPS: GaugeVec = register_gauge_vec!(
        "videocall_encoder_target_bitrate_kbps",
        "PID controller computed target bitrate in kbps",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create encoder_target_bitrate_kbps metric");

    // ===== DECODE-BUDGET STATE (#987 / PR #999) =====

    /// Current effective tile cap enforced by the decode-budget controller.
    pub static ref DECODE_BUDGET_EFFECTIVE_CAP: GaugeVec = register_gauge_vec!(
        "videocall_decode_budget_effective_cap",
        "Current effective decode-budget tile cap",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create decode_budget_effective_cap metric");

    /// Natural/unconstrained tile count before any decode-budget cap is applied.
    pub static ref DECODE_BUDGET_NATURAL: GaugeVec = register_gauge_vec!(
        "videocall_decode_budget_natural",
        "Natural (unconstrained) decode-budget tile count before capping",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create decode_budget_natural metric");

    /// Decode-budget pressured latch (1=pressured, 0=not pressured).
    pub static ref DECODE_BUDGET_PRESSURED: GaugeVec = register_gauge_vec!(
        "videocall_decode_budget_pressured",
        "Decode-budget pressured latch (1=pressured, 0=not pressured)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create decode_budget_pressured metric");

    /// Decode-budget override mode (0=unspecified, 1=auto, 2=fixed).
    pub static ref DECODE_BUDGET_OVERRIDE_MODE: GaugeVec = register_gauge_vec!(
        "videocall_decode_budget_override_mode",
        "Decode-budget override mode (0=unspecified, 1=auto, 2=fixed)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create decode_budget_override_mode metric");

    /// User-configured fixed tile cap, set only when override_mode == fixed.
    pub static ref DECODE_BUDGET_OVERRIDE_FIXED_N: GaugeVec = register_gauge_vec!(
        "videocall_decode_budget_override_fixed_n",
        "Decode-budget user-configured fixed tile cap (override_mode=fixed)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create decode_budget_override_fixed_n metric");

    /// Tiles ACTUALLY being decoded right now = min(effective_cap, natural)
    /// (#1143). The per-client "how many videos is this client showing" signal:
    /// `effective_cap` is the ceiling and `natural` is the unconstrained layout
    /// count, but neither alone answers it when the layout has fewer tiles than
    /// the cap allows. Pairs with the existing `videocall_decode_budget_effective_cap`
    /// (the cap itself, already exported) — this is the realized count.
    pub static ref DECODE_ACTIVE_SET_SIZE: GaugeVec = register_gauge_vec!(
        "videocall_decode_active_set_size",
        "Tiles actually being decoded right now (min of decode-budget cap and natural layout count)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create decode_active_set_size metric");

    // ===== CAPABILITY & SIMULCAST-LAYER GAUGES (#1143) =====

    /// Client capability score as a NUMERIC gauge (#1143). The TELEM-6 benchmark
    /// iteration count already rides as a string label on `videocall_client_info`,
    /// but a label cannot be thresholded/averaged/quantiled in PromQL without
    /// `label_replace` hacks — answering "how many clients scored <5000?" required
    /// pulling every series and computing client-side. This exposes the same value
    /// as a real measurement so the distribution is a one-line query / dashboard
    /// panel. Same value, same label set as the other per-reporter client gauges.
    pub static ref CAPABILITY_SCORE: GaugeVec = register_gauge_vec!(
        "videocall_capability_score",
        "Client TELEM-6 capability-benchmark score as a numeric value (also a label on videocall_client_info; this form is queryable)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create capability_score metric");

    /// Effective simulcast layer count the publisher is configured to encode/send
    /// (#1143). p90==1 across a meeting is the inert-simulcast signal that the
    /// cc7tp analysis could only see in console logs. `media_kind` distinguishes
    /// camera vs screen; the client currently reports the CAMERA encoder's state
    /// (`media_kind="camera"`).
    pub static ref ENCODER_EFFECTIVE_LAYERS: GaugeVec = register_gauge_vec!(
        "videocall_encoder_effective_layers",
        "Number of simulcast video layers the publisher is configured to encode/send (ladder depth); p90==1 over a meeting = inert simulcast",
        &["meeting_id", "session_id", "peer_id", "display_name", "media_kind"]
    )
    .expect("Failed to create encoder_effective_layers metric");

    /// Currently-ACTIVE simulcast layer count (#1143): how many of the effective
    /// layers are presently encoded + sent. The AQ controller sheds the top
    /// layer(s) under congestion, so this can be `<` effective; the gap is the
    /// shed depth. `media_kind` as above.
    pub static ref ENCODER_ACTIVE_LAYERS: GaugeVec = register_gauge_vec!(
        "videocall_encoder_active_layers",
        "Number of simulcast video layers currently active (encoded + sent); < effective_layers when the AQ controller has shed the top layer(s)",
        &["meeting_id", "session_id", "peer_id", "display_name", "media_kind"]
    )
    .expect("Failed to create encoder_active_layers metric");

    // ===== PER-PEER QUALITY METRICS (new/transition) =====

    /// Audio concealment percentage from NetEQ expand events (0.0-100.0)
    pub static ref AUDIO_CONCEALMENT_PCT: GaugeVec = register_gauge_vec!(
        "videocall_audio_concealment_pct",
        "Audio concealment percentage from NetEQ expand events (0.0-100.0)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create audio_concealment_pct metric");

    /// Cumulative decoder errors per peer
    pub static ref DECODER_ERRORS_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_decoder_errors_total",
        "Cumulative decoder error count per peer pair",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create decoder_errors_total metric");

    // ===== SCREEN SHARE PER-PEER METRICS =====

    /// Screen share FPS observed by receiver
    pub static ref SCREEN_VIDEO_FPS: GaugeVec = register_gauge_vec!(
        "videocall_screen_video_fps",
        "Screen share frames per second observed by the receiver",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create screen_video_fps metric");

    /// Screen share bitrate observed by receiver (kbps)
    pub static ref SCREEN_VIDEO_BITRATE_KBPS: GaugeVec = register_gauge_vec!(
        "videocall_screen_video_bitrate_kbps",
        "Screen share bitrate observed by the receiver in kbps",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create screen_video_bitrate_kbps metric");

    // ===== TIER TRANSITION COUNTER =====
    //
    // CARDINALITY NOTE: 9-label CounterVec. The from_tier/to_tier labels create
    // one series per unique tier transition pair per session. Hysteresis timers
    // (3s min interval) limit practical growth to ~10-20 combos per session.
    // At scale targets (15 meetings × 20 users) expect ~4,500 series.
    // CounterVec series are NOT cleaned up on session disconnect (counters are
    // cumulative); they expire via Prometheus metric_relabel_configs TTL.
    // If cardinality grows beyond expectations, drop from_tier/to_tier labels.

    /// Cumulative tier transition events
    pub static ref TIER_TRANSITIONS_TOTAL: CounterVec = register_counter_vec!(
        "videocall_tier_transition_total",
        "Cumulative tier transitions by direction, stream, and trigger",
        &["meeting_id", "session_id", "peer_id", "display_name",
          "direction", "stream", "from_tier", "to_tier", "trigger"]
    )
    .expect("Failed to create tier_transition_total metric");

    // ===== RELAY SERVER-SIDE METRICS (in-process on relay binaries) =====
    //
    // CARDINALITY NOTE: These metrics use `room` (meeting_id) as a label.
    // Meeting IDs are user-provided so the label space is unbounded.
    // GaugeVec metrics (queue depth, active sessions) are cleaned up on
    // session disconnect. CounterVec metrics (drops, bytes) cannot be
    // cleaned per-label; over time with many unique rooms, series count
    // will grow. Mitigated by Prometheus metric_relabel_configs that
    // filter ~96% of series. If cardinality becomes a problem, switch
    // counters to room-less aggregates or add periodic label cleanup.

    /// Total packet drops from try_send() failures on outbound channels/mailboxes.
    ///
    /// This counter is for UNINTENTIONAL backpressure loss only (full outbound
    /// queue / actor mailbox). Intentional, policy-driven drops (e.g. viewport
    /// filtering) are tracked on their own metric so backpressure dashboards
    /// and alerts that sum across labels are not polluted. See
    /// [`RELAY_VIEWPORT_FILTERED_TOTAL`].
    ///
    /// `drop_reason` values:
    /// - `mailbox_full`: a fan-out `try_send` into a receiver's actor mailbox
    ///   failed (the #1057 room-wide-freeze signature). Emitted at the inbound
    ///   fan-out hop (`chat_server.rs` `handle_msg`) for Critical/Control
    ///   packets, packets the relay could not classify (unparseable wrapper /
    ///   UNSPECIFIED `media_kind`), and `Closed`-mailbox drops.
    /// - `channel_full`: a `try_send` into the policy-aware outbound channel
    ///   failed (emitted at the per-transport `Handler<Message>` hop).
    /// - `priority_drop_video` / `priority_drop_audio`: a droppable MEDIA frame
    ///   (VIDEO/SCREEN → `_video`, AUDIO → `_audio`) was the kind SACRIFICED on
    ///   overflow. Emitted BOTH at the outbound-channel hop (a true preemptive
    ///   priority drop, keyed on channel fill) AND, as of #1145, at the inbound
    ///   fan-out hop when a `Full` mailbox forced a media drop — there the
    ///   label is ATTRIBUTION ONLY (the actix mailbox exposes no preemption
    ///   API, so the packet could not be enqueued regardless; the label records
    ///   WHICH kind was sacrificed so a fan-out burst reads as "video shed"
    ///   rather than undifferentiated `mailbox_full`). Classified off the OUTER
    ///   cleartext `media_kind` (E2EE-safe), never the inner MediaType.
    /// The label set mirrors the OUTBOUND taxonomy on
    /// [`OUTBOUND_CHANNEL_DROPS_TOTAL`] so one dashboard query spans both hops.
    pub static ref RELAY_PACKET_DROPS_TOTAL: CounterVec = register_counter_vec!(
        "relay_packet_drops_total",
        "Total packets dropped due to full outbound queue or mailbox (backpressure loss only; intentional viewport drops are counted by relay_viewport_filtered_total)",
        &["room", "transport", "drop_reason"]
    )
    .expect("Failed to create relay_packet_drops_total metric");

    /// VIDEO packets INTENTIONALLY not forwarded because the receiver's
    /// viewport set (HCL issue #988) does not include the source session.
    ///
    /// This is an expected, bandwidth-saving drop — NOT a backpressure loss —
    /// so it is deliberately kept off `relay_packet_drops_total`.
    ///
    /// CARDINALITY: `room` is user-provided (unbounded over time), same
    /// caveats as the other room-labeled counters above.
    pub static ref RELAY_VIEWPORT_FILTERED_TOTAL: CounterVec = register_counter_vec!(
        "relay_viewport_filtered_total",
        "Total VIDEO packets intentionally dropped by viewport-aware relay filtering (off-screen source not in receiver's viewport)",
        &["room"]
    )
    .expect("Failed to create relay_viewport_filtered_total metric");

    /// VIDEO packets that PASSED the viewport filter and were forwarded — the
    /// denominator complement of `relay_viewport_filtered_total` (HCL #988).
    ///
    /// Without this baseline the filtered counter has no scale: you cannot tell
    /// "5 drops/s out of 5000 forwarded/s" (healthy) from "5 drops/s out of 6
    /// forwarded/s" (the wrongly-dropping / froze-my-video signature). The
    /// "% filtered" panel is `filtered / (filtered + forwarded)`.
    ///
    /// Incremented in the `is_video && !drop_video` branch — the exact
    /// complement of the filtered increment, so the two are mutually exclusive
    /// and together cover every VIDEO packet that reached the filter.
    ///
    /// CARDINALITY: `room` only (user-provided, unbounded over time — same
    /// caveat as the other room-labeled counters above). No per-source/session
    /// label: session IDs churn on reconnect (see
    /// `videocall_outbound_channel_drops_total` for the same call).
    pub static ref RELAY_VIEWPORT_FORWARDED_TOTAL: CounterVec = register_counter_vec!(
        "relay_viewport_forwarded_total",
        "Total VIDEO packets forwarded after passing viewport-aware relay filtering (on-screen, or fail-open). Denominator complement of relay_viewport_filtered_total",
        &["room"]
    )
    .expect("Failed to create relay_viewport_forwarded_total metric");

    /// Current viewport (desired-streams) set size, per room (HCL #988).
    ///
    /// Updated on every ACCEPTED VIEWPORT inside `try_intercept_viewport`. A
    /// collapse toward 0/1 while peers are still publishing is the
    /// wrongly-dropping ("froze my video") signature — the relay is the only
    /// place this is observable, because the client-FPS cross-check telemetry
    /// does NOT land on these clusters.
    ///
    /// A `GaugeVec` (NOT a counter) so the per-room series can be REMOVED when
    /// the room drains — see the cleanup in `forget_room_if_empty` /
    /// `forget_session`. Counters cannot be unregistered cheaply; a stale gauge
    /// series for a dead room would otherwise read its last value forever.
    ///
    /// CARDINALITY: `room` only. Because we deliberately do NOT key on the
    /// receiver session (unbounded, churns on reconnect), the gauge is
    /// LAST-WRITER-WINS across the receivers in a room: it reflects the most
    /// recently-accepted viewport set size for the room, which is sufficient
    /// for the "is the whole room collapsing toward 0/1" signal. Per-session
    /// forensics live in the `chat_server=debug` VIDEO-drop log, not in labels.
    pub static ref RELAY_VIEWPORT_SET_SIZE: GaugeVec = register_gauge_vec!(
        "relay_viewport_set_size",
        "Most recently accepted viewport (desired-streams) set size per room; a collapse toward 0/1 is the wrongly-dropping signature (HCL #988)",
        &["room"]
    )
    .expect("Failed to create relay_viewport_set_size metric");

    /// VIEWPORT control-packet update outcomes, per room (HCL #988).
    ///
    /// Makes the DoS guards in `try_intercept_viewport` observable — today the
    /// cap (`VIEWPORT_MAX_SESSION_IDS`) and the rate limit
    /// (`VIEWPORT_MIN_UPDATE_INTERVAL`) fire SILENTLY. Also gives plain
    /// "VIEWPORT received" visibility via the `accepted` outcome.
    ///
    /// `outcome` is bounded — exactly 4 values:
    /// - `accepted`:              update was applied to the receiver's set.
    /// - `rate_limited`:          arrived within `VIEWPORT_MIN_UPDATE_INTERVAL`
    ///   of the last accepted update; consumed but ignored.
    /// - `truncated`:             the session_id list exceeded
    ///   `VIEWPORT_MAX_SESSION_IDS` and was capped (fail-open on the excess).
    ///   Counted in ADDITION to `accepted` for the same packet (it was both
    ///   truncated AND applied).
    /// - `ignored_other_subject`: arrived on a subject other than the receiver's
    ///   own; expected for normal NATS fan-out and dropped without mutating state.
    ///
    /// CARDINALITY: bounded — `room` × 4 outcomes. No per-session label.
    pub static ref RELAY_VIEWPORT_UPDATES_TOTAL: CounterVec = register_counter_vec!(
        "relay_viewport_updates_total",
        "VIEWPORT control-packet update outcomes per room (accepted|rate_limited|truncated|ignored_other_subject) (HCL #988)",
        &["room", "outcome"]
    )
    .expect("Failed to create relay_viewport_updates_total metric");

    /// Simulcast VIDEO packets INTENTIONALLY not forwarded because the
    /// receiver's recorded layer preference (#989, Phase 1b) for the source
    /// session selects a DIFFERENT simulcast layer than this packet carries.
    ///
    /// Like [`RELAY_VIEWPORT_FILTERED_TOTAL`] this is an expected,
    /// bandwidth-saving drop — NOT a backpressure loss — so it is deliberately
    /// kept off `relay_packet_drops_total`. It runs strictly AFTER the viewport
    /// filter, so a packet counted here was already viewport-wanted.
    ///
    /// CARDINALITY: `room` only (user-provided, unbounded over time), same
    /// caveats as the other room-labeled counters above.
    pub static ref RELAY_LAYER_FILTERED_TOTAL: CounterVec = register_counter_vec!(
        "relay_layer_filtered_total",
        "Total simulcast VIDEO packets intentionally dropped by per-receiver layer selection (layer != receiver's recorded preference for the source) (#989)",
        &["room"]
    )
    .expect("Failed to create relay_layer_filtered_total metric");

    /// Simulcast media packets that ENTERED the per-receiver layer filter and
    /// were forwarded — the denominator complement of `relay_layer_filtered_total`
    /// (#989), measured over EXACTLY the same population as the filtered counter.
    ///
    /// IMPORTANT — counted population (matches the increment in `handle_msg`):
    /// both this counter and `relay_layer_filtered_total` are incremented ONLY
    /// inside the layer-filter gate, which requires ALL of:
    ///   * a layer-filterable media kind (VIDEO / SCREEN / AUDIO), AND
    ///   * a NON-ZERO cleartext `simulcast_layer_id` (layer 0 / base is forwarded
    ///     BEFORE this gate and is therefore NOT counted here), AND
    ///   * `LayerPrefs::has_any()` true (this receiver has recorded at least one
    ///     LAYER_PREFERENCE — the no-preference / empty-map fast path forwards
    ///     WITHOUT entering the gate and is therefore NOT counted here).
    /// Within that population a packet lands in THIS counter when it is forwarded:
    /// the layer matched the recorded preference, OR there is no recorded entry
    /// for this specific (source, kind) (per-source fail-open), OR the prefs lock
    /// was poisoned / the source was unparseable (fail-open). It lands in
    /// `relay_layer_filtered_total` only when a recorded (source, kind) entry
    /// selects a DIFFERENT layer.
    ///
    /// Because both counters share the identical gate, the "% layer-filtered"
    /// panel `filtered / (filtered + forwarded)` is the fraction of
    /// layer-filterable, non-zero-layer, has-prefs traffic that was dropped — it
    /// deliberately does NOT include layer-0 or no-preference forwards in the
    /// denominator (those never enter either counter), which is what makes the
    /// ratio a clean drop rate over the population the filter actually acts on.
    ///
    /// CARDINALITY: `room` only. No per-source/session label (session IDs churn
    /// on reconnect).
    pub static ref RELAY_LAYER_FORWARDED_TOTAL: CounterVec = register_counter_vec!(
        "relay_layer_forwarded_total",
        "Simulcast media packets forwarded by the per-receiver layer filter (non-zero layer + receiver has prefs; matched, no entry for this source/kind, or fail-open). Denominator complement of relay_layer_filtered_total over the SAME gated population — layer-0 and no-preference forwards are NOT counted (#989, doc #1069)",
        &["room"]
    )
    .expect("Failed to create relay_layer_forwarded_total metric");

    /// Per-LAYER distribution of forwarded simulcast media packets, per room
    /// (#1105). Unlike [`RELAY_LAYER_FORWARDED_TOTAL`] (which counts only the
    /// non-base, has-prefs slice and is the filter denominator), this counts
    /// EVERY filterable media packet (VIDEO/SCREEN/AUDIO) that survives both the
    /// viewport (#988) and layer (#989) filters and is about to be forwarded —
    /// including base layer 0 and the no-prefs fail-open case. It therefore
    /// answers "what is the LAYER MIX flowing in this room?" (e.g. 80% L0 / 15%
    /// L1 / 5% L2 → most receivers are constrained to the base layer).
    ///
    /// Increment site is the actual forward path: a packet counted here passed
    /// every drop gate that APPLIES to it and is a forwardable MEDIA packet
    /// (VIDEO survives both the viewport (#988) and layer (#989) filters; AUDIO
    /// and SCREEN are never viewport-gated — VIDEO-only — so they reach the
    /// increment by passing the layer filter alone). It is NOT incremented for
    /// control packets, self-echo drops, observer drops, or any dropped media.
    ///
    /// CARDINALITY: bounded — `room` × exactly 4 `layer_id` buckets
    /// (`"0"`, `"1"`, `"2"`, `"other"`). The wire `simulcast_layer_id` is a
    /// forgeable `u32` OUTSIDE the AEAD seal (#993), so it is BUCKETED in code
    /// before becoming a label: ids 0/1/2 map to their own bucket and EVERY
    /// other value (3..=u32::MAX, including a forged `u32::MAX`) collapses into
    /// the single `"other"` bucket. This makes it IMPOSSIBLE for an attacker (or
    /// a future >3-layer ladder) to create unbounded series via this label. NO
    /// per-source/session/peer label is ever added (session ids churn on
    /// reconnect — that per-client granularity is #1105 item 3, deliberately
    /// deferred).
    pub static ref RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL: CounterVec = register_counter_vec!(
        "relay_layer_forwarded_by_layer_total",
        "Per-layer distribution of forwarded simulcast media packets per room; layer_id bucketed to 0|1|2|other (#1105)",
        &["room", "layer_id"]
    )
    .expect("Failed to create relay_layer_forwarded_by_layer_total metric");

    /// LAYER_PREFERENCE control-packet update outcomes, per room (#989).
    ///
    /// Mirrors [`RELAY_VIEWPORT_UPDATES_TOTAL`]: makes the DoS guards in
    /// `try_intercept_layer_preference` observable (the cap and the rate limit
    /// would otherwise fire silently) and gives plain "LAYER_PREFERENCE
    /// received" visibility via the `accepted` outcome.
    ///
    /// `outcome` is bounded — exactly 5 values. NOTE on co-occurrence (doc #1069):
    /// `truncated` and `layer_id_out_of_bound` are emitted from the SHAPING step
    /// that runs BEFORE the rate-limit / write-lock decision, so they count
    /// shaping *attempts* and are NOT conditioned on the packet being applied.
    /// `accepted` and `rate_limited` are mutually exclusive and emitted from the
    /// later decision. A single packet therefore records at most one of
    /// {`accepted`, `rate_limited`} PLUS, independently, zero or more of
    /// {`truncated`, `layer_id_out_of_bound`}. In particular a packet may record
    /// `truncated` together with `rate_limited` (its shape was over-cap but it
    /// arrived too soon to be applied) — these do NOT imply `accepted`. Treat
    /// `truncated` / `layer_id_out_of_bound` as "malformed-shape attempt rate",
    /// not as "applied with truncation".
    /// - `accepted`:              update was applied to the receiver's map.
    /// - `rate_limited`:          arrived within
    ///   `LAYER_PREFERENCE_MIN_UPDATE_INTERVAL` of the last accepted update;
    ///   consumed but ignored.
    /// - `truncated`:             the entries list exceeded
    ///   `LAYER_PREFERENCE_MAX_ENTRIES` and the excess was dropped (fail-open).
    ///   Emitted from the pre-lock shaping step regardless of whether the packet
    ///   is then accepted or rate-limited (see the co-occurrence note above).
    /// - `layer_id_out_of_bound`: at least one entry's `desired_layer` exceeded
    ///   `LAYER_PREFERENCE_MAX_LAYER_ID` and was skipped (fail-open per source,
    ///   #1082). Emitted from the same pre-lock shaping step, regardless of
    ///   whether the packet is then accepted or rate-limited.
    /// - `ignored_other_subject`: arrived on a subject other than the receiver's
    ///   own; expected for normal NATS fan-out and dropped without mutating state.
    ///
    /// CARDINALITY: bounded — `room` × 5 outcomes. No per-session label.
    pub static ref RELAY_LAYER_PREFERENCE_UPDATES_TOTAL: CounterVec = register_counter_vec!(
        "relay_layer_preference_updates_total",
        "LAYER_PREFERENCE control-packet update outcomes per room (accepted|rate_limited|truncated|layer_id_out_of_bound|ignored_other_subject) (#989, #1082)",
        &["room", "outcome"]
    )
    .expect("Failed to create relay_layer_preference_updates_total metric");

    /// LAYER_HINT control-packets the relay EMITTED to publishers, per room
    /// (#1108, Stage 3 — publish-side layer suppression).
    ///
    /// Mirrors [`RELAY_LAYER_PREFERENCE_UPDATES_TOTAL`] but for the OUTBOUND
    /// (relay -> publisher) direction: it counts every LAYER_HINT the relay
    /// publishes to a publisher's own self-subject after a per-source layer-union
    /// recompute decided the publisher's encode set should change. Without this
    /// the suppress/restore decisions (and the debounce) would fire silently.
    ///
    /// `direction` is bounded — exactly 2 values:
    /// - `suppress`: a LOWER union than the publisher was last told to encode —
    ///   emitted only after the suppress-lazy debounce window
    ///   ([`crate::constants::LAYER_HINT_SUPPRESS_DEBOUNCE_MS`]) so flaps do not
    ///   thrash a publisher's encoder.
    /// - `restore`:  a HIGHER union (a receiver wants more, or a constraining
    ///   receiver left) — emitted IMMEDIATELY (restore-eager), never debounced.
    ///
    /// Change-detected, unchanged-skip emissions are NOT counted (nothing is
    /// sent). CARDINALITY: bounded — `room` × 2 directions. No per-source/session
    /// label (session IDs churn on reconnect, exactly as for the layer-filter
    /// counters above).
    pub static ref RELAY_LAYER_HINT_EMITTED_TOTAL: CounterVec = register_counter_vec!(
        "relay_layer_hint_emitted_total",
        "LAYER_HINT control-packets emitted by the relay to publishers per room (suppress|restore) (#1108)",
        &["room", "direction"]
    )
    .expect("Failed to create relay_layer_hint_emitted_total metric");

    /// Current outbound channel occupancy per transport
    pub static ref RELAY_OUTBOUND_QUEUE_DEPTH: GaugeVec = register_gauge_vec!(
        "relay_outbound_queue_depth",
        "Current outbound channel occupancy",
        &["room", "transport"]
    )
    .expect("Failed to create relay_outbound_queue_depth metric");

    /// NATS publish latency histogram (milliseconds)
    pub static ref RELAY_NATS_PUBLISH_LATENCY_MS: Histogram = register_histogram!(
        "relay_nats_publish_latency_ms",
        "Time to publish a media packet to NATS (ms)",
        vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0]
    )
    .expect("Failed to create relay_nats_publish_latency_ms metric");

    /// Active sessions per room on this relay instance
    pub static ref RELAY_ACTIVE_SESSIONS_PER_ROOM: GaugeVec = register_gauge_vec!(
        "relay_active_sessions_per_room",
        "Number of active connections per meeting room on this relay",
        &["room", "transport"]
    )
    .expect("Failed to create relay_active_sessions_per_room metric");

    /// Total bytes forwarded per room (use rate() in PromQL for bps)
    pub static ref RELAY_ROOM_BYTES_TOTAL: CounterVec = register_counter_vec!(
        "relay_room_bytes_total",
        "Total bytes forwarded per room (use rate() for bps)",
        &["room", "direction"]
    )
    .expect("Failed to create relay_room_bytes_total metric");

    // ===== AUTH & TRANSPORT TELEMETRY (Phase 8b — TELEM-7, TELEM-8, AUTH-3) =====
    //
    // These counters back the alerting rules that fire when JWT rejection rate
    // or relay outbound-channel drops cross threshold. Designed so on-call can
    // query rate(...)[5m] without log scraping. See discussion #562 Phase 8b.

    /// JWT room-token rejections, labeled by reason.
    ///
    /// CARDINALITY: bounded — exactly 5 series (`token_expired`, `invalid_signature`,
    /// `missing_claim`, `malformed`, `other`). Safe for indefinite retention.
    ///
    /// Incremented from `token_validator::decode_room_token` and
    /// `validate_room_token` on the error-return path so every JWT auth failure
    /// is counted regardless of whether it came from the WS or WT entry point.
    pub static ref AUTH_REJECTIONS_TOTAL: CounterVec = register_counter_vec!(
        "videocall_auth_rejections_total",
        "Total JWT room-token rejections by reason",
        &["reason"]
    )
    .expect("Failed to create videocall_auth_rejections_total metric");
    //
    // CARDINALITY DECISION (dashboard audit Tier B #3 / stale-JWT #562):
    // We deliberately do NOT add a `room`/`meeting_id` label to this counter.
    // The production connection flow is token-based (`GET /lobby?token=<JWT>`):
    // the room is carried ONLY inside the JWT `claims.room`, which is extracted
    // exclusively AFTER a successful `decode_room_token` (see
    // `token_validator::decode_room_token_inner`). The dominant rejection
    // reasons — `token_expired`, `invalid_signature`, `malformed` — fail BEFORE
    // the claims are decoded (or with claims we must not trust), so the room is
    // genuinely unknown at rejection time. There is no room in the URL path for
    // the token flow (only the deprecated FF-off `/lobby/{user}/{room}` path has
    // one). Adding a `room` label would therefore be empty/`""` for the majority
    // of rejections — the "fabricated label that's empty half the time" the audit
    // warned against — and would falsely imply per-meeting attribution the data
    // cannot support. The stale-JWT killer (#562) stays a FLEET-WIDE rate here;
    // per-meeting attribution must come from the meeting-api token-issuance side,
    // not the relay reject path.

    /// Per-session outbound-channel / slow-drain drops, attributable to a named
    /// receiver session (dashboard audit Tier B #1).
    ///
    /// WHY A SEPARATE COUNTER (not a `session_id` label on
    /// `videocall_outbound_channel_drops_total`): session IDs are per-connection
    /// `u64`s that churn on every reconnect/re-election, so the label space is
    /// unbounded over time and would explode storage if mixed into a
    /// long-retained protocol-wide counter. Isolating it here lets us GC the
    /// per-session series the instant the session ends, keeping the live series
    /// count bounded to (active sessions × transport) at any moment — typically
    /// a few hundred for our 10-15 meetings × 20 users scale target, never the
    /// open-ended historical set.
    ///
    /// CARDINALITY BOUND: `room` × `transport` (2) × `session_id` (live only).
    /// CLEANUP (issue #1090): `SessionLogic::on_stopping` (the same per-session
    /// teardown hook that decrements `relay_active_sessions_per_room`) removes
    /// every `(room, transport, session_id, kind)` tuple by iterating the FULL
    /// fixed `kind` taxonomy [`RELAY_DROP_KINDS`] UNCONDITIONALLY — not a
    /// per-session "kinds I emitted" tracking set. `remove_label_values` on a
    /// never-created tuple is a benign `Err`, so a disconnected session leaves
    /// no residual series regardless of which subset it actually incremented.
    /// This is leak-proof by construction: there is no second bookkeeping
    /// structure that could fall out of sync with the emit sites.
    ///
    /// This is the counter Grafana joins against to NAME the slow receiver in a
    /// room ("session 12345 is shedding video") instead of only "a session in
    /// this room is slow". `kind` is the subset of [`RELAY_DROP_KINDS`] the
    /// transport actors pass to `record_session_drop`
    /// (`audio|video|screen|media|control|rtt|unknown|priority_drop_video|
    /// priority_drop_audio|overflow_critical`).
    pub static ref RELAY_SESSION_DROPS_TOTAL: CounterVec = register_counter_vec!(
        "relay_session_drops_total",
        "Per-session outbound-channel drops attributable to a specific receiver session (GC'd on session close). Names the slow receiver for the meeting-investigation dashboard",
        &["room", "transport", "session_id", "kind"]
    )
    .expect("Failed to create relay_session_drops_total metric");

    /// Inbound actor-mailbox overflow drops (dashboard audit Tier B #2; #1057).
    ///
    /// This is the room-wide-freeze signature: when `ChatServer`'s NATS fan-out
    /// does `session_recipient.try_send(Message)` and the receiving session
    /// actor's mailbox is full, the packet cannot be enqueued and is dropped
    /// with NO CONGESTION feedback to the sender at THIS hop (CONGESTION is
    /// owned by the downstream outbound-channel hop via `on_outbound_drop`) —
    /// the failure mode diagnosed in #1057. Pre-#1057 the mailbox was the actix
    /// default (16 slots); post-#1057 it is sized to the outbound channel(s),
    /// and post-#1144 to that × a small burst-headroom factor — but this
    /// counter must still exist so a FUTURE mailbox overflow remains visible
    /// the instant it recurs.
    ///
    /// NOTE (#1145): the DROP itself is no longer attributed indiscriminately.
    /// The room-tagged `relay_packet_drops_total{drop_reason}` now records
    /// WHICH KIND was sacrificed on a `Full` mailbox (`priority_drop_video` /
    /// `priority_drop_audio` for droppable media, `mailbox_full` for
    /// Critical/Control/unclassifiable). THIS aggregate counter, by contrast,
    /// still increments on EVERY inbound-mailbox drop regardless of kind, so
    /// the fleet-wide freeze signature (`rate()` summed over transport) is
    /// unchanged by that per-room attribution split.
    ///
    /// DISTINCT FROM `relay_packet_drops_total{drop_reason="mailbox_full"}`:
    /// that existing series is room-tagged (unbounded `room` label, good for
    /// per-room forensics but noisy for fleet alerting). This counter is the
    /// low-cardinality fleet-alerting sibling — `transport` only — so an SRE can
    /// `rate(relay_inbound_mailbox_drops_total[5m])` to detect the freeze
    /// signature without scraping the per-room series. BOTH are emitted at the
    /// same `try_send`-to-mailbox failure sites; the room-tagged one is kept for
    /// drill-down. `relay_packet_drops_total` keeps `transport="nats_delivery"`
    /// on these (the publish-side identity); this counter uses the actual
    /// receiver transport (`webtransport`|`websocket`) so the freeze can be
    /// attributed to the transport whose mailbox overflowed.
    ///
    /// CARDINALITY BOUND: exactly 2 series (`webtransport`, `websocket`). Safe
    /// for indefinite retention; no cleanup required.
    pub static ref RELAY_INBOUND_MAILBOX_DROPS_TOTAL: CounterVec = register_counter_vec!(
        "relay_inbound_mailbox_drops_total",
        "Inbound actor-mailbox overflow drops by receiver transport (room-wide-freeze signature, #1057). Low-cardinality fleet-alerting sibling of relay_packet_drops_total{drop_reason=mailbox_full}",
        &["transport"]
    )
    .expect("Failed to create relay_inbound_mailbox_drops_total metric");

    /// Inbound WebTransport BRIDGE drops at the socket -> actor-mailbox hop (#1146).
    ///
    /// DISTINCT from `relay_inbound_mailbox_drops_total`: that counter covers the
    /// `ChatServer` NATS-fan-out -> receiving-session-actor mailbox hop (and is
    /// cardinality-bound to exactly the two receiver transports). THIS counter
    /// covers the OTHER inbound hop unique to WebTransport — the per-session
    /// bridge readers (`webtransport/bridge.rs`) that `try_send` freshly read
    /// frames INTO the `WtChatSession` actor's mailbox. Unlike the WS inbound
    /// path (which streams via `StreamHandler`, bypassing the bounded `try_send`),
    /// WT inbound `try_send`s and so can drop. Before #1146 the datagram/audio
    /// path discarded the result entirely (`let _ =`) and the unistream path only
    /// `warn!`ed — both invisible to dashboards/alerts.
    ///
    /// `path` distinguishes `datagram` (carries audio + control) from `unistream`
    /// (media frames) so an inbound-side storm can be attributed to the right
    /// QUIC delivery mode. `transport` is always `webtransport` here (the WS path
    /// cannot hit this site), kept for label-shape parity with the sibling
    /// counters and so a single `relay_inbound_bridge_drops_total` query reads
    /// naturally alongside them.
    ///
    /// CARDINALITY BOUND: at most 2 series (`webtransport` x {datagram,unistream}).
    /// Safe for indefinite retention; no cleanup required.
    pub static ref RELAY_INBOUND_BRIDGE_DROPS_TOTAL: CounterVec = register_counter_vec!(
        "relay_inbound_bridge_drops_total",
        "Inbound WebTransport bridge drops at the socket->actor-mailbox hop, by transport and QUIC path (datagram|unistream) (#1146)",
        &["transport", "path"]
    )
    .expect("Failed to create relay_inbound_bridge_drops_total metric");

    /// Outbound (relay→client) channel drops, labeled by transport and packet kind.
    ///
    /// CARDINALITY: bounded — `transport` is `webtransport`|`websocket` and
    /// `kind` is one of `audio`|`video`|`screen`|`media`|`control`|`rtt`|`unknown`|
    /// `priority_drop_video`|`priority_drop_audio`|`overflow_critical`.
    /// ~20 series total.
    ///
    /// CARDINALITY TRADE-OFF: We deliberately do NOT include `session_id` as a
    /// label — session IDs are unbounded and would explode storage. The existing
    /// `relay_packet_drops_total` carries `room` for room-level attribution; this
    /// new counter is the protocol-wide aggregate that backs alerting (rate()
    /// over 5m). Use `relay_packet_drops_total` for per-room investigation.
    ///
    /// `kind` values (set in `wt_chat_session::drop_kind_label` and the
    /// matching helper in `ws_chat_session`):
    /// - `audio`: MEDIA packet whose inner `MediaPacket.media_type == AUDIO`,
    ///   dropped on a real channel-full event (NOT the new priority policy —
    ///   audio at >=95% goes to `priority_drop_audio` instead).
    ///   Added 2026-05-08 to attribute congestion-storm drops to audio.
    /// - `video`: MEDIA packet whose inner `MediaPacket.media_type == VIDEO`,
    ///   dropped on a real channel-full event.
    /// - `screen`: MEDIA packet whose inner `MediaPacket.media_type == SCREEN`,
    ///   dropped on a real channel-full event.
    /// - `media`: legacy catch-all for MEDIA packets we could not refine —
    ///   encrypted/unparseable inner payloads, HEARTBEAT, KEYFRAME_REQUEST,
    ///   or any future MediaType not in the audio/video/screen set. Kept
    ///   so existing alerts pivoting on `kind="media"` still see a series.
    /// - `control`: any non-media outbound (heartbeats, session-assigned, etc.)
    ///   dropped on a real channel-full event (rare: control should never
    ///   be preempted by the priority policy — see `overflow_critical`).
    /// - `rtt`: RTT echo path that drops on a full datagram queue
    /// - `unknown`: caller could not classify (parse failure / unparsed paths)
    /// - `priority_drop_video`: MEDIA video or screen frame *preemptively*
    ///   dropped at the enqueue site because the per-session outbound
    ///   channel reached the video-drop fill ratio (~80% by default).
    ///   See `actors::priority_drop` for the policy.
    ///   Added 2026-05-11 (discussion #699): under saturation we drop
    ///   video first because audio loss is far worse for UX.
    /// - `priority_drop_audio`: MEDIA audio frame *preemptively* dropped
    ///   because the per-session outbound channel reached the audio-drop
    ///   fill ratio (~95% by default). Should be rare; surfacing this
    ///   non-zero in production means audio is being lost despite the
    ///   priority cushion. Treat as an alerting trigger.
    ///   Added 2026-05-11 (discussion #699).
    /// - `overflow_critical`: a Critical-class control packet
    ///   (SESSION_ASSIGNED, CONGESTION, RSA_PUB_KEY, MEETING) was
    ///   dropped on a real channel-full event. Should be exceptional;
    ///   indicates the channel is so saturated that even the highest-
    ///   priority lifecycle packets cannot be admitted. Page on this.
    ///   Added 2026-05-11 (discussion #699).
    ///
    /// BACKWARDS-COMPAT NOTE FOR DASHBOARDS: queries grouped on `kind="media"`
    /// will only see the catch-all bucket after this change; audio/video/screen
    /// drops now land on their own labels. Update saved Grafana queries with
    /// `kind=~"audio|video|screen|media"` (or sum across) to preserve totals.
    pub static ref OUTBOUND_CHANNEL_DROPS_TOTAL: CounterVec = register_counter_vec!(
        "videocall_outbound_channel_drops_total",
        "Total outbound channel drops (try_send full) by transport and packet kind",
        &["transport", "kind"]
    )
    .expect("Failed to create videocall_outbound_channel_drops_total metric");

    // ===== CLIENT TELEMETRY: TELEM-7, TELEM-8, TELEM-9 =====

    /// TELEM-7: Static per-session client metadata (value always 1, info in labels)
    pub static ref CLIENT_INFO: GaugeVec = register_gauge_vec!(
        "videocall_client_info",
        "Static per-session client metadata (value always 1, info in labels)",
        &["meeting_id", "session_id", "display_name",
          "cores", "architecture", "gpu_family",
          "network_effective_type", "capability_score"]
    )
    .expect("Failed to create videocall_client_info metric");

    /// TELEM-8: Long task duration histogram (main-thread stalls)
    pub static ref CLIENT_LONGTASK_DURATION_MS: HistogramVec = register_histogram_vec!(
        "videocall_client_longtask_duration_ms",
        "Main-thread long task durations observed by the client (ms)",
        &["meeting_id", "session_id", "display_name"],
        vec![50.0, 100.0, 500.0, 1000.0, 5000.0, 10000.0, 30000.0]
    )
    .expect("Failed to create videocall_client_longtask_duration_ms metric");

    /// TELEM-9: Main-thread rAF cadence (frames per second)
    pub static ref CLIENT_RENDER_FPS: GaugeVec = register_gauge_vec!(
        "videocall_client_render_fps",
        "Main-thread rAF cadence (fps)",
        &["meeting_id", "session_id", "display_name"]
    )
    .expect("Failed to create videocall_client_render_fps metric");
}

// =============================================================================
// Phase 8b unit tests
// =============================================================================
//
// These tests verify the counter wiring in isolation. End-to-end behavior is
// covered by `token_validator::tests` (auth) and is the responsibility of the
// integration test suite for the transport drop sites.
#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// Snapshot the counter, mutate, and assert delta. The counter is a global
    /// static so any concurrent test in the same process could alter it; we
    /// gate with `#[serial]` to keep deltas exact.
    fn snapshot(counter: &CounterVec, labels: &[&str]) -> f64 {
        counter.with_label_values(labels).get()
    }

    #[test]
    #[serial(outbound_channel_drops_metric)]
    fn outbound_channel_drops_increments_per_kind() {
        // Cardinality contract: every documented `kind` must be an
        // independent series. The audio/video/screen labels were added
        // 2026-05-08 to refine the legacy `media` bucket; the
        // priority_drop_video/priority_drop_audio/overflow_critical
        // labels were added 2026-05-11 for the priority drop policy
        // (discussion #699). This test also acts as the regression
        // guard against accidental label typos drifting between the
        // helpers and the dashboards.
        let kinds = [
            "audio",
            "video",
            "screen",
            "media",
            "control",
            "rtt",
            "unknown",
            "priority_drop_video",
            "priority_drop_audio",
            "overflow_critical",
        ];
        let before: Vec<f64> = kinds
            .iter()
            .map(|k| snapshot(&OUTBOUND_CHANNEL_DROPS_TOTAL, &["webtransport", k]))
            .collect();

        for k in &kinds {
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["webtransport", k])
                .inc();
        }

        for (i, k) in kinds.iter().enumerate() {
            let after = snapshot(&OUTBOUND_CHANNEL_DROPS_TOTAL, &["webtransport", k]);
            assert_eq!(
                after - before[i],
                1.0,
                "kind={k} should have incremented exactly once"
            );
        }
    }

    #[test]
    #[serial(outbound_channel_drops_metric)]
    fn outbound_channel_drops_distinguishes_transport_label() {
        // Verify that `webtransport` and `websocket` series are independent —
        // bumping one must not bump the other. This is a regression guard:
        // mistakenly hard-coding "webtransport" in the WS path would cause
        // the WS counter to silently stay at zero in production.
        let wt_before = snapshot(&OUTBOUND_CHANNEL_DROPS_TOTAL, &["webtransport", "media"]);
        let ws_before = snapshot(&OUTBOUND_CHANNEL_DROPS_TOTAL, &["websocket", "media"]);
        OUTBOUND_CHANNEL_DROPS_TOTAL
            .with_label_values(&["websocket", "media"])
            .inc();
        let wt_after = snapshot(&OUTBOUND_CHANNEL_DROPS_TOTAL, &["webtransport", "media"]);
        let ws_after = snapshot(&OUTBOUND_CHANNEL_DROPS_TOTAL, &["websocket", "media"]);
        assert_eq!(
            ws_after - ws_before,
            1.0,
            "websocket+media bump should land on the websocket series"
        );
        assert_eq!(
            wt_after - wt_before,
            0.0,
            "websocket+media bump must not leak into the webtransport series"
        );
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_rejections_counter_is_labeled_by_reason() {
        // Cardinality contract: only the five documented reasons are valid
        // labels. This test bumps each one and asserts independence.
        let reasons = [
            "token_expired",
            "invalid_signature",
            "missing_claim",
            "malformed",
            "other",
        ];
        let before: Vec<f64> = reasons
            .iter()
            .map(|r| snapshot(&AUTH_REJECTIONS_TOTAL, &[r]))
            .collect();
        for r in &reasons {
            AUTH_REJECTIONS_TOTAL.with_label_values(&[r]).inc();
        }
        for (i, r) in reasons.iter().enumerate() {
            let after = snapshot(&AUTH_REJECTIONS_TOTAL, &[r]);
            assert_eq!(
                after - before[i],
                1.0,
                "reason={r} should have incremented exactly once"
            );
        }
    }

    // ===== Viewport observability (HCL #988) =====

    #[test]
    #[serial(viewport_forwarded_metric)]
    fn viewport_forwarded_is_labeled_by_room_and_independent_of_filtered() {
        // The forwarded counter is the denominator complement of the filtered
        // counter. Regression guard: the two must be independent series so the
        // "% filtered" panel (filtered / (filtered + forwarded)) is correct;
        // mistakenly bumping both on one decision would skew the ratio.
        let room = "wiretest_room_fwd";
        let fwd_before = snapshot(&RELAY_VIEWPORT_FORWARDED_TOTAL, &[room]);
        let filt_before = snapshot(&RELAY_VIEWPORT_FILTERED_TOTAL, &[room]);

        RELAY_VIEWPORT_FORWARDED_TOTAL
            .with_label_values(&[room])
            .inc();

        assert_eq!(
            snapshot(&RELAY_VIEWPORT_FORWARDED_TOTAL, &[room]) - fwd_before,
            1.0,
            "forwarded bump must land on the forwarded series for this room"
        );
        assert_eq!(
            snapshot(&RELAY_VIEWPORT_FILTERED_TOTAL, &[room]) - filt_before,
            0.0,
            "forwarded bump must NOT leak into the filtered series"
        );
    }

    #[test]
    #[serial(viewport_updates_metric)]
    fn viewport_updates_increments_per_outcome() {
        // Cardinality contract: exactly four outcomes are valid labels. This
        // test bumps each one and asserts independence — a regression guard
        // against label typos drifting between try_intercept_viewport and the
        // dashboard's `outcome=~` breakdown.
        let room = "wiretest_room_upd";
        let outcomes = [
            "accepted",
            "rate_limited",
            "truncated",
            "ignored_other_subject",
        ];
        let before: Vec<f64> = outcomes
            .iter()
            .map(|o| snapshot(&RELAY_VIEWPORT_UPDATES_TOTAL, &[room, o]))
            .collect();

        for o in &outcomes {
            RELAY_VIEWPORT_UPDATES_TOTAL
                .with_label_values(&[room, o])
                .inc();
        }

        for (i, o) in outcomes.iter().enumerate() {
            let after = snapshot(&RELAY_VIEWPORT_UPDATES_TOTAL, &[room, o]);
            assert_eq!(
                after - before[i],
                1.0,
                "outcome={o} should have incremented exactly once"
            );
        }
    }

    #[test]
    #[serial(viewport_set_size_metric)]
    fn viewport_set_size_gauge_observes_and_removes_per_room() {
        // The set-size gauge is a GaugeVec (NOT a counter) so the per-room
        // series can be torn down when the room drains. This test verifies the
        // set/observe semantics AND that remove_label_values drops the series
        // (the cleanup contract relied on by forget_room_if_empty).
        let room = "wiretest_room_size";

        RELAY_VIEWPORT_SET_SIZE.with_label_values(&[room]).set(5.0);
        assert_eq!(
            RELAY_VIEWPORT_SET_SIZE.with_label_values(&[room]).get(),
            5.0,
            "gauge must reflect the last observed set size"
        );

        // Collapse-toward-1 signature is just a smaller observation.
        RELAY_VIEWPORT_SET_SIZE.with_label_values(&[room]).set(1.0);
        assert_eq!(
            RELAY_VIEWPORT_SET_SIZE.with_label_values(&[room]).get(),
            1.0
        );

        // Room drained: the series must be removable so it does not read its
        // last value forever for a dead room.
        RELAY_VIEWPORT_SET_SIZE
            .remove_label_values(&[room])
            .expect("series for an active room must be removable");
        // After removal a fresh handle starts at the gauge default (0.0).
        assert_eq!(
            RELAY_VIEWPORT_SET_SIZE.with_label_values(&[room]).get(),
            0.0,
            "removed series must not retain its prior value"
        );
    }

    // ===== Room-label cardinality bound (issue #996) + drop-kind GC (#1090) =====

    /// `RELAY_DROP_KINDS` is the single source of truth for the room-drain GC
    /// (`forget_room_metrics`) and the per-session GC (`on_stopping`). If it
    /// stops covering a `kind`/`drop_reason` the emit sites actually use, that
    /// series would leak forever. This pins it as a SUPERSET of:
    ///   * the `videocall_outbound_channel_drops_total` / `relay_session_drops_total`
    ///     `kind` taxonomy (the same literals asserted by
    ///     `outbound_channel_drops_increments_per_kind`), and
    ///   * the `relay_packet_drops_total` `drop_reason` literals emitted by the
    ///     fan-out and transport hops.
    ///
    /// Mutating `RELAY_DROP_KINDS` to drop any of these fails this test.
    #[test]
    fn relay_drop_kinds_covers_all_emitted_drop_labels() {
        // Mirror of the literals in `outbound_channel_drops_increments_per_kind`
        // (kept as an independent copy ON PURPOSE so this test references a
        // second witness of the taxonomy, not the const under test).
        let outbound_kinds = [
            "audio",
            "video",
            "screen",
            "media",
            "control",
            "rtt",
            "unknown",
            "priority_drop_video",
            "priority_drop_audio",
            "overflow_critical",
        ];
        // The `drop_reason` literals passed to `relay_packet_drops_total` in
        // `chat_server::handle_msg` (fan-out) and the WS/WT `Handler<Message>`
        // hops. `priority_drop_*` overlap with the outbound set above.
        let packet_drop_reasons = ["mailbox_full", "channel_full"];

        for k in outbound_kinds.iter().chain(packet_drop_reasons.iter()) {
            assert!(
                RELAY_DROP_KINDS.contains(k),
                "RELAY_DROP_KINDS must cover emitted drop label {k:?} or the \
                 room-drain / session GC would leak its series (issues #996/#1090)"
            );
        }
    }

    /// `RELAY_DROP_TRANSPORTS` must cover every `transport` value emitted to
    /// `relay_packet_drops_total` so the room-drain GC removes every
    /// `(room, transport, drop_reason)` tuple.
    #[test]
    fn relay_drop_transports_covers_all_emitted_transports() {
        // `websocket`/`webtransport` from the per-transport `Handler<Message>`
        // hops; `nats_delivery` from the inbound fan-out hop in handle_msg.
        for t in ["websocket", "webtransport", "nats_delivery"] {
            assert!(
                RELAY_DROP_TRANSPORTS.contains(&t),
                "RELAY_DROP_TRANSPORTS must cover emitted transport {t:?} or the \
                 room-drain GC would leak relay_packet_drops_total series (#996)"
            );
        }
    }

    /// REAL ENFORCEMENT of the #996 bound: after `forget_room_metrics(room)`,
    /// EVERY room-labeled relay series for that room must be gone. We seed one
    /// series per metric (covering the multi-label counters with a representative
    /// tuple from each bounded taxonomy), drain the room, and assert each handle
    /// re-reads as the default (0) — proving the prior series was removed, not
    /// merely zeroed. A removed counter starts a fresh series at 0 on the next
    /// `with_label_values`, so `get() == 0.0` after seeding `!= 0` proves removal.
    #[test]
    #[serial(forget_room_metrics)]
    fn forget_room_metrics_removes_every_room_series() {
        let room = "wiretest_forget_room_996";

        // Seed one series on each room-labeled metric.
        RELAY_VIEWPORT_FILTERED_TOTAL
            .with_label_values(&[room])
            .inc();
        RELAY_VIEWPORT_FORWARDED_TOTAL
            .with_label_values(&[room])
            .inc();
        RELAY_LAYER_FILTERED_TOTAL.with_label_values(&[room]).inc();
        RELAY_LAYER_FORWARDED_TOTAL.with_label_values(&[room]).inc();
        RELAY_ROOM_BYTES_TOTAL
            .with_label_values(&[room, "outbound"])
            .inc();
        RELAY_VIEWPORT_UPDATES_TOTAL
            .with_label_values(&[room, "accepted"])
            .inc();
        RELAY_LAYER_PREFERENCE_UPDATES_TOTAL
            .with_label_values(&[room, "accepted"])
            .inc();
        RELAY_LAYER_HINT_EMITTED_TOTAL
            .with_label_values(&[room, "suppress"])
            .inc();
        RELAY_PACKET_DROPS_TOTAL
            .with_label_values(&[room, "nats_delivery", "mailbox_full"])
            .inc();
        RELAY_PACKET_DROPS_TOTAL
            .with_label_values(&[room, "websocket", "priority_drop_video"])
            .inc();
        RELAY_OUTBOUND_QUEUE_DEPTH
            .with_label_values(&[room, "websocket"])
            .set(7.0);
        RELAY_ACTIVE_SESSIONS_PER_ROOM
            .with_label_values(&[room, "webtransport"])
            .set(3.0);
        RELAY_VIEWPORT_SET_SIZE.with_label_values(&[room]).set(4.0);

        // Confirm the seeds are non-zero (otherwise the post-removal assert
        // below would pass vacuously — Adversarial check #2).
        assert_eq!(
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[room, "nats_delivery", "mailbox_full"])
                .get(),
            1.0,
            "seed must be observable before removal, else the test is vacuous"
        );
        assert_eq!(
            RELAY_OUTBOUND_QUEUE_DEPTH
                .with_label_values(&[room, "websocket"])
                .get(),
            7.0
        );

        // Drain the room.
        forget_room_metrics(room);

        // Every room-labeled series must now be gone. A removed series reads
        // back at the type default on a fresh handle.
        assert_eq!(
            RELAY_VIEWPORT_FILTERED_TOTAL
                .with_label_values(&[room])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_VIEWPORT_FORWARDED_TOTAL
                .with_label_values(&[room])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_LAYER_FILTERED_TOTAL.with_label_values(&[room]).get(),
            0.0
        );
        assert_eq!(
            RELAY_LAYER_FORWARDED_TOTAL.with_label_values(&[room]).get(),
            0.0
        );
        assert_eq!(
            RELAY_ROOM_BYTES_TOTAL
                .with_label_values(&[room, "outbound"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_VIEWPORT_UPDATES_TOTAL
                .with_label_values(&[room, "accepted"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_LAYER_PREFERENCE_UPDATES_TOTAL
                .with_label_values(&[room, "accepted"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_LAYER_HINT_EMITTED_TOTAL
                .with_label_values(&[room, "suppress"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[room, "nats_delivery", "mailbox_full"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[room, "websocket", "priority_drop_video"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_OUTBOUND_QUEUE_DEPTH
                .with_label_values(&[room, "websocket"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_ACTIVE_SESSIONS_PER_ROOM
                .with_label_values(&[room, "webtransport"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_VIEWPORT_SET_SIZE.with_label_values(&[room]).get(),
            0.0
        );

        // Clean up the fresh zero-valued handles created by the asserts above so
        // this test leaves no residue for other serial runs.
        forget_room_metrics(room);
    }

    /// #1090 leak-proof property: iterating the FULL `RELAY_DROP_KINDS` taxonomy
    /// on teardown removes a session's `relay_session_drops_total` series EVEN
    /// for kinds that session never incremented — i.e. the GC does not depend on
    /// a per-session "kinds I emitted" tracking set. This replicates the exact
    /// loop `SessionLogic::on_stopping` runs (same const, same label order).
    #[test]
    #[serial(session_drops_gc)]
    fn session_drop_gc_iterates_full_taxonomy_unconditionally() {
        let room = "wiretest_session_gc_1090";
        let transport = "websocket";
        let session_id = "999000111";

        // This session only ever dropped ONE kind.
        RELAY_SESSION_DROPS_TOTAL
            .with_label_values(&[room, transport, session_id, "video"])
            .inc();
        assert_eq!(
            RELAY_SESSION_DROPS_TOTAL
                .with_label_values(&[room, transport, session_id, "video"])
                .get(),
            1.0,
            "seed must be observable before teardown (non-vacuous)"
        );

        // Replicate on_stopping: iterate the FULL taxonomy unconditionally.
        // `remove_label_values` on a never-created (…, kind) tuple is a benign
        // Err, so a session that only dropped `video` is still fully cleaned.
        for kind in RELAY_DROP_KINDS {
            let _ =
                RELAY_SESSION_DROPS_TOTAL.remove_label_values(&[room, transport, session_id, kind]);
        }

        // The seeded `video` series — a member of the taxonomy the session DID
        // increment — must be gone.
        assert_eq!(
            RELAY_SESSION_DROPS_TOTAL
                .with_label_values(&[room, transport, session_id, "video"])
                .get(),
            0.0,
            "the full-taxonomy sweep must remove the kind the session emitted"
        );

        // Clean up the fresh zero handle this assert created.
        let _ =
            RELAY_SESSION_DROPS_TOTAL.remove_label_values(&[room, transport, session_id, "video"]);
    }
}
