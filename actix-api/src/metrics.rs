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
// that authoritative enumeration.
//
// `metrics::tests::relay_drop_kinds_covers_all_emitted_drop_labels` cross-checks
// that `RELAY_DROP_KINDS` covers the labels the code emits, with two tiers of
// guarantee (issue #1186):
//   * For the `kind` labels produced by FUNCTIONS — `drop_kind_label`
//     (ws/wt transports) and `OutboundPriority::priority_drop_label` — the test
//     ENUMERATES the real emit functions over their full input domains
//     (`MediaType::VALUES`, all `OutboundPriority` variants). A NEW label
//     returned by either function is therefore caught automatically, and a new
//     `MediaType` / `OutboundPriority` variant that changes the output is forced
//     into the enumeration. This tier cannot silently leak a new kind.
//   * For the bare string LITERALS emitted with no enumerable source of truth
//     (`mailbox_full`, `channel_full` at the fan-out / transport drop sites and
//     `overflow_critical` at the Critical-overflow site) the test keeps a
//     hand-maintained witness list. That tier guards against DELETIONS from
//     `RELAY_DROP_KINDS` only — a brand-new literal added at one of those sites
//     without updating both the const and the witness would NOT be caught.

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

/// Every `kind` label value of `relay_layer_preference_sessions{room, kind,
/// layer_id}` (#1170 demand-side gauge). Only the two media kinds that carry a
/// simulcast ladder are tracked — AUDIO has no layers so it is intentionally
/// absent (a receiver never expresses a per-layer preference for audio).
pub const RELAY_LAYER_PREFERENCE_KINDS: &[&str] = &["video", "screen"];

/// Every `layer_id` bucket label value shared by the per-layer relay metrics
/// (`relay_layer_forwarded_by_layer_total` #1105 and
/// `relay_layer_preference_sessions` #1170). The wire `simulcast_layer_id` is a
/// forgeable `u32` outside the AEAD seal, so it is BUCKETED in code (0/1/2 to
/// their own bucket, everything else to `"other"`) before becoming a label —
/// capping this label to EXACTLY 4 values regardless of what arrives on the
/// wire. This array is the room-drain GC's source of truth for those four
/// buckets.
pub const RELAY_LAYER_ID_BUCKETS: &[&str] = &["0", "1", "2", "other"];

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
    // relay_viewport_nonvideo_at_drop_branch_total{room} (#1437): single-label
    // `room`, one tuple — GC'd like its FILTERED/FORWARDED siblings.
    let _ = RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL.remove_label_values(&[room]);
    let _ = RELAY_LAYER_FILTERED_TOTAL.remove_label_values(&[room]);
    let _ = RELAY_LAYER_FORWARDED_TOTAL.remove_label_values(&[room]);
    // relay_congestion_filtered_total{room} (#1220).
    let _ = RELAY_CONGESTION_FILTERED_TOTAL.remove_label_values(&[room]);
    // relay_inner_session_self_filtered_total{room} (#618, #629).
    let _ = RELAY_INNER_SESSION_SELF_FILTERED_TOTAL.remove_label_values(&[room]);
    // relay_downlink_congestion_filtered_total{room} (#1219 Half 2) — same
    // room-keyed unicast-filter sibling; swept alongside its CONGESTION cousin.
    let _ = RELAY_DOWNLINK_CONGESTION_FILTERED_TOTAL.remove_label_values(&[room]);

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

    // relay_layer_preference_sessions{room, kind, layer_id} (#1170): full
    // cartesian product of the two bounded taxonomies. The periodic sweep
    // re-sets these (including zeros) for LIVE rooms; once a room drains the
    // sweep stops writing it, so its last values would otherwise linger — this
    // removal erases them at drain time.
    for kind in RELAY_LAYER_PREFERENCE_KINDS {
        for bucket in RELAY_LAYER_ID_BUCKETS {
            let _ = RELAY_LAYER_PREFERENCE_SESSIONS.remove_label_values(&[room, kind, bucket]);
        }
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

/// Remove a single session's `relay_session_drops_total` series across the FULL
/// drop-kind taxonomy (issue #1090).
///
/// `relay_session_drops_total{room, transport, session_id, kind}` carries an
/// unbounded-over-time `session_id` label, so the series for a disconnected
/// session must be removed the moment its actor stops or it leaks for the
/// process lifetime. This is the single source of truth for that sweep: it is
/// called by `SessionLogic::on_stopping` (the runtime path) AND pinned directly
/// by `metrics::tests::session_drop_gc_iterates_full_taxonomy_unconditionally`,
/// so the test exercises the real GC code instead of an inline replica of it.
/// (That test pins THIS helper's full-taxonomy sweep; it does not exercise the
/// `on_stopping` call site itself — see #1380.)
///
/// LEAK-PROOF: we iterate the entire fixed [`RELAY_DROP_KINDS`] taxonomy
/// unconditionally rather than a per-session "kinds I emitted" tracking set.
/// `remove_label_values` on a `(…, kind)` tuple that was never created returns a
/// benign `Err` (hence each call is `let _ =`-discarded), so a session that only
/// ever incremented a subset of kinds is still fully cleaned, and there is no
/// second bookkeeping structure that could silently fall out of sync.
pub fn forget_session_drops(room: &str, transport: &str, session_id: &str) {
    for kind in RELAY_DROP_KINDS {
        let _ = RELAY_SESSION_DROPS_TOTAL.remove_label_values(&[room, transport, session_id, kind]);
    }
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

    /// Per-peer audio playout latency in ms (#1299): how far behind live a receiver's audio
    /// playout sits — NetEQ's WebRTC-style FILTERED current buffer level (the EWMA-smoothed
    /// playout buffer the Accelerate gate compares against high_limit). The audio sibling of
    /// `videocall_video_playout_latency_ms` (#1252). Distinct from `videocall_neteq_audio_buffer_ms`
    /// (the RAW instantaneous snapshot) and `videocall_neteq_target_delay_ms` (the TARGET, not the
    /// actual level). 0 = at live; a sustained multi-second value is the #1299 lag. Set
    /// UNCONDITIONALLY so the gauge recovers to 0 when audio catches back up to live.
    /// Observability only — no governor/resync attached.
    pub static ref AUDIO_PLAYOUT_LATENCY_MS: GaugeVec = register_gauge_vec!(
        "videocall_audio_playout_latency_ms",
        "Per-peer audio playout latency in ms (how far behind live); NetEQ filtered playout buffer level. Sustained multi-second => #1299 lag",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create audio_playout_latency_ms metric");

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

    /// Per-peer buffered video playout latency in ms (#1252): how far behind live a receiver's
    /// decoded video is, spanning the jitter-buffer backlog (stage 1) + WebCodecs decoder queue
    /// (stage 2). Reported only while the tile is actively receiving (fps_received > 0); 0 = at
    /// live. Sustained > 1800ms is the #1252 audio-ahead-of-video lag.
    pub static ref VIDEO_PLAYOUT_LATENCY_MS: GaugeVec = register_gauge_vec!(
        "videocall_video_playout_latency_ms",
        "Per-peer buffered video playout latency in ms (how far behind live); spans jitter-buffer backlog + decoder queue. Sustained >1800ms => #1252 lag",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_playout_latency_ms metric");

    /// Per-peer stage-1 attribution of `videocall_video_playout_latency_ms` (#1252): the
    /// jitter-buffer backlog span alone. Lets a dashboard tell whether the #1024 release-side gate
    /// REMOVED the backlog (total drops with this) or merely RELOCATED it into the decoder queue
    /// (total stays high while this drops).
    pub static ref VIDEO_PLAYOUT_STAGE1_SPAN_MS: GaugeVec = register_gauge_vec!(
        "videocall_video_playout_stage1_span_ms",
        "Per-peer jitter-buffer backlog span in ms — stage-1 attribution of videocall_video_playout_latency_ms",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_playout_stage1_span_ms metric");

    /// Per-peer stage-3 paint lag in ms (#1252): decoded-but-unpainted frames living in the
    /// worker->main `postMessage` queue + main-thread paint task queue — a region the stage-2
    /// decoder-queue depth cannot observe. Computed in the worker as
    /// (frames_emitted − frames_painted) × source frame interval. Reported only while the tile is
    /// actively receiving (fps_received > 0); 0 = at live. Complements
    /// `videocall_video_playout_latency_ms`: if total lag stays high while latency_ms is low, the
    /// backlog has relocated downstream into the paint path.
    pub static ref VIDEO_PLAYOUT_PAINT_LAG_MS: GaugeVec = register_gauge_vec!(
        "videocall_video_playout_paint_lag_ms",
        "Per-peer stage-3 paint lag in ms — decoded-but-unpainted backlog in the worker->main postMessage + paint queues (#1252)",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_playout_paint_lag_ms metric");

    /// Per-peer content staleness in ms (#1641): the AGE of the video content currently being
    /// painted — how old the just-painted frame's content is relative to live — as distinct from
    /// `videocall_video_playout_paint_lag_ms`, which measures queue DEPTH (decoded-but-unpainted
    /// backlog). A receiver that drains a stale backlog can keep paint_lag near 0 while still
    /// painting minutes-old content; this gauge surfaces that age. Reported only while the tile is
    /// actively receiving (fps_received > 0); 0 = at live. UNBOUNDED, unlike
    /// `videocall_video_playout_latency_ms` whose client-side value is capped at 1800ms — so this is
    /// the metric that exposes the #1631 M2 "video lagged by minutes while playout_latency_ms read
    /// ~0" failure mode that the capped/queue-depth gauges structurally cannot show.
    pub static ref VIDEO_CONTENT_STALENESS_MS: GaugeVec = register_gauge_vec!(
        "videocall_video_content_staleness_ms",
        "Per-peer content age in ms of the painted video — content-staleness (#1641); UNBOUNDED (unlike playout_latency_ms's 1800ms cap). A stream draining stale content keeps paint_lag ~0 while this climbs; surfaces the #1631 M2 minutes-of-lag",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_content_staleness_ms metric");

    /// Per-peer cumulative count of resync-to-live governor skips (#1252): how many times the
    /// decode-side governor jumped this receiver→source stream forward to live to shed accumulated
    /// lag. A COUNTER value held in a GaugeVec (set to the current cumulative total) so the per-pair
    /// `remove_label_values` cleanup GCs it with the sibling playout gauges. It rises within a
    /// decoder-pipeline lifetime but resets to 0 on the client's `reset_pipeline()` (decoder-error
    /// recovery), so query with `increase()`/`rate()`, which tolerate the reset. Unlike the ms
    /// gauges it is reported unconditionally (even at fps 0). A rising value proves the governor fired.
    pub static ref VIDEO_SKIP_TO_LIVE_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_video_skip_to_live_total",
        "Cumulative resync-to-live governor skips per receiver→source pair (#1252); a rising value proves the governor fired",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_skip_to_live_total metric");

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

    /// RTT-probe drop backpressure (#522): cumulative count of RTT probes the
    /// client dropped because the in-flight probe queue was at its cap, as of the
    /// latest client health snapshot. GaugeVec set() to the client's cumulative
    /// value (NOT inc()); chart with rate()/increase(). See the CLIENT_REELECTION_TOTAL
    /// type-decision note.
    pub static ref RTT_PROBE_DROPPED_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_rtt_probe_dropped_total",
        "Cumulative RTT probes dropped at the client's in-flight queue cap (#522 backpressure) as of the latest client health snapshot",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create rtt_probe_dropped_total metric");

    /// RTT-probe stale-suppression count (#522): cumulative count of 1 Hz client
    /// diagnostics ticks on which the active link's RTT-probe pipeline was stale and
    /// active_server_rtt was suppressed, as of the latest client health snapshot.
    /// GaugeVec set() to the client's cumulative value (NOT inc()).
    pub static ref RTT_PROBE_STALE_SUPPRESSIONS_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_rtt_probe_stale_suppressions_total",
        "Cumulative client diagnostics ticks where the active link's RTT-probe pipeline was stale and active_server_rtt was suppressed (#522) as of the latest client health snapshot",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create rtt_probe_stale_suppressions_total metric");

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

    /// Cumulative encoder auto-restart cycles reported by the client (#527),
    /// partitioned by encoder `kind` (camera|screen) and `reason`
    /// (closed_codec|memory|configure|other).
    ///
    /// TYPE DECISION — GaugeVec, NOT CounterVec: identical reasoning to
    /// CLIENT_REELECTION_TOTAL above. The client reports a CUMULATIVE per-reason
    /// total in every health packet; the expander `.set()`s this gauge to that
    /// value. `.inc()`-ing a CounterVec per packet would multiply-count the same
    /// cumulative value once per second. The client value is monotonic within a
    /// page session, so Grafana charts restart RATE with `rate()`/`increase()`
    /// exactly as for the sibling `*_total` gauges; a page reload that resets the
    /// client statics shows as a gauge drop (the same accepted caveat).
    ///
    /// CARDINALITY: `meeting_id` × `session_id` × 2 `kind` × 4 `reason` (bounded).
    /// Per-session series are GC'd by the metrics-server's stale-session cleanup.
    pub static ref ENCODER_RESTART_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_encoder_restart_total",
        "Cumulative encoder auto-restart cycles reported by the client, by encoder kind (camera|screen) and reason (closed_codec|memory|configure|other). GaugeVec set() to the client's cumulative value; chart with rate()/increase() (#527)",
        &["meeting_id", "session_id", "kind", "reason"]
    )
    .expect("Failed to create videocall_encoder_restart_total metric");

    // ===== ENCODER & SCREEN SHARE METRICS (sender-side, P0/P1) =====
    // NOTE(#1184): videocall_encoder_fps_ratio / videocall_encoder_bitrate_ratio
    // removed — dead telemetry whose source proto fields no longer exist.

    /// Encoder queue depth signal driving encoder (sender-side) decisions.
    pub static ref ENCODER_QUEUE_DEPTH: GaugeVec = register_gauge_vec!(
        "videocall_encoder_queue_depth",
        "Encoder queue depth (sender-side backpressure signal driving adaptive quality decisions)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create encoder_queue_depth metric");

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

    /// Client battery level (0.0–1.0) as a NUMERIC gauge (#1392). PR #1368 widened
    /// the TELEM-7 `CLIENT_INFO` publish gate to admit a battery-only health packet,
    /// but the battery *value* rode on no metric — `CLIENT_INFO`'s labels are
    /// cores/architecture/gpu_family/network_effective_type/capability_score only.
    /// This exposes the reported level as a real measurement so it can be
    /// thresholded/averaged/quantiled in PromQL (e.g. "fraction of clients under
    /// 20% battery"). Mirrors `CAPABILITY_SCORE`: same per-reporter label set, set
    /// only when actually reported so an absent battery stays absent (not a
    /// misleading 0).
    pub static ref BATTERY_LEVEL: GaugeVec = register_gauge_vec!(
        "videocall_client_battery_level",
        "Client battery level as a numeric value in [0,1] (0.0 = empty, 1.0 = full); absent when the client did not report a battery level",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_battery_level metric");

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

    /// Audio congestion ceiling: the congestion-driven dynamic layer cap (#1561).
    /// Distinct from active layers: this is only the runtime cap applied under
    /// congestion. In the uncapped state the exporter reports the effective
    /// ladder depth so dashboards can compare the two without a sentinel value.
    pub static ref AUDIO_CONGESTION_CEILING: GaugeVec = register_gauge_vec!(
        "videocall_audio_congestion_ceiling",
        "Audio congestion-driven layer ceiling; equals effective audio layers when uncapped and is lower while congestion shedding is active",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create audio_congestion_ceiling metric");

    /// Receiver-side layer selection: which simulcast layer THIS receiver is
    /// subscribing to from a given peer for a given media kind (#1561).
    pub static ref RECEIVED_LAYER: GaugeVec = register_gauge_vec!(
        "videocall_received_layer",
        "Simulcast layer index (0=base) this receiver has chosen for a given peer and media kind; absent when receiving the top layer (unconstrained)",
        &["meeting_id", "session_id", "peer_id", "display_name", "from_peer", "media_kind"]
    )
    .expect("Failed to create received_layer metric");

    /// Battery charging state (#1556): 1.0 = charging, 0.0 = discharging.
    pub static ref BATTERY_CHARGING: GaugeVec = register_gauge_vec!(
        "videocall_client_battery_charging",
        "Battery charging state (1=charging, 0=discharging); absent when battery API unavailable",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_battery_charging metric");

    /// Connection medium type (#1556): exposed as a dedicated gauge with value=1
    /// and the type as a label, since CLIENT_INFO already has many labels.
    pub static ref CLIENT_NETWORK_TYPE: GaugeVec = register_gauge_vec!(
        "videocall_client_network_type",
        "Connection medium indicator (value=1); network_type label carries the medium (wifi/ethernet/cellular)",
        &["meeting_id", "session_id", "peer_id", "display_name", "network_type"]
    )
    .expect("Failed to create client_network_type metric");

    /// Max downlink speed of the connection medium in Mbps (#1556).
    pub static ref CLIENT_NETWORK_DOWNLINK_MAX: GaugeVec = register_gauge_vec!(
        "videocall_client_network_downlink_max",
        "Max downlink speed of the connection medium in Mbps (e.g. wifi=54, ethernet=1000)",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_network_downlink_max metric");

    /// CPU throttle indicator (#1556): 1.0 when capability_score/cores < 150.
    pub static ref CLIENT_CPU_THROTTLED: GaugeVec = register_gauge_vec!(
        "videocall_client_cpu_throttled",
        "CPU throttle indicator (1=throttled, 0=normal); based on capability_score/cores ratio < 150",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create client_cpu_throttled metric");

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

    /// #1437 invariant tripwire — MUST ALWAYS BE 0 in production.
    ///
    /// Counts the IMPOSSIBLE case: a NON-VIDEO packet reaching the viewport
    /// drop-decision site. The viewport filter is VIDEO-only and is guarded by
    /// the `is_video` check at `chat_server.rs` (~line 4903, HCL #988); by
    /// construction nothing but `MediaKind::VIDEO` can reach the drop branch, so
    /// this counter can only increment if a FUTURE REFACTOR bypasses or widens
    /// that `is_video` guard and lets AUDIO/SCREEN/unknown reach the viewport
    /// drop-decision site. A NONZERO value is therefore a STRUCTURAL-INVARIANT
    /// BREACH, not a normal viewport drop — it means the #988 guard regressed,
    /// and should page (see the #1437 alert in alert_rules.yml). It is wired
    /// purely as observability (a `debug_assert!` + this counter); it never
    /// changes forwarding behavior. See #1437, #988.
    ///
    /// CARDINALITY: `room` only (user-provided, unbounded over time) — same
    /// caveat as the other room-labeled counters above. Swept by
    /// `forget_room_metrics` when the room drains.
    pub static ref RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL: CounterVec = register_counter_vec!(
        "relay_viewport_nonvideo_at_drop_branch_total",
        "INVARIANT TRIPWIRE (must always be 0): non-VIDEO packets that reached the viewport drop-decision site — only possible if the #988 is_video guard regresses (HCL #1437)",
        &["room"]
    )
    .expect("Failed to create relay_viewport_nonvideo_at_drop_branch_total metric");

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

    /// CONGESTION packets dropped at the relay because the receiving session was
    /// NOT the target (#1220 — CONGESTION unicast-correctness).
    ///
    /// CONGESTION is relay-authored and self-ADDRESSED: it is published onto the
    /// target sender's own per-session subject (`room.{room}.{sender_sid}`), but
    /// the NATS room wildcard delivers it to EVERY session in the room. Before
    /// #1220 each NON-target receiver forwarded it to its transport, where the
    /// client discarded it. This counter records each such non-target CONGESTION
    /// dropped at the relay BEFORE the transport hop (subject/`session_id`
    /// scoping — see the filter in `chat_server.rs::handle_msg`). It is an
    /// EXPECTED, fan-out-correctness drop — like
    /// [`RELAY_VIEWPORT_FILTERED_TOTAL`] / [`RELAY_LAYER_FILTERED_TOTAL`] it is
    /// deliberately kept OFF `relay_packet_drops_total` (which is backpressure
    /// loss). In a healthy N-person room you expect ~(N-1) increments here per 1
    /// CONGESTION actually delivered.
    ///
    /// CARDINALITY: `room` only (user-provided, unbounded over time), same
    /// caveats as the other room-labeled counters above.
    pub static ref RELAY_CONGESTION_FILTERED_TOTAL: CounterVec = register_counter_vec!(
        "relay_congestion_filtered_total",
        "Total CONGESTION packets dropped at the relay for non-target receivers (self-addressed control packet not destined for this session) (#1220)",
        &["room"]
    )
    .expect("Failed to create relay_congestion_filtered_total metric");

    /// Packets dropped at the relay because the embedded inner `session_id`
    /// matched the receiver's OWN current session even though the NATS subject
    /// pointed at a STALE (post-reconnect) session — the #618 leak (#629 — this
    /// metric).
    ///
    /// On 2026-05-08 a production meeting leaked self-DIAGNOSTICS: a stale
    /// subscription survived a reconnect and delivered a packet whose subject
    /// differed from the receiver's CURRENT session but whose embedded
    /// `session_id` (stamped by `Handler<ClientMessage>`) still belonged to this
    /// connection. The subject-only self-skip missed it; #618 added the
    /// inner-`session_id` self-skip (`inner_session_self` in
    /// `chat_server.rs::handle_msg`) to close the leak. This counter records each
    /// packet dropped where the inner `session_id` matched our own session BUT
    /// the subject did NOT (`inner_session_self && !subject_self`) — i.e. the
    /// subject pointed at a stale/different session: the post-reconnect leak
    /// shape and nothing else.
    ///
    /// It does NOT count ordinary `subject_self` self-echoes. Those are normal
    /// and uninteresting, and — critically — they ALSO have `inner_session_self`
    /// true: the relay stamps `session_id = session` on publish (the publish
    /// path in `chat_server.rs`), so a routine echo arrives with BOTH
    /// `subject_self` AND `inner_session_self` true. The `!subject_self` arm in
    /// the increment guard is what excludes them, so the leak signal is not
    /// drowned by routine self-echo volume. It is an EXPECTED,
    /// fan-out-/reconnect-correctness drop — like
    /// [`RELAY_CONGESTION_FILTERED_TOTAL`] / [`RELAY_VIEWPORT_FILTERED_TOTAL`] /
    /// [`RELAY_LAYER_FILTERED_TOTAL`] it is deliberately kept OFF
    /// `relay_packet_drops_total` (which is backpressure loss). #629 wants
    /// operators to see the rate of THIS specific filter so a recurrence of the
    /// stale-subscription leak is observable.
    ///
    /// CARDINALITY: `room` only (user-provided, unbounded over time; bounded to
    /// live rooms by the room-drain GC `forget_room_metrics`), same caveats as
    /// the other room-labeled counters above.
    pub static ref RELAY_INNER_SESSION_SELF_FILTERED_TOTAL: CounterVec = register_counter_vec!(
        "relay_inner_session_self_filtered_total",
        "Total packets dropped at the relay where the inner session_id matched the receiver's own session but the subject did NOT (stale post-reconnect subject); excludes routine subject-matched self-echoes (#618 leak; #629 metric)",
        &["room"]
    )
    .expect("Failed to create relay_inner_session_self_filtered_total metric");

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

    /// DEMAND-side per-layer distribution: how many receiver sessions currently
    /// request each simulcast layer, per room and media kind (#1170).
    ///
    /// This is the COMPLEMENT of [`RELAY_LAYER_FORWARDED_BY_LAYER_TOTAL`]
    /// (#1105), which counts the FORWARDED side ("what layer mix is flowing").
    /// This gauge answers the other half: "what layer mix are receivers ASKING
    /// for?" — the input to the relay's suppress/restore decisions. Comparing
    /// the two surfaces, e.g., a room where most sessions request L0 but the
    /// relay is still forwarding L2 (a publisher not yet honoring a LAYER_HINT).
    ///
    /// AGGREGATION (the #1170-flagged semantics, decided here): each receiver
    /// session is classified by its MAX requested layer for the kind — the
    /// highest `desired_layer` across every source it has expressed a preference
    /// for — because the relay must produce/forward up to that highest layer for
    /// that receiver, so the max is the single layer that characterizes the
    /// session's demand. This is computed SEPARATELY per `kind` (a receiver may
    /// want low SCREEN but full VIDEO). The gauge value for
    /// `{room, kind, layer_id}` is the COUNT of sessions in `room` whose max
    /// requested layer for `kind` buckets to `layer_id`.
    ///
    /// FAIL-OPEN EXCLUSION: a receiver with NO recorded preference entry for a
    /// kind wants the full ladder (it has expressed no demand) and is NOT
    /// counted. This is deliberate: counting it would conflate "no signal yet"
    /// (every freshly-joined session) with "explicitly requests the top layer",
    /// inflating the top bucket. The gauge therefore reflects only EXPRESSED
    /// downgrade demand. A room with simulcast off / no LAYER_PREFERENCE packets
    /// yet leaves every series at zero.
    ///
    /// CARDINALITY: bounded — `room` × exactly 2 `kind`
    /// ([`RELAY_LAYER_PREFERENCE_KINDS`]) × exactly 4 `layer_id` buckets
    /// ([`RELAY_LAYER_ID_BUCKETS`]). NO per-source/session/peer label is EVER
    /// added (session ids churn on reconnect — per-client granularity is #1170
    /// item 3, deliberately deferred). The `layer_id` is bucketed in code (see
    /// `chat_server::layer_id_bucket`) before becoming a label, so a forged wire
    /// id cannot create unbounded series. Series for a drained room are removed
    /// by [`forget_room_metrics`]; series for active rooms are explicitly
    /// re-SET (including zeros) every sweep so a bucket that no longer has demand
    /// reads `0`, never a stale value.
    pub static ref RELAY_LAYER_PREFERENCE_SESSIONS: GaugeVec = register_gauge_vec!(
        "relay_layer_preference_sessions",
        "Count of receiver sessions requesting each simulcast layer per room and kind; demand-side complement of relay_layer_forwarded_by_layer_total; layer_id bucketed to 0|1|2|other (#1170)",
        &["room", "kind", "layer_id"]
    )
    .expect("Failed to create relay_layer_preference_sessions metric");

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

    /// Persistent server->client WebTransport uni stream RESETS at the OUTBOUND
    /// bridge writer (`webtransport/bridge.rs` `spawn_unistream_writer`), by
    /// transport and `reason` (#1638).
    ///
    /// DISTINCT from `videocall_outbound_channel_drops_total`: that counter
    /// covers PRODUCER-side `try_send` drops at the `WtChatSession`
    /// `Handler<Message>` enqueue hop (before the bytes ever reach the writer).
    /// THIS counter covers the WRITER-side event the #1638 fix introduces — the
    /// single persistent uni stream being torn down and re-opened because a write
    /// onto it could not complete. The in-flight frame that triggered the reset
    /// is dropped (the receiver was wedged), so this is also a drop, but it is a
    /// transport-layer stream reset rather than a queue overflow and is counted
    /// separately so operators can tell "the receiver's downlink stalled past the
    /// write deadline" (`reason="write_timeout"`, the #1638 shed) apart from "the
    /// stream errored / was already torn down" (`reason="write_error"`, the
    /// pre-existing single-retry path). Both end in reset+reopen.
    ///
    /// `transport` is always `webtransport` here (only the WT bridge owns a
    /// persistent uni stream), kept for label-shape parity with the sibling
    /// bridge counters.
    ///
    /// CARDINALITY BOUND: at most 2 series
    /// (`webtransport` x {write_timeout, write_error}). Safe for indefinite
    /// retention; no cleanup required.
    pub static ref RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL: CounterVec = register_counter_vec!(
        "relay_outbound_bridge_stream_resets_total",
        "Persistent server->client WebTransport uni stream resets at the outbound bridge writer, by transport and reason (write_timeout|write_error) (#1638)",
        &["transport", "reason"]
    )
    .expect("Failed to create relay_outbound_bridge_stream_resets_total metric");

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

    // ===== RECEIVER DOWNLINK CONGESTION (#1219 Half 2) =====

    /// Per-receiver downlink congestion shedding episodes (entered shedding mode)
    /// (#1219 Half 2).
    ///
    /// Incremented once on the rising edge of each episode — when this receiver's
    /// REAL downlink backpressure (its bounded per-session outbound channel
    /// overflowing, observed by the windowed `CongestionTracker`) crosses into
    /// active congestion and the relay begins shedding non-base layers. This is
    /// the receiver-directed complement of the existing sender-directed
    /// CONGESTION signal: it detects when the relay-to-receiver link (not the
    /// sender-to-relay link) is saturated. (It is NOT keyed off the actor-mailbox
    /// `Full`, which measures room-wide fan-out burst, not a single receiver's
    /// downlink — see #1219 Half-2 B1.)
    ///
    /// CARDINALITY: bounded — `transport` only (2 values: `websocket`,
    /// `webtransport`). Safe for indefinite retention.
    pub static ref RELAY_RECEIVER_DOWNLINK_CONGESTION_TOTAL: CounterVec = register_counter_vec!(
        "relay_receiver_downlink_congestion_total",
        "Receivers entering downlink congestion shedding mode (windowed CongestionTracker crossing) (#1219)",
        &["transport"]
    )
    .expect("Failed to create relay_receiver_downlink_congestion_total metric");

    /// Per-receiver downlink congestion recovery (exited shedding mode) (#1219
    /// Half 2).
    ///
    /// Incremented once on the falling edge of each episode — when the receiver's
    /// downlink-congestion relief window (`RECEIVER_DOWNLINK_RELIEF_WINDOW`)
    /// elapses with no fresh overflow, so the windowed signal goes inactive and
    /// the relay resumes full-layer forwarding. Recovery is the natural decay of
    /// the window, NOT a count of strictly-consecutive successful sends (which
    /// could be reset forever by an occasional drop and wedge a healthy link —
    /// #1219 Half-2 B2). The difference `congestion_total - recovered_total`
    /// gives the current count of receivers still in shedding mode.
    ///
    /// CARDINALITY: bounded — `transport` only (2 values). Safe for indefinite
    /// retention.
    pub static ref RELAY_RECEIVER_DOWNLINK_RECOVERED_TOTAL: CounterVec = register_counter_vec!(
        "relay_receiver_downlink_recovered_total",
        "Receivers exiting downlink congestion shedding mode (relief window elapsed) (#1219)",
        &["transport"]
    )
    .expect("Failed to create relay_receiver_downlink_recovered_total metric");

    /// Non-base-layer media packets shed by the downlink congestion pre-filter
    /// (#1219 Half 2).
    ///
    /// Incremented each time a non-base (layer > 0) VIDEO/SCREEN packet is
    /// discarded for a receiver in shedding mode (its windowed downlink signal is
    /// active) BEFORE reaching `try_send`. This is the volume of proactive
    /// shedding the congestion relief performs — distinct from the
    /// `relay_packet_drops_total{drop_reason=mailbox_full}` counter (which counts
    /// packets lost at the mailbox itself). Together they tell the story:
    /// shedding removes volume before the mailbox enqueue, so it should REDUCE
    /// the downstream mailbox/channel drops a saturated receiver would otherwise
    /// incur.
    ///
    /// CARDINALITY: bounded — `transport` only (2 values). Safe for indefinite
    /// retention.
    pub static ref RELAY_DOWNLINK_SHED_TOTAL: CounterVec = register_counter_vec!(
        "relay_downlink_shed_total",
        "Non-base-layer media packets shed before try_send for receivers in downlink congestion (#1219)",
        &["transport"]
    )
    .expect("Failed to create relay_downlink_shed_total metric");

    /// DOWNLINK_CONGESTION control packets dropped by the relay's unicast filter
    /// because they did not target the receiving session (#1219 Half 2).
    ///
    /// The relay publishes DOWNLINK_CONGESTION on the target receiver's OWN
    /// per-session subject, but the `room.{room}.*` NATS wildcard fans every
    /// packet out to every session; this counts the non-target copies dropped
    /// before they reach a transport. Kept SEPARATE from
    /// `relay_congestion_filtered_total` (the sender-keyed CONGESTION sibling) so
    /// the two relay-authored unicast packet classes stay distinguishable on the
    /// dashboards — they have different root causes and fan-out shapes.
    ///
    /// CARDINALITY: bounded — `room` only, same as the CONGESTION sibling.
    pub static ref RELAY_DOWNLINK_CONGESTION_FILTERED_TOTAL: CounterVec = register_counter_vec!(
        "relay_downlink_congestion_filtered_total",
        "DOWNLINK_CONGESTION packets dropped by the unicast filter (not targeting this session) (#1219)",
        &["room"]
    )
    .expect("Failed to create relay_downlink_congestion_filtered_total metric");

    // ===== RELAY-MEASURED PER-CONNECTION QUIC PATH HEALTH (#1637, epic #1636) =====
    //
    // LEAD SIGNAL SET for distinguishing the two incident mechanisms epic #1636 is
    // chasing on the single-threaded WT relay. Read these FOUR per-connection
    // gauges TOGETHER — no single one separates B from C; the separability comes
    // from the PATTERN across them plus `videocall_relay_scheduler_lag_ms`:
    //
    //   (i)   ACK-DELAYED degradation (mechanism C, partial): `rtt`, `lost_packets`
    //         and `congestion_events` RISE together across many sessions at one
    //         instant while `sent_packets` keeps climbing. ACKs are still arriving
    //         (just late/lossy), so the ACK-gated estimators move. Shared network.
    //
    //   (ii)  FULL COLLAPSE (mechanism C, the 14:07:00 synchronized-SESSION-DROPPED
    //         shape): the downlink stops returning ACKs entirely. `rtt`,
    //         `lost_packets`, `congestion_events` FREEZE FLAT — they update ONLY on
    //         ACK receipt (`RttEstimator::update`/`detect_lost_packets`/`process_ecn`,
    //         all reached only from `on_ack_received` in quinn-proto 0.11.13
    //         connection/mod.rs:1522/1530/…). BUT the relay keeps TRANSMITTING into
    //         the void (egress + runtime are fine), so `sent_packets` KEEPS CLIMBING
    //         (incremented on every packet built — `PacketBuilder::finish`,
    //         packet_builder.rs:217 — independent of ACKs). So: rtt/loss/congestion
    //         flat + `sent_packets` climbing ⇒ C-collapse, NOT B. This case is the
    //         whole reason `sent_packets` exists; without it a full collapse is
    //         INDISTINGUISHABLE from mechanism B and from a healthy idle link.
    //
    //   (iii) THREAD-STARVATION (mechanism B / Gun #2, #1639): the relay's single
    //         runtime thread is wedged, so it cannot run the SEND path either —
    //         `sent_packets` ALSO goes flat — AND `videocall_relay_scheduler_lag_ms`
    //         spikes. EVERYTHING (rtt, loss, congestion, sent_packets) flat + a
    //         scheduler-lag spike ⇒ B.
    //
    // These are sampled SERVER-SIDE from the quinn `Connection` (via
    // `web_transport_quinn::Session`'s `Deref<Target = quinn::Connection>`), so
    // unlike `videocall_client_active_server_rtt_ms` (the CLIENT's view, which
    // depends on the client's own clock + probe pipeline and is absent when a
    // client is wedged) this is the RELAY's authoritative measurement of every
    // live WT downlink. The sampler runs in `webtransport::handle_webtransport_session`
    // ~every `WT_HEARTBEAT_INTERVAL` (5s) and reads ALL four values from a SINGLE
    // `stats()` snapshot per tick (see `publish_connection_path_stats`).
    //
    // NOTE — there is NO "last-ACK age" field in quinn (quinn-proto 0.11
    // `connection/stats.rs` exposes `PathStats { rtt, lost_packets, lost_bytes,
    // congestion_events, sent_packets, cwnd, current_mtu, .. }` and
    // `Connection::rtt()`, but no direct ack-age accessor). The issue's "last-ACK
    // age" intent is served HERE by the (rtt+loss+congestion) freeze contrasted
    // against `sent_packets` still climbing — together they answer "is this path
    // still being serviced, and are WE still sending?" without fabricating a field
    // quinn does not expose.

    /// Relay-measured smoothed RTT for a live WebTransport connection, in ms
    /// (#1637). Read from `quinn` `ConnectionStats.path.rtt` (identical to
    /// `Connection::rtt()` — both return `self.path.rtt.get()`), the connection's
    /// current smoothed round-trip estimate, ~every `WT_HEARTBEAT_INTERVAL`.
    ///
    /// HOW TO READ B-vs-C (see the section header above for the full 3-case table):
    /// this gauge ALONE does not separate B from C. A CORRELATED RISE across many
    /// `session_id`s ⇒ ACK-delayed mechanism-C degradation. But RTT going/ staying
    /// FLAT is AMBIGUOUS on its own — it is the shape of BOTH a full mechanism-C
    /// collapse (ACKs stopped, so this ACK-gated estimator freezes) AND mechanism-B
    /// thread-starvation AND a healthy idle link. Disambiguate with
    /// `videocall_relay_connection_path_sent_packets` (climbing ⇒ C-collapse, we're
    /// still sending; flat ⇒ B/idle) and `videocall_relay_scheduler_lag_ms`
    /// (spiking ⇒ B). "Flat RTT ⇒ B" WITHOUT those two corroborators is wrong.
    ///
    /// CAVEAT — `initial_rtt` floor: a path that has NOT YET had an ACK sampled
    /// returns the `initial_rtt` CONSTANT, not a real measurement. quinn-proto 0.11
    /// defaults that to 333ms (`config/transport.rs:373`; the relay does not
    /// override it), because `RttEstimator::get()` is `smoothed.unwrap_or(latest)`
    /// and `latest` starts at `initial_rtt` (`connection/paths.rs:307`,`:299`). So
    /// a flat ~333ms reading can mean "no ACK sampled yet" (e.g. a path reset)
    /// rather than a healthy 333ms link — do not misread it as a real RTT. The
    /// sampler deliberately SKIPS the first tick on a brand-new connection (see
    /// `sample_connection_path_stats`) to avoid emitting this floor at t=0, but a
    /// mid-life path reset can still surface it.
    ///
    /// CARDINALITY: `room` × `session_id` (live only). `session_id` is the
    /// SAME canonical per-session id (`SessionLogic::id`, stringified) carried by
    /// `relay_session_drops_total`, so this gauge JOINS with the per-session drop
    /// series for the same connection. NO `display_name`/`peer_id` label (would be
    /// high-cardinality and adds nothing to the shared-vs-isolated signal). The
    /// per-session series is REMOVED at session teardown by
    /// [`forget_connection_path_stats`] (issue #996 pattern), so a process that
    /// served N sessions over its life does not accrue N permanent series.
    pub static ref RELAY_CONNECTION_RTT_MS: GaugeVec = register_gauge_vec!(
        "videocall_relay_connection_rtt_ms",
        "Relay-measured smoothed RTT (quinn path.rtt) for a live WebTransport connection, in ms; sampled ~every WT heartbeat. Correlated cross-session RISE => ACK-delayed shared downlink/NIC (mechanism C). FLAT is ambiguous: disambiguate with path_sent_packets (climbing=>C-collapse) + scheduler_lag (spike=>B). A flat ~333ms may be the unsampled initial_rtt floor, not a real RTT (#1637)",
        &["room", "session_id"]
    )
    .expect("Failed to create videocall_relay_connection_rtt_ms metric");

    /// Relay-measured CUMULATIVE lost-packet count on a live WebTransport
    /// connection's primary path (#1637). Sourced from quinn-proto
    /// `ConnectionStats.path.lost_packets`, sampled every `WT_HEARTBEAT_INTERVAL`.
    ///
    /// EXPORTED AS THE CUMULATIVE VALUE (a counter held in a GaugeVec, the same
    /// convention as `videocall_video_skip_to_live_total`): the gauge is `.set()`
    /// to quinn's running total for this connection, which is monotonic for the
    /// connection's lifetime. Query the RATE with `rate()`/`increase()` (those
    /// tolerate the series vanishing at disconnect). A GaugeVec — not a CounterVec
    /// — specifically so the per-`session_id` series can be REMOVED at teardown
    /// (counters cannot be cheaply unregistered); see [`forget_connection_path_stats`].
    ///
    /// READ ALONGSIDE `videocall_relay_connection_rtt_ms`: a loss-rate spike
    /// shared across sessions corroborates a mechanism-C downlink/NIC event.
    ///
    /// CARDINALITY: `room` × `session_id` (live only), GC'd at teardown.
    pub static ref RELAY_CONNECTION_PATH_LOST_PACKETS: GaugeVec = register_gauge_vec!(
        "videocall_relay_connection_path_lost_packets",
        "Relay-measured cumulative lost packets (quinn ConnectionStats.path.lost_packets) on a live WebTransport connection's primary path; cumulative value held in a gauge so it can be GC'd at session end — chart the rate with rate()/increase() (#1637)",
        &["room", "session_id"]
    )
    .expect("Failed to create videocall_relay_connection_path_lost_packets metric");

    /// Relay-measured CUMULATIVE congestion-controller event count on a live
    /// WebTransport connection's primary path (#1637). Sourced from quinn-proto
    /// `ConnectionStats.path.congestion_events`, sampled every
    /// `WT_HEARTBEAT_INTERVAL`. Same cumulative-in-a-gauge convention and the same
    /// `rate()`/`increase()` query guidance as
    /// [`RELAY_CONNECTION_PATH_LOST_PACKETS`] above.
    ///
    /// A congestion-event-rate spike that is SHARED across many sessions at one
    /// instant is additional mechanism-C corroboration (the relay's uplink/NIC, or
    /// a shared bottleneck, throttling everyone at once); per-session isolated
    /// spikes are ordinary single-client congestion and are NOT the incident
    /// signal.
    ///
    /// CARDINALITY: `room` × `session_id` (live only), GC'd at teardown.
    pub static ref RELAY_CONNECTION_PATH_CONGESTION_EVENTS: GaugeVec = register_gauge_vec!(
        "videocall_relay_connection_path_congestion_events",
        "Relay-measured cumulative congestion events (quinn ConnectionStats.path.congestion_events) on a live WebTransport connection's primary path; cumulative value held in a gauge so it can be GC'd at session end — chart the rate with rate()/increase() (#1637)",
        &["room", "session_id"]
    )
    .expect("Failed to create videocall_relay_connection_path_congestion_events metric");

    /// Relay-measured CUMULATIVE count of packets the relay has SENT on a live
    /// WebTransport connection's primary path (#1637). Sourced from quinn-proto
    /// `ConnectionStats.path.sent_packets`, read from the same per-tick `stats()`
    /// snapshot as the three gauges above.
    ///
    /// THE B-vs-C DISAMBIGUATOR (this is why the gauge exists). Unlike `rtt` /
    /// `lost_packets` / `congestion_events` — which update ONLY on ACK receipt and
    /// therefore FREEZE FLAT the instant a downlink fully collapses — `sent_packets`
    /// is incremented on the EGRESS path, on every packet the relay builds
    /// (quinn-proto `PacketBuilder::finish`, packet_builder.rs:217), independent of
    /// whether any ACK ever comes back. So:
    ///   * `sent_packets` CLIMBING while rtt/loss/congestion are FLAT ⇒ the relay is
    ///     still transmitting but getting nothing back: a downlink/NIC COLLAPSE
    ///     (mechanism C), NOT a relay stall. Without this gauge that collapse is
    ///     indistinguishable from mechanism B and from a healthy idle link.
    ///   * `sent_packets` ALSO FLAT (plus a `videocall_relay_scheduler_lag_ms`
    ///     spike) ⇒ the relay's single runtime thread is wedged and cannot even run
    ///     the send path: mechanism B (thread-starvation / Gun #2, #1639).
    ///
    /// Cumulative-in-a-gauge, same convention/GC/`rate()`-query guidance as the
    /// loss/congestion gauges above. Note it counts BUILT packets across all
    /// connections' fan-out, so a per-connection `rate()` is the relay's send rate
    /// TO that receiver — exactly the "are we still sending to them?" signal.
    ///
    /// CARDINALITY: `room` × `session_id` (live only), GC'd at teardown by
    /// [`forget_connection_path_stats`].
    pub static ref RELAY_CONNECTION_PATH_SENT_PACKETS: GaugeVec = register_gauge_vec!(
        "videocall_relay_connection_path_sent_packets",
        "Relay-measured cumulative packets SENT (quinn ConnectionStats.path.sent_packets, egress-counted, NOT ACK-gated) on a live WebTransport connection. Climbing while rtt/loss freeze flat => downlink collapse (mechanism C); flat + scheduler_lag spike => relay thread-starvation (mechanism B). Cumulative-in-a-gauge; chart with rate()/increase() (#1637)",
        &["room", "session_id"]
    )
    .expect("Failed to create videocall_relay_connection_path_sent_packets metric");

    // ===== TOKIO SCHEDULER-LAG PROBE (#1637, epic #1636 — INSURANCE SIGNAL) =====

    /// Tokio scheduler lag of the WebTransport relay's runtime, in ms — a
    /// HISTOGRAM (#1637).
    ///
    /// This is the ONLY signal that resolves sub-second correlated scheduling
    /// jitter on the relay's SINGLE-THREADED `#[actix_rt::main]` runtime (the
    /// latent Gun #2 / #1639). The cgroup CPU average (cAdvisor `rate()` over
    /// >=15s) is structurally blind to a short stall: a 200ms freeze hides
    /// completely in a 5-second CPU average, yet it is exactly long enough to make
    /// every receiver's outbound channel back up at once. The only way to see it
    /// is to measure how late a timer that SHOULD fire on the relay's runtime
    /// actually fires.
    ///
    /// WHAT IT MEASURES: a dedicated probe task (spawned in
    /// `bin/webtransport_server.rs` `main` via `actix_rt::spawn`, so it lives on
    /// the SAME single-thread runtime whose lag we want to observe) ticks a fixed
    /// `tokio::time::interval` and `observe()`s how late each tick was POLLED versus
    /// the deadline it was SCHEDULED to fire — `Interval::tick().await` returns that
    /// scheduled deadline `Instant`, so the lag is `Instant::now() - deadline` via
    /// [`scheduler_lag_from_deadline`]. When the runtime is healthy the task is
    /// polled on time and observes ~0. When some other future on that one thread
    /// holds the thread without yielding (a long synchronous span, a blocking call,
    /// a fan-out burst that monopolises the executor) the probe's poll is DELAYED
    /// past the deadline by exactly that stall, and that delay is observed.
    /// Measuring from the returned deadline (not an expected-vs-actual period) makes
    /// this correct under both `MissedTickBehavior::Burst` and `Delay` — see
    /// [`scheduler_lag_from_deadline`] for why. A probe on any OTHER thread would
    /// measure nothing — running on the relay's own runtime is the whole point.
    ///
    /// WHY A HISTOGRAM, NOT A GAUGE: the relay is Prometheus-scraped every ~15s but
    /// the probe samples every 500ms. A Gauge holding only the most-recent sample
    /// would be overwritten back to ~0 within 500ms of any spike, so ~29 of every
    /// 30 sub-second stalls would be invisible at scrape time — the metric would
    /// have the exact blind spot it was built to remove. A Histogram instead
    /// ACCUMULATES every observation into monotonic bucket counters; ~30
    /// observations land between scrapes and a spike is preserved in the upper
    /// buckets regardless of when the scrape fires. Query a spike with
    /// `increase(videocall_relay_scheduler_lag_ms_bucket{le="100"}[1m])` vs the
    /// `le="+Inf"` total, or `histogram_quantile(0.99, ...)`.
    ///
    /// BUCKETS (ms): 1/5/10/25/50/100/250/500/1000/2500. Sub-10ms is healthy
    /// timer jitter on a busy runtime; the 100ms+ buckets are the freeze class
    /// epic #1636 cares about (a single-thread stall long enough to back up every
    /// receiver's outbound channel at once). A nonzero `increase()` in the >=100ms
    /// buckets during an incident window is the mechanism-B fingerprint.
    ///
    /// HOW TO READ B-vs-C: upper-bucket `increase()` HERE during an incident, with
    /// the per-connection `sent_packets` ALSO flat (relay not even sending), is
    /// mechanism B (the relay thread stalled; the network was beside the point). No
    /// upper-bucket movement here while the RTT/loss gauges blow up together — or
    /// while `sent_packets` keeps climbing under flat rtt/loss — is mechanism C
    /// (shared downlink/NIC; the relay was scheduling fine, the link degraded).
    ///
    /// CARDINALITY: a single histogram, NO labels — process-global runtime health.
    /// No cleanup needed (it is never per-session).
    pub static ref RELAY_SCHEDULER_LAG_MS: Histogram = register_histogram!(
        "videocall_relay_scheduler_lag_ms",
        "Tokio scheduler lag of the WebTransport relay's single-threaded runtime, in ms (actual minus expected wake of a fixed-interval probe running ON that runtime), as a histogram so sub-second spikes survive the ~15s scrape. Upper-bucket increase() with sent_packets flat => relay thread-starvation (mechanism B, #1639); no upper-bucket movement while RTT/loss spike or sent_packets climbs => shared downlink/NIC (mechanism C) (#1637)",
        vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0]
    )
    .expect("Failed to create videocall_relay_scheduler_lag_ms metric");
}

/// Publish a per-connection QUIC path-health snapshot to all FOUR per-`session_id`
/// gauges (#1637). This is the SINGLE production emission seam — the relay's
/// per-tick sampler (`webtransport::sample_connection_path_stats`) reads ONE
/// `quinn` `ConnectionStats` snapshot per tick and calls exactly this, so the
/// scalar→gauge mapping is pinned to a function a host unit test can drive without
/// a live quinn connection
/// (`metrics::tests::publish_connection_path_stats_sets_all_four_gauges`). Reverting
/// any one `.set()` here therefore fails that test.
///
/// `rtt_ms` is the already-converted millisecond RTT (the caller runs
/// `duration_to_millis_f64` on the `Duration`); the three packet counters are the
/// raw cumulative `u64`s from `ConnectionStats.path`, set as `f64` (a gauge's
/// native type) — Prometheus gauges are `f64` and packet counts well within
/// `f64`'s exact-integer range. See each gauge's doc for how the four values
/// separate mechanism B from C.
pub fn publish_connection_path_stats(
    room: &str,
    session_id: &str,
    rtt_ms: f64,
    lost_packets: u64,
    congestion_events: u64,
    sent_packets: u64,
) {
    RELAY_CONNECTION_RTT_MS
        .with_label_values(&[room, session_id])
        .set(rtt_ms);
    RELAY_CONNECTION_PATH_LOST_PACKETS
        .with_label_values(&[room, session_id])
        .set(lost_packets as f64);
    RELAY_CONNECTION_PATH_CONGESTION_EVENTS
        .with_label_values(&[room, session_id])
        .set(congestion_events as f64);
    RELAY_CONNECTION_PATH_SENT_PACKETS
        .with_label_values(&[room, session_id])
        .set(sent_packets as f64);
}

/// Convert a [`std::time::Duration`] to whole-plus-fractional milliseconds as an
/// `f64`, for setting a millisecond-valued Prometheus gauge (#1637).
///
/// Extracted as a free function so the conversion is unit-testable WITHOUT a live
/// quinn connection (the RTT sampler reads `Connection::rtt() -> Duration` and
/// feeds it straight here). `Duration::as_secs_f64() * 1000.0` preserves
/// sub-millisecond precision (e.g. a 1.5ms RTT reads `1.5`, not `1`), which a
/// `Duration::as_millis()` truncation would lose — and sub-ms precision matters
/// at the low end where a healthy LAN RTT lives.
pub fn duration_to_millis_f64(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Tokio scheduler lag in milliseconds: how late a probe tick was POLLED versus
/// the instant it was SCHEDULED to fire (#1637, epic #1636).
///
/// `deadline` is the `tokio::time::Instant` that `Interval::tick().await` RETURNS
/// — verified against tokio 1.48.0 `interval.rs:467,492-493`: `poll_tick` captures
/// `self.delay.deadline()` (the scheduled fire time) and returns THAT, not the
/// poll time. `polled_at` is `tokio::time::Instant::now()` captured the moment the
/// probe task actually got to run after the tick. The lag is therefore
/// `polled_at - deadline` — the lateness directly, with NO period subtraction and
/// NO separately-captured start time.
///
/// WHY THIS IS ROBUST (the #1636 insurance signal must not hinge on one config
/// line): because it uses the REAL deadline tokio returns, the measurement is
/// correct under BOTH `MissedTickBehavior::Burst` (the default) AND `Delay`. The
/// previous expected-vs-actual formulation was correct ONLY under `Delay` (which
/// re-anchors the next deadline so a per-iteration start time never lands after a
/// missed deadline); under `Burst` a missed deadline makes `tick()` return
/// immediately and an expected-vs-actual probe would read ~0, hiding the very
/// stall it exists to catch. Deadline-based math has no such dependency.
///
/// `saturating_duration_since` clamps to 0 when `polled_at < deadline` (a tick
/// cannot be polled before its scheduled time as REAL lag; only clock granularity
/// could produce a nominally-early read), so an on-time/early poll reports 0ms,
/// never a negative. The positive tail is the signal: the time the relay's single
/// runtime thread was held by something else past this tick's due instant.
///
/// Pure and side-effect-free so it can be unit-tested against the named cases
/// (on-time => 0, late => the overage in ms, early => 0) by referencing THIS
/// function — the probe task calls exactly this, so the test guards the real
/// computation, not a re-implementation.
pub fn scheduler_lag_from_deadline(
    deadline: tokio::time::Instant,
    polled_at: tokio::time::Instant,
) -> f64 {
    duration_to_millis_f64(polled_at.saturating_duration_since(deadline))
}

/// The tokio scheduler-lag probe loop (#1637, epic #1636 — INSURANCE SIGNAL).
///
/// This is the PRODUCTION probe body, extracted into a library function (not left
/// inline in `bin/webtransport_server.rs`) for ONE reason: binaries are not
/// unit-testable, so an inline loop's `.observe()` could be deleted with every
/// test still green — the runtime emission would be unguarded. Living here, the
/// loop is exercised by `metrics::tests::run_scheduler_lag_probe_observes_into_histogram`,
/// which drives it under tokio paused time and asserts the histogram sample count
/// rises; deleting the `.observe(...)` below makes that test fail.
///
/// `main` spawns this with `actix_rt::spawn(run_scheduler_lag_probe(...))` so it
/// runs ON the relay's single-threaded runtime — the whole point of the signal (a
/// probe on any other thread would measure that thread's scheduler, not the
/// relay's). It never returns (infinite loop); the process owns its lifetime.
/// `pub` (not `pub(crate)`) because the `webtransport_server` binary is a separate
/// crate target that imports it from the `sec_api` library.
///
/// Lag is measured from the tick's SCHEDULED DEADLINE — `Interval::tick().await`
/// returns the `Instant` the tick was scheduled to fire (tokio 1.48
/// interval.rs:467,492-493), so `now - deadline` is the lateness directly. This is
/// correct under both `MissedTickBehavior::Burst` and `Delay`; `Delay` is retained
/// only as the cadence policy (no burst catch-up after a stall), NOT load-bearing
/// for correctness. See [`scheduler_lag_from_deadline`].
pub async fn run_scheduler_lag_probe(period: std::time::Duration) {
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // `tick()` returns the deadline the tick was scheduled to fire; the first
        // tick fires immediately with deadline ~= now, so its lag is ~0 (no priming
        // tick needed — the deadline-based math handles it).
        let deadline = interval.tick().await;
        let now = tokio::time::Instant::now();
        RELAY_SCHEDULER_LAG_MS.observe(scheduler_lag_from_deadline(deadline, now));
    }
}

/// Spawn the scheduler-lag probe on the relay's single-thread runtime (#1637).
///
/// Extracted (mirrors `webtransport::spawn_connection_path_sampler`) so the
/// `actix_rt::spawn` + probe loop + `.observe()` wiring is exercised by a host
/// test: `metrics::tests::spawn_scheduler_lag_probe_observes_into_histogram` calls
/// THIS fn and asserts the histogram sample count rises, so deleting the spawn
/// here fails that test. `actix_rt::spawn` keeps the probe on the relay runtime
/// whose lag it measures (a probe on any other thread would measure that thread's
/// scheduler, not the relay's).
///
/// The residual `main() -> spawn_scheduler_lag_probe()` call is the one
/// irreducible composition line — same status as `main`'s NATS-connect /
/// health-server-bind / `webtransport::start` wiring: a binary's `main` cannot be
/// unit-tested without a full smoke harness, so we deliberately do NOT add a
/// fragile binary smoke test for it. Every reducible seam below `main` is now
/// covered.
///
/// `pub` (not `pub(crate)`) because the `webtransport_server` binary is a separate
/// crate target that imports it from the `sec_api` library — same as
/// [`run_scheduler_lag_probe`].
pub fn spawn_scheduler_lag_probe(period: std::time::Duration) -> actix_rt::task::JoinHandle<()> {
    actix_rt::spawn(run_scheduler_lag_probe(period))
}

/// Remove the per-connection QUIC path-health gauges for a single WebTransport
/// `session_id` (#1637; issue #996 cardinality-GC pattern).
///
/// `videocall_relay_connection_rtt_ms` /
/// `videocall_relay_connection_path_lost_packets` /
/// `videocall_relay_connection_path_congestion_events` /
/// `videocall_relay_connection_path_sent_packets` carry an unbounded-over-time
/// `session_id` label, so — like [`forget_session_drops`] — the series for a
/// disconnected connection must be removed the instant its session ends or it
/// leaks for the process lifetime.
///
/// CALL SITE (note the difference from [`forget_session_drops`]): this is invoked
/// inline in `webtransport::handle_webtransport_session` right after
/// `bridge.wait_for_disconnect()` returns — the same place the sampler task is
/// aborted — NOT from `SessionLogic::on_stopping` (where `forget_session_drops`
/// runs). Both fire on every normal disconnect, so neither leaks; the sampler is
/// only ever spawned for WT (it needs the quinn connection), so co-locating its
/// GC with its spawn/teardown in the WT entry point keeps the two on one code
/// path rather than reaching into the shared actor hook.
///
/// `remove_label_values` returns a benign `Err` when the series was never created
/// (e.g. the sampler had not yet taken its first sample), so each call is
/// intentionally `let _ =`-discarded.
pub fn forget_connection_path_stats(room: &str, session_id: &str) {
    let _ = RELAY_CONNECTION_RTT_MS.remove_label_values(&[room, session_id]);
    let _ = RELAY_CONNECTION_PATH_LOST_PACKETS.remove_label_values(&[room, session_id]);
    let _ = RELAY_CONNECTION_PATH_CONGESTION_EVENTS.remove_label_values(&[room, session_id]);
    let _ = RELAY_CONNECTION_PATH_SENT_PACKETS.remove_label_values(&[room, session_id]);
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
    #[serial(viewport_nonvideo_guard_metric)]
    fn viewport_nonvideo_guard_is_labeled_by_room_and_independent_of_filtered_and_forwarded() {
        // The #1437 invariant tripwire must be its own series — bumping it must
        // not leak into the FILTERED or FORWARDED counters (the "% filtered"
        // panel and the blackout alert both depend on those two being clean).
        let room = "wiretest_room_nonvideo_guard";
        let guard_before = snapshot(&RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL, &[room]);
        let filt_before = snapshot(&RELAY_VIEWPORT_FILTERED_TOTAL, &[room]);
        let fwd_before = snapshot(&RELAY_VIEWPORT_FORWARDED_TOTAL, &[room]);

        RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL
            .with_label_values(&[room])
            .inc();

        assert_eq!(
            snapshot(&RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL, &[room]) - guard_before,
            1.0,
            "guard bump must land on the nonvideo-guard series for this room"
        );
        assert_eq!(
            snapshot(&RELAY_VIEWPORT_FILTERED_TOTAL, &[room]) - filt_before,
            0.0,
            "guard bump must NOT leak into the filtered series"
        );
        assert_eq!(
            snapshot(&RELAY_VIEWPORT_FORWARDED_TOTAL, &[room]) - fwd_before,
            0.0,
            "guard bump must NOT leak into the forwarded series"
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
    /// (`forget_room_metrics`) and the per-session GC (`forget_session_drops`).
    /// If it stops covering a `kind`/`drop_reason` the emit sites actually use,
    /// that series would leak forever.
    ///
    /// This guard derives its witness from the emit sites themselves wherever
    /// they are functions, so a NEW emitted kind is caught — not just deletions
    /// from the const (issue #1186):
    ///
    ///   * `drop_kind_label` (ws + wt transports): ENUMERATED over its full input
    ///     domain — `parsed ∈ {false, true}` × `is_media ∈ {false, true}` ×
    ///     `media_type ∈ {None} ∪ MediaType::VALUES`. `MediaType::VALUES` is the
    ///     protobuf-generated variant list, so adding a `MediaType` variant that
    ///     maps to a new label surfaces here automatically. Both transport copies
    ///     are enumerated and asserted equal, so drift between them is also caught.
    ///   * `OutboundPriority::priority_drop_label`: ENUMERATED over every
    ///     `OutboundPriority` variant. The local `all_priorities` list is pinned
    ///     to an exhaustive `match` below, so adding a variant fails to COMPILE
    ///     until the witness is updated.
    ///   * `mailbox_full` / `channel_full` / `overflow_critical`: bare string
    ///     literals at their emit sites with no enumerable source of truth, so
    ///     these remain a hand-maintained witness. This portion guards against
    ///     DELETIONS from `RELAY_DROP_KINDS` only (see the module-level comment).
    ///
    /// Mutating `RELAY_DROP_KINDS` to drop any covered label fails this test, and
    /// a new `drop_kind_label` / `priority_drop_label` output that is absent from
    /// `RELAY_DROP_KINDS` also fails it.
    #[test]
    fn relay_drop_kinds_covers_all_emitted_drop_labels() {
        use crate::actors::priority_drop::OutboundPriority;
        use crate::actors::transports::{ws_chat_session, wt_chat_session};
        use protobuf::Enum; // brings `MediaType::VALUES` (trait const) into scope
        use videocall_types::protos::media_packet::media_packet::MediaType;

        // ---- Tier 1a: enumerate the real `drop_kind_label` emit functions. ----
        // Build the full input domain: media_type is None plus every protobuf
        // MediaType variant (the generated source of truth for that enum).
        let media_types: Vec<Option<MediaType>> = std::iter::once(None)
            .chain(MediaType::VALUES.iter().copied().map(Some))
            .collect();

        let mut emitted: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for &parsed in &[false, true] {
            for &is_media in &[false, true] {
                for &mt in &media_types {
                    let ws_label = ws_chat_session::drop_kind_label(parsed, is_media, mt);
                    let wt_label = wt_chat_session::drop_kind_label(parsed, is_media, mt);
                    // The two transport copies MUST stay in lock-step — a drift
                    // would silently double the taxonomy the GC must cover.
                    assert_eq!(
                        ws_label, wt_label,
                        "ws/wt drop_kind_label disagree for (parsed={parsed}, \
                         is_media={is_media}, media_type={mt:?}); the copies must \
                         stay in lock-step (#1186)"
                    );
                    emitted.insert(ws_label);
                }
            }
        }

        // ---- Tier 1b: enumerate the real `priority_drop_label` emit fn. ----
        // `all_priorities` is pinned to an exhaustive match so a new variant
        // breaks compilation until this list (and the witness) are updated.
        let all_priorities = [
            OutboundPriority::Critical,
            OutboundPriority::Control,
            OutboundPriority::Audio,
            OutboundPriority::Video,
            OutboundPriority::Screen,
        ];
        for p in all_priorities {
            // Compile-time exhaustiveness guard: adding an OutboundPriority
            // variant forces this match (and `all_priorities` above) to be
            // updated, so the enumeration can never silently miss a variant.
            match p {
                OutboundPriority::Critical
                | OutboundPriority::Control
                | OutboundPriority::Audio
                | OutboundPriority::Video
                | OutboundPriority::Screen => {}
            }
            if let Some(label) = p.priority_drop_label() {
                emitted.insert(label);
            }
        }

        // Every label produced by the emit FUNCTIONS must be covered.
        for k in &emitted {
            assert!(
                RELAY_DROP_KINDS.contains(k),
                "RELAY_DROP_KINDS must cover emitted drop label {k:?} produced by \
                 a drop_kind_label / priority_drop_label emit site, or the \
                 room-drain / session GC would leak its series (issues #996/#1090)"
            );
        }

        // ---- Tier 2: hand-maintained witness for bare string literals. ----
        // Drop `kind`/`drop_reason` labels emitted as string LITERALS (not via an
        // enumerable function), so they have no compile-time source of truth and
        // this list only guards against DELETIONS from `RELAY_DROP_KINDS`:
        //   * `mailbox_full` / `channel_full` — passed to `relay_packet_drops_total`
        //     in `chat_server::handle_msg` (fan-out) and the WS/WT
        //     `Handler<Message>` / channel-full hops.
        //   * `overflow_critical` — the Critical-overflow `kind` at the
        //     outbound-channel-full sites.
        //   * `rtt` — emitted to `videocall_outbound_channel_drops_total` at the
        //     WT RTT-echo channel-full site (`wt_chat_session`); `drop_kind_label`
        //     never returns it (a `MediaType::RTT` packet maps to the `media`
        //     catch-all), so it must be witnessed by hand here.
        let string_literal_drop_labels =
            ["mailbox_full", "channel_full", "overflow_critical", "rtt"];
        for k in &string_literal_drop_labels {
            assert!(
                RELAY_DROP_KINDS.contains(k),
                "RELAY_DROP_KINDS must cover string-literal drop label {k:?} or the \
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
        RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL
            .with_label_values(&[room])
            .inc();
        RELAY_LAYER_FILTERED_TOTAL.with_label_values(&[room]).inc();
        RELAY_LAYER_FORWARDED_TOTAL.with_label_values(&[room]).inc();
        RELAY_CONGESTION_FILTERED_TOTAL
            .with_label_values(&[room])
            .inc();
        RELAY_INNER_SESSION_SELF_FILTERED_TOTAL
            .with_label_values(&[room])
            .inc();
        RELAY_DOWNLINK_CONGESTION_FILTERED_TOTAL
            .with_label_values(&[room])
            .inc();
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
        // relay_layer_preference_sessions{room, kind, layer_id} (#1170): seed one
        // representative cell from each bounded taxonomy.
        RELAY_LAYER_PREFERENCE_SESSIONS
            .with_label_values(&[room, "video", "0"])
            .set(5.0);
        RELAY_LAYER_PREFERENCE_SESSIONS
            .with_label_values(&[room, "screen", "other"])
            .set(2.0);

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
        assert_eq!(
            RELAY_LAYER_PREFERENCE_SESSIONS
                .with_label_values(&[room, "video", "0"])
                .get(),
            5.0,
            "demand-gauge seed must be observable before removal (non-vacuous)"
        );
        assert_eq!(
            RELAY_DOWNLINK_CONGESTION_FILTERED_TOTAL
                .with_label_values(&[room])
                .get(),
            1.0,
            "downlink-congestion-filtered seed must be observable before removal (non-vacuous)"
        );
        assert_eq!(
            RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL
                .with_label_values(&[room])
                .get(),
            1.0,
            "nonvideo-guard seed must be observable before removal (non-vacuous)"
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
            RELAY_VIEWPORT_NONVIDEO_AT_DROP_BRANCH_TOTAL
                .with_label_values(&[room])
                .get(),
            0.0,
            "relay_viewport_nonvideo_at_drop_branch_total{{room}} must be swept by \
             forget_room_metrics (#1437)"
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
            RELAY_CONGESTION_FILTERED_TOTAL
                .with_label_values(&[room])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_INNER_SESSION_SELF_FILTERED_TOTAL
                .with_label_values(&[room])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_DOWNLINK_CONGESTION_FILTERED_TOTAL
                .with_label_values(&[room])
                .get(),
            0.0,
            "relay_downlink_congestion_filtered_total{{room}} must be swept by \
             forget_room_metrics (#1219 Half 2)"
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
        // Both seeded demand-gauge cells must be removed by forget_room_metrics.
        assert_eq!(
            RELAY_LAYER_PREFERENCE_SESSIONS
                .with_label_values(&[room, "video", "0"])
                .get(),
            0.0
        );
        assert_eq!(
            RELAY_LAYER_PREFERENCE_SESSIONS
                .with_label_values(&[room, "screen", "other"])
                .get(),
            0.0
        );

        // Clean up the fresh zero-valued handles created by the asserts above so
        // this test leaves no residue for other serial runs.
        forget_room_metrics(room);
    }

    /// #1090 leak-proof property: tearing a session down removes its
    /// `relay_session_drops_total` series EVEN for kinds that session never
    /// incremented — i.e. the GC does not depend on a per-session "kinds I
    /// emitted" tracking set.
    ///
    /// SCOPE (issue #1186 / #1380): this pins the GC HELPER
    /// [`forget_session_drops`] — the function `SessionLogic::on_stopping`
    /// invokes — by calling it directly and asserting it sweeps the FULL
    /// taxonomy. It seeds a SECOND kind the session never "officially" emitted
    /// and asserts the sweep removes it too, so shrinking the loop in
    /// `forget_session_drops` to a subset leaves a residual series and fails
    /// here. What this test does NOT cover (#1380): the `on_stopping` CALL SITE
    /// wiring. `on_stopping` needs an `Addr<ChatServer>` + NATS client + tracker
    /// to drive, so reverting it to an inline per-session-subset loop that
    /// bypasses this helper (the exact #1090 regression shape) would still pass
    /// CI — that wiring is guarded only by the one-line call site under the
    /// LEAK-PROOF comment in `session_logic.rs::on_stopping`, not by this test.
    /// Do not over-trust this as a guard against re-breaking the call site.
    #[test]
    #[serial(session_drops_gc)]
    fn session_drop_gc_iterates_full_taxonomy_unconditionally() {
        let room = "wiretest_session_gc_1090";
        let transport = "websocket";
        let session_id = "999000111";

        // The kind this session "really" dropped, plus a second, DIFFERENT kind
        // from the taxonomy to prove the sweep is unconditional (not "only kinds
        // I emitted"). `overflow_critical` is the last entry in RELAY_DROP_KINDS,
        // so a sweep that stops short of the full taxonomy would leave it behind.
        let emitted_kind = "video";
        let unrelated_kind = "overflow_critical";
        assert_ne!(emitted_kind, unrelated_kind);
        assert!(
            RELAY_DROP_KINDS.contains(&emitted_kind) && RELAY_DROP_KINDS.contains(&unrelated_kind),
            "both seeded kinds must be members of the taxonomy the sweep iterates"
        );

        RELAY_SESSION_DROPS_TOTAL
            .with_label_values(&[room, transport, session_id, emitted_kind])
            .inc();
        RELAY_SESSION_DROPS_TOTAL
            .with_label_values(&[room, transport, session_id, unrelated_kind])
            .inc();
        assert_eq!(
            RELAY_SESSION_DROPS_TOTAL
                .with_label_values(&[room, transport, session_id, emitted_kind])
                .get(),
            1.0,
            "seed must be observable before teardown (non-vacuous)"
        );
        assert_eq!(
            RELAY_SESSION_DROPS_TOTAL
                .with_label_values(&[room, transport, session_id, unrelated_kind])
                .get(),
            1.0,
            "second seed must be observable before teardown (non-vacuous)"
        );

        // Exercise the REAL GC path that `on_stopping` runs.
        forget_session_drops(room, transport, session_id);

        // BOTH seeded series must be gone — the one the session "emitted" AND the
        // unrelated taxonomy member — proving the sweep is full and unconditional.
        assert_eq!(
            RELAY_SESSION_DROPS_TOTAL
                .with_label_values(&[room, transport, session_id, emitted_kind])
                .get(),
            0.0,
            "forget_session_drops must remove the kind the session emitted"
        );
        assert_eq!(
            RELAY_SESSION_DROPS_TOTAL
                .with_label_values(&[room, transport, session_id, unrelated_kind])
                .get(),
            0.0,
            "forget_session_drops must sweep the FULL taxonomy, not just emitted \
             kinds — a subset sweep would leave this residual series (#1090/#1186)"
        );

        // Clean up the fresh zero handles the asserts above created.
        let _ = RELAY_SESSION_DROPS_TOTAL.remove_label_values(&[
            room,
            transport,
            session_id,
            emitted_kind,
        ]);
        let _ = RELAY_SESSION_DROPS_TOTAL.remove_label_values(&[
            room,
            transport,
            session_id,
            unrelated_kind,
        ]);
    }

    // =========================================================================
    // #1637 — scheduler-lag computation + per-connection path-stat GC
    // =========================================================================

    use std::time::Duration;

    /// `duration_to_millis_f64` preserves sub-millisecond precision and the
    /// integer-ms cases the RTT gauge relies on. Pins the PRODUCTION conversion
    /// the RTT sampler feeds `Connection::rtt()` through — break the `* 1000.0`
    /// (e.g. to `as_millis()` truncation) and the 1.5ms case fails.
    #[test]
    fn duration_to_millis_f64_converts_with_sub_ms_precision() {
        assert_eq!(duration_to_millis_f64(Duration::ZERO), 0.0);
        assert_eq!(duration_to_millis_f64(Duration::from_millis(40)), 40.0);
        assert_eq!(duration_to_millis_f64(Duration::from_secs(1)), 1000.0);
        // 1.5ms must NOT truncate to 1 — sub-ms precision matters at LAN RTTs.
        assert_eq!(duration_to_millis_f64(Duration::from_micros(1500)), 1.5);
    }

    /// `scheduler_lag_from_deadline` returns how late a tick was polled vs its
    /// scheduled deadline, clamped at 0 for on-time/early polls. These are the
    /// exact cases #1637 names. Builds two `tokio::time::Instant`s by arithmetic
    /// (no real sleeping) and calls the PRODUCTION function — flipping the
    /// subtraction order to `deadline.saturating_duration_since(polled_at)` makes
    /// the 200ms-late case read 0.0 and FAILS this test, pinning that `polled_at -
    /// deadline` (not the reverse) is the lag.
    #[test]
    fn scheduler_lag_from_deadline_named_cases() {
        // A fixed base so both Instants share a monotonic origin (the probe passes
        // tokio Instants straight through; arithmetic on them is exact here).
        let deadline = tokio::time::Instant::now();

        // On-time poll: polled exactly at the deadline => 0ms lag (no false spike).
        assert_eq!(scheduler_lag_from_deadline(deadline, deadline), 0.0);

        // Late by 200ms: the runtime polled the tick 200ms past its scheduled
        // fire instant — exactly the stall the probe must surface.
        assert_eq!(
            scheduler_lag_from_deadline(deadline, deadline + Duration::from_millis(200)),
            200.0,
            "a tick polled 200ms after its deadline must report 200.0ms of lag"
        );

        // Early poll (polled_at < deadline; only clock granularity could cause it):
        // saturates to 0 rather than a negative.
        assert_eq!(
            scheduler_lag_from_deadline(deadline + Duration::from_millis(40), deadline),
            0.0,
            "a poll before the deadline must saturate to 0, never go negative"
        );
    }

    /// `publish_connection_path_stats` sets ALL FOUR per-connection gauges from a
    /// known snapshot, then `forget_connection_path_stats` removes all four
    /// (#1637 / #996). This pins BOTH production seams: the per-tick EMISSION
    /// mapping (the function the sampler calls every tick — reverting any one
    /// `.set()` makes the matching assert below fail) AND the teardown GC sweep
    /// (deleting any one `remove_label_values` makes the matching 0.0 assert fail).
    /// It calls the REAL production functions, not inline replicas, so a mutation
    /// to either is caught here.
    #[test]
    #[serial(relay_connection_path_stats_metric)]
    fn publish_connection_path_stats_sets_all_four_gauges_then_gc_removes_them() {
        let room = "gc-room-1637";
        let session_id = "987654321";

        // Distinct sentinel values so a copy-paste bug that wires one gauge to the
        // wrong field is caught (each gauge must read ITS OWN value, not another's).
        let rtt_ms = 123.0_f64;
        let lost_packets = 7_u64;
        let congestion_events = 3_u64;
        let sent_packets = 9001_u64;

        // PRODUCTION emission seam — the exact function the per-tick sampler calls.
        publish_connection_path_stats(
            room,
            session_id,
            rtt_ms,
            lost_packets,
            congestion_events,
            sent_packets,
        );

        // Each gauge must hold ITS OWN published value. Reverting/mis-wiring any
        // single `.set()` in `publish_connection_path_stats` breaks exactly one of
        // these (non-vacuous: the values are distinct and nonzero).
        assert_eq!(
            RELAY_CONNECTION_RTT_MS
                .with_label_values(&[room, session_id])
                .get(),
            rtt_ms,
            "rtt gauge must reflect the published rtt_ms"
        );
        assert_eq!(
            RELAY_CONNECTION_PATH_LOST_PACKETS
                .with_label_values(&[room, session_id])
                .get(),
            lost_packets as f64,
            "lost_packets gauge must reflect the published count"
        );
        assert_eq!(
            RELAY_CONNECTION_PATH_CONGESTION_EVENTS
                .with_label_values(&[room, session_id])
                .get(),
            congestion_events as f64,
            "congestion_events gauge must reflect the published count"
        );
        assert_eq!(
            RELAY_CONNECTION_PATH_SENT_PACKETS
                .with_label_values(&[room, session_id])
                .get(),
            sent_packets as f64,
            "sent_packets gauge must reflect the published count (B-vs-C disambiguator)"
        );

        // PRODUCTION teardown sweep — the exact function the session teardown calls.
        forget_connection_path_stats(room, session_id);

        // All four series must be gone (re-fetch yields a fresh 0.0 handle).
        // Deleting any one removal line in `forget_connection_path_stats` leaves a
        // residual nonzero value here and fails the matching assert.
        assert_eq!(
            RELAY_CONNECTION_RTT_MS
                .with_label_values(&[room, session_id])
                .get(),
            0.0,
            "rtt gauge must be removed at teardown"
        );
        assert_eq!(
            RELAY_CONNECTION_PATH_LOST_PACKETS
                .with_label_values(&[room, session_id])
                .get(),
            0.0,
            "lost_packets gauge must be removed at teardown"
        );
        assert_eq!(
            RELAY_CONNECTION_PATH_CONGESTION_EVENTS
                .with_label_values(&[room, session_id])
                .get(),
            0.0,
            "congestion_events gauge must be removed at teardown"
        );
        assert_eq!(
            RELAY_CONNECTION_PATH_SENT_PACKETS
                .with_label_values(&[room, session_id])
                .get(),
            0.0,
            "sent_packets gauge must be removed at teardown"
        );

        // Clean up the fresh zero handles the asserts above created.
        forget_connection_path_stats(room, session_id);
    }

    /// Regression test for the scheduler-lag probe's PRODUCTION emission wiring
    /// (#1637, Codex P1). Drives the real `run_scheduler_lag_probe` loop — the same
    /// function `webtransport_server` main spawns — under tokio PAUSED time and
    /// asserts the histogram's sample count RISES. Deleting the `.observe(...)`
    /// line inside `run_scheduler_lag_probe` makes the count stop rising and FAILS
    /// this test, so the runtime emission is no longer unguarded (the pure
    /// `scheduler_lag_from_deadline` test pins the arithmetic; THIS pins the loop).
    ///
    /// `RELAY_SCHEDULER_LAG_MS` is a process-global single histogram shared by all
    /// tests, so this is `#[serial]` (distinct key) and asserts a DELTA, not an
    /// absolute count — another test may also have observed into it.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    #[serial(relay_scheduler_lag_probe)]
    async fn run_scheduler_lag_probe_observes_into_histogram() {
        let period = std::time::Duration::from_millis(500);

        let before = RELAY_SCHEDULER_LAG_MS.get_sample_count();

        // Spawn the REAL production probe loop. It never returns, so we drive it a
        // few ticks under paused time then abort it.
        let handle = tokio::spawn(run_scheduler_lag_probe(period));

        // Under `start_paused`, timers only fire when we advance the clock. The
        // first `interval.tick()` is immediate; advancing by several periods (with
        // a yield after each so the spawned task is actually polled and runs its
        // `.observe()`) forces multiple ticks to fire and be observed.
        for _ in 0..3 {
            tokio::time::advance(period).await;
            tokio::task::yield_now().await;
        }

        let after = RELAY_SCHEDULER_LAG_MS.get_sample_count();
        handle.abort();

        assert!(
            after > before,
            "run_scheduler_lag_probe must observe at least one sample into the \
             histogram (before={before}, after={after}); a non-increasing count \
             means the production .observe() wiring is gone"
        );
    }

    /// Regression test for the scheduler-lag probe's SPAWN wiring (#1637, Codex
    /// P1) — parity with `webtransport`'s loopback sampler-spawn test. Calls the
    /// REAL `spawn_scheduler_lag_probe` (the same fn `webtransport_server` main
    /// calls) and asserts the histogram's sample count RISES. Deleting the
    /// `actix_rt::spawn(...)` inside `spawn_scheduler_lag_probe` makes the count
    /// stop rising and FAILS this test. The sibling
    /// `run_scheduler_lag_probe_observes_into_histogram` pins the LOOP/observe;
    /// THIS pins the spawn — together the only uncovered line is the irreducible
    /// `main() -> spawn_scheduler_lag_probe()` composition.
    ///
    /// `#[actix_rt::test]` (NOT `#[tokio::test]`): `actix_rt::spawn` needs the
    /// actix System/LocalSet that `actix_rt::test` provides — exactly like the
    /// loopback sampler-spawn test. Same `#[serial]` key as the loop test so the
    /// two probe tests cannot race the process-global histogram; asserts a DELTA.
    ///
    /// Non-flaky: 10ms period + a 60ms real sleep gives ~6 ticks of margin, and
    /// `Interval`'s immediate first tick guarantees >=1 observe even if scheduling
    /// is slow. `abort()` cleanly stops the otherwise-infinite probe loop.
    #[actix_rt::test]
    #[serial(relay_scheduler_lag_probe)]
    async fn spawn_scheduler_lag_probe_observes_into_histogram() {
        let before = RELAY_SCHEDULER_LAG_MS.get_sample_count();

        // Drive the REAL production spawn helper.
        let handle = spawn_scheduler_lag_probe(std::time::Duration::from_millis(10));

        // Let a few real ticks land (immediate first tick => >=1 observe).
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        handle.abort();

        let after = RELAY_SCHEDULER_LAG_MS.get_sample_count();
        assert!(
            after > before,
            "spawn_scheduler_lag_probe must spawn the probe and observe at least \
             one sample (before={before}, after={after}); if not, the \
             actix_rt::spawn wiring inside the helper is gone"
        );
    }
}
