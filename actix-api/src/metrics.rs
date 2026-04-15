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
    register_counter, register_counter_vec, register_gauge_vec, register_histogram, Counter,
    CounterVec, Encoder, GaugeVec, Histogram,
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

    /// Video frames dropped by receiver
    pub static ref VIDEO_FRAMES_DROPPED: GaugeVec = register_gauge_vec!(
        "videocall_video_frames_dropped",
        "Number of video frames dropped by the receiver",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create video_frames_dropped metric");

    /// Audio packet loss percentage (0.0-100.0)
    pub static ref AUDIO_PACKET_LOSS_PCT: GaugeVec = register_gauge_vec!(
        "videocall_audio_packet_loss_pct",
        "Audio packet loss percentage calculated from NetEQ concealment events",
        &["meeting_id", "session_id", "from_peer", "to_peer", "reporter_name", "peer_name"]
    )
    .expect("Failed to create audio_packet_loss_pct metric");

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

    /// Cumulative datagram drops (writable stream locked)
    pub static ref DATAGRAM_DROPS_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_datagram_drops_total",
        "Cumulative datagrams dropped due to locked writable stream",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create datagram_drops_total metric");

    /// Cumulative WebSocket packets dropped (backpressure)
    pub static ref WEBSOCKET_DROPS_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_websocket_drops_total",
        "Cumulative WebSocket packets dropped due to send buffer backpressure",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create websocket_drops_total metric");

    /// Cumulative keyframe requests sent (PLI)
    pub static ref KEYFRAME_REQUESTS_SENT_TOTAL: GaugeVec = register_gauge_vec!(
        "videocall_keyframe_requests_sent_total",
        "Cumulative keyframe requests (PLI) sent by this client",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create keyframe_requests_sent_total metric");

    // ===== ENCODER & SCREEN SHARE METRICS (sender-side, P0/P1) =====

    /// Encoder fps_ratio (received/target) driving tier decisions
    pub static ref ENCODER_FPS_RATIO: GaugeVec = register_gauge_vec!(
        "videocall_encoder_fps_ratio",
        "Ratio of received FPS to target FPS driving adaptive quality decisions",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create encoder_fps_ratio metric");

    /// Peer FPS signal driving encoder decisions.
    /// NOTE: As of PR-A (#312), this reports p75 aggregated FPS, not worst-peer FPS.
    /// TODO(PR-G): rename metric to `videocall_encoder_p75_peer_fps`.
    pub static ref ENCODER_WORST_PEER_FPS: GaugeVec = register_gauge_vec!(
        "videocall_encoder_worst_peer_fps",
        "FPS from the worst-performing receiver driving encoder decisions",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create encoder_worst_peer_fps metric");

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

    /// Encoder bitrate ratio
    pub static ref ENCODER_BITRATE_RATIO: GaugeVec = register_gauge_vec!(
        "videocall_encoder_bitrate_ratio",
        "Ratio of current bitrate to ideal bitrate for tier selection",
        &["meeting_id", "session_id", "peer_id", "display_name"]
    )
    .expect("Failed to create encoder_bitrate_ratio metric");

    // ===== PER-PEER QUALITY METRICS (new/transition) =====

    /// Audio concealment percentage (renamed from audio_packet_loss_pct)
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

    /// Total packet drops from try_send() failures on outbound channels/mailboxes
    pub static ref RELAY_PACKET_DROPS_TOTAL: CounterVec = register_counter_vec!(
        "relay_packet_drops_total",
        "Total packets dropped due to full outbound queue or mailbox",
        &["room", "transport", "drop_reason"]
    )
    .expect("Failed to create relay_packet_drops_total metric");

    /// Current outbound channel occupancy (WebTransport bounded channel only)
    pub static ref RELAY_OUTBOUND_QUEUE_DEPTH: GaugeVec = register_gauge_vec!(
        "relay_outbound_queue_depth",
        "Current outbound channel occupancy (WebTransport only)",
        &["room"]
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
}
