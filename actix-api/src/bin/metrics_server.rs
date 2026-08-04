use actix_web::{web, App, HttpResponse, HttpServer, Result};
use async_nats::{Client, Message};
use futures::StreamExt;
use protobuf::Message as PbMessage;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::task;
use tracing::{debug, error, info};
use videocall_types::protos::health_packet::HealthPacket as PbHealthPacket;

use prometheus::{Encoder, TextEncoder};

// Shared state for latest health data from all servers
type HealthDataStore = Arc<Mutex<HashMap<String, Value>>>;

// Session tracking for cleanup
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct SessionInfo {
    session_id: String,
    meeting_id: String,
    reporting_user_id: String,
    display_name: String,
    last_seen: Instant,
    // Peers we have published metrics for in this session (as to_peer)
    to_peers: HashSet<String>,
    // Peer IDs we have published peer connection metrics for
    peer_ids: HashSet<String>,
    // Server info we have published active server metrics for (server_url, server_type)
    active_servers: HashSet<(String, String)>,
    // TELEM-7: last CLIENT_INFO label values (cores, arch, gpu, net, score) for cleanup
    client_info_labels: Option<[String; 5]>,
    // #1556: last network_type label for CLIENT_NETWORK_TYPE cleanup
    last_network_type: Option<String>,
    // #1561: (peer_session_id, media_kind) pairs we have published RECEIVED_LAYER for.
    // Diffed each packet to remove stale series when a constraint clears.
    received_layer_peers: HashSet<(String, String)>,
    // Issue 2047: [direction, stream, from_tier, to_tier, trigger] tuples we have
    // published TIER_TRANSITIONS_TOTAL for, so the session's series can be reaped
    // on departure. Bounded by the label allowlist, so this set cannot grow with
    // attacker-supplied strings.
    tier_transition_labels: HashSet<[String; 5]>,
}

type SessionTracker = Arc<Mutex<HashMap<String, SessionInfo>>>;

// Prometheus metrics (same as existing diagnostics.rs)
// Import shared Prometheus metrics
use sec_api::metrics::{
    ACTIVE_SESSIONS_TOTAL, ADAPTIVE_AUDIO_TIER, ADAPTIVE_SCREEN_TIER, ADAPTIVE_VIDEO_TIER,
    AUDIO_CONCEALMENT_PCT, AUDIO_CONGESTION_CEILING, AUDIO_DATAGRAM_LOSS_PER_SEC,
    AUDIO_DATAGRAM_RAW_LOSS_PER_SEC, AUDIO_PLAYOUT_LATENCY_MS, AUDIO_QUALITY_SCORE,
    BATTERY_CHARGING, BATTERY_LEVEL, CALL_QUALITY_SCORE, CAPABILITY_SCORE, CLIENT_ACTIVE_SERVER,
    CLIENT_ACTIVE_SERVER_RTT_MS, CLIENT_AGENT_MEMORY_BYTES, CLIENT_AUDIO_CONCEALMENT_PCT,
    CLIENT_CPU_THROTTLED, CLIENT_DATAGRAM_READ_LOOP_MAX_GAP_MS, CLIENT_INFO,
    CLIENT_LONGTASK_DURATION_MS, CLIENT_MEMORY_TOTAL_BYTES, CLIENT_MEMORY_USED_BYTES,
    CLIENT_NETWORK_DOWNLINK_MAX, CLIENT_NETWORK_TYPE, CLIENT_PACKETS_RECEIVED_PER_SEC,
    CLIENT_PACKETS_SENT_PER_SEC, CLIENT_REELECTION_TOTAL, CLIENT_RENDER_FPS,
    CLIENT_SEND_QUEUE_BYTES, CLIENT_TAB_THROTTLED, CLIENT_TAB_VISIBLE, CLIENT_WASM_MEMORY_BYTES,
    DATAGRAM_DROPS, DECODER_ERRORS_TOTAL, DECODE_ACTIVE_SET_SIZE, DECODE_BUDGET_EFFECTIVE_CAP,
    DECODE_BUDGET_NATURAL, DECODE_BUDGET_OVERRIDE_FIXED_N, DECODE_BUDGET_OVERRIDE_MODE,
    DECODE_BUDGET_PRESSURED, ENCODER_ACTIVE_LAYERS, ENCODER_EFFECTIVE_LAYERS, ENCODER_OUTPUT_FPS,
    ENCODER_QUEUE_DEPTH, ENCODER_RESTART_TOTAL, ENCODER_TARGET_BITRATE_KBPS, HEALTH_REPORTS_TOTAL,
    KEYFRAME_REQUESTS_PER_SEC, KEYFRAME_REQUESTS_SENT_TOTAL, MEETING_PARTICIPANTS,
    NETEQ_ACCELERATE_OPS_PER_SEC, NETEQ_AUDIO_BUFFER_MS, NETEQ_EXPAND_OPS_PER_SEC,
    NETEQ_NORMAL_OPS_PER_SEC, NETEQ_PACKETS_AWAITING_DECODE, NETEQ_PACKETS_PER_SEC,
    NETEQ_TARGET_DELAY_MS, NON_FINITE_SAMPLES_DROPPED_TOTAL, PEER_AUDIO_ENABLED, PEER_CAN_LISTEN,
    PEER_CAN_SEE, PEER_CONNECTIONS_TOTAL, PEER_VIDEO_ENABLED, RECEIVED_LAYER,
    RTT_PROBE_DROPPED_TOTAL, RTT_PROBE_STALE_SUPPRESSIONS_TOTAL, SCREEN_ENCODER_MAX_STALL_GAP_MS,
    SCREEN_ENCODER_OUTPUT_FPS, SCREEN_ENCODER_STALL_EPISODES, SCREEN_SHARING_ACTIVE,
    SCREEN_VIDEO_BITRATE_KBPS, SCREEN_VIDEO_CONTENT_STALENESS_MS, SCREEN_VIDEO_FPS,
    SCREEN_VIDEO_PLAYOUT_LATENCY_MS, SCREEN_VIDEO_PLAYOUT_PAINT_LAG_MS,
    SCREEN_VIDEO_PLAYOUT_STAGE1_SPAN_MS, SCREEN_VIDEO_SKIP_TO_LIVE_TOTAL, SELF_AUDIO_ENABLED,
    SELF_VIDEO_ENABLED, TIER_TRANSITIONS_DROPPED_TOTAL, TIER_TRANSITIONS_TOTAL,
    UNISTREAM_BYTES_DRAINED_TOTAL, UNISTREAM_BYTES_OFFERED_TOTAL,
    UNISTREAM_STALE_DELTA_DROPS_TOTAL, VIDEOCALL_PEER_INFO, VIDEO_BITRATE_KBPS,
    VIDEO_CONTENT_STALENESS_MS, VIDEO_FPS, VIDEO_FRAMES_DROPPED, VIDEO_PLAYOUT_LATENCY_MS,
    VIDEO_PLAYOUT_PAINT_LAG_MS, VIDEO_PLAYOUT_STAGE1_SPAN_MS, VIDEO_QUALITY_SCORE,
    VIDEO_SEQ_LOSS_PER_SEC, VIDEO_SKIP_TO_LIVE_TOTAL, WEBSOCKET_DROPS,
    WT_INCOMING_DATAGRAM_HIGH_WATER_MARK, WT_INCOMING_DATAGRAM_MAX_AGE_MS,
};

async fn metrics_handler(
    data: web::Data<HealthDataStore>,
    session_tracker: web::Data<SessionTracker>,
) -> Result<HttpResponse> {
    drop(data.lock().unwrap_or_else(|e| e.into_inner()));

    // Clean up stale sessions before processing metrics
    cleanup_stale_sessions(&session_tracker);

    // Do not mutate metrics here. Metrics are updated only on fresh NATS messages.

    // Encode metrics for Prometheus
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = Vec::new();

    match encoder.encode(&metric_families, &mut buffer) {
        Ok(_) => {
            let output = String::from_utf8_lossy(&buffer);
            Ok(HttpResponse::Ok()
                .content_type("text/plain; version=0.0.4")
                .body(output.to_string()))
        }
        Err(e) => {
            error!("Failed to encode metrics: {}", e);
            Ok(HttpResponse::InternalServerError().body("Failed to encode metrics"))
        }
    }
}

/// Clean up sessions that haven't reported in the last 30 seconds.
fn cleanup_stale_sessions(session_tracker: &SessionTracker) {
    use std::time::Duration;
    let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let timeout = Duration::from_secs(30); // 30 second timeout

    let mut to_remove = Vec::new();

    for (key, session_info) in tracker.iter() {
        if now.duration_since(session_info.last_seen) > timeout {
            to_remove.push(key.clone());
        }
    }

    // Meetings that lost at least one session this pass; their participant gauge
    // must be recomputed (and removed entirely if the meeting is now empty).
    let mut affected_meetings: HashSet<String> = HashSet::new();
    for key in to_remove {
        if let Some(session_info) = tracker.remove(&key) {
            info!(
                "Cleaning up stale session: {} (meeting: {}, peer: {})",
                session_info.session_id, session_info.meeting_id, session_info.reporting_user_id
            );
            affected_meetings.insert(session_info.meeting_id.clone());

            // Remove all metrics for this session
            remove_session_metrics(&session_info);
        }
    }

    // Recompute the participant gauge for every meeting that lost a session so it
    // decrements as sessions expire — and is removed when the meeting empties. This
    // is the fix for the gauge leak (issue #1040): the count is now derived from the
    // authoritative session tracker instead of being latched to one reporter's last
    // health packet, so stale meetings drop to 0 / off the dashboards.
    if !affected_meetings.is_empty() {
        recompute_meeting_participants(&tracker, &affected_meetings);
    }
}

/// Recompute the `MEETING_PARTICIPANTS` gauge for the given meetings from the
/// authoritative session tracker.
///
/// The participant count for a meeting is the number of distinct live sessions in
/// the tracker for that `meeting_id` (one live reporting client == one participant;
/// the per-session count already includes "self", so no `+1` is applied here). When
/// a meeting has no live sessions left, its series is removed so idle meetings drop
/// off dashboards — matching how the other per-session metrics are removed on cleanup.
///
/// The caller must hold the `session_tracker` lock; the locked map is passed in to
/// avoid re-locking (and the resulting deadlock).
///
/// SINGLE-REPLICA ASSUMPTION (issue #1075).
/// This derivation is only correct when `metrics_server` runs as a **single replica**.
/// The `session_tracker` is a per-process `Arc<Mutex<HashMap<..>>>` (see `SessionTracker`)
/// populated from the NATS subscription, which uses the queue group
/// `metrics-server-health-diagnostics` (see `nats_health_consumer`). A queue group
/// load-balances each health packet to exactly ONE subscriber, so with N replicas every
/// replica's tracker holds only the subset of sessions whose packets it happened to
/// receive. Each replica would then write `videocall_meeting_participants{meeting_id}`
/// for the SAME meeting with its own partial count; because they share label values,
/// each scrape target reports a partial count and the dashboard value
/// undercounts / flaps across replicas.
///
/// The production deployment IS single-replica: `helm/metrics-api/values.yaml` pins
/// `serverStats.replicas: 1` and no per-environment override raises it, so this
/// assumption holds today. If `metrics_server` is ever scaled to >1 replica, this gauge
/// must be aggregated across replicas at the recording-rule layer instead (e.g. emit a
/// per-replica partial count keyed by an instance/replica label and `sum by (meeting_id)`
/// in a Prometheus recording rule, or move the derivation behind a single aggregator).
fn recompute_meeting_participants(
    tracker: &HashMap<String, SessionInfo>,
    meetings: &HashSet<String>,
) {
    // Count distinct live sessions per affected meeting.
    let mut counts: HashMap<&str, u64> = meetings.iter().map(|m| (m.as_str(), 0u64)).collect();
    for info in tracker.values() {
        if let Some(count) = counts.get_mut(info.meeting_id.as_str()) {
            *count += 1;
        }
    }

    for (meeting_id, count) in counts {
        if count == 0 {
            // Meeting is empty — drop the series entirely instead of leaving a phantom 0.
            let _ = MEETING_PARTICIPANTS.remove_label_values(&[meeting_id]);
        } else {
            MEETING_PARTICIPANTS
                .with_label_values(&[meeting_id])
                .set(count as f64);
        }
    }
}

/// Remove all Prometheus metrics for a given session
fn remove_session_metrics(session_info: &SessionInfo) {
    // Remove series for this session using precise label combinations
    let _ = ACTIVE_SESSIONS_TOTAL
        .remove_label_values(&[&session_info.meeting_id, &session_info.session_id]);

    // Remove self-reported enabled metrics for the reporting peer in this meeting
    let _ = SELF_AUDIO_ENABLED
        .remove_label_values(&[&session_info.meeting_id, &session_info.reporting_user_id]);
    let _ = SELF_VIDEO_ENABLED
        .remove_label_values(&[&session_info.meeting_id, &session_info.reporting_user_id]);
    let _ = VIDEOCALL_PEER_INFO.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
        &session_info.display_name,
    ]);

    // Remove tab visibility, throttled, memory, send queue, packet rate metrics
    let _ = CLIENT_TAB_VISIBLE.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
    ]);
    let _ = CLIENT_MEMORY_USED_BYTES.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
    ]);
    let _ = CLIENT_MEMORY_TOTAL_BYTES.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
    ]);
    // #1032: non-heap memory series share the JS-heap label family.
    let _ = CLIENT_WASM_MEMORY_BYTES.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
    ]);
    let _ = CLIENT_AGENT_MEMORY_BYTES.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
    ]);

    // Remove send queue, packet rates, tab throttled, and receiver-side metrics
    let reporter_labels = [
        &session_info.meeting_id as &str,
        &session_info.session_id,
        &session_info.reporting_user_id,
    ];
    let _ = CLIENT_SEND_QUEUE_BYTES.remove_label_values(&reporter_labels);
    let _ = CLIENT_PACKETS_RECEIVED_PER_SEC.remove_label_values(&reporter_labels);
    let _ = CLIENT_PACKETS_SENT_PER_SEC.remove_label_values(&reporter_labels);
    let _ = CLIENT_TAB_THROTTLED.remove_label_values(&reporter_labels);
    let _ = ADAPTIVE_VIDEO_TIER.remove_label_values(&reporter_labels);
    let _ = ADAPTIVE_AUDIO_TIER.remove_label_values(&reporter_labels);
    let _ = DATAGRAM_DROPS.remove_label_values(&reporter_labels);
    let _ = UNISTREAM_BYTES_OFFERED_TOTAL.remove_label_values(&reporter_labels);
    let _ = UNISTREAM_BYTES_DRAINED_TOTAL.remove_label_values(&reporter_labels);
    let _ = UNISTREAM_STALE_DELTA_DROPS_TOTAL.remove_label_values(&reporter_labels);
    let _ = WEBSOCKET_DROPS.remove_label_values(&reporter_labels);
    let _ = KEYFRAME_REQUESTS_SENT_TOTAL.remove_label_values(&reporter_labels);
    // Issue 2031: per-client WT receive-health gauges.
    let _ = CLIENT_DATAGRAM_READ_LOOP_MAX_GAP_MS.remove_label_values(&reporter_labels);
    let _ = WT_INCOMING_DATAGRAM_HIGH_WATER_MARK.remove_label_values(&reporter_labels);
    let _ = WT_INCOMING_DATAGRAM_MAX_AGE_MS.remove_label_values(&reporter_labels);
    // Concealment carries a transport label — sweep both possible values so no
    // residual series lingers after the reporter departs on either transport.
    let _ = CLIENT_AUDIO_CONCEALMENT_PCT.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
        "webtransport",
    ]);
    let _ = CLIENT_AUDIO_CONCEALMENT_PCT.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
        "websocket",
    ]);
    // #522 RTT-probe resilience gauges: same per-reporter GC so the high-cardinality
    // session_id label leaves no residual series on disconnect.
    let _ = RTT_PROBE_DROPPED_TOTAL.remove_label_values(&reporter_labels);
    let _ = RTT_PROBE_STALE_SUPPRESSIONS_TOTAL.remove_label_values(&reporter_labels);
    let _ = ENCODER_QUEUE_DEPTH.remove_label_values(&reporter_labels);
    let _ = ADAPTIVE_SCREEN_TIER.remove_label_values(&reporter_labels);
    let _ = SCREEN_SHARING_ACTIVE.remove_label_values(&reporter_labels);
    let _ = ENCODER_OUTPUT_FPS.remove_label_values(&reporter_labels);
    // #2147: same per-reporter GC as its camera sibling, so the high-cardinality
    // session_id label leaves no residual series on disconnect. Load-bearing here
    // specifically because this gauge legitimately reports 0: without GC a
    // disconnected publisher's last reading would persist and read as a live
    // screen encoder producing nothing.
    let _ = SCREEN_ENCODER_OUTPUT_FPS.remove_label_values(&reporter_labels);
    let _ = SCREEN_ENCODER_STALL_EPISODES.remove_label_values(&reporter_labels);
    let _ = SCREEN_ENCODER_MAX_STALL_GAP_MS.remove_label_values(&reporter_labels);
    let _ = ENCODER_TARGET_BITRATE_KBPS.remove_label_values(&reporter_labels);
    let _ = DECODE_BUDGET_EFFECTIVE_CAP.remove_label_values(&reporter_labels);
    let _ = DECODE_BUDGET_NATURAL.remove_label_values(&reporter_labels);
    let _ = DECODE_BUDGET_PRESSURED.remove_label_values(&reporter_labels);
    let _ = DECODE_BUDGET_OVERRIDE_MODE.remove_label_values(&reporter_labels);
    let _ = DECODE_BUDGET_OVERRIDE_FIXED_N.remove_label_values(&reporter_labels);
    // #1143 gauges: same per-session GC so the high-cardinality session_id
    // label leaves no residual series on disconnect.
    let _ = DECODE_ACTIVE_SET_SIZE.remove_label_values(&reporter_labels);
    let _ = CAPABILITY_SCORE.remove_label_values(&reporter_labels);
    let _ = BATTERY_LEVEL.remove_label_values(&reporter_labels);
    // Layer gauges carry an extra media_kind label; GC all three kinds
    // (camera, screen, audio) that may have been published.
    for kind in ["camera", "screen", "audio"] {
        let layer_labels: [&str; 4] = [
            &session_info.meeting_id,
            &session_info.session_id,
            &session_info.reporting_user_id,
            kind,
        ];
        let _ = ENCODER_EFFECTIVE_LAYERS.remove_label_values(&layer_labels);
        let _ = ENCODER_ACTIVE_LAYERS.remove_label_values(&layer_labels);
    }

    // #1561: Audio congestion ceiling (4-label reporter gauge)
    let _ = AUDIO_CONGESTION_CEILING.remove_label_values(&reporter_labels);
    // #1556: Battery charging, network downlink max, CPU throttled (4-label reporter gauges)
    let _ = BATTERY_CHARGING.remove_label_values(&reporter_labels);
    let _ = CLIENT_NETWORK_DOWNLINK_MAX.remove_label_values(&reporter_labels);
    let _ = CLIENT_CPU_THROTTLED.remove_label_values(&reporter_labels);
    // #1556: CLIENT_NETWORK_TYPE carries an extra network_type label
    if let Some(ref net_type) = session_info.last_network_type {
        let _ = CLIENT_NETWORK_TYPE.remove_label_values(&[
            session_info.meeting_id.as_str(),
            session_info.session_id.as_str(),
            session_info.reporting_user_id.as_str(),
            net_type.as_str(),
        ]);
    }

    // #1561: Receiver-side layer selections — use the exact tracked set so we
    // only attempt to remove series we actually published (not all to_peers).
    for (peer_id, kind) in &session_info.received_layer_peers {
        let _ = RECEIVED_LAYER.remove_label_values(&[
            session_info.meeting_id.as_str(),
            session_info.session_id.as_str(),
            session_info.reporting_user_id.as_str(),
            peer_id.as_str(),
            kind.as_str(),
        ]);
    }

    // TELEM-8/9 cleanup (2-label: meeting_id, session_id)
    let telem_labels: [&str; 2] = [&session_info.meeting_id, &session_info.session_id];
    let _ = CLIENT_LONGTASK_DURATION_MS.remove_label_values(&telem_labels);
    let _ = CLIENT_RENDER_FPS.remove_label_values(&telem_labels);

    // Tier B #3: re-election outcome series (meeting_id, session_id, result).
    // Remove all four bounded result buckets for this session so the
    // (unbounded-over-time) session_id label leaves no residual series on
    // disconnect — same lifecycle GC the other per-session client series use.
    for result in ["proceeded", "aborted", "preserved", "failed"] {
        let _ = CLIENT_REELECTION_TOTAL.remove_label_values(&[
            &session_info.meeting_id,
            &session_info.session_id,
            result,
        ]);
    }
    // #527: encoder restart series (meeting_id, session_id, kind, reason). Remove
    // the full bounded cartesian (2 kinds × 4 reasons) for this session so the
    // session_id label leaves no residual series on disconnect.
    for kind in ["camera", "screen"] {
        for reason in ["closed_codec", "memory", "configure", "other"] {
            let _ = ENCODER_RESTART_TOTAL.remove_label_values(&[
                &session_info.meeting_id,
                &session_info.session_id,
                kind,
                reason,
            ]);
        }
    }
    // TELEM-7: remove CLIENT_INFO using stored label values
    if let Some(ref info_labels) = session_info.client_info_labels {
        let _ = CLIENT_INFO.remove_label_values(&[
            &session_info.meeting_id,
            &session_info.session_id,
            &info_labels[0],
            &info_labels[1],
            &info_labels[2],
            &info_labels[3],
            &info_labels[4],
        ]);
    }

    // Remove active server metrics for this session
    for (server_url, server_type) in &session_info.active_servers {
        let server_labels = [
            &session_info.meeting_id,
            &session_info.session_id,
            &session_info.reporting_user_id,
            server_url.as_str(),
            server_type.as_str(),
        ];
        let _ = CLIENT_ACTIVE_SERVER.remove_label_values(&server_labels);
        let _ = CLIENT_ACTIVE_SERVER_RTT_MS.remove_label_values(&server_labels);
    }

    // Remove all peer connection series we set
    for peer_id in &session_info.peer_ids {
        let _ = PEER_CONNECTIONS_TOTAL.remove_label_values(&[&session_info.meeting_id, peer_id]);
    }

    // Issue 2047: reap this session's TIER_TRANSITIONS_TOTAL series. Iterating
    // the tuples actually published (rather than the cartesian product of the
    // five bounded taxonomies, ~2k combinations) keeps departure O(emitted).
    // Without this the counter never shed a departed session and grew for the
    // process lifetime — the third multiplier in the issue-2047 finding.
    for labels in &session_info.tier_transition_labels {
        let [direction, stream, from_tier, to_tier, trigger] = labels;
        let _ = TIER_TRANSITIONS_TOTAL.remove_label_values(&[
            &session_info.meeting_id,
            &session_info.session_id,
            &session_info.reporting_user_id,
            direction,
            stream,
            from_tier,
            to_tier,
            trigger,
        ]);
    }

    // Remove all to_peer series we set for this session
    for to_peer in &session_info.to_peers {
        remove_per_peer_metrics(
            &session_info.meeting_id,
            &session_info.session_id,
            &session_info.reporting_user_id,
            to_peer,
        );
    }

    // MEETING_PARTICIPANTS is intentionally NOT touched here: it is keyed only by
    // meeting_id (not per-session), so it is recomputed/removed by
    // recompute_meeting_participants() in cleanup_stale_sessions() once all of this
    // meeting's stale sessions have been removed from the tracker.
    debug!(
        "Removed all series for session {} (meeting: {}, peer: {})",
        session_info.session_id, session_info.meeting_id, session_info.reporting_user_id
    );
}

/// Given the peers we have previously published per-pair metrics for
/// (`stored_to_peers`) and the peer ids present in the CURRENT health packet
/// (`current_peer_ids`), return the peers that have DEPARTED this reporter's
/// view — i.e. those we still hold series for but that are no longer reported.
///
/// This is the diff that drives per-packet pruning (issue #1092): a peer that
/// leaves a still-live reporter's view drops out of `peer_stats`, so it is never
/// re-written and — without this prune — its per-pair gauges would be frozen at
/// their last value in Prometheus until the whole reporter session is reaped.
///
/// Pure (no I/O, no metrics side effects) so the diff logic is unit-testable in
/// isolation from the Prometheus registry and the live ingest path.
fn peers_to_prune(
    stored_to_peers: &HashSet<String>,
    current_peer_ids: &HashSet<&str>,
) -> Vec<String> {
    stored_to_peers
        .iter()
        .filter(|peer| !current_peer_ids.contains(peer.as_str()))
        .cloned()
        .collect()
}

/// Remove all per-peer Prometheus metrics for a specific reporter→peer pair.
/// Used for session cleanup and for pruning a peer that has left a still-live
/// reporter's view (issue #1092). Per-pair series are keyed only by the four
/// stable ids `[meeting_id, session_id, from_peer, to_peer]` (#1954 dropped the
/// user-supplied `reporter_name`/`peer_name` PII labels).
fn remove_per_peer_metrics(
    meeting_id: &str,
    session_id: &str,
    reporting_user_id: &str,
    to_peer: &str,
) {
    let labels = [meeting_id, session_id, reporting_user_id, to_peer];

    // Per-peer metrics (22 kept, 7 low-value ones removed for cardinality reduction)
    let _ = PEER_CAN_LISTEN.remove_label_values(&labels);
    let _ = PEER_CAN_SEE.remove_label_values(&labels);
    let _ = NETEQ_AUDIO_BUFFER_MS.remove_label_values(&labels);
    let _ = AUDIO_PLAYOUT_LATENCY_MS.remove_label_values(&labels);
    let _ = NETEQ_TARGET_DELAY_MS.remove_label_values(&labels);
    let _ = NETEQ_PACKETS_AWAITING_DECODE.remove_label_values(&labels);
    let _ = NETEQ_PACKETS_PER_SEC.remove_label_values(&labels);
    let _ = NETEQ_NORMAL_OPS_PER_SEC.remove_label_values(&labels);
    let _ = NETEQ_EXPAND_OPS_PER_SEC.remove_label_values(&labels);
    let _ = NETEQ_ACCELERATE_OPS_PER_SEC.remove_label_values(&labels);
    let _ = VIDEO_FPS.remove_label_values(&labels);
    let _ = VIDEO_BITRATE_KBPS.remove_label_values(&labels);
    let _ = VIDEO_FRAMES_DROPPED.remove_label_values(&labels);
    let _ = PEER_AUDIO_ENABLED.remove_label_values(&labels);
    let _ = PEER_VIDEO_ENABLED.remove_label_values(&labels);
    let _ = AUDIO_QUALITY_SCORE.remove_label_values(&labels);
    let _ = VIDEO_QUALITY_SCORE.remove_label_values(&labels);
    let _ = VIDEO_SEQ_LOSS_PER_SEC.remove_label_values(&labels);
    let _ = AUDIO_DATAGRAM_LOSS_PER_SEC.remove_label_values(&labels);
    let _ = AUDIO_DATAGRAM_RAW_LOSS_PER_SEC.remove_label_values(&labels); // issue 2031
    let _ = VIDEO_PLAYOUT_LATENCY_MS.remove_label_values(&labels);
    let _ = VIDEO_PLAYOUT_STAGE1_SPAN_MS.remove_label_values(&labels);
    let _ = VIDEO_PLAYOUT_PAINT_LAG_MS.remove_label_values(&labels);
    let _ = VIDEO_CONTENT_STALENESS_MS.remove_label_values(&labels);
    let _ = VIDEO_SKIP_TO_LIVE_TOTAL.remove_label_values(&labels);
    let _ = KEYFRAME_REQUESTS_PER_SEC.remove_label_values(&labels);
    let _ = CALL_QUALITY_SCORE.remove_label_values(&labels);
    let _ = AUDIO_CONCEALMENT_PCT.remove_label_values(&labels);
    let _ = DECODER_ERRORS_TOTAL.remove_label_values(&labels);
    let _ = SCREEN_VIDEO_FPS.remove_label_values(&labels);
    let _ = SCREEN_VIDEO_BITRATE_KBPS.remove_label_values(&labels);
    // Screen-share playout family (#1660): sweep the screen siblings so they do not
    // leak per-pair series after the peer/session departs (mirrors the camera
    // VIDEO_PLAYOUT_* / VIDEO_CONTENT_STALENESS_MS / VIDEO_SKIP_TO_LIVE_TOTAL removals above).
    let _ = SCREEN_VIDEO_PLAYOUT_LATENCY_MS.remove_label_values(&labels);
    let _ = SCREEN_VIDEO_PLAYOUT_STAGE1_SPAN_MS.remove_label_values(&labels);
    let _ = SCREEN_VIDEO_PLAYOUT_PAINT_LAG_MS.remove_label_values(&labels);
    let _ = SCREEN_VIDEO_CONTENT_STALENESS_MS.remove_label_values(&labels);
    let _ = SCREEN_VIDEO_SKIP_TO_LIVE_TOTAL.remove_label_values(&labels);

    // NOTE: RECEIVED_LAYER is intentionally NOT reaped here. Its series are
    // reaped authoritatively from the #1561 tracked set
    // (`session_info.received_layer_peers`) in `remove_session_metrics` (whole-
    // session departure) and in the per-packet constraint-clear path — those use
    // the exact 5-label key `[meeting_id, session_id, peer_id, from_peer,
    // media_kind]` for series actually published. A speculative
    // `for kind in [video,screen,audio]` sweep here previously duplicated that
    // job (and after #1580 passed a stale 6th display_name value, silently
    // erroring on every call); removed to keep the tracked set the single source
    // of truth.
}

/// Bounded taxonomy for the client-supplied `active_server_type` label
/// (issue 2047).
///
/// `active_server_type` arrives verbatim from the client and is used as a
/// Prometheus LABEL on `CLIENT_ACTIVE_SERVER` and `CLIENT_ACTIVE_SERVER_RTT_MS`,
/// so an unbounded value mints a new series per distinct string — the same
/// cardinality lever issue 2031 already closed for the `transport` label on
/// `CLIENT_AUDIO_CONCEALMENT_PCT` (which publishes only for
/// `"webtransport"`/`"websocket"`). These are the only two transports a
/// conformant client reports (`HealthReporter` folds the elected connection's
/// type), so anything else is a stale or forged client.
const KNOWN_ACTIVE_SERVER_TYPES: [&str; 2] = ["webtransport", "websocket"];

/// Collapse an unrecognized `active_server_type` onto a fixed label set.
///
/// Returns the value unchanged when it is a known transport, `""` when the
/// client did not populate the field at all (preserving the pre-existing
/// "blank label = unknown source" convention the RTT gauge documents), and
/// `"unknown"` for any other string. Total label cardinality is therefore at
/// most three values, whatever a client sends.
fn bounded_active_server_type(raw: &str) -> &str {
    if raw.is_empty() || KNOWN_ACTIVE_SERVER_TYPES.contains(&raw) {
        raw
    } else {
        "unknown"
    }
}

// ---------------------------------------------------------------------------
// Bounded taxonomy for the `TierTransition` labels (issue 2047, security)
// ---------------------------------------------------------------------------
//
// `TIER_TRANSITIONS_TOTAL` is a CounterVec carrying FIVE client-authored strings
// (`direction`, `stream`, `from_tier`, `to_tier`, `trigger`) on top of the three
// identity labels. Before this change all five were copied verbatim, which is a
// strictly worse cardinality lever than the `active_server_type` one above:
//
//   * `tier_transitions` is a REPEATED field with no server-side length cap, so
//     a single packet under `MAX_FRAME_SIZE` could carry tens of thousands of
//     entries (see `MAX_TIER_TRANSITIONS_PER_PACKET`);
//   * HEALTH has no per-sender rate limiter (only KEYFRAME_REQUEST and REACTION
//     do — `SessionLogic::keyframe_limiter` / `reaction_limiter`), so packets can
//     be sent in a tight loop; and
//   * the series had no cleanup path, so they persisted for the process lifetime.
//
// Bounding all five collapses the per-session label space from unbounded to
// `2 x 3 x N x N x 3` where `N` is the tier-label count — a few thousand series
// worst case instead of unbounded growth.
//
// SOURCE OF TRUTH: the tier LABELS are read from `videocall_aq::constants`, the
// same arrays the client indexes when it builds a `TierTransitionRecord`
// (`videocall-aq/src/manager.rs` stores `video_tiers[i].label`). Deriving the
// allowlist from the shared crate instead of hardcoding a copy means a new tier
// cannot silently start reporting as "unknown".
//
// The direction/stream/trigger sets below were verified against the EMITTING
// code, not the proto comments: `TierTransitionRecord`'s `direction`, `stream`
// and `trigger` are `&'static str` and every construction site in
// `videocall-aq/src/manager.rs` uses one of these literals, with
// `screen_encoder.rs` overriding `stream` to "screen" for the screen buffer.
//
// NOTE: the proto comment on `TierTransition.trigger`
// (`protobuf/types/health_packet.proto`) is STALE — it lists "fps"/"bitrate",
// which no longer appear anywhere in the emitting code; the real third value is
// "backpressure". Trusting that comment would have made every real
// backpressure-triggered transition collapse to "unknown".
const KNOWN_TIER_DIRECTIONS: [&str; 2] = ["up", "down"];
const KNOWN_TIER_STREAMS: [&str; 3] = ["video", "audio", "screen"];
const KNOWN_TIER_TRIGGERS: [&str; 3] = ["backpressure", "congestion", "coordination"];

/// Placeholder every out-of-taxonomy `TierTransition` label collapses onto.
const UNKNOWN_LABEL: &str = "unknown";

/// Maximum `tier_transitions` entries ingested from ONE health packet.
///
/// The allowlist above bounds how many distinct SERIES a client can create; this
/// bounds the per-packet WORK (label lookup + `.inc()` under the registry lock),
/// which the allowlist does not. Without it, one ~4 MB frame
/// (`MAX_FRAME_SIZE`) of minimal `TierTransition` messages is ~50k label
/// operations per packet, repeatable in a loop.
///
/// 64 is ~8x the legitimate worst case. A client drains its transition buffers
/// once per health packet (default interval 5000 ms — `VideoCallClientOptions::
/// health_reporting_interval_ms`, the only configured value in the UI) from TWO
/// `AdaptiveQualityManager` buffers (camera + screen), and each manager gates
/// transitions behind `MIN_TIER_TRANSITION_INTERVAL_MS` (1500 ms) — so ~3 per
/// stream, ~6-8 total per packet. The headroom absorbs a reconfigured interval up
/// to ~48s without dropping real events.
const MAX_TIER_TRANSITIONS_PER_PACKET: usize = 64;

/// Collapse a client-supplied `TierTransition` label onto its fixed taxonomy,
/// returning [`UNKNOWN_LABEL`] for anything outside it.
fn bounded_tier_label<'a>(raw: &'a str, allowed: &[&str]) -> &'a str {
    if allowed.contains(&raw) {
        raw
    } else {
        UNKNOWN_LABEL
    }
}

/// Is `raw` a tier label defined by the shared adaptive-quality constants?
///
/// Checks the video, screen and audio tier arrays — the three the client can
/// index when recording a transition.
fn is_known_tier_name(raw: &str) -> bool {
    use videocall_aq::constants::{AUDIO_QUALITY_TIERS, SCREEN_QUALITY_TIERS, VIDEO_QUALITY_TIERS};
    VIDEO_QUALITY_TIERS.iter().any(|t| t.label == raw)
        || SCREEN_QUALITY_TIERS.iter().any(|t| t.label == raw)
        || AUDIO_QUALITY_TIERS.iter().any(|t| t.label == raw)
}

/// Bound a `from_tier` / `to_tier` label to the shared tier taxonomy.
fn bounded_tier_name(raw: &str) -> &str {
    if is_known_tier_name(raw) {
        raw
    } else {
        UNKNOWN_LABEL
    }
}

/// Guard for every CLIENT-REPORTED floating-point telemetry sample (issue 2047).
///
/// Prometheus gauges and histograms accept any `f64`, including `NaN` and
/// `±Inf`. A single non-finite sample is not a one-off blemish: it POISONS every
/// aggregate a dashboard runs over that series (an `avg()` or `sum()` spanning a
/// `NaN` returns `NaN`) and, for a histogram, latches its `_sum` to `NaN` for the
/// process lifetime. Client telemetry is the one input
/// here that is entirely attacker-controlled, and several of these values are
/// computed client-side as ratios — so `0/0` reaches the wire without anyone
/// being malicious.
///
/// Non-finite samples are SKIPPED, not clamped to `0.0`: a zero would be
/// indistinguishable from a genuine "nothing happening" reading and would drag
/// averages down, whereas skipping leaves the gauge at its last good value and
/// lets the normal staleness/GC path retire it.
///
/// Applied to every sink fed by a `double`/`float` proto field. Sinks fed by
/// integer fields, booleans, or literal constants cannot be non-finite by
/// construction and keep plain `.set()`.
///
/// This predicate is the SINGLE gate — the gauge and histogram wrappers below
/// both route through it, so the rule and its drop accounting are defined in
/// exactly one place.
fn is_publishable_sample(value: f64) -> bool {
    if value.is_finite() {
        true
    } else {
        NON_FINITE_SAMPLES_DROPPED_TOTAL.inc();
        false
    }
}

/// `set()` a gauge only when the client-reported sample is finite
/// ([`is_publishable_sample`]).
trait SetFinite {
    fn set_finite(&self, value: f64);
}

impl SetFinite for prometheus::Gauge {
    fn set_finite(&self, value: f64) {
        if is_publishable_sample(value) {
            self.set(value);
        }
    }
}

/// `observe()` a histogram sample only when it is finite
/// ([`is_publishable_sample`]).
///
/// A non-finite observation corrupts `_sum` irrecoverably: buckets are integer
/// counts, but the sum is a running `f64` with no way back from `NaN` short of
/// restarting the process.
trait ObserveFinite {
    fn observe_finite(&self, value: f64);
}

impl ObserveFinite for prometheus::Histogram {
    fn observe_finite(&self, value: f64) {
        if is_publishable_sample(value) {
            self.observe(value);
        }
    }
}

fn process_health_packet_to_metrics_pb(
    health_packet: &PbHealthPacket,
    session_tracker: &SessionTracker,
) -> anyhow::Result<()> {
    HEALTH_REPORTS_TOTAL.inc();
    // Force registration of the issue-2047 rejection counter on the first health
    // packet. `lazy_static` registers a metric on first DEREF, and the only other
    // deref is inside `is_publishable_sample`'s failure branch — so without this
    // the series would be missing from /metrics entirely until the first bad
    // sample, and a dashboard panel would read "No data" instead of 0.
    NON_FINITE_SAMPLES_DROPPED_TOTAL.inc_by(0.0);

    let meeting_id = if health_packet.meeting_id.is_empty() {
        "unknown"
    } else {
        &health_packet.meeting_id
    };

    let session_id = if health_packet.session_id.is_empty() {
        "unknown"
    } else {
        &health_packet.session_id
    };

    let reporting_user_id_str = if health_packet.reporting_user_id.is_empty() {
        "unknown".to_string()
    } else {
        videocall_types::user_id_bytes_to_string(&health_packet.reporting_user_id)
    };
    let reporting_user_id = reporting_user_id_str.as_str();

    // Extract reporter's display name; fall back to email if absent.
    // Still feeds the per-client peer_info metric (#1580) and the peer_info
    // reap/rename-dedup below; it is NOT a per-pair label after #1954.
    let reporter_display_name = health_packet
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(reporting_user_id)
        .to_string();

    // Update session tracker: create entry on first packet, then refresh last_seen only.
    // Using entry().or_insert_with() preserves accumulated to_peers/peer_ids/active_servers
    // across packets. The previous tracker.insert() reset them every packet, causing a leak
    // where peers that left mid-session had their Prometheus labels written but never cleaned up.
    let session_key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
    {
        let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
        let info = tracker
            .entry(session_key.clone())
            .or_insert_with(|| SessionInfo {
                session_id: session_id.to_string(),
                meeting_id: meeting_id.to_string(),
                reporting_user_id: reporting_user_id.to_string(),
                display_name: reporter_display_name.clone(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            });
        info.last_seen = Instant::now();
        if info.display_name != reporter_display_name {
            let _ = VIDEOCALL_PEER_INFO.remove_label_values(&[
                meeting_id,
                session_id,
                reporting_user_id,
                info.display_name.as_str(),
            ]);
        }
        info.display_name = reporter_display_name.clone();
    }

    // Process metrics for this session
    {
        // Strip JWT token from URL to prevent leaking credentials in Prometheus labels.
        // Handles both ?token=... (only param) and &token=... (among other params).
        // Note: upstream scrubbing in `client_diagnostics.rs::scrub_client_supplied_urls`
        // unconditionally zeroes `active_server_url` (defense-in-depth against JWT leakage
        // to Prometheus labels), so in practice this branch only fires for legacy/test paths
        // that bypass that scrub. We still compute the clean URL here for those paths.
        let server_url_clean = if let Some(q_pos) = health_packet.active_server_url.find('?') {
            let base = &health_packet.active_server_url[..q_pos];
            let query = &health_packet.active_server_url[q_pos + 1..];
            let filtered: Vec<&str> = query
                .split('&')
                .filter(|p| !p.starts_with("token="))
                .collect();
            if filtered.is_empty() {
                base.to_string()
            } else {
                format!("{}?{}", base, filtered.join("&"))
            }
        } else {
            health_packet.active_server_url.clone()
        };
        let server_url_clean = server_url_clean.as_str();

        // For the RTT metric we allow the server_type label to be an empty string when
        // unset — dashboards already treat blank labels as "unknown source". Note: the
        // upstream URL scrub (`client_diagnostics.rs::scrub_client_supplied_urls`) clears
        // `active_server_url` but does NOT touch `active_server_type`, so this branch
        // handles the legitimate "client didn't populate type" case. The
        // CLIENT_ACTIVE_SERVER gauge below keeps its "unknown" placeholder since it still
        // requires a URL.
        //
        // Issue 2047: both labels are bounded to a fixed taxonomy first — the raw
        // field is a free-form client string, so publishing it verbatim let a
        // client mint one series per distinct value.
        let server_type_for_rtt = bounded_active_server_type(&health_packet.active_server_type);
        let server_type_for_active = if server_type_for_rtt.is_empty() {
            "unknown"
        } else {
            server_type_for_rtt
        };

        // Publish RTT independently of active_server_url presence. The upstream scrub
        // strips the URL to prevent JWT leakage, but the RTT value itself is meaningful
        // on its own — gate only on rtt != 0.0 so passthrough clients that never
        // measured an RTT don't emit a zero sample.
        if health_packet.active_server_rtt_ms != 0.0 {
            CLIENT_ACTIVE_SERVER_RTT_MS
                .with_label_values(&[
                    meeting_id,
                    session_id,
                    reporting_user_id,
                    server_url_clean,
                    server_type_for_rtt,
                ])
                .set_finite(health_packet.active_server_rtt_ms);

            // Track the label set used for this RTT publish so cleanup can remove it
            // later, including the scrubbed empty-URL / empty-type case.
            {
                let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                let key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
                if let Some(info) = tracker.get_mut(&key) {
                    info.active_servers.insert((
                        server_url_clean.to_string(),
                        server_type_for_rtt.to_string(),
                    ));
                }
            }
        }

        // Client-side active server info (optional) — requires a non-empty URL because
        // the metric's semantic purpose is to identify *which* server the client picked.
        if !health_packet.active_server_url.is_empty() {
            CLIENT_ACTIVE_SERVER
                .with_label_values(&[
                    meeting_id,
                    session_id,
                    reporting_user_id,
                    server_url_clean,
                    server_type_for_active,
                ])
                .set(1.0);

            // Track server info used for cleanup
            {
                let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                let key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
                if let Some(info) = tracker.get_mut(&key) {
                    info.active_servers.insert((
                        server_url_clean.to_string(),
                        server_type_for_active.to_string(),
                    ));
                }
            }
        }
        // Set active session metric
        ACTIVE_SESSIONS_TOTAL
            .with_label_values(&[meeting_id, session_id])
            .set(1.0);

        // Self-state reported by the sender (authoritative)
        debug!(
            "Setting SELF_AUDIO_ENABLED for meeting={}, peer={}, value={}",
            meeting_id, reporting_user_id, health_packet.reporting_audio_enabled
        );
        SELF_AUDIO_ENABLED
            .with_label_values(&[meeting_id, reporting_user_id])
            .set(if health_packet.reporting_audio_enabled {
                1.0
            } else {
                0.0
            });

        debug!(
            "Setting SELF_VIDEO_ENABLED for meeting={}, peer={}, value={}",
            meeting_id, reporting_user_id, health_packet.reporting_video_enabled
        );
        SELF_VIDEO_ENABLED
            .with_label_values(&[meeting_id, reporting_user_id])
            .set(if health_packet.reporting_video_enabled {
                1.0
            } else {
                0.0
            });

        VIDEOCALL_PEER_INFO
            .with_label_values(&[
                meeting_id,
                session_id,
                reporting_user_id,
                reporter_display_name.as_str(),
            ])
            .set(1.0);

        // Tab visibility (HealthPacket level)
        debug!(
            "Setting CLIENT_TAB_VISIBLE for meeting={}, session={}, peer={}, value={}",
            meeting_id, session_id, reporting_user_id, health_packet.is_tab_visible
        );
        CLIENT_TAB_VISIBLE
            .with_label_values(&[meeting_id, session_id, reporting_user_id])
            .set(if health_packet.is_tab_visible {
                1.0
            } else {
                0.0
            });

        // Memory usage (HealthPacket level, Chrome only)
        if let Some(mem_used) = health_packet.memory_used_bytes {
            debug!(
                "Setting CLIENT_MEMORY_USED_BYTES for meeting={}, session={}, peer={}, value={} bytes",
                meeting_id, session_id, reporting_user_id, mem_used
            );
            CLIENT_MEMORY_USED_BYTES
                .with_label_values(&[meeting_id, session_id, reporting_user_id])
                .set(mem_used as f64);
        }

        if let Some(mem_total) = health_packet.memory_total_bytes {
            debug!(
                "Setting CLIENT_MEMORY_TOTAL_BYTES for meeting={}, session={}, peer={}, value={} bytes",
                meeting_id, session_id, reporting_user_id, mem_total
            );
            CLIENT_MEMORY_TOTAL_BYTES
                .with_label_values(&[meeting_id, session_id, reporting_user_id])
                .set(mem_total as f64);
        }

        // #1032: WASM linear memory (always available on the client).
        if let Some(wasm_mem) = health_packet.wasm_memory_bytes {
            CLIENT_WASM_MEMORY_BYTES
                .with_label_values(&[meeting_id, session_id, reporting_user_id])
                .set(wasm_mem as f64);
        }

        // #1032: total agent memory (Chrome + crossOriginIsolated only; absent
        // otherwise, in which case the series simply never appears).
        if let Some(agent_mem) = health_packet.agent_memory_bytes {
            CLIENT_AGENT_MEMORY_BYTES
                .with_label_values(&[meeting_id, session_id, reporting_user_id])
                .set(agent_mem as f64);
        }

        // Tier B #3 / #562: transport re-election outcomes. The client reports a
        // CUMULATIVE total per result in every packet; we `.set()` the gauge to
        // that value (NOT `.inc()`, which would multiply-count the same total
        // each second — see the type-decision note on CLIENT_REELECTION_TOTAL).
        // `result` is bounded to exactly these four values; the per-(meeting,
        // session) series are GC'd by the stale-session cleanup below. Each
        // field is absent until the client has seen at least one such outcome,
        // so the series only appears once a re-election has actually happened.
        for (result, value) in [
            ("proceeded", health_packet.reelection_proceeded_total),
            ("aborted", health_packet.reelection_aborted_total),
            ("preserved", health_packet.reelection_preserved_total),
            ("failed", health_packet.reelection_failed_total),
        ] {
            if let Some(total) = value {
                CLIENT_REELECTION_TOTAL
                    .with_label_values(&[meeting_id, session_id, result])
                    .set(total as f64);
            }
        }

        // #527: encoder auto-restart cycles, expanded into a single labeled
        // gauge videocall_encoder_restart_total{kind, reason}. Same set()-to-
        // cumulative convention as the re-election counter above. Each field is
        // absent until the encoder has actually restarted for that reason, so a
        // series only appears once a restart of that (kind, reason) has occurred.
        for (kind, reason, value) in [
            (
                "camera",
                "closed_codec",
                health_packet.camera_encoder_restarts_closed_codec,
            ),
            (
                "camera",
                "memory",
                health_packet.camera_encoder_restarts_memory,
            ),
            (
                "camera",
                "configure",
                health_packet.camera_encoder_restarts_configure,
            ),
            (
                "camera",
                "other",
                health_packet.camera_encoder_restarts_other,
            ),
            (
                "screen",
                "closed_codec",
                health_packet.screen_encoder_restarts_closed_codec,
            ),
            (
                "screen",
                "memory",
                health_packet.screen_encoder_restarts_memory,
            ),
            (
                "screen",
                "configure",
                health_packet.screen_encoder_restarts_configure,
            ),
            (
                "screen",
                "other",
                health_packet.screen_encoder_restarts_other,
            ),
        ] {
            if let Some(total) = value {
                ENCODER_RESTART_TOTAL
                    .with_label_values(&[meeting_id, session_id, kind, reason])
                    .set(total as f64);
            }
        }

        // Communication and browser state metrics
        let reporter_labels: [&str; 3] = [meeting_id, session_id, reporting_user_id];

        if let Some(send_queue) = health_packet.send_queue_bytes {
            CLIENT_SEND_QUEUE_BYTES
                .with_label_values(&reporter_labels)
                .set(send_queue as f64);
        }

        if let Some(rx_pps) = health_packet.packets_received_per_sec {
            CLIENT_PACKETS_RECEIVED_PER_SEC
                .with_label_values(&reporter_labels)
                .set_finite(rx_pps);
        }

        if let Some(tx_pps) = health_packet.packets_sent_per_sec {
            CLIENT_PACKETS_SENT_PER_SEC
                .with_label_values(&reporter_labels)
                .set_finite(tx_pps);
        }

        CLIENT_TAB_THROTTLED
            .with_label_values(&reporter_labels)
            .set(if health_packet.is_tab_throttled {
                1.0
            } else {
                0.0
            });

        // Receiver-side quality metrics
        if let Some(tier) = health_packet.adaptive_video_tier {
            ADAPTIVE_VIDEO_TIER
                .with_label_values(&reporter_labels)
                .set(tier as f64);
        }
        if let Some(tier) = health_packet.adaptive_audio_tier {
            ADAPTIVE_AUDIO_TIER
                .with_label_values(&reporter_labels)
                .set(tier as f64);
        }
        if let Some(drops) = health_packet.datagram_drops_total {
            DATAGRAM_DROPS
                .with_label_values(&reporter_labels)
                .set(drops as f64);
        }

        // Issue 2031: per-client WebTransport receive-health telemetry.
        // read-loop max gap folds unconditionally on the client (0.0 on WS), so
        // .set() lets the gauge recover to 0 instead of latching. The `if let
        // Some` guards an older client that omits it.
        if let Some(gap) = health_packet.wt_datagram_read_loop_max_gap_ms {
            CLIENT_DATAGRAM_READ_LOOP_MAX_GAP_MS
                .with_label_values(&reporter_labels)
                .set_finite(gap);
        }
        // Queue read-back: a one-shot per-browser constant, present only once the
        // WT queue was configured (absent for a WS-only client).
        if let Some(hwm) = health_packet.wt_incoming_datagram_high_water_mark {
            WT_INCOMING_DATAGRAM_HIGH_WATER_MARK
                .with_label_values(&reporter_labels)
                .set_finite(hwm);
        }
        if let Some(max_age) = health_packet.wt_incoming_datagram_max_age_ms {
            WT_INCOMING_DATAGRAM_MAX_AGE_MS
                .with_label_values(&reporter_labels)
                .set_finite(max_age);
        }
        // Per-client mean audio concealment, SPLIT BY the reporter's active
        // transport (the ground-truth WS-vs-WT severity gap). Only exported for a
        // known transport so an empty/unknown active_server_type never spawns a
        // junk `transport=""` series.
        if let Some(concealment) = health_packet.client_audio_concealment_pct {
            let transport = health_packet.active_server_type.as_str();
            // Issue 2047: routed through the shared taxonomy rather than a
            // duplicated string comparison, so this gate and
            // `bounded_active_server_type` cannot drift apart.
            if KNOWN_ACTIVE_SERVER_TYPES.contains(&transport) {
                CLIENT_AUDIO_CONCEALMENT_PCT
                    .with_label_values(&[meeting_id, session_id, reporting_user_id, transport])
                    .set_finite(concealment);
                // Issue 2031: a live WT<->WS switch (routine once the issue-2029
                // fallback ships) would otherwise leave the OLD transport's series
                // for this same identity latched at its last value until session
                // GC, misleading the by-transport panel. Clear every OTHER
                // transport's series so only the currently-active one reports.
                // Ignore the not-found error (no prior sibling series is the
                // common case). Issue 2047: iterating the shared taxonomy instead
                // of hardcoding the two-way pairing keeps this correct if a third
                // transport is ever added.
                for sibling in KNOWN_ACTIVE_SERVER_TYPES
                    .iter()
                    .filter(|t| **t != transport)
                {
                    let _ = CLIENT_AUDIO_CONCEALMENT_PCT.remove_label_values(&[
                        meeting_id,
                        session_id,
                        reporting_user_id,
                        sibling,
                    ]);
                }
            }
        }
        if let Some(bytes) = health_packet.unistream_bytes_offered_total {
            UNISTREAM_BYTES_OFFERED_TOTAL
                .with_label_values(&reporter_labels)
                .set(bytes as f64);
        }
        if let Some(bytes) = health_packet.unistream_bytes_drained_total {
            UNISTREAM_BYTES_DRAINED_TOTAL
                .with_label_values(&reporter_labels)
                .set(bytes as f64);
        }
        if let Some(drops) = health_packet.unistream_stale_delta_drops_total {
            UNISTREAM_STALE_DELTA_DROPS_TOTAL
                .with_label_values(&reporter_labels)
                .set(drops as f64);
        }
        if let Some(drops) = health_packet.websocket_drops_total {
            WEBSOCKET_DROPS
                .with_label_values(&reporter_labels)
                .set(drops as f64);
        }
        if let Some(kf_reqs) = health_packet.keyframe_requests_sent_total {
            KEYFRAME_REQUESTS_SENT_TOTAL
                .with_label_values(&reporter_labels)
                .set(kf_reqs as f64);
        }

        // RTT-probe resilience signals (#522). Each is a CUMULATIVE total reported by
        // the client every packet; .set() the gauge to that value keyed by the
        // reporter labels (NOT .inc(), NOT summed across reporters — that would
        // multiply/double-count). Absent until the client has seen at least one
        // event, so the series only appears once there is something to show.
        if let Some(dropped) = health_packet.rtt_probe_dropped_total {
            RTT_PROBE_DROPPED_TOTAL
                .with_label_values(&reporter_labels)
                .set(dropped as f64);
        }
        if let Some(suppressions) = health_packet.rtt_probe_stale_suppressions_total {
            RTT_PROBE_STALE_SUPPRESSIONS_TOTAL
                .with_label_values(&reporter_labels)
                .set(suppressions as f64);
        }

        // Encoder decision inputs (P0). NOTE(#1184): encoder_fps_ratio removed
        // (dead telemetry — receiver FPS no longer feeds the sender AQ).
        // NOTE(#1231): the `encoder_p75_peer_fps` wire field now carries the
        // encoder queue depth (sender-side backpressure); the gauge was renamed
        // to videocall_encoder_queue_depth. The proto field name is unchanged.
        if let Some(queue_depth) = health_packet.encoder_p75_peer_fps {
            ENCODER_QUEUE_DEPTH
                .with_label_values(&reporter_labels)
                .set_finite(queue_depth);
        }
        if let Some(tier) = health_packet.adaptive_screen_tier {
            ADAPTIVE_SCREEN_TIER
                .with_label_values(&reporter_labels)
                .set(tier as f64);
        }
        if let Some(active) = health_packet.screen_sharing_active {
            SCREEN_SHARING_ACTIVE
                .with_label_values(&reporter_labels)
                .set(if active { 1.0 } else { 0.0 });
        }

        // Encoder outputs (P1)
        if let Some(fps) = health_packet.encoder_output_fps {
            ENCODER_OUTPUT_FPS
                .with_label_values(&reporter_labels)
                .set(fps as f64);
        }
        // #2147: SCREEN encoder output fps. The client sends an honest 0 whenever a
        // screen encoder is BOUND but producing nothing, and omits the field only
        // when none is bound at all, so `if let Some` is the whole gate — do NOT add
        // a `> 0` filter here. Adding one would reproduce the #2079 blind spot this
        // metric exists to close (a stalled screen encoder would become
        // indistinguishable from an absent one).
        //
        // A 0 therefore does NOT mean "not sharing" — the dioxus-ui client binds its
        // screen encoder eagerly at Host mount, so it reports 0 while merely idle.
        // Join with `videocall_screen_sharing_active` to interpret it (see the gauge
        // declaration in metrics.rs and the proto field's own doc).
        if let Some(fps) = health_packet.screen_encoder_output_fps {
            SCREEN_ENCODER_OUTPUT_FPS
                .with_label_values(&reporter_labels)
                .set(fps as f64);
        }
        // #2147: the stall pair — the half fps CANNOT show. `fps > 0` with these
        // RISING is the #1899/#2143 freeze (synthetic re-encodes keep fps nonzero
        // while receivers sit on stale content); `fps > 0` with these flat/absent is
        // genuinely healthy. The CLIENT gates these `> 0` at the producer
        // (`health_reporter.rs`) because they are monotonic counters where a 0 carries
        // no information, so an absent field here means "no stalls yet" — do NOT add a
        // server-side `> 0` gate, the `if let Some` is the whole gate. That is
        // deliberately the OPPOSITE convention from the fps field above, where 0 IS a
        // real reading and must not be gated at either end.
        if let Some(episodes) = health_packet.screen_encoder_stall_episodes {
            SCREEN_ENCODER_STALL_EPISODES
                .with_label_values(&reporter_labels)
                .set(episodes as f64);
        }
        if let Some(gap_ms) = health_packet.screen_encoder_max_stall_gap_ms {
            SCREEN_ENCODER_MAX_STALL_GAP_MS
                .with_label_values(&reporter_labels)
                .set(gap_ms as f64);
        }
        if let Some(kbps) = health_packet.encoder_target_bitrate_kbps {
            ENCODER_TARGET_BITRATE_KBPS
                .with_label_values(&reporter_labels)
                .set_finite(kbps);
        }
        // NOTE(#1184): encoder_bitrate_ratio removed (dead telemetry).

        // Decode-budget state (#987 / PR #999)
        if let Some(db) = health_packet.decode_budget.as_ref() {
            DECODE_BUDGET_EFFECTIVE_CAP
                .with_label_values(&reporter_labels)
                .set(db.effective_cap as f64);
            DECODE_BUDGET_NATURAL
                .with_label_values(&reporter_labels)
                .set(db.natural as f64);
            DECODE_BUDGET_PRESSURED
                .with_label_values(&reporter_labels)
                .set(if db.pressured { 1.0 } else { 0.0 });
            DECODE_BUDGET_OVERRIDE_MODE
                .with_label_values(&reporter_labels)
                .set(db.override_mode.value() as f64);
            DECODE_BUDGET_OVERRIDE_FIXED_N
                .with_label_values(&reporter_labels)
                .set(db.override_fixed_n as f64);
            // #1143: tiles ACTUALLY decoding right now (the "videos showing"
            // count). The client stamps active_set = min(effective_cap, natural);
            // this gauge is the realized companion to the effective_cap ceiling.
            DECODE_ACTIVE_SET_SIZE
                .with_label_values(&reporter_labels)
                .set(db.active_set as f64);
        }

        // Send-side simulcast layer counts (#1143). p90==1 across a meeting is
        // the inert-simulcast signal. The proto currently carries the CAMERA
        // encoder's state, so these are labeled media_kind="camera".
        if let Some(layers) = health_packet.effective_video_layers {
            ENCODER_EFFECTIVE_LAYERS
                .with_label_values(&[meeting_id, session_id, reporting_user_id, "camera"])
                .set(layers as f64);
        }
        if let Some(layers) = health_packet.active_video_layers {
            ENCODER_ACTIVE_LAYERS
                .with_label_values(&[meeting_id, session_id, reporting_user_id, "camera"])
                .set(layers as f64);
        }

        // #1561: Screen encoder simulcast layer counts
        if let Some(layers) = health_packet.effective_screen_layers {
            ENCODER_EFFECTIVE_LAYERS
                .with_label_values(&[meeting_id, session_id, reporting_user_id, "screen"])
                .set(layers as f64);
        }
        if let Some(layers) = health_packet.active_screen_layers {
            ENCODER_ACTIVE_LAYERS
                .with_label_values(&[meeting_id, session_id, reporting_user_id, "screen"])
                .set(layers as f64);
        }

        // #1561: Audio encoder simulcast layer counts
        if let Some(layers) = health_packet.effective_audio_layers {
            ENCODER_EFFECTIVE_LAYERS
                .with_label_values(&[meeting_id, session_id, reporting_user_id, "audio"])
                .set(layers as f64);
        }
        // Audio active layers are reported independently from the congestion
        // ceiling because a user-selected send cap also reduces publication.
        // Fall back to the old derivation for rolling upgrades from clients that
        // do not yet carry active_audio_layers.
        if let Some(effective) = health_packet.effective_audio_layers {
            let active = health_packet.active_audio_layers.unwrap_or_else(|| {
                health_packet
                    .audio_congestion_ceiling
                    .map(|c| effective.min(c))
                    .unwrap_or(effective)
            });
            ENCODER_ACTIVE_LAYERS
                .with_label_values(&[meeting_id, session_id, reporting_user_id, "audio"])
                .set(active as f64);
        }

        // #1561: Audio congestion ceiling. In the uncapped (healthy) state the
        // client omits this field; emit the effective count so Grafana always
        // has a value (ceiling == effective → no shed).
        {
            let ceiling_val = match (
                health_packet.audio_congestion_ceiling,
                health_packet.effective_audio_layers,
            ) {
                (Some(c), _) => Some(c as f64),
                (None, Some(e)) => Some(e as f64),
                _ => None,
            };
            if let Some(v) = ceiling_val {
                AUDIO_CONGESTION_CEILING
                    .with_label_values(&[meeting_id, session_id, reporting_user_id])
                    .set(v);
            }
        }

        // #1561: Receiver-side layer selections. Track which (peer, kind) pairs
        // are currently constrained so we can remove stale series when a constraint
        // clears (peer recovers to top layer → entry disappears from the map).
        {
            let mut current_pairs: HashSet<(String, String)> = HashSet::new();
            for (peer_session_id, layer) in &health_packet.received_video_layer {
                RECEIVED_LAYER
                    .with_label_values(&[
                        meeting_id,
                        session_id,
                        reporting_user_id,
                        peer_session_id.as_str(),
                        "video",
                    ])
                    .set(*layer as f64);
                current_pairs.insert((peer_session_id.clone(), "video".to_string()));
            }
            for (peer_session_id, layer) in &health_packet.received_screen_layer {
                RECEIVED_LAYER
                    .with_label_values(&[
                        meeting_id,
                        session_id,
                        reporting_user_id,
                        peer_session_id.as_str(),
                        "screen",
                    ])
                    .set(*layer as f64);
                current_pairs.insert((peer_session_id.clone(), "screen".to_string()));
            }
            for (peer_session_id, layer) in &health_packet.received_audio_layer {
                RECEIVED_LAYER
                    .with_label_values(&[
                        meeting_id,
                        session_id,
                        reporting_user_id,
                        peer_session_id.as_str(),
                        "audio",
                    ])
                    .set(*layer as f64);
                current_pairs.insert((peer_session_id.clone(), "audio".to_string()));
            }

            // Remove stale series for constraints that cleared since last packet
            let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(info) = tracker.get_mut(&session_key) {
                for (peer_id, kind) in info.received_layer_peers.difference(&current_pairs) {
                    let _ = RECEIVED_LAYER.remove_label_values(&[
                        meeting_id,
                        session_id,
                        reporting_user_id,
                        peer_id.as_str(),
                        kind.as_str(),
                    ]);
                }
                info.received_layer_peers = current_pairs;
            }
        }

        // #1556: Battery charging state
        if let Some(charging) = health_packet.client_battery_charging {
            BATTERY_CHARGING
                .with_label_values(&[meeting_id, session_id, reporting_user_id])
                .set(if charging { 1.0 } else { 0.0 });
        }

        // #1556: Network type
        if let Some(ref net_type) = health_packet.client_network_type {
            // Remove stale series if network type changed, then store new value.
            {
                let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(info) = tracker.get_mut(&session_key) {
                    if let Some(ref prev) = info.last_network_type {
                        if prev != net_type {
                            let _ = CLIENT_NETWORK_TYPE.remove_label_values(&[
                                meeting_id,
                                session_id,
                                reporting_user_id,
                                prev.as_str(),
                            ]);
                        }
                    }
                    info.last_network_type = Some(net_type.clone());
                }
            }
            CLIENT_NETWORK_TYPE
                .with_label_values(&[meeting_id, session_id, reporting_user_id, net_type.as_str()])
                .set(1.0);
        }

        // #1556: Network downlink max
        if let Some(max_mbps) = health_packet.client_network_downlink_max {
            CLIENT_NETWORK_DOWNLINK_MAX
                .with_label_values(&[meeting_id, session_id, reporting_user_id])
                .set_finite(max_mbps);
        }

        // #1556: CPU throttle flag
        if let Some(throttled) = health_packet.client_cpu_throttled {
            CLIENT_CPU_THROTTLED
                .with_label_values(&[meeting_id, session_id, reporting_user_id])
                .set(if throttled { 1.0 } else { 0.0 });
        }

        // Tier transition events (P2): increment counter for each transition.
        //
        // Issue 2047 (SECURITY): all five of these labels are client-authored
        // strings. Each is collapsed onto its fixed taxonomy first, and the
        // vector is truncated to `MAX_TIER_TRANSITIONS_PER_PACKET`, so neither
        // the series count nor the per-packet work is attacker-controlled.
        // Truncation is silent-but-counted rather than a hard error: a
        // legitimate client never reaches the cap (see the const's rationale),
        // and rejecting the whole packet would discard the reporter's unrelated
        // telemetry along with it.
        let transitions = &health_packet.tier_transitions;
        if transitions.len() > MAX_TIER_TRANSITIONS_PER_PACKET {
            debug!(
                "Truncating {} tier_transitions to {} for meeting={} session={}",
                transitions.len(),
                MAX_TIER_TRANSITIONS_PER_PACKET,
                meeting_id,
                session_id
            );
            TIER_TRANSITIONS_DROPPED_TOTAL
                .inc_by((transitions.len() - MAX_TIER_TRANSITIONS_PER_PACKET) as f64);
        }
        for t in transitions.iter().take(MAX_TIER_TRANSITIONS_PER_PACKET) {
            let direction = bounded_tier_label(&t.direction, &KNOWN_TIER_DIRECTIONS);
            let stream = bounded_tier_label(&t.stream, &KNOWN_TIER_STREAMS);
            let trigger = bounded_tier_label(&t.trigger, &KNOWN_TIER_TRIGGERS);
            let from_tier = bounded_tier_name(&t.from_tier);
            let to_tier = bounded_tier_name(&t.to_tier);

            TIER_TRANSITIONS_TOTAL
                .with_label_values(&[
                    meeting_id,
                    session_id,
                    reporting_user_id,
                    direction,
                    stream,
                    from_tier,
                    to_tier,
                    trigger,
                ])
                .inc();

            // Track the exact label tuples published so `remove_session_metrics`
            // can reap them when the session goes away. The ~40 sibling metrics
            // are reaped there by fixed label shape; a CounterVec with five
            // variable labels has no fixed shape to enumerate cheaply, so this
            // mirrors the tracked-set pattern already used for `active_servers`
            // and `received_layer_peers` — O(tuples actually emitted), not
            // O(cartesian product). Without it these series would outlive every
            // session for the process lifetime (the #1092 leak class).
            let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(info) = tracker.get_mut(&session_key) {
                info.tier_transition_labels.insert([
                    direction.to_string(),
                    stream.to_string(),
                    from_tier.to_string(),
                    to_tier.to_string(),
                    trigger.to_string(),
                ]);
            }
        }

        // TELEM-7: client_info gauge (static metadata)
        // Publish when ANY client metadata field is present (not just cores/arch).
        if health_packet.client_cores.is_some()
            || health_packet.client_architecture.is_some()
            || health_packet.client_gpu_family.is_some()
            || health_packet.client_network_effective_type.is_some()
            || health_packet.client_capability_score.is_some()
            || health_packet.client_battery_level.is_some()
        {
            let cores_str = health_packet
                .client_cores
                .map(|c| c.to_string())
                .unwrap_or_default();
            let arch = health_packet
                .client_architecture
                .as_deref()
                .unwrap_or("")
                .to_string();
            let gpu = health_packet
                .client_gpu_family
                .as_deref()
                .unwrap_or("")
                .to_string();
            let net = health_packet
                .client_network_effective_type
                .as_deref()
                .unwrap_or("")
                .to_string();
            let score = health_packet
                .client_capability_score
                .map(|s| s.to_string())
                .unwrap_or_default();

            // Store labels in session tracker for cleanup; remove stale series on label change.
            {
                let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(info) = tracker.get_mut(&session_key) {
                    let new_labels = [
                        cores_str.clone(),
                        arch.clone(),
                        gpu.clone(),
                        net.clone(),
                        score.clone(),
                    ];
                    if let Some(ref prev) = info.client_info_labels {
                        if *prev != new_labels {
                            let _ = CLIENT_INFO.remove_label_values(&[
                                meeting_id, session_id, &prev[0], &prev[1], &prev[2], &prev[3],
                                &prev[4],
                            ]);
                        }
                    }
                    info.client_info_labels = Some(new_labels);
                }
            }

            CLIENT_INFO
                .with_label_values(&[
                    meeting_id, session_id, &cores_str, &arch, &gpu, &net, &score,
                ])
                .set(1.0);

            // #1143: also export the capability score as a NUMERIC gauge so it
            // can be thresholded/averaged/quantiled in PromQL directly, instead
            // of only riding as a string label on CLIENT_INFO above. Only set
            // when actually reported, so absent scores stay absent (not a
            // misleading 0). Same per-reporter label set as the sibling gauges.
            if let Some(cap) = health_packet.client_capability_score {
                CAPABILITY_SCORE
                    .with_label_values(&reporter_labels)
                    .set(cap as f64);
            }

            // #1392: also export the battery level as a NUMERIC gauge so it can be
            // thresholded/averaged/quantiled in PromQL directly, instead of only
            // gating CLIENT_INFO's publish (PR #1368). Only set when actually
            // reported, so an absent battery stays absent (not a misleading 0).
            // Same per-reporter label set as the sibling gauges.
            if let Some(battery) = health_packet.client_battery_level {
                BATTERY_LEVEL
                    .with_label_values(&reporter_labels)
                    .set_finite(battery);
            }
        }

        // TELEM-8: longtask histogram observations
        for dur in &health_packet.longtask_durations_ms {
            CLIENT_LONGTASK_DURATION_MS
                .with_label_values(&[meeting_id, session_id])
                .observe_finite(*dur);
        }

        // TELEM-9: render FPS gauge
        if let Some(fps) = health_packet.render_fps {
            CLIENT_RENDER_FPS
                .with_label_values(&[meeting_id, session_id])
                .set_finite(fps);
        }

        // Per-packet prune of departed peers (issue #1092).
        //
        // A peer that LEAVES a still-live reporter's view drops out of this
        // packet's `peer_stats`, but its per-pair series were already written on
        // earlier packets and are never re-written — so without this prune they
        // freeze at their last value in Prometheus until the WHOLE reporter
        // session is reaped (30s+). Here we diff the session's stored `to_peers`
        // against the peers present in THIS packet and, for each peer now absent,
        // delete its per-pair series and drop it from the session's tracking sets.
        //
        // This runs UNCONDITIONALLY (not gated on a non-empty `peer_stats`) so the
        // "reporter now sees nobody" case — where `peer_stats` is empty — still
        // prunes every previously-tracked peer. The reporter's own self-pair is
        // never in `to_peers` (only observed peers are inserted), and the 4-label
        // reporter-level metrics are keyed without `to_peer`, so neither is touched.
        {
            let current_peer_ids: HashSet<&str> = health_packet
                .peer_stats
                .keys()
                .map(|s| s.as_str())
                .collect();
            let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(info) = tracker.get_mut(&session_key) {
                let departed = peers_to_prune(&info.to_peers, &current_peer_ids);
                for peer_id in &departed {
                    remove_per_peer_metrics(meeting_id, session_id, reporting_user_id, peer_id);
                    // Prune ONLY the per-pair tracking set. `to_peers` keys the per-pair
                    // series that `remove_per_peer_metrics` just deleted; dropping the peer
                    // from it stops those series from being re-written on the next packet
                    // and lets a future re-appearance re-register cleanly.
                    //
                    // `peer_ids` is DELIBERATELY retained. It is the lifetime-of-session
                    // set that drives PEER_CONNECTIONS_TOTAL cleanup in
                    // remove_session_metrics: that gauge is keyed only
                    // (meeting_id, peer_id) — i.e. SHARED across all reporters in the
                    // meeting — so it must NOT be removed when ONE reporter stops seeing
                    // the peer (other reporters may still observe it). It is removed only
                    // on whole-session reap, iterating `peer_ids`. Removing the peer from
                    // `peer_ids` here would silently disable that cleanup and re-leak the
                    // gauge (the exact #1092 class, for PEER_CONNECTIONS_TOTAL).
                    info.to_peers.remove(peer_id);
                }
                if !departed.is_empty() {
                    debug!(
                        "Pruned {} departed peer(s) for session {} (meeting {}): {:?}",
                        departed.len(),
                        session_id,
                        meeting_id,
                        departed
                    );
                }
            }
        }

        // Process peer health data
        if !health_packet.peer_stats.is_empty() {
            // Record the peers this reporter observes in the per-pair tracking set
            // (`to_peers`, which drives the #1092 per-packet prune) and the
            // lifetime-of-session `peer_ids` set (which drives the meeting-scoped
            // PEER_CONNECTIONS_TOTAL reap in remove_session_metrics). Per-pair series
            // are keyed only by stable ids after #1954, so there is no display-name
            // rename to detect here anymore — just maintain the sets under one lock.
            {
                let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                let key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
                if let Some(info) = tracker.get_mut(&key) {
                    for peer_id in health_packet.peer_stats.keys() {
                        info.to_peers.insert(peer_id.clone());
                        info.peer_ids.insert(peer_id.clone());
                    }
                }
            }

            for (peer_id, peer_data) in &health_packet.peer_stats {
                let peer_labels: [&str; 4] = [meeting_id, session_id, reporting_user_id, peer_id];

                PEER_CONNECTIONS_TOTAL
                    .with_label_values(&[meeting_id, peer_id])
                    .set(1.0);

                PEER_CAN_LISTEN
                    .with_label_values(&peer_labels)
                    .set(if peer_data.can_listen { 1.0 } else { 0.0 });

                PEER_CAN_SEE
                    .with_label_values(&peer_labels)
                    .set(if peer_data.can_see { 1.0 } else { 0.0 });

                // NetEQ metrics
                if let Some(neteq_stats) = peer_data.neteq_stats.as_ref() {
                    // These are current snapshots, so 0 is a real reading: an empty buffer,
                    // an empty decode queue, or no packets in the latest 1s window. Publish 0
                    // so each gauge can recover instead of latching its last positive value.
                    NETEQ_AUDIO_BUFFER_MS
                        .with_label_values(&peer_labels)
                        .set_finite(neteq_stats.current_buffer_size_ms);

                    NETEQ_TARGET_DELAY_MS
                        .with_label_values(&peer_labels)
                        .set_finite(neteq_stats.target_delay_ms);

                    // Audio playout latency (#1299): how far behind live this peer's audio is
                    // (NetEQ filtered playout buffer level). Set UNCONDITIONALLY so the gauge
                    // recovers to 0 when audio catches back up to live — same rationale as the
                    // video playout gauge below. Audio sibling of videocall_video_playout_latency_ms.
                    AUDIO_PLAYOUT_LATENCY_MS
                        .with_label_values(&peer_labels)
                        .set_finite(neteq_stats.playout_latency_ms);

                    NETEQ_PACKETS_AWAITING_DECODE
                        .with_label_values(&peer_labels)
                        .set_finite(neteq_stats.packets_awaiting_decode);

                    NETEQ_PACKETS_PER_SEC
                        .with_label_values(&peer_labels)
                        .set_finite(neteq_stats.packets_per_sec);

                    // Core NetEQ operation counters (high diagnostic value only)
                    if let Some(network) = neteq_stats.network.as_ref() {
                        if let Some(ops) = network.operation_counters.as_ref() {
                            NETEQ_NORMAL_OPS_PER_SEC
                                .with_label_values(&peer_labels)
                                .set_finite(ops.normal_per_sec);
                            NETEQ_EXPAND_OPS_PER_SEC
                                .with_label_values(&peer_labels)
                                .set_finite(ops.expand_per_sec);
                            NETEQ_ACCELERATE_OPS_PER_SEC
                                .with_label_values(&peer_labels)
                                .set_finite(ops.accelerate_per_sec);
                        }
                    }
                }

                // Video metrics
                if let Some(video_stats) = peer_data.video_stats.as_ref() {
                    // Issue 2145: fps and bitrate are set UNCONDITIONALLY, so the gauges recover
                    // to 0 when a still-connected receiver's camera stream stops or freezes —
                    // deliberately IDENTICAL to the SCREEN_VIDEO_FPS / SCREEN_VIDEO_BITRATE_KBPS
                    // block below, which has always set unconditionally. Do NOT re-add a
                    // `!= 0` guard to either side: the camera/screen split here is not a
                    // difference in meaning, and the guard that used to sit on camera was the bug.
                    //
                    // Why 0 is a REAL reading, not "not measured": both fields ride the SAME
                    // per-heartbeat DiagEvent (`diagnostics_manager.rs::send_diagnostic_packets`),
                    // which substitutes `(0.0, 0.0, 0.0)` for (fps, bitrate, decode_errors) once a
                    // tracker has seen no frame for longer than its staleness window (a bare
                    // `1000.0` ms literal there, not a named constant), and the client folds both
                    // into the proto unconditionally (`health_reporter.rs`, camera video mapping).
                    // So a genuine 0 reaches this line and MUST be published: the only removal of
                    // these series is `remove_per_peer_metrics`, which is disconnect/peer-departure
                    // GC (#1092), NOT a staleness sweep. Guarding on `!= 0` therefore did not make
                    // the series go absent — it left it registered and scraped at its last HEALTHY
                    // value (e.g. 30) while no frames were arriving, i.e. a gauge that actively lies.
                    //
                    // READ 0 AS "no frames are arriving", NOT as "this stream is broken". The client
                    // applies no `video_enabled` / `can_see` / freshness gate before folding, so an
                    // idle-BY-DESIGN receiver reports an honest 0 too: a peer whose camera is simply
                    // off, or a hidden/DecodeBudget-paused tile once the #988 viewport filter stops
                    // forwarding its video. The sibling comment on the playout gauges below says the
                    // same thing from the other direction ("a paused/hidden tile reads 0 here rather
                    // than a stale latch"). To separate expected idle from unexpected no-arrival,
                    // read this alongside `videocall_peer_video_enabled` / `videocall_peer_can_see`.
                    // `videocall_video_content_staleness_ms` cannot distinguish them once fps is 0:
                    // the client deliberately publishes its 0 default when no frames arrive. It can
                    // corroborate stale content only while frames continue arriving. Also note that,
                    // after a tracker exists, the zero is not a one-heartbeat window: each
                    // zero-substituted DiagEvent refreshes `last_camera_update_ms`, and
                    // `last_camera_stats` is retained until frames resume or the peer is removed.
                    //
                    // `set_finite` still drops a NaN/inf sample (issue 2047) — that is a separate
                    // concern from zero and is the reason a bare `.set()` is not used for fps.
                    // `bitrate_kbps` is a `u64`, so its `as f64` cast is always finite.
                    VIDEO_FPS
                        .with_label_values(&peer_labels)
                        .set_finite(video_stats.fps_received);
                    VIDEO_BITRATE_KBPS
                        .with_label_values(&peer_labels)
                        .set(video_stats.bitrate_kbps as f64);

                    // Buffered video playout latency (#1252): how far behind live this peer's
                    // video is (jitter-buffer backlog + decoder queue), plus its stage-1
                    // attribution. Set UNCONDITIONALLY so the gauges recover to 0 when the receiver
                    // catches back up to live — the client only reports a nonzero value while the
                    // tile is actively receiving (fps_received > 0), so a paused/hidden tile reads
                    // 0 here rather than a stale latch.
                    VIDEO_PLAYOUT_LATENCY_MS
                        .with_label_values(&peer_labels)
                        .set_finite(video_stats.playout_latency_ms);
                    VIDEO_PLAYOUT_STAGE1_SPAN_MS
                        .with_label_values(&peer_labels)
                        .set_finite(video_stats.playout_stage1_span_ms);
                    // Stage-3 paint lag (#1252): decoded-but-unpainted backlog in the worker->main
                    // postMessage + paint queues. Set UNCONDITIONALLY (same rationale as above) so
                    // the gauge recovers to 0 when the paint path drains; the client reports a
                    // nonzero value only while fps_received > 0.
                    VIDEO_PLAYOUT_PAINT_LAG_MS
                        .with_label_values(&peer_labels)
                        .set_finite(video_stats.playout_paint_lag_ms);
                    // Content staleness (#1641): the AGE of the video content being painted, as
                    // distinct from the queue-DEPTH gauges above. Set UNCONDITIONALLY (same
                    // recover-to-0 rationale as the playout gauges): the client reports a nonzero
                    // value only while fps_received > 0, so a paused/hidden tile reads 0 here rather
                    // than a stale latch. UNBOUNDED (unlike playout_latency_ms's 1800ms cap) — this
                    // is the gauge that exposes the #1631 M2 minutes-of-lag a draining-stale stream
                    // hides from paint_lag.
                    VIDEO_CONTENT_STALENESS_MS
                        .with_label_values(&peer_labels)
                        .set_finite(video_stats.content_staleness_ms);
                    // Resync-to-live governor skips (#1252): cumulative COUNTER value held in a
                    // gauge. Set UNCONDITIONALLY (same recover-to-0 pattern as the gauges above): the
                    // client folds this field unconditionally, so an absent/idle stream reports its
                    // last cumulative total (or the proto default 0 if never set) rather than a stale
                    // latch. A monotonically-rising value proves the governor fired.
                    VIDEO_SKIP_TO_LIVE_TOTAL
                        .with_label_values(&peer_labels)
                        .set(video_stats.playout_skip_to_live_total as f64);
                }

                // Screen video metrics (separate from camera)
                // Always set when present -- allows gauges to recover to 0.0
                // when screen share stops or quality collapses.
                //
                // Issue 2145: the camera block above now does the SAME thing. These two paths are
                // deliberately aligned — a receiver-observed 0 is a REPORTABLE reading for both
                // kinds. Until 2145 the camera sibling carried a `!= 0.0` guard with no comment
                // explaining the asymmetry; it was a bug, not a design choice. Do NOT re-add a
                // zero guard to either block.
                //
                // As on camera, 0 means "no frames are arriving" — NOT "this stream is broken".
                // For screen that distinction is even sharper: a LEGITIMATELY STATIC share is
                // expected to read 0 once its keyframe-floor budget drains
                // (`SCREEN_STATIC_KEYFRAME_FLOOR_BUDGET`, issue 2147), so 0 here is frequently the
                // healthy steady state. Receiver fps and content-staleness cannot by themselves
                // distinguish static-and-fine from stalled once fps is 0: the client publishes a
                // 0 content-staleness default in that state. Use the publisher's
                // `videocall_screen_sharing_active` and
                // `videocall_screen_encoder_stall_episodes_total` signals (2147).
                if let Some(screen_stats) = peer_data.screen_video_stats.as_ref() {
                    SCREEN_VIDEO_FPS
                        .with_label_values(&peer_labels)
                        .set_finite(screen_stats.fps_received);
                    SCREEN_VIDEO_BITRATE_KBPS
                        .with_label_values(&peer_labels)
                        .set(screen_stats.bitrate_kbps as f64);

                    // Screen-share playout family (#1660): screen siblings of the camera
                    // VIDEO_PLAYOUT_* / VIDEO_CONTENT_STALENESS_MS / VIDEO_SKIP_TO_LIVE_TOTAL
                    // block above. PR #1657 routed the screen decoder's playout stats into
                    // screen_video_stats; this exports them so a screen-share freeze can be
                    // charted (previously only the camera bucket's playout family reached
                    // Prometheus). Set UNCONDITIONALLY (same recover-to-0 rationale as the
                    // camera block): the client reports nonzero ms values only while
                    // fps_received > 0, so a stopped/hidden share reads 0 here rather than a
                    // stale latch; content_staleness_ms is UNBOUNDED (unlike the 1800ms-capped
                    // playout_latency_ms) and skip_to_live_total is a cumulative counter folded
                    // unconditionally.
                    SCREEN_VIDEO_PLAYOUT_LATENCY_MS
                        .with_label_values(&peer_labels)
                        .set_finite(screen_stats.playout_latency_ms);
                    SCREEN_VIDEO_PLAYOUT_STAGE1_SPAN_MS
                        .with_label_values(&peer_labels)
                        .set_finite(screen_stats.playout_stage1_span_ms);
                    SCREEN_VIDEO_PLAYOUT_PAINT_LAG_MS
                        .with_label_values(&peer_labels)
                        .set_finite(screen_stats.playout_paint_lag_ms);
                    SCREEN_VIDEO_CONTENT_STALENESS_MS
                        .with_label_values(&peer_labels)
                        .set_finite(screen_stats.content_staleness_ms);
                    SCREEN_VIDEO_SKIP_TO_LIVE_TOTAL
                        .with_label_values(&peer_labels)
                        .set(screen_stats.playout_skip_to_live_total as f64);
                }

                // Decode errors
                if peer_data.frames_dropped_per_sec > 0.0 {
                    VIDEO_FRAMES_DROPPED
                        .with_label_values(&peer_labels)
                        .set_finite(peer_data.frames_dropped_per_sec);
                }

                if let Some(total) = peer_data.decoder_errors_total {
                    DECODER_ERRORS_TOTAL
                        .with_label_values(&peer_labels)
                        .set(total as f64);
                }

                // Freeze indicators: video packet loss + keyframe-request storms.
                // Always set when present -- lets gauges recover to 0.0 when a
                // loss burst or PLI storm clears, instead of latching the last bad value.
                if let Some(loss) = peer_data.video_seq_loss_per_sec {
                    VIDEO_SEQ_LOSS_PER_SEC
                        .with_label_values(&peer_labels)
                        .set_finite(loss);
                }
                if let Some(kf) = peer_data.keyframe_requests_per_sec {
                    KEYFRAME_REQUESTS_PER_SEC
                        .with_label_values(&peer_labels)
                        .set_finite(kf);
                }

                // Receive-side audio DATAGRAM loss (#1878): audio sibling of the
                // video seq-loss gauge above. Current clients fold this
                // unconditionally (the live value on WebTransport, definitional
                // 0.0 on WebSocket), so it is always set and the gauge recovers to
                // 0 instead of latching — including on a mid-call WT→WS fallback.
                // The `if let Some` still guards against an older client that
                // predates the field and omits it.
                if let Some(loss) = peer_data.audio_datagram_loss_per_sec {
                    AUDIO_DATAGRAM_LOSS_PER_SEC
                        .with_label_values(&peer_labels)
                        .set_finite(loss);
                }

                // Issue 2031: uncapped magnitude companion. Current clients fold
                // it unconditionally (live value on WT, definitional 0.0 on WS),
                // so it is always set and the gauge recovers to 0 rather than
                // latching — including on a mid-call WT->WS fallback. The
                // `if let Some` still guards an older client that omits it.
                if let Some(raw_loss) = peer_data.audio_datagram_raw_loss_per_sec {
                    AUDIO_DATAGRAM_RAW_LOSS_PER_SEC
                        .with_label_values(&peer_labels)
                        .set_finite(raw_loss);
                }

                // Audio concealment percentage (from NetEQ expand events)
                // Always set — allows gauge to recover to 0.0 when concealment clears
                AUDIO_CONCEALMENT_PCT
                    .with_label_values(&peer_labels)
                    .set_finite(peer_data.audio_concealment_pct);

                // Quality scores
                if let Some(score) = peer_data.audio_quality_score {
                    AUDIO_QUALITY_SCORE
                        .with_label_values(&peer_labels)
                        .set_finite(score);
                }
                if let Some(score) = peer_data.video_quality_score {
                    VIDEO_QUALITY_SCORE
                        .with_label_values(&peer_labels)
                        .set_finite(score);
                }
                if let Some(score) = peer_data.call_quality_score {
                    CALL_QUALITY_SCORE
                        .with_label_values(&peer_labels)
                        .set_finite(score);
                }

                // Peer status flags
                PEER_AUDIO_ENABLED
                    .with_label_values(&peer_labels)
                    .set(if peer_data.audio_enabled { 1.0 } else { 0.0 });
                PEER_VIDEO_ENABLED
                    .with_label_values(&peer_labels)
                    .set(if peer_data.video_enabled { 1.0 } else { 0.0 });
            }

            // Update meeting participants from the authoritative session tracker
            // (issue #1040). Counting distinct live sessions for this meeting — rather
            // than this one reporter's `peer_stats.len() + 1` — fixes both the gauge
            // leak (stale meetings now decrement to 0 / are removed via
            // cleanup_stale_sessions) and the per-reporter skew where different
            // reporters in the same meeting disagree on the peer count mid-join/leave.
            {
                let tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                let mut meeting = HashSet::with_capacity(1);
                meeting.insert(meeting_id.to_string());
                recompute_meeting_participants(&tracker, &meeting);
            }
        }
    }

    Ok(())
}

async fn nats_health_consumer(
    nats_client: Client,
    health_store: HealthDataStore,
    session_tracker: SessionTracker,
) -> anyhow::Result<()> {
    // Subscribe to all health diagnostics topics from all regions.
    //
    // SINGLE-REPLICA ASSUMPTION (issue #1075): this is a NATS *queue group*, so each
    // health packet is delivered to exactly ONE subscriber in the group. The per-process
    // `session_tracker` that backs the `videocall_meeting_participants` gauge is therefore
    // only complete when there is a single `metrics_server` replica subscribed. With >1
    // replica the packets for one meeting fan out across replicas, each replica counts
    // only its subset, and the gauge undercounts / flaps. Production runs single-replica
    // (`helm/metrics-api/values.yaml` pins `serverStats.replicas: 1`). See
    // `recompute_meeting_participants` for the aggregation strategy required to scale out.
    let queue_group = "metrics-server-health-diagnostics";
    let mut subscription = nats_client
        .queue_subscribe("health.diagnostics.>", queue_group.to_string())
        .await?;

    info!("Subscribed to NATS topic: health.diagnostics.>");

    while let Some(message) = subscription.next().await {
        debug!("Received health message from NATS: {}", message.subject);
        if let Err(e) = handle_health_message(message, &health_store, &session_tracker).await {
            error!("Failed to handle health message: {}", e);
        }
    }

    Ok(())
}

async fn handle_health_message(
    message: Message,
    health_store: &HealthDataStore,
    session_tracker: &SessionTracker,
) -> anyhow::Result<()> {
    let topic = &message.subject;
    debug!("Received health data from topic: {}", topic);

    // Parse protobuf health packet
    let health_packet: PbHealthPacket = PbHealthPacket::parse_from_bytes(&message.payload)?;

    // Freshness guard: discard stale packets
    let now_ms: u128 = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let packet_ts_ms_opt: Option<u128> = Some(health_packet.timestamp_ms as u128);

    // 30 seconds timeout
    let is_fresh = match packet_ts_ms_opt {
        Some(ts) => now_ms.saturating_sub(ts) <= 30_000,
        None => true, // if unknown, accept
    };

    if is_fresh {
        // Update Prometheus metrics immediately on ingest
        if let Err(e) = process_health_packet_to_metrics_pb(&health_packet, session_tracker) {
            error!("Failed to process health packet for metrics: {}", e);
        }
    } else {
        debug!("Discarded stale health packet on topic {}", topic);
    }

    // Store latest health data using topic as key
    {
        let mut store = health_store.lock().unwrap_or_else(|e| e.into_inner());
        let json_val = json!({
            "session_id": health_packet.session_id,
            "meeting_id": health_packet.meeting_id,
            "reporting_user_id": if health_packet.reporting_user_id.is_empty() {
                "unknown".to_string()
            } else {
                videocall_types::user_id_bytes_to_string(&health_packet.reporting_user_id)
            },
            "timestamp_ms": health_packet.timestamp_ms,
        });
        store.insert(topic.to_string(), json_val);
    }

    debug!("Stored health data for {}", topic);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Get configuration from environment
    let port = std::env::var("METRICS_PORT")
        .unwrap_or_else(|_| "9091".to_string())
        .parse::<u16>()?;

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());

    info!("Starting metrics server on port {}", port);
    info!("Connecting to NATS at {}", nats_url);

    // Connect to NATS
    let nats_client = async_nats::connect(&nats_url).await?;
    info!("Connected to NATS successfully");

    // Create shared health data store
    let health_store: HealthDataStore = Arc::new(Mutex::new(HashMap::new()));

    // Create shared session tracker
    let session_tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

    // Start NATS consumer in background
    let nats_store = health_store.clone();
    let nats_client_clone = nats_client.clone();
    let nats_tracker = session_tracker.clone();
    task::spawn(async move {
        if let Err(e) = nats_health_consumer(nats_client_clone, nats_store, nats_tracker).await {
            error!("NATS consumer failed: {}", e);
        }
    });

    // Start HTTP server
    info!("Starting HTTP server on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(health_store.clone()))
            .app_data(web::Data::new(session_tracker.clone()))
            .route("/metrics", web::get().to(metrics_handler))
            .route(
                "/health",
                web::get().to(|| async { HttpResponse::Ok().body("OK") }),
            )
    })
    .bind(format!("0.0.0.0:{port}"))?
    .run()
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use videocall_types::protos::health_packet::{
        HealthPacket as PbHealthPacket, NetEqNetwork as PbNetEqNetwork,
        NetEqOperationCounters as PbNetEqOperationCounters, NetEqStats as PbNetEqStats,
        PeerStats as PbPeerStats, TierTransition as PbTierTransition, VideoStats as PbVideoStats,
    };

    #[test]
    fn test_active_server_metrics_export() {
        // Build a health packet with active server fields set
        let mut hp = PbHealthPacket::new();
        hp.session_id = "s1".to_string();
        hp.meeting_id = "m1".to_string();
        hp.reporting_user_id = "alice@example.com".as_bytes().to_vec();
        hp.timestamp_ms = 12345;
        hp.reporting_audio_enabled = true;
        hp.reporting_video_enabled = true;
        hp.active_server_url = "wss://ws-a".to_string();
        hp.active_server_type = "websocket".to_string();
        hp.active_server_rtt_ms = 42.5;

        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Process and ensure no error
        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // Metrics presence is indirectly verified by successful processing; we avoid scraping here.
        // Detailed Prometheus gather assertions can be added if needed.
    }

    /// Helper function to create a test health packet (protobuf)
    fn create_test_health_packet(
        session_id: &str,
        meeting_id: &str,
        reporting_user_id: &str,
        peer_stats: std::collections::HashMap<String, PbPeerStats>,
    ) -> PbHealthPacket {
        let mut hp = PbHealthPacket::new();
        hp.session_id = session_id.to_string();
        hp.meeting_id = meeting_id.to_string();
        hp.reporting_user_id = reporting_user_id.as_bytes().to_vec();
        hp.timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        hp.peer_stats = peer_stats;
        hp
    }

    fn series_exists(metric_name: &str, expected_labels: &[(&str, &str)]) -> bool {
        let families = prometheus::gather();
        for family in families {
            if family.get_name() == metric_name {
                for metric in family.get_metric() {
                    let mut all_match = true;
                    for (lname, lval) in expected_labels {
                        let mut found = false;
                        for label in metric.get_label() {
                            if label.get_name() == *lname && label.get_value() == *lval {
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            all_match = false;
                            break;
                        }
                    }
                    if all_match {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn matching_series_count(metric_name: &str, expected_labels: &[(&str, &str)]) -> usize {
        prometheus::gather()
            .into_iter()
            .find(|family| family.get_name() == metric_name)
            .map(|family| {
                family
                    .get_metric()
                    .iter()
                    .filter(|metric| {
                        expected_labels.iter().all(|(lname, lval)| {
                            metric.get_label().iter().any(|label| {
                                label.get_name() == *lname && label.get_value() == *lval
                            })
                        })
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    fn matching_series_has_label(
        metric_name: &str,
        expected_labels: &[(&str, &str)],
        label_name: &str,
    ) -> bool {
        prometheus::gather()
            .into_iter()
            .find(|family| family.get_name() == metric_name)
            .map(|family| {
                family
                    .get_metric()
                    .iter()
                    .filter(|metric| {
                        expected_labels.iter().all(|(lname, lval)| {
                            metric.get_label().iter().any(|label| {
                                label.get_name() == *lname && label.get_value() == *lval
                            })
                        })
                    })
                    .any(|metric| {
                        metric
                            .get_label()
                            .iter()
                            .any(|label| label.get_name() == label_name)
                    })
            })
            .unwrap_or(false)
    }

    fn gauge_value(metric_name: &str, expected_labels: &[(&str, &str)]) -> Option<f64> {
        let families = prometheus::gather();
        for family in families {
            if family.get_name() == metric_name {
                for metric in family.get_metric() {
                    let labels_match = expected_labels.iter().all(|(lname, lval)| {
                        metric
                            .get_label()
                            .iter()
                            .any(|label| label.get_name() == *lname && label.get_value() == *lval)
                    });
                    if labels_match {
                        return Some(metric.get_gauge().get_value());
                    }
                }
            }
        }
        None
    }

    /// Value of a labeled COUNTER series (issue 2047). `gauge_value` reads the
    /// gauge union member and returns `None` for a CounterVec, so the
    /// tier-transition assertions need this sibling.
    fn counter_value(metric_name: &str, expected_labels: &[(&str, &str)]) -> Option<f64> {
        prometheus::gather()
            .into_iter()
            .find(|family| family.get_name() == metric_name)
            .and_then(|family| {
                family
                    .get_metric()
                    .iter()
                    .find(|metric| {
                        expected_labels.iter().all(|(lname, lval)| {
                            metric.get_label().iter().any(|label| {
                                label.get_name() == *lname && label.get_value() == *lval
                            })
                        })
                    })
                    .map(|metric| metric.get_counter().get_value())
            })
    }

    /// Running `_sum` of a labeled histogram series (issue 2047): the field a
    /// non-finite `observe()` would latch to `NaN` for the process lifetime.
    fn histogram_sum(metric_name: &str, expected_labels: &[(&str, &str)]) -> Option<f64> {
        prometheus::gather()
            .into_iter()
            .find(|family| family.get_name() == metric_name)
            .and_then(|family| {
                family
                    .get_metric()
                    .iter()
                    .find(|metric| {
                        expected_labels.iter().all(|(lname, lval)| {
                            metric.get_label().iter().any(|label| {
                                label.get_name() == *lname && label.get_value() == *lval
                            })
                        })
                    })
                    .map(|metric| metric.get_histogram().get_sample_sum())
            })
    }

    // ======================================================================
    // Issue 2047: non-finite client telemetry must never reach a metric
    // ======================================================================

    /// A NaN gauge sample must be DROPPED, leaving the previous good value in
    /// place — not written, and not clamped to 0.0.
    ///
    /// `active_server_rtt_ms` is the sharpest site to pin this: its pre-existing
    /// `!= 0.0` guard does NOT stop a NaN (`NaN != 0.0` is `true`), so the value
    /// reaches the sink and only the new finite check can reject it.
    ///
    /// Mutation coverage: revert this site's `.set_finite(...)` to `.set(...)`
    /// and the gauge becomes NaN, so the `Some(41.5)` assert fails (NaN compares
    /// unequal to everything, including itself). Delete the
    /// `NON_FINITE_SAMPLES_DROPPED_TOTAL.inc()` from `is_publishable_sample` and
    /// the counter assert fails.
    #[test]
    fn nan_gauge_sample_is_dropped_and_last_good_value_survives() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let labels = [
            ("meeting_id", "m_nan_2047"),
            ("session_id", "s_nan_2047"),
            ("peer_id", "reporter_nan_2047"),
        ];

        // A good sample first, so the test can prove the NaN did not overwrite it.
        let mut good = create_test_health_packet(
            "s_nan_2047",
            "m_nan_2047",
            "reporter_nan_2047",
            HashMap::new(),
        );
        good.active_server_type = "websocket".to_string();
        good.active_server_rtt_ms = 41.5;
        assert!(process_health_packet_to_metrics_pb(&good, &tracker).is_ok());
        assert_eq!(
            gauge_value("videocall_client_active_server_rtt_ms", &labels),
            Some(41.5),
            "baseline: the good RTT sample must publish"
        );

        let dropped_before = NON_FINITE_SAMPLES_DROPPED_TOTAL.get();

        // Now the poisoned packet, same labels.
        let mut poisoned = good.clone();
        poisoned.active_server_rtt_ms = f64::NAN;
        assert!(process_health_packet_to_metrics_pb(&poisoned, &tracker).is_ok());

        assert_eq!(
            gauge_value("videocall_client_active_server_rtt_ms", &labels),
            Some(41.5),
            "a NaN sample must be skipped, leaving the last good value intact"
        );
        assert!(
            NON_FINITE_SAMPLES_DROPPED_TOTAL.get() > dropped_before,
            "the rejection must be counted so operators can see a client emitting NaN"
        );
    }

    /// ±Inf is rejected on the same path as NaN, asserted on a PER-PEER gauge so
    /// the guard is pinned on both label shapes (reporter-level above,
    /// peer-level here).
    ///
    /// Mutation coverage: revert `AUDIO_CONCEALMENT_PCT`'s `.set_finite(...)` to
    /// `.set(...)` and the gauge holds `inf`, failing the `Some(12.5)` assert.
    #[test]
    fn infinite_peer_gauge_sample_is_dropped() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let peer_labels = [
            ("meeting_id", "m_inf_2047"),
            ("session_id", "s_inf_2047"),
            ("from_peer", "reporter_inf_2047"),
            ("to_peer", "peer_inf_2047"),
        ];

        let mut peer = PbPeerStats::new();
        peer.audio_concealment_pct = 12.5;
        let mut peer_stats = HashMap::new();
        peer_stats.insert("peer_inf_2047".to_string(), peer);
        let good = create_test_health_packet(
            "s_inf_2047",
            "m_inf_2047",
            "reporter_inf_2047",
            peer_stats.clone(),
        );
        assert!(process_health_packet_to_metrics_pb(&good, &tracker).is_ok());
        assert_eq!(
            gauge_value("videocall_audio_concealment_pct", &peer_labels),
            Some(12.5),
            "baseline: the good concealment sample must publish"
        );

        let mut poisoned_peer = PbPeerStats::new();
        poisoned_peer.audio_concealment_pct = f64::INFINITY;
        let mut poisoned_stats = HashMap::new();
        poisoned_stats.insert("peer_inf_2047".to_string(), poisoned_peer);
        let poisoned = create_test_health_packet(
            "s_inf_2047",
            "m_inf_2047",
            "reporter_inf_2047",
            poisoned_stats,
        );
        assert!(process_health_packet_to_metrics_pb(&poisoned, &tracker).is_ok());

        assert_eq!(
            gauge_value("videocall_audio_concealment_pct", &peer_labels),
            Some(12.5),
            "an infinite sample must be skipped, leaving the last good value intact"
        );
    }

    /// A non-finite HISTOGRAM observation must be rejected too. A gauge poisoned
    /// by NaN recovers on the next good sample; a histogram's `_sum` does NOT —
    /// it stays NaN for the process lifetime, which is why `observe_finite`
    /// exists alongside `set_finite`.
    ///
    /// Mutation coverage: revert `CLIENT_LONGTASK_DURATION_MS`'s
    /// `.observe_finite(...)` to `.observe(...)` and the sum becomes NaN, failing
    /// the finite assert below.
    #[test]
    fn non_finite_longtask_observation_does_not_poison_histogram_sum() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let labels = [("meeting_id", "m_hist_2047"), ("session_id", "s_hist_2047")];

        let mut hp = create_test_health_packet(
            "s_hist_2047",
            "m_hist_2047",
            "reporter_hist_2047",
            HashMap::new(),
        );
        hp.longtask_durations_ms = vec![120.0, f64::NAN, f64::NEG_INFINITY, 80.0];
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        let sum = histogram_sum("videocall_client_longtask_duration_ms", &labels)
            .expect("longtask histogram series must exist");
        assert!(
            sum.is_finite(),
            "a non-finite observation must never reach the histogram (_sum = {sum})"
        );
        assert_eq!(
            sum, 200.0,
            "only the two finite durations may contribute to _sum"
        );
    }

    // --- #2147: screen-encoder fps must reach the gauge, INCLUDING a real 0 ------

    /// A `screen_encoder_output_fps` of **0** must reach
    /// `videocall_screen_encoder_output_fps`. This is the whole point of the field:
    /// its camera sibling is `> 0`-gated at the SOURCE (#2079), which makes a
    /// genuine stall absent and indistinguishable from never-started. If anyone
    /// "makes it consistent" by adding a `> 0` filter on the server side, this
    /// fails.
    ///
    /// MUTATION: wrap the `SCREEN_ENCODER_OUTPUT_FPS.set(...)` block in
    /// `if fps > 0`, or delete the block entirely, and the 0 assertion fails
    /// (`None` instead of `Some(0.0)`).
    #[test]
    fn screen_encoder_fps_zero_reaches_the_gauge() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let labels = [
            ("meeting_id", "m_scrfps_2147"),
            ("session_id", "s_scrfps_2147"),
        ];

        let mut hp = create_test_health_packet(
            "s_scrfps_2147",
            "m_scrfps_2147",
            "reporter_scrfps_2147",
            HashMap::new(),
        );
        // The freeze scenario: the CAMERA encoder is healthy while the SCREEN
        // encoder produces nothing. Distinct values so a copy-paste of the camera
        // source into the screen gauge is caught too.
        hp.encoder_output_fps = Some(9);
        hp.screen_encoder_output_fps = Some(0);
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        assert_eq!(
            gauge_value("videocall_screen_encoder_output_fps", &labels),
            Some(0.0),
            "#2147: a bound-but-idle/stalled screen encoder must publish an honest 0, \
             not vanish the way the `> 0`-gated camera gauge does (#2079)"
        );
        assert_eq!(
            gauge_value("videocall_encoder_output_fps", &labels),
            Some(9.0),
            "the camera gauge must carry the CAMERA value — the two must not be crossed"
        );
    }

    /// An ABSENT `screen_encoder_output_fps` must leave the gauge unset — the
    /// server must not fabricate a 0 for a client that binds no screen encoder.
    ///
    /// MUTATION: change the `if let Some(fps)` to
    /// `.set(health_packet.screen_encoder_output_fps.unwrap_or(0) as f64)` and this
    /// fails (`Some(0.0)` instead of `None`).
    #[test]
    fn absent_screen_encoder_fps_leaves_the_gauge_unset() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let labels = [
            ("meeting_id", "m_scrfps_absent_2147"),
            ("session_id", "s_scrfps_absent_2147"),
        ];

        let hp = create_test_health_packet(
            "s_scrfps_absent_2147",
            "m_scrfps_absent_2147",
            "reporter_scrfps_absent_2147",
            HashMap::new(),
        );
        assert!(hp.screen_encoder_output_fps.is_none(), "precondition");
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        assert_eq!(
            gauge_value("videocall_screen_encoder_output_fps", &labels),
            None,
            "#2147: an absent field must not mint a fabricated 0 series"
        );
    }

    /// **The #2143 discrimination test.** The fps gauge alone CANNOT distinguish a
    /// frozen screen share from a healthy one — it counts encoded chunks, and the
    /// synthetic retained-frame re-encodes keep it nonzero while receivers sit on
    /// stale content. The stall pair is what makes the two distinguishable, so this
    /// asserts BOTH scenarios end-to-end through the production path with the SAME
    /// fps value, proving fps is not what separates them.
    ///
    /// MUTATION: delete either `SCREEN_ENCODER_STALL_EPISODES.set(...)` or
    /// `SCREEN_ENCODER_MAX_STALL_GAP_MS.set(...)` and the freeze case loses the only
    /// signal that distinguishes it, failing here.
    #[test]
    fn stall_pair_distinguishes_a_frozen_share_from_a_healthy_one() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // --- FREEZE: fps looks fine (synthetic re-encodes) but ticks are starved.
        let frozen = [
            ("meeting_id", "m_stall_frozen"),
            ("session_id", "s_stall_frozen"),
        ];
        let mut hp = create_test_health_packet(
            "s_stall_frozen",
            "m_stall_frozen",
            "reporter_stall_frozen",
            HashMap::new(),
        );
        hp.screen_encoder_output_fps = Some(3);
        hp.screen_encoder_stall_episodes = Some(11);
        hp.screen_encoder_max_stall_gap_ms = Some(23_150);
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        // --- HEALTHY: the SAME fps, no stall episodes at all.
        let healthy = [
            ("meeting_id", "m_stall_healthy"),
            ("session_id", "s_stall_healthy"),
        ];
        let mut hp2 = create_test_health_packet(
            "s_stall_healthy",
            "m_stall_healthy",
            "reporter_stall_healthy",
            HashMap::new(),
        );
        hp2.screen_encoder_output_fps = Some(3);
        // Counters absent — the client omits them while zero.
        assert!(process_health_packet_to_metrics_pb(&hp2, &tracker).is_ok());

        // fps is IDENTICAL across the two, so it cannot be the discriminator.
        assert_eq!(
            gauge_value("videocall_screen_encoder_output_fps", &frozen),
            gauge_value("videocall_screen_encoder_output_fps", &healthy),
            "#2147 premise: fps alone must NOT distinguish these — if it does, this \
             test has stopped testing the thing it names"
        );

        // The stall pair IS the discriminator.
        assert_eq!(
            gauge_value("videocall_screen_encoder_stall_episodes_total", &frozen),
            Some(11.0),
            "#2147: the freeze must be visible as rising stall episodes"
        );
        assert_eq!(
            gauge_value("videocall_screen_encoder_max_stall_gap_ms", &frozen),
            Some(23150.0),
            "#2147: the worst gap gives the freeze its severity"
        );
        assert_eq!(
            gauge_value("videocall_screen_encoder_stall_episodes_total", &healthy),
            None,
            "a healthy share must mint no stall series (counters omitted while 0)"
        );
        assert_eq!(
            gauge_value("videocall_screen_encoder_max_stall_gap_ms", &healthy),
            None,
            "likewise for the severity gauge"
        );
    }

    /// The stall pair must be SWEPT on teardown alongside the fps gauge, or a
    /// disconnected publisher's freeze evidence lingers as a live-looking series.
    ///
    /// MUTATION: delete either stall `remove_label_values` line from
    /// `remove_session_metrics` and this fails.
    #[test]
    fn stall_pair_series_are_swept_on_session_teardown() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let labels = [("meeting_id", "m_stall_gc"), ("session_id", "s_stall_gc")];

        let mut hp = create_test_health_packet(
            "s_stall_gc",
            "m_stall_gc",
            "reporter_stall_gc",
            HashMap::new(),
        );
        hp.screen_encoder_stall_episodes = Some(4);
        hp.screen_encoder_max_stall_gap_ms = Some(900);
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());
        assert_eq!(
            gauge_value("videocall_screen_encoder_stall_episodes_total", &labels),
            Some(4.0),
            "precondition: the series exists before teardown"
        );

        {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let info = guard
                .values()
                .find(|i| i.session_id == "s_stall_gc")
                .expect("the health packet must have registered a session");
            remove_session_metrics(info);
        }

        assert_eq!(
            gauge_value("videocall_screen_encoder_stall_episodes_total", &labels),
            None,
            "#2147: the stall-episode series must be swept on teardown"
        );
        assert_eq!(
            gauge_value("videocall_screen_encoder_max_stall_gap_ms", &labels),
            None,
            "#2147: the max-gap series must be swept on teardown"
        );
    }

    /// The new gauge must be SWEPT on session teardown like its camera sibling.
    /// Load-bearing precisely because this gauge reports real zeroes: an unswept
    /// series would persist for a disconnected publisher and read as a live screen
    /// encoder producing nothing.
    ///
    /// MUTATION: delete the `SCREEN_ENCODER_OUTPUT_FPS.remove_label_values(...)`
    /// line from `remove_session_metrics` and this fails (series still present).
    #[test]
    fn screen_encoder_fps_series_is_swept_on_session_teardown() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let labels = [
            ("meeting_id", "m_scrfps_gc_2147"),
            ("session_id", "s_scrfps_gc_2147"),
        ];

        let mut hp = create_test_health_packet(
            "s_scrfps_gc_2147",
            "m_scrfps_gc_2147",
            "reporter_scrfps_gc_2147",
            HashMap::new(),
        );
        hp.screen_encoder_output_fps = Some(7);
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());
        assert_eq!(
            gauge_value("videocall_screen_encoder_output_fps", &labels),
            Some(7.0),
            "precondition: the series exists before teardown"
        );

        // Tear the session down through the production sweep path, using the
        // SessionInfo the production path itself registered above (rather than a
        // hand-built one, so the label set can never drift from reality).
        {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let info = guard
                .values()
                .find(|i| i.session_id == "s_scrfps_gc_2147")
                .expect("the health packet must have registered a session");
            remove_session_metrics(info);
        }

        assert_eq!(
            gauge_value("videocall_screen_encoder_output_fps", &labels),
            None,
            "#2147: the screen-fps series must be swept on teardown, or a \
             disconnected publisher keeps asserting a live encoder at 0 fps"
        );
    }

    /// `CLIENT_ACTIVE_SERVER`'s `server_type` label must be bounded to a fixed
    /// taxonomy (issue 2047), the same treatment issue 2031 gave the `transport`
    /// label. An unbounded label lets one client mint a new series per packet.
    ///
    /// Mutation coverage: revert `server_type_for_rtt` to
    /// `health_packet.active_server_type.as_str()` and the forged value appears
    /// as its own series, failing the first assert.
    #[test]
    fn unknown_active_server_type_is_collapsed_to_a_bounded_label() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let forged = "carrier-pigeon-\u{1F426}-build-4711";

        let mut hp = create_test_health_packet(
            "s_type_2047",
            "m_type_2047",
            "reporter_type_2047",
            HashMap::new(),
        );
        hp.active_server_url = "wss://relay.example.com".to_string();
        hp.active_server_type = forged.to_string();
        hp.active_server_rtt_ms = 33.0;
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        for metric in [
            "videocall_client_active_server",
            "videocall_client_active_server_rtt_ms",
        ] {
            assert!(
                !series_exists(
                    metric,
                    &[
                        ("meeting_id", "m_type_2047"),
                        ("session_id", "s_type_2047"),
                        ("server_type", forged),
                    ]
                ),
                "{metric} must not carry an unbounded client-supplied server_type"
            );
            assert!(
                series_exists(
                    metric,
                    &[
                        ("meeting_id", "m_type_2047"),
                        ("session_id", "s_type_2047"),
                        ("server_type", "unknown"),
                    ]
                ),
                "{metric} must collapse an unrecognized server_type onto 'unknown'"
            );
        }
    }

    /// The bounding function itself: known transports pass through untouched (so
    /// existing dashboards are unaffected), the empty string keeps the
    /// pre-existing "blank = unknown source" convention the RTT gauge documents,
    /// and everything else collapses to one placeholder.
    #[test]
    fn bounded_active_server_type_admits_only_the_known_taxonomy() {
        assert_eq!(bounded_active_server_type("webtransport"), "webtransport");
        assert_eq!(bounded_active_server_type("websocket"), "websocket");
        assert_eq!(bounded_active_server_type(""), "");
        assert_eq!(bounded_active_server_type("WebSocket"), "unknown");
        assert_eq!(bounded_active_server_type("quic"), "unknown");
        assert_eq!(bounded_active_server_type("../../etc/passwd"), "unknown");
    }

    // ======================================================================
    // Issue 2047: TierTransition label allowlist + per-packet ingest cap
    // ======================================================================

    fn tier_transition(
        direction: &str,
        stream: &str,
        from_tier: &str,
        to_tier: &str,
        trigger: &str,
    ) -> PbTierTransition {
        let mut t = PbTierTransition::new();
        t.direction = direction.to_string();
        t.stream = stream.to_string();
        t.from_tier = from_tier.to_string();
        t.to_tier = to_tier.to_string();
        t.trigger = trigger.to_string();
        t
    }

    /// Every one of the five client-authored `TierTransition` labels must be
    /// collapsed onto its taxonomy. A forged value must NOT appear as its own
    /// series, and the legitimate values must still pass through untouched so
    /// existing dashboards keep working.
    ///
    /// Mutation coverage: revert any of the five to the raw `&t.<field>` and its
    /// forged value appears as a series, failing that field's assert.
    #[test]
    fn forged_tier_transition_labels_collapse_to_the_bounded_taxonomy() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let mut hp = create_test_health_packet(
            "s_tier_2047",
            "m_tier_2047",
            "reporter_tier_2047",
            HashMap::new(),
        );
        // One legitimate transition (verified against videocall-aq: trigger
        // "backpressure", NOT the stale proto comment's "fps"/"bitrate").
        hp.tier_transitions.push(tier_transition(
            "down",
            "video",
            "hd",
            "medium",
            "backpressure",
        ));
        // One fully forged transition — all five labels off-taxonomy.
        hp.tier_transitions.push(tier_transition(
            "sideways-\u{1F680}",
            "hologram",
            "tier-4711",
            "tier-4712",
            "solar-flare",
        ));
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        let base = [("meeting_id", "m_tier_2047"), ("session_id", "s_tier_2047")];
        // The legitimate transition survives verbatim.
        for (name, value) in [
            ("direction", "down"),
            ("stream", "video"),
            ("from_tier", "hd"),
            ("to_tier", "medium"),
            ("trigger", "backpressure"),
        ] {
            let mut labels = base.to_vec();
            labels.push((name, value));
            assert!(
                series_exists("videocall_tier_transition_total", &labels),
                "the legitimate {name}={value} label must pass through unchanged"
            );
        }
        // No forged value becomes a label.
        for (name, value) in [
            ("direction", "sideways-\u{1F680}"),
            ("stream", "hologram"),
            ("from_tier", "tier-4711"),
            ("to_tier", "tier-4712"),
            ("trigger", "solar-flare"),
        ] {
            let mut labels = base.to_vec();
            labels.push((name, value));
            assert!(
                !series_exists("videocall_tier_transition_total", &labels),
                "forged {name}={value} must not reach a Prometheus label"
            );
        }
        // ...it collapses onto the sentinel instead.
        assert!(
            series_exists(
                "videocall_tier_transition_total",
                &[
                    ("meeting_id", "m_tier_2047"),
                    ("session_id", "s_tier_2047"),
                    ("direction", "unknown"),
                    ("stream", "unknown"),
                    ("from_tier", "unknown"),
                    ("to_tier", "unknown"),
                    ("trigger", "unknown"),
                ]
            ),
            "an off-taxonomy transition must collapse onto the bounded sentinel"
        );
    }

    /// A `tier_transitions` vector far over the cap must ingest only up to the
    /// cap, counting the remainder as dropped. This is the DoS half of the
    /// finding: the allowlist bounds SERIES count, the cap bounds per-packet WORK.
    ///
    /// Mutation coverage: delete the `.take(MAX_TIER_TRANSITIONS_PER_PACKET)` and
    /// all 400 entries ingest, so the distinct-series count exceeds the cap and
    /// the drop-counter assert fails.
    #[test]
    fn over_cap_tier_transitions_are_truncated_and_counted() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let mut hp = create_test_health_packet(
            "s_cap_2047",
            "m_cap_2047",
            "reporter_cap_2047",
            HashMap::new(),
        );

        // 400 IDENTICAL, fully legitimate transitions. Identical is the point:
        // they all land on ONE series, so the counter's VALUE is exactly the
        // number of entries ingested. (Distinct-tuple variants make this test
        // fake — the label allowlist already bounds the distinct-SERIES count, so
        // a series-count assert passes with or without the cap.)
        let total = 400usize;
        for _ in 0..total {
            hp.tier_transitions.push(tier_transition(
                "down",
                "video",
                "hd",
                "medium",
                "backpressure",
            ));
        }
        assert!(total > MAX_TIER_TRANSITIONS_PER_PACKET);

        let dropped_before = TIER_TRANSITIONS_DROPPED_TOTAL.get();
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        // The load-bearing assert: the series was incremented exactly `cap`
        // times, not 400. The labels are unique to this test, so the series
        // starts at zero and no other test can perturb the count.
        assert_eq!(
            counter_value(
                "videocall_tier_transition_total",
                &[
                    ("meeting_id", "m_cap_2047"),
                    ("session_id", "s_cap_2047"),
                    ("direction", "down"),
                    ("stream", "video"),
                    ("from_tier", "hd"),
                    ("to_tier", "medium"),
                    ("trigger", "backpressure"),
                ]
            ),
            Some(MAX_TIER_TRANSITIONS_PER_PACKET as f64),
            "exactly `cap` entries may be ingested from one packet, not {total}"
        );

        assert_eq!(
            TIER_TRANSITIONS_DROPPED_TOTAL.get() - dropped_before,
            (total - MAX_TIER_TRANSITIONS_PER_PACKET) as f64,
            "every entry past the cap must be counted as dropped"
        );
    }

    /// The session's tier-transition series must be reaped when the session is,
    /// like their ~40 siblings — otherwise the counter grows for the process
    /// lifetime as sessions come and go (the #1092 leak class).
    ///
    /// Mutation coverage: delete the `tier_transition_labels` loop from
    /// `remove_session_metrics` and the post-reap assert fails.
    #[test]
    fn tier_transition_series_are_reaped_with_the_session() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let mut hp = create_test_health_packet(
            "s_reap_2047",
            "m_reap_2047",
            "reporter_reap_2047",
            HashMap::new(),
        );
        hp.tier_transitions.push(tier_transition(
            "down",
            "screen",
            "high",
            "low",
            "congestion",
        ));
        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        let labels = [
            ("meeting_id", "m_reap_2047"),
            ("session_id", "s_reap_2047"),
            ("direction", "down"),
            ("stream", "screen"),
            ("trigger", "congestion"),
        ];
        assert!(
            series_exists("videocall_tier_transition_total", &labels),
            "baseline: the transition series must publish"
        );

        let info = {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            guard.values().next().expect("session tracked").clone()
        };
        remove_session_metrics(&info);

        assert!(
            !series_exists("videocall_tier_transition_total", &labels),
            "the session's tier-transition series must be removed on departure"
        );
    }

    /// The tier-name allowlist is derived from `videocall-aq`, the same constants
    /// the client indexes when recording a transition — so a tier added there is
    /// accepted here automatically instead of silently collapsing to "unknown".
    ///
    /// Mutation coverage: drop any of the three arrays from `is_known_tier_name`
    /// and that family's assert fails.
    #[test]
    fn tier_name_allowlist_tracks_the_shared_aq_constants() {
        use videocall_aq::constants::{
            AUDIO_QUALITY_TIERS, SCREEN_QUALITY_TIERS, VIDEO_QUALITY_TIERS,
        };
        for t in VIDEO_QUALITY_TIERS {
            assert_eq!(bounded_tier_name(t.label), t.label);
        }
        for t in SCREEN_QUALITY_TIERS {
            assert_eq!(bounded_tier_name(t.label), t.label);
        }
        for t in AUDIO_QUALITY_TIERS {
            assert_eq!(bounded_tier_name(t.label), t.label);
        }
        assert_eq!(bounded_tier_name("hd_1080p_ultra"), "unknown");
        assert_eq!(bounded_tier_name(""), "unknown");
    }

    #[test]
    fn received_layer_series_removed_when_constraint_clears() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let mut constrained = create_test_health_packet(
            "session_received_gc_1561",
            "meeting_received_gc_1561",
            "reporter_received_gc_1561",
            HashMap::new(),
        );
        constrained
            .received_video_layer
            .insert("source_received_gc_1561".to_string(), 0);

        process_health_packet_to_metrics_pb(&constrained, &tracker)
            .expect("first packet should publish the constrained layer");
        let labels = [
            ("meeting_id", "meeting_received_gc_1561"),
            ("session_id", "session_received_gc_1561"),
            ("peer_id", "reporter_received_gc_1561"),
            ("from_peer", "source_received_gc_1561"),
            ("media_kind", "video"),
        ];
        assert_eq!(
            gauge_value("videocall_received_layer", &labels),
            Some(0.0),
            "the first packet must execute the received-layer publish path"
        );

        let recovered = create_test_health_packet(
            "session_received_gc_1561",
            "meeting_received_gc_1561",
            "reporter_received_gc_1561",
            HashMap::new(),
        );
        process_health_packet_to_metrics_pb(&recovered, &tracker)
            .expect("second packet should clear the constraint");
        assert!(
            !series_exists("videocall_received_layer", &labels),
            "a recovered peer must not retain its previous constrained-layer series"
        );
    }

    #[test]
    fn network_type_change_removes_previous_label_series() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let mut wifi = create_test_health_packet(
            "session_network_gc_1556",
            "meeting_network_gc_1556",
            "reporter_network_gc_1556",
            HashMap::new(),
        );
        wifi.client_network_type = Some("wifi".to_string());
        process_health_packet_to_metrics_pb(&wifi, &tracker)
            .expect("wifi packet should publish network type");

        let wifi_labels = [
            ("meeting_id", "meeting_network_gc_1556"),
            ("session_id", "session_network_gc_1556"),
            ("peer_id", "reporter_network_gc_1556"),
            ("network_type", "wifi"),
        ];
        assert!(series_exists("videocall_client_network_type", &wifi_labels));

        let mut ethernet = wifi.clone();
        ethernet.client_network_type = Some("ethernet".to_string());
        process_health_packet_to_metrics_pb(&ethernet, &tracker)
            .expect("ethernet packet should replace network type");
        let ethernet_labels = [
            ("meeting_id", "meeting_network_gc_1556"),
            ("session_id", "session_network_gc_1556"),
            ("peer_id", "reporter_network_gc_1556"),
            ("network_type", "ethernet"),
        ];
        assert!(
            !series_exists("videocall_client_network_type", &wifi_labels),
            "the old wifi label series must be removed"
        );
        assert_eq!(
            gauge_value("videocall_client_network_type", &ethernet_labels),
            Some(1.0)
        );
    }

    #[test]
    fn audio_active_layers_do_not_relabel_user_cap_as_congestion() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let mut packet = create_test_health_packet(
            "session_audio_caps_1561",
            "meeting_audio_caps_1561",
            "reporter_audio_caps_1561",
            HashMap::new(),
        );
        packet.effective_audio_layers = Some(3);
        packet.active_audio_layers = Some(1);
        packet.audio_congestion_ceiling = None;
        process_health_packet_to_metrics_pb(&packet, &tracker)
            .expect("audio layer metrics should be accepted");

        let reporter_labels = [
            ("meeting_id", "meeting_audio_caps_1561"),
            ("session_id", "session_audio_caps_1561"),
            ("peer_id", "reporter_audio_caps_1561"),
        ];
        let mut active_labels = reporter_labels.to_vec();
        active_labels.push(("media_kind", "audio"));
        assert_eq!(
            gauge_value("videocall_encoder_active_layers", &active_labels),
            Some(1.0),
            "the user cap must reduce actual active publication"
        );
        assert_eq!(
            gauge_value("videocall_audio_congestion_ceiling", &reporter_labels),
            Some(3.0),
            "without congestion, the congestion ceiling must remain uncapped"
        );
    }

    /// Helper function to create test peer stats with NetEQ data (protobuf)
    fn create_test_peer_stats(
        peer_id: &str,
        can_listen: bool,
        can_see: bool,
        audio_buffer_ms: f64,
        packets_awaiting_decode: f64,
    ) -> (String, PbPeerStats) {
        let mut counters = PbNetEqOperationCounters::new();
        counters.normal_per_sec = 10.0;
        counters.expand_per_sec = 2.0;
        counters.accelerate_per_sec = 1.0;
        counters.fast_accelerate_per_sec = 0.0;
        counters.preemptive_expand_per_sec = 5.0;
        counters.merge_per_sec = 0.0;
        counters.comfort_noise_per_sec = 0.0;
        counters.dtmf_per_sec = 0.0;
        counters.undefined_per_sec = 0.0;

        let mut network = PbNetEqNetwork::new();
        network.operation_counters = ::protobuf::MessageField::some(counters);

        let mut ns = PbNetEqStats::new();
        ns.current_buffer_size_ms = audio_buffer_ms;
        ns.packets_awaiting_decode = packets_awaiting_decode;
        ns.network = ::protobuf::MessageField::some(network);

        let mut ps = PbPeerStats::new();
        ps.can_listen = can_listen;
        ps.can_see = can_see;
        ps.audio_enabled = can_listen;
        ps.video_enabled = can_see;
        ps.neteq_stats = ::protobuf::MessageField::some(ns);
        (peer_id.to_string(), ps)
    }

    /// Helper for peer stats including video
    fn create_test_peer_stats_with_video(
        peer_id: &str,
        can_listen: bool,
        can_see: bool,
        fps_received: f64,
        frames_buffered: f64,
        frames_decoded: u64,
        bitrate_kbps: u64,
    ) -> (String, PbPeerStats) {
        let mut vs = PbVideoStats::new();
        vs.fps_received = fps_received;
        vs.frames_buffered = frames_buffered;
        vs.frames_decoded = frames_decoded;
        vs.bitrate_kbps = bitrate_kbps;

        let mut ps = PbPeerStats::new();
        ps.can_listen = can_listen;
        ps.can_see = can_see;
        ps.audio_enabled = can_listen;
        ps.video_enabled = can_see;
        ps.video_stats = ::protobuf::MessageField::some(vs);
        (peer_id.to_string(), ps)
    }

    #[test]
    fn test_session_info_creation() {
        let session_info = SessionInfo {
            session_id: "session_123".to_string(),
            meeting_id: "meeting_456".to_string(),
            reporting_user_id: "alice".to_string(),
            last_seen: Instant::now(),
            to_peers: HashSet::new(),
            peer_ids: HashSet::new(),
            display_name: "test_user".to_string(),
            active_servers: HashSet::new(),
            client_info_labels: None,
            last_network_type: None,
            received_layer_peers: HashSet::new(),
            tier_transition_labels: HashSet::new(),
        };

        assert_eq!(session_info.session_id, "session_123");
        assert_eq!(session_info.meeting_id, "meeting_456");
        assert_eq!(session_info.reporting_user_id, "alice");
        assert!(session_info.last_seen.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_session_tracker_operations() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Test inserting a session
        {
            let mut tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_1_session_1_alice".to_string();
            let session_info = SessionInfo {
                session_id: "session_1".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_user_id: "alice".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            tracker_guard.insert(session_key.clone(), session_info);
            assert_eq!(tracker_guard.len(), 1);
            assert!(tracker_guard.contains_key(&session_key));
        }

        // Test updating session timestamp
        {
            let mut tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_1_session_1_alice".to_string();
            if let Some(session_info) = tracker_guard.get_mut(&session_key) {
                session_info.last_seen = Instant::now();
            }
            assert_eq!(tracker_guard.len(), 1);
        }

        // Test removing a session
        {
            let mut tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_1_session_1_alice".to_string();
            tracker_guard.remove(&session_key);
            assert_eq!(tracker_guard.len(), 0);
            assert!(!tracker_guard.contains_key(&session_key));
        }
    }

    #[test]
    fn test_cleanup_stale_sessions() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Add a fresh session
        {
            let mut tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_1_session_1_alice".to_string();
            let session_info = SessionInfo {
                session_id: "session_1".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_user_id: "alice".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            tracker_guard.insert(session_key, session_info);
        }

        // Add a stale session (simulated by setting old timestamp)
        {
            let mut tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_1_session_2_bob".to_string();
            let mut session_info = SessionInfo {
                session_id: "session_2".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_user_id: "bob".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            // Simulate old timestamp by subtracting 40 seconds
            session_info.last_seen -= Duration::from_secs(40);
            tracker_guard.insert(session_key, session_info);
        }

        // Verify we have 2 sessions before cleanup
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tracker_guard.len(), 2);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker);

        // Verify only the fresh session remains
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tracker_guard.len(), 1);
            assert!(tracker_guard.contains_key("meeting_1_session_1_alice"));
            assert!(!tracker_guard.contains_key("meeting_1_session_2_bob"));
        }
    }

    #[test]
    fn test_process_health_packet_to_metrics_basic() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Create test peer stats
        let mut peer_stats = std::collections::HashMap::new();
        let (peer_id, peer_stat) = create_test_peer_stats("bob", true, false, 100.0, 5.0);
        peer_stats.insert(peer_id, peer_stat);

        let health_packet =
            create_test_health_packet("session_123", "meeting_456", "alice", peer_stats);

        // Process the health packet
        let result = process_health_packet_to_metrics_pb(&health_packet, &tracker);
        assert!(result.is_ok());

        // Verify session was tracked
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_456_session_123_alice".to_string();
            assert!(tracker_guard.contains_key(&session_key));

            let session_info = tracker_guard.get(&session_key).unwrap();
            assert_eq!(session_info.session_id, "session_123");
            assert_eq!(session_info.meeting_id, "meeting_456");
            assert_eq!(session_info.reporting_user_id, "alice");
        }
    }

    #[test]
    fn test_self_enabled_metrics_export() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let mut peer_stats = std::collections::HashMap::new();
        let (peer_id, peer_stat) = create_test_peer_stats("bob", true, true, 50.0, 1.0);
        peer_stats.insert(peer_id, peer_stat);

        let mut hp = create_test_health_packet("sess_self", "meet_self", "alice", peer_stats);
        hp.reporting_audio_enabled = true;
        hp.reporting_video_enabled = true;
        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(series_exists(
            "videocall_self_audio_enabled",
            &[("meeting_id", "meet_self"), ("peer_id", "alice")]
        ));
        assert!(series_exists(
            "videocall_self_video_enabled",
            &[("meeting_id", "meet_self"), ("peer_id", "alice")]
        ));
    }

    #[test]
    fn test_peer_enabled_and_video_buffered_metrics_export() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut peer_stats = std::collections::HashMap::new();
        // audio enabled true, video enabled false, but with some video stats present
        let (peer_id, ps) =
            create_test_peer_stats_with_video("bob", true, false, 24.0, 10.0, 100, 300);
        peer_stats.insert(peer_id.clone(), ps);

        let hp = create_test_health_packet("sess_ab", "meet_ab", "alice", peer_stats);
        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(series_exists(
            "videocall_peer_audio_enabled",
            &[
                ("meeting_id", "meet_ab"),
                ("session_id", "sess_ab"),
                ("from_peer", "alice"),
                ("to_peer", "bob")
            ]
        ));
        assert!(series_exists(
            "videocall_peer_video_enabled",
            &[
                ("meeting_id", "meet_ab"),
                ("session_id", "sess_ab"),
                ("from_peer", "alice"),
                ("to_peer", "bob")
            ]
        ));
    }

    #[test]
    fn test_content_staleness_metric_export_is_unbounded() {
        // Regression test for #1641 (epic #1636 Phase 2). The producer reports
        // VideoStats.content_staleness_ms — the AGE of the painted video content — and the metrics
        // server must export it as videocall_video_content_staleness_ms. This is the gauge that
        // exposes the #1631 M2 failure mode (video lagged by minutes while playout_latency_ms read
        // ~0): it is UNBOUNDED, unlike the 1800ms-capped playout_latency_ms. Reverting the
        // VIDEO_CONTENT_STALENESS_MS `.set(...)` export line makes gauge_value() return None and
        // breaks this test — that is the mutation sensitivity CLAUDE.md requires.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Active tile (fps_received > 0) painting content that is 5000ms (5s) stale — a value well
        // above playout_latency_ms's 1800ms client-side cap, proving this gauge is UNBOUNDED.
        let mut vs = PbVideoStats::new();
        vs.fps_received = 24.0;
        vs.content_staleness_ms = 5000.0;

        let mut ps = PbPeerStats::new();
        ps.can_see = true;
        ps.video_enabled = true;
        ps.video_stats = ::protobuf::MessageField::some(vs);

        let mut peer_stats = std::collections::HashMap::new();
        peer_stats.insert("bob_cs_1641".to_string(), ps);

        let hp =
            create_test_health_packet("sess_cs_1641", "meet_cs_1641", "alice_cs_1641", peer_stats);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // from_peer = reporter (the health-packet's reporting_user_id);
        // to_peer = the peer being reported on (the peer_stats map key). Mirrors
        // test_peer_enabled_and_video_buffered_metrics_export's alice(reporter)/bob(peer) mapping.
        assert_eq!(
            gauge_value(
                "videocall_video_content_staleness_ms",
                &[
                    ("meeting_id", "meet_cs_1641"),
                    ("session_id", "sess_cs_1641"),
                    ("from_peer", "alice_cs_1641"),
                    ("to_peer", "bob_cs_1641"),
                ],
            ),
            Some(5000.0),
            "the VIDEO_CONTENT_STALENESS_MS export path must run and set the unbounded (>1800ms) \
             content age; None here means the .set(video_stats.content_staleness_ms) line is missing"
        );
    }

    #[test]
    fn test_audio_datagram_loss_metric_export() {
        // Regression test for #1878. A WebTransport reporter folds the windowed
        // receive-side audio DATAGRAM loss rate into PeerStats.audio_datagram_loss_per_sec,
        // and the metrics server must export it as the per-peer gauge
        // videocall_audio_datagram_loss_per_sec (the audio sibling of
        // videocall_video_seq_loss_per_sec). Reverting the AUDIO_DATAGRAM_LOSS_PER_SEC
        // `.set(loss)` export line makes gauge_value() return None and breaks this
        // test — the mutation sensitivity CLAUDE.md requires.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // A WT audio peer losing 12.5 audio datagrams/sec (a burst NetEQ cannot conceal).
        let mut ps = PbPeerStats::new();
        ps.can_listen = true;
        ps.audio_enabled = true;
        ps.audio_datagram_loss_per_sec = Some(12.5);

        let mut peer_stats = std::collections::HashMap::new();
        peer_stats.insert("bob_adl_1878".to_string(), ps);

        let hp = create_test_health_packet(
            "sess_adl_1878",
            "meet_adl_1878",
            "alice_adl_1878",
            peer_stats,
        );

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // from_peer = reporter (reporting_user_id); to_peer = the peer being reported on
        // (the peer_stats map key). Same mapping as the video seq-loss / staleness exports.
        assert_eq!(
            gauge_value(
                "videocall_audio_datagram_loss_per_sec",
                &[
                    ("meeting_id", "meet_adl_1878"),
                    ("session_id", "sess_adl_1878"),
                    ("from_peer", "alice_adl_1878"),
                    ("to_peer", "bob_adl_1878"),
                ],
            ),
            Some(12.5),
            "the AUDIO_DATAGRAM_LOSS_PER_SEC export path must run and set the folded loss rate; \
             None here means the .set(loss) export line is missing"
        );
    }

    #[test]
    fn test_audio_datagram_raw_loss_metric_export() {
        // Issue 2031. The uncapped magnitude companion to the capped per-peer
        // loss gauge. Reverting the AUDIO_DATAGRAM_RAW_LOSS_PER_SEC `.set(raw_loss)`
        // export line makes gauge_value() return None and breaks this test.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut ps = PbPeerStats::new();
        ps.can_listen = true;
        ps.audio_enabled = true;
        // A heavy burst: capped saturates at ~64, raw reads the true 210 magnitude.
        ps.audio_datagram_loss_per_sec = Some(63.0);
        ps.audio_datagram_raw_loss_per_sec = Some(210.0);

        let mut peer_stats = std::collections::HashMap::new();
        peer_stats.insert("bob_raw_2031".to_string(), ps);

        let hp = create_test_health_packet(
            "sess_raw_2031",
            "meet_raw_2031",
            "alice_raw_2031",
            peer_stats,
        );

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert_eq!(
            gauge_value(
                "videocall_audio_datagram_raw_loss_per_sec",
                &[
                    ("meeting_id", "meet_raw_2031"),
                    ("session_id", "sess_raw_2031"),
                    ("from_peer", "alice_raw_2031"),
                    ("to_peer", "bob_raw_2031"),
                ],
            ),
            Some(210.0),
            "the AUDIO_DATAGRAM_RAW_LOSS_PER_SEC export path must set the uncapped magnitude; \
             None here means the .set(raw_loss) export line is missing"
        );
    }

    #[test]
    fn test_wt_receive_telemetry_per_client_metrics_export() {
        // Issue 2031. The four per-client WT receive-health gauges: read-loop max
        // gap, queue hwm/max-age read-back, and concealment SPLIT BY transport.
        // Reverting any of the corresponding `.set(...)` export lines makes the
        // matching assertion return None (or the wrong transport label) and fails.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut hp = create_test_health_packet(
            "sess_wtrx_2031",
            "meet_wtrx_2031",
            "alice_wtrx_2031",
            std::collections::HashMap::new(),
        );
        // Reporter is on WebTransport — the concealment must land under
        // transport="webtransport".
        hp.active_server_type = "webtransport".to_string();
        hp.wt_datagram_read_loop_max_gap_ms = Some(420.0);
        hp.wt_incoming_datagram_high_water_mark = Some(2048.0);
        hp.wt_incoming_datagram_max_age_ms = Some(3000.0);
        hp.client_audio_concealment_pct = Some(56.0);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        let reporter = [
            ("meeting_id", "meet_wtrx_2031"),
            ("session_id", "sess_wtrx_2031"),
            ("peer_id", "alice_wtrx_2031"),
        ];

        assert_eq!(
            gauge_value("videocall_client_datagram_read_loop_max_gap_ms", &reporter),
            Some(420.0),
            "read-loop max gap must export as the per-client gauge (reader-starvation signal)"
        );
        assert_eq!(
            gauge_value("videocall_wt_incoming_datagram_high_water_mark", &reporter),
            Some(2048.0),
            "observed incomingHighWaterMark read-back must export"
        );
        assert_eq!(
            gauge_value("videocall_wt_incoming_datagram_max_age_ms", &reporter),
            Some(3000.0),
            "observed incomingMaxAge read-back must export"
        );
        // Concealment must carry the transport label sourced from active_server_type.
        assert_eq!(
            gauge_value(
                "videocall_client_audio_concealment_pct",
                &[
                    ("meeting_id", "meet_wtrx_2031"),
                    ("session_id", "sess_wtrx_2031"),
                    ("peer_id", "alice_wtrx_2031"),
                    ("transport", "webtransport"),
                ],
            ),
            Some(56.0),
            "per-client concealment must export split by transport=webtransport"
        );
        // And it must NOT appear under the wrong transport.
        assert_eq!(
            gauge_value(
                "videocall_client_audio_concealment_pct",
                &[
                    ("meeting_id", "meet_wtrx_2031"),
                    ("session_id", "sess_wtrx_2031"),
                    ("peer_id", "alice_wtrx_2031"),
                    ("transport", "websocket"),
                ],
            ),
            None,
            "a WebTransport reporter's concealment must not appear under transport=websocket"
        );
    }

    #[test]
    fn test_concealment_sibling_transport_series_cleared_on_switch() {
        // Issue 2031. A live WT->WS switch (routine once the issue-2029 fallback
        // ships) must not leave the old transport's concealment series latched.
        // Report concealment first as webtransport, then as websocket for the SAME
        // (meeting, session, peer) identity; the webtransport series must be GONE.
        //
        // MUTATION: removing the sibling `remove_label_values` at the ingest site
        // leaves the stale webtransport series at 40.0, so the `None` assertion
        // below fails — the sensitivity this test guards.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let wt_labels = [
            ("meeting_id", "meet_sib_2031"),
            ("session_id", "sess_sib_2031"),
            ("peer_id", "carol_sib_2031"),
            ("transport", "webtransport"),
        ];
        let ws_labels = [
            ("meeting_id", "meet_sib_2031"),
            ("session_id", "sess_sib_2031"),
            ("peer_id", "carol_sib_2031"),
            ("transport", "websocket"),
        ];

        // 1) On WebTransport, concealment 40%.
        let mut hp_wt = create_test_health_packet(
            "sess_sib_2031",
            "meet_sib_2031",
            "carol_sib_2031",
            std::collections::HashMap::new(),
        );
        hp_wt.active_server_type = "webtransport".to_string();
        hp_wt.client_audio_concealment_pct = Some(40.0);
        assert!(process_health_packet_to_metrics_pb(&hp_wt, &tracker).is_ok());
        assert_eq!(
            gauge_value("videocall_client_audio_concealment_pct", &wt_labels),
            Some(40.0),
            "webtransport concealment series must be present after the WT report"
        );

        // 2) The SAME client switches to WebSocket, concealment 15%.
        let mut hp_ws = create_test_health_packet(
            "sess_sib_2031",
            "meet_sib_2031",
            "carol_sib_2031",
            std::collections::HashMap::new(),
        );
        hp_ws.active_server_type = "websocket".to_string();
        hp_ws.client_audio_concealment_pct = Some(15.0);
        assert!(process_health_packet_to_metrics_pb(&hp_ws, &tracker).is_ok());

        // The stale sibling (webtransport) series must be CLEARED...
        assert_eq!(
            gauge_value("videocall_client_audio_concealment_pct", &wt_labels),
            None,
            "the stale webtransport concealment series must be cleared on the WT->WS switch"
        );
        // ...and the now-active websocket series present with the new value.
        assert_eq!(
            gauge_value("videocall_client_audio_concealment_pct", &ws_labels),
            Some(15.0),
            "the active websocket concealment series must be present after the switch"
        );
    }

    #[test]
    fn test_screen_playout_family_exported_from_screen_bucket() {
        // Regression test for #1660 (umbrella #1903). PR #1657 routed a peer's SCREEN-decoder
        // playout stats into the client's screen_video_stats bucket, but the server exported the
        // playout family from the CAMERA video_stats bucket only — the screen block set just
        // SCREEN_VIDEO_FPS / SCREEN_VIDEO_BITRATE_KBPS, so screen content-staleness (and the whole
        // playout family for screens) never reached Prometheus and a screen-share freeze could not
        // be charted. This drives peer_data.screen_video_stats (NOT video_stats) and asserts every
        // screen-prefixed playout gauge is exported. Reverting any of the five screen `.set(...)`
        // export lines makes the matching gauge_value() return None and fails the corresponding
        // assert — the mutation sensitivity CLAUDE.md requires. Because ONLY screen_video_stats is
        // populated here (video_stats is left None), it also proves the family is read from the
        // screen bucket, not the camera bucket.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Active screen tile (fps_received > 0) whose painted content is 7000ms (7s) stale — a
        // value well above playout_latency_ms's 1800ms client-side cap, proving the screen
        // content-staleness gauge is UNBOUNDED like its camera sibling.
        let mut screen_vs = PbVideoStats::new();
        screen_vs.fps_received = 12.0;
        screen_vs.playout_latency_ms = 1200.0;
        screen_vs.playout_stage1_span_ms = 800.0;
        screen_vs.playout_paint_lag_ms = 300.0;
        screen_vs.content_staleness_ms = 7000.0;
        screen_vs.playout_skip_to_live_total = 4;

        let mut ps = PbPeerStats::new();
        ps.can_see = true;
        ps.video_enabled = true;
        // Deliberately leave ps.video_stats (the camera bucket) None: the values below can only
        // come from the screen bucket, so a stray camera-bucket read would export 0/None.
        ps.screen_video_stats = ::protobuf::MessageField::some(screen_vs);

        let mut peer_stats = std::collections::HashMap::new();
        peer_stats.insert("bob_scr_1660".to_string(), ps);

        let hp = create_test_health_packet(
            "sess_scr_1660",
            "meet_scr_1660",
            "alice_scr_1660",
            peer_stats,
        );

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // from_peer = reporter (reporting_user_id); to_peer = the reported peer (peer_stats key).
        // #1954: per-pair series are keyed only by the 4 stable ids (no reporter_name/peer_name).
        let labels = [
            ("meeting_id", "meet_scr_1660"),
            ("session_id", "sess_scr_1660"),
            ("from_peer", "alice_scr_1660"),
            ("to_peer", "bob_scr_1660"),
        ];

        assert_eq!(
            gauge_value("videocall_screen_video_content_staleness_ms", &labels),
            Some(7000.0),
            "SCREEN_VIDEO_CONTENT_STALENESS_MS must export the unbounded (>1800ms) screen content \
             age from screen_video_stats; None => the .set(screen_stats.content_staleness_ms) line \
             is missing (the #1660 gap this test guards)"
        );
        assert_eq!(
            gauge_value("videocall_screen_video_playout_latency_ms", &labels),
            Some(1200.0),
            "SCREEN_VIDEO_PLAYOUT_LATENCY_MS must export screen_stats.playout_latency_ms"
        );
        assert_eq!(
            gauge_value("videocall_screen_video_playout_stage1_span_ms", &labels),
            Some(800.0),
            "SCREEN_VIDEO_PLAYOUT_STAGE1_SPAN_MS must export screen_stats.playout_stage1_span_ms"
        );
        assert_eq!(
            gauge_value("videocall_screen_video_playout_paint_lag_ms", &labels),
            Some(300.0),
            "SCREEN_VIDEO_PLAYOUT_PAINT_LAG_MS must export screen_stats.playout_paint_lag_ms"
        );
        assert_eq!(
            gauge_value("videocall_screen_video_skip_to_live_total", &labels),
            Some(4.0),
            "SCREEN_VIDEO_SKIP_TO_LIVE_TOTAL must export screen_stats.playout_skip_to_live_total"
        );
    }

    #[test]
    fn test_neteq_snapshot_gauges_recover_to_zero() {
        // Regression test for the same zero-latching pattern as issue 2145. These fields are
        // current NetEQ snapshots, not optional measurements: 0 means the playout buffer and
        // decode queue are empty, or no packets arrived in the latest one-second window.
        //
        // The first packet establishes positive values and the second reports 0 on the same
        // pair. Re-adding any old `!= 0.0` guard leaves that gauge latched at its first value,
        // making each assertion independently mutation-sensitive.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let meeting_id = "meet_neteq0_2145";
        let session_id = "sess_neteq0_2145";
        let reporter = "alice_neteq0_2145";
        let peer = "bob_neteq0_2145";
        let labels = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("from_peer", reporter),
            ("to_peer", peer),
        ];

        let make_peer_stats = |buffer_ms: f64, queued: f64, packets_per_sec: f64| {
            let mut neteq = PbNetEqStats::new();
            neteq.current_buffer_size_ms = buffer_ms;
            neteq.packets_awaiting_decode = queued;
            neteq.packets_per_sec = packets_per_sec;

            let mut peer_stats = PbPeerStats::new();
            peer_stats.can_listen = true;
            peer_stats.audio_enabled = true;
            peer_stats.neteq_stats = ::protobuf::MessageField::some(neteq);

            let mut peers = HashMap::new();
            peers.insert(peer.to_string(), peer_stats);
            peers
        };

        let healthy = create_test_health_packet(
            session_id,
            meeting_id,
            reporter,
            make_peer_stats(120.0, 4.0, 50.0),
        );
        assert!(process_health_packet_to_metrics_pb(&healthy, &tracker).is_ok());
        assert_eq!(
            gauge_value("videocall_neteq_audio_buffer_ms", &labels),
            Some(120.0)
        );
        assert_eq!(
            gauge_value("videocall_neteq_packets_awaiting_decode", &labels),
            Some(4.0)
        );
        assert_eq!(
            gauge_value("videocall_neteq_packets_per_sec", &labels),
            Some(50.0)
        );

        let idle = create_test_health_packet(
            session_id,
            meeting_id,
            reporter,
            make_peer_stats(0.0, 0.0, 0.0),
        );
        assert!(process_health_packet_to_metrics_pb(&idle, &tracker).is_ok());
        assert_eq!(
            gauge_value("videocall_neteq_audio_buffer_ms", &labels),
            Some(0.0),
            "an empty NetEQ buffer must overwrite the previous positive snapshot"
        );
        assert_eq!(
            gauge_value("videocall_neteq_packets_awaiting_decode", &labels),
            Some(0.0),
            "an empty decode queue must overwrite the previous positive snapshot"
        );
        assert_eq!(
            gauge_value("videocall_neteq_packets_per_sec", &labels),
            Some(0.0),
            "a silent one-second window must overwrite the previous packet rate"
        );

        let session_key = format!("{meeting_id}_{session_id}_{reporter}");
        let info = {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(&session_key).unwrap().clone()
        };
        remove_session_metrics(&info);
        assert!(!series_exists("videocall_neteq_audio_buffer_ms", &labels));
        assert!(!series_exists(
            "videocall_neteq_packets_awaiting_decode",
            &labels
        ));
        assert!(!series_exists("videocall_neteq_packets_per_sec", &labels));
    }

    #[test]
    fn test_camera_fps_and_bitrate_recover_to_zero_for_connected_peer() {
        // Regression test for issue 2145. VIDEO_FPS / VIDEO_BITRATE_KBPS used to be wrapped in
        // `if video_stats.fps_received != 0.0` / `if video_stats.bitrate_kbps != 0`, so a genuine
        // 0 from a STILL-CONNECTED receiver was silently dropped. Because the only removal of
        // these series is remove_per_peer_metrics (disconnect / peer-departure GC, #1092 — NOT a
        // staleness sweep), the child series did not go absent: it stayed registered and kept
        // being scraped at its last HEALTHY value while no frames were arriving. A gauge that
        // lies is worse than one that is missing.
        //
        // The two-packet shape is load-bearing and is the whole point of the test: packet 1
        // establishes a healthy 30 fps / 1200 kbps, packet 2 reports 0 from the SAME reporter→peer
        // pair, and the asserts demand 0. A single-packet test that only fed 0 would also fail on
        // the UNFIXED code, but with None because the guarded child would never be registered.
        // The two-packet shape proves the real regression: an existing gauge is OVERWRITTEN
        // instead of retaining its last healthy value. Re-adding either `!= 0` guard leaves the
        // packet-1 value latched, so the corresponding assert fails with Some(30.0) /
        // Some(1200.0) instead of Some(0.0).
        //
        // Reachability of the fps == 0 + Some(video_stats) state (not a synthetic case):
        // diagnostics_manager.rs::send_diagnostic_packets substitutes (0.0, 0.0, 0.0) for
        // (fps, bitrate, decode_errors) once a tracker has seen no frame for longer than its
        // staleness window, and health_reporter.rs folds both fields into the proto
        // unconditionally. After a tracker exists, that state persists while no frames arrive:
        // `last_camera_stats` is retained, and folding each zero-substituted event refreshes
        // `last_camera_update_ms`. It ends when frames resume or the peer is removed.
        // (STATS_STALE_MS gates only the can_see/can_listen booleans and the quality-score guard,
        // NOT this fold — do not read it as a 5s bound on the zeros.) So a frozen or camera-off
        // sender in a live call emits exactly this packet repeatedly while no frames arrive.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let meeting_id = "meet_fps0_2145";
        let session_id = "sess_fps0_2145";
        let reporter = "alice_fps0_2145";
        let peer = "bob_fps0_2145";

        // from_peer = reporter (reporting_user_id); to_peer = the reported peer (peer_stats key).
        // #1954: per-pair series are keyed only by the 4 stable ids.
        let labels = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("from_peer", reporter),
            ("to_peer", peer),
        ];

        // Packet 1: a healthy camera stream. This is what the gauge would latch at.
        let mut healthy_vs = PbVideoStats::new();
        healthy_vs.fps_received = 30.0;
        healthy_vs.bitrate_kbps = 1200;

        let mut healthy_ps = PbPeerStats::new();
        healthy_ps.can_see = true;
        healthy_ps.video_enabled = true;
        healthy_ps.video_stats = ::protobuf::MessageField::some(healthy_vs);

        let mut peer_stats_healthy = std::collections::HashMap::new();
        peer_stats_healthy.insert(peer.to_string(), healthy_ps);
        let hp_healthy =
            create_test_health_packet(session_id, meeting_id, reporter, peer_stats_healthy);
        assert!(process_health_packet_to_metrics_pb(&hp_healthy, &tracker).is_ok());

        // Precondition: the healthy value is actually on the gauge, so a later Some(0.0) can only
        // mean it was OVERWRITTEN — not that it was never written.
        assert_eq!(
            gauge_value("videocall_video_fps", &labels),
            Some(30.0),
            "precondition: the healthy 30 fps sample must be on the gauge before the zero packet"
        );
        assert_eq!(
            gauge_value("videocall_video_bitrate_kbps", &labels),
            Some(1200.0),
            "precondition: the healthy 1200 kbps sample must be on the gauge before the zero packet"
        );

        // Packet 2: the sender's video has frozen but the receiver is STILL CONNECTED and still
        // reporting this peer, so `peer` remains in peer_stats (no #1092 prune) and video_stats is
        // still Some — only the values are 0.
        let mut frozen_vs = PbVideoStats::new();
        frozen_vs.fps_received = 0.0;
        frozen_vs.bitrate_kbps = 0;

        let mut frozen_ps = PbPeerStats::new();
        frozen_ps.can_see = true;
        frozen_ps.video_enabled = true;
        frozen_ps.video_stats = ::protobuf::MessageField::some(frozen_vs);

        let mut peer_stats_frozen = std::collections::HashMap::new();
        peer_stats_frozen.insert(peer.to_string(), frozen_ps);
        let hp_frozen =
            create_test_health_packet(session_id, meeting_id, reporter, peer_stats_frozen);
        assert!(process_health_packet_to_metrics_pb(&hp_frozen, &tracker).is_ok());

        // The series must still EXIST (this is not a GC path — the peer never left) and must now
        // read 0, not the stale 30 / 1200.
        assert!(
            series_exists("videocall_video_fps", &labels),
            "the fps series must remain registered — the peer is still connected and still \
             reported, so this is not the #1092 prune path"
        );
        assert_eq!(
            gauge_value("videocall_video_fps", &labels),
            Some(0.0),
            "videocall_video_fps must express the genuine 0 from a still-connected receiver; \
             Some(30.0) here means the `if video_stats.fps_received != 0.0` guard is back and the \
             gauge is latched at its last healthy value while no frames are arriving (issue 2145)"
        );
        assert_eq!(
            gauge_value("videocall_video_bitrate_kbps", &labels),
            Some(0.0),
            "videocall_video_bitrate_kbps must express the genuine 0 (same DiagEvent, same \
             inactivity substitution as fps); Some(1200.0) here means the \
             `if video_stats.bitrate_kbps != 0` guard is back (issue 2145)"
        );

        // Sweep this test's children out of the PROCESS-GLOBAL registry, matching the convention
        // the sibling folding tests in this module follow. The unique `_fps0_2145` label suffix
        // means a leak could not make another test pass or fail spuriously, but every
        // `series_exists`/`gauge_value` helper calls `prometheus::gather()`, which is O(all
        // registered series) — so leaked children tax every later test in the file.
        let session_key = format!("{meeting_id}_{session_id}_{reporter}");
        let info = {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(&session_key).unwrap().clone()
        };
        remove_session_metrics(&info);
        assert!(
            !series_exists("videocall_video_fps", &labels),
            "remove_session_metrics must sweep the fps series (it is in remove_per_peer_metrics)"
        );
    }

    #[test]
    fn test_screen_playout_family_gc_removes_series() {
        // Cleanup regression for #1660 — mirrors test_remove_session_metrics_removes_exported_series
        // and test_rtt_probe_resilience_metrics_exported_and_gc, but drives the screen playout
        // family. The five new screen gauges are per-pair (6-label) series just like the camera
        // family, so they MUST be swept in remove_per_peer_metrics or they leak per-pair cardinality
        // after the reporter's session is reaped. Reverting any of the five
        // SCREEN_VIDEO_*.remove_label_values lines leaves that series alive after GC and fails the
        // corresponding !series_exists assert — the mutation sensitivity for the cleanup sweep.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut screen_vs = PbVideoStats::new();
        screen_vs.fps_received = 12.0;
        screen_vs.playout_latency_ms = 1500.0;
        screen_vs.playout_stage1_span_ms = 900.0;
        screen_vs.playout_paint_lag_ms = 250.0;
        screen_vs.content_staleness_ms = 6000.0;
        screen_vs.playout_skip_to_live_total = 2;

        let mut ps = PbPeerStats::new();
        ps.can_see = true;
        ps.video_enabled = true;
        ps.screen_video_stats = ::protobuf::MessageField::some(screen_vs);

        let meeting_id = "meet_scr_gc_1660";
        let session_id = "sess_scr_gc_1660";
        let reporting_user_id = "alice_scr_gc_1660";
        let to_peer = "bob_scr_gc_1660";

        let mut peer_stats = std::collections::HashMap::new();
        peer_stats.insert(to_peer.to_string(), ps);

        let hp = create_test_health_packet(session_id, meeting_id, reporting_user_id, peer_stats);
        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        let labels = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("from_peer", reporting_user_id),
            ("to_peer", to_peer),
        ];
        let screen_playout_series = [
            "videocall_screen_video_playout_latency_ms",
            "videocall_screen_video_playout_stage1_span_ms",
            "videocall_screen_video_playout_paint_lag_ms",
            "videocall_screen_video_content_staleness_ms",
            "videocall_screen_video_skip_to_live_total",
        ];

        // All five must be present after export (guards the export path too).
        assert!(
            screen_playout_series
                .iter()
                .all(|name| series_exists(name, &labels)),
            "all five screen playout-family series must exist after export"
        );

        // GC the reporter session and confirm every screen playout series is swept.
        let session_key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
        let info = {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(&session_key).unwrap().clone()
        };
        remove_session_metrics(&info);

        assert!(
            screen_playout_series
                .iter()
                .all(|name| !series_exists(name, &labels)),
            "remove_per_peer_metrics must sweep every screen playout-family series; a surviving \
             series means a SCREEN_VIDEO_*.remove_label_values line is missing (per-pair leak)"
        );
    }

    #[test]
    fn test_metrics_handler_does_not_process_cached_health() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let health_store: HealthDataStore = Arc::new(Mutex::new(HashMap::new()));

        let mut peer_stats = std::collections::HashMap::new();
        let (peer_id, peer_stat) = create_test_peer_stats("bob", true, false, 100.0, 5.0);
        peer_stats.insert(peer_id, peer_stat);

        {
            let mut store = health_store.lock().unwrap_or_else(|e| e.into_inner());
            // Store a dummy JSON value; cached store is not used for metrics anyway
            store.insert(
                "health.diagnostics.test".to_string(),
                serde_json::json!({"cached": true}),
            );
        }

        // metrics_handler should not mutate metrics/tracker from cached store
        let rt = tokio::runtime::Runtime::new().unwrap();
        {
            let tracker_clone = tracker.clone();
            rt.block_on(async move {
                let resp =
                    metrics_handler(web::Data::new(health_store), web::Data::new(tracker_clone))
                        .await;
                assert!(resp.is_ok());
            });
        }

        let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
        assert!(guard.is_empty());
    }

    #[test]
    fn test_remove_session_metrics_removes_exported_series() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Publish metrics
        let mut peer_stats = std::collections::HashMap::new();
        let (peer_id, peer_stat) = create_test_peer_stats("bob", true, true, 150.0, 8.0);
        peer_stats.insert(peer_id.clone(), peer_stat);
        let meeting_id = "meeting_rm";
        let session_id = "session_rm";
        let reporting_user_id = "alice";
        let mut packet =
            create_test_health_packet(session_id, meeting_id, reporting_user_id, peer_stats);
        // #1737 Phase 0: set the two unistream byte totals so their reporter-keyed
        // gauges are exported and can be checked for reaping below.
        packet.unistream_bytes_offered_total = Some(5000);
        packet.unistream_bytes_drained_total = Some(1200);
        // #1737 Phase 1: the stale-delta-drops gauge shares the same reporter
        // labels and reap path.
        packet.unistream_stale_delta_drops_total = Some(37);
        let result = process_health_packet_to_metrics_pb(&packet, &tracker);
        assert!(result.is_ok());

        // Confirm a series exists
        assert!(series_exists(
            "videocall_neteq_packets_awaiting_decode",
            &[
                ("meeting_id", meeting_id),
                ("session_id", session_id),
                ("from_peer", reporting_user_id),
                ("to_peer", "bob"),
            ],
        ));

        // #1737 Phase 0: the two unistream byte gauges carry an unbounded
        // session_id label and MUST be reaped by remove_session_metrics. Their
        // reporter_labels key is (meeting_id, session_id, peer_id=reporting_user_id).
        // Guarding them here makes the reap mutation-sensitive: deleting either
        // remove_label_values line in remove_session_metrics leaves the series
        // live and fails the post-removal assertion below.
        let unistream_reporter_labels = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("peer_id", reporting_user_id),
        ];
        assert!(series_exists(
            "videocall_unistream_bytes_offered_total",
            &unistream_reporter_labels,
        ));
        assert!(series_exists(
            "videocall_unistream_bytes_drained_total",
            &unistream_reporter_labels,
        ));
        assert!(series_exists(
            "videocall_unistream_stale_delta_drops_total",
            &unistream_reporter_labels,
        ));

        // #1580: peer_info must also be reaped by remove_session_metrics. Its
        // key is (meeting_id, session_id, peer_id=reporting_user_id,
        // display_name). Guarding this here makes the reap-path removal
        // mutation-sensitive — deleting the VIDEOCALL_PEER_INFO line in
        // remove_session_metrics fails this assertion (previously untested; the
        // rename-path test does not exercise the reap path).
        let peer_info_labels = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("peer_id", reporting_user_id),
            ("display_name", "alice"),
        ];
        assert!(series_exists("videocall_peer_info", &peer_info_labels));

        // Remove and ensure it disappears
        let session_key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
        let info = {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(&session_key).unwrap().clone()
        };
        remove_session_metrics(&info);

        assert!(!series_exists(
            "videocall_neteq_packets_awaiting_decode",
            &[
                ("meeting_id", meeting_id),
                ("session_id", session_id),
                ("from_peer", reporting_user_id),
                ("to_peer", "bob"),
            ],
        ));
        assert!(
            !series_exists("videocall_peer_info", &peer_info_labels),
            "peer_info must be reaped by remove_session_metrics (#1580)"
        );
        assert!(
            !series_exists(
                "videocall_unistream_bytes_offered_total",
                &unistream_reporter_labels,
            ),
            "unistream_bytes_offered_total must be reaped by remove_session_metrics (#1737)"
        );
        assert!(
            !series_exists(
                "videocall_unistream_bytes_drained_total",
                &unistream_reporter_labels,
            ),
            "unistream_bytes_drained_total must be reaped by remove_session_metrics (#1737)"
        );
        assert!(
            !series_exists(
                "videocall_unistream_stale_delta_drops_total",
                &unistream_reporter_labels,
            ),
            "unistream_stale_delta_drops_total must be reaped by remove_session_metrics (#1737)"
        );
    }

    #[test]
    fn test_rtt_probe_resilience_metrics_exported_and_gc() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Build a packet with the two #522 RTT-probe cumulative totals set to
        // distinct non-zero values.
        let peer_stats: std::collections::HashMap<String, PbPeerStats> =
            std::collections::HashMap::new();
        let mut packet =
            create_test_health_packet("session_rtt522", "meeting_rtt522", "probeuser", peer_stats);
        packet.rtt_probe_dropped_total = Some(7);
        packet.rtt_probe_stale_suppressions_total = Some(3);

        let result = process_health_packet_to_metrics_pb(&packet, &tracker);
        assert!(result.is_ok());

        // Value-checking helper: mirrors series_exists's label-matching loop but
        // returns the gauge value of the first fully-matching metric. series_exists
        // only checks existence, so this is the mutation-stronger value assertion.
        let gauge_value = |metric_name: &str, expected_labels: &[(&str, &str)]| -> Option<f64> {
            for family in prometheus::gather() {
                if family.get_name() == metric_name {
                    for metric in family.get_metric() {
                        let all_match = expected_labels.iter().all(|(lname, lval)| {
                            metric.get_label().iter().any(|label| {
                                label.get_name() == *lname && label.get_value() == *lval
                            })
                        });
                        if all_match {
                            return Some(metric.get_gauge().get_value());
                        }
                    }
                }
            }
            None
        };

        let dropped_labels = [
            ("meeting_id", "meeting_rtt522"),
            ("session_id", "session_rtt522"),
            ("peer_id", "probeuser"),
        ];
        let suppressions_labels = dropped_labels;

        assert!(series_exists(
            "videocall_rtt_probe_dropped_total",
            &dropped_labels
        ));
        assert!(series_exists(
            "videocall_rtt_probe_stale_suppressions_total",
            &suppressions_labels,
        ));
        assert_eq!(
            gauge_value("videocall_rtt_probe_dropped_total", &dropped_labels),
            Some(7.0),
        );
        assert_eq!(
            gauge_value(
                "videocall_rtt_probe_stale_suppressions_total",
                &suppressions_labels,
            ),
            Some(3.0),
        );

        // GC the session and confirm both series disappear.
        let session_key = "meeting_rtt522_session_rtt522_probeuser".to_string();
        let info = {
            let g = tracker.lock().unwrap_or_else(|e| e.into_inner());
            g.get(&session_key).unwrap().clone()
        };
        remove_session_metrics(&info);

        assert!(!series_exists(
            "videocall_rtt_probe_dropped_total",
            &dropped_labels
        ));
        assert!(!series_exists(
            "videocall_rtt_probe_stale_suppressions_total",
            &suppressions_labels,
        ));
    }

    #[test]
    fn encoder_restart_metric_expands_cumulative_fields_and_gc_removes_series() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let meeting_id = "meeting_restart_metric";
        let session_id = "session_restart_metric";
        let reporting_user_id = "alice";
        let mut hp =
            create_test_health_packet(session_id, meeting_id, reporting_user_id, HashMap::new());
        hp.camera_encoder_restarts_closed_codec = Some(3);
        hp.screen_encoder_restarts_configure = Some(5);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        let camera_labels = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("kind", "camera"),
            ("reason", "closed_codec"),
        ];
        let screen_labels = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("kind", "screen"),
            ("reason", "configure"),
        ];
        assert_eq!(
            gauge_value("videocall_encoder_restart_total", &camera_labels),
            Some(3.0)
        );
        assert_eq!(
            gauge_value("videocall_encoder_restart_total", &screen_labels),
            Some(5.0)
        );

        let session_key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
        let info = {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(&session_key).unwrap().clone()
        };
        remove_session_metrics(&info);

        assert!(
            !series_exists("videocall_encoder_restart_total", &camera_labels),
            "camera restart series must be removed by session GC"
        );
        assert!(
            !series_exists("videocall_encoder_restart_total", &screen_labels),
            "screen restart series must be removed by session GC"
        );
    }

    #[test]
    fn test_process_health_packet_to_metrics_with_neteq_data() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Create comprehensive peer stats with NetEQ data
        let mut peer_stats = std::collections::HashMap::new();

        // Add peer with full NetEQ stats
        let (peer_id1, peer_stat1) = create_test_peer_stats("bob", true, true, 150.0, 8.0);
        peer_stats.insert(peer_id1, peer_stat1);

        // Add peer with minimal stats
        let (peer_id2, peer_stat2) = create_test_peer_stats("charlie", false, true, 0.0, 0.0);
        peer_stats.insert(peer_id2, peer_stat2);

        let health_packet =
            create_test_health_packet("session_789", "meeting_999", "alice", peer_stats);

        // Process the health packet
        let result = process_health_packet_to_metrics_pb(&health_packet, &tracker);
        assert!(result.is_ok());

        // Verify session tracking
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_999_session_789_alice".to_string();
            assert!(tracker_guard.contains_key(&session_key));
        }
    }

    #[test]
    fn test_process_health_packet_to_metrics_malformed_data() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Test minimal packet
        let peer_stats = std::collections::HashMap::new();
        let hp = create_test_health_packet("session_123", "meeting_123", "alice", peer_stats);
        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());
    }

    #[test]
    fn test_session_cleanup_integration() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Add multiple sessions with different timestamps
        {
            let mut tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());

            // Fresh session
            let session_key1 = "meeting_1_session_1_alice".to_string();
            let session_info1 = SessionInfo {
                session_id: "session_1".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_user_id: "alice".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            tracker_guard.insert(session_key1, session_info1);

            // Stale session
            let session_key2 = "meeting_1_session_2_bob".to_string();
            let mut session_info2 = SessionInfo {
                session_id: "session_2".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_user_id: "bob".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            session_info2.last_seen -= Duration::from_secs(40);
            tracker_guard.insert(session_key2, session_info2);

            // Another fresh session
            let session_key3 = "meeting_2_session_3_charlie".to_string();
            let session_info3 = SessionInfo {
                session_id: "session_3".to_string(),
                meeting_id: "meeting_2".to_string(),
                reporting_user_id: "charlie".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            tracker_guard.insert(session_key3, session_info3);
        }

        // Verify initial state
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tracker_guard.len(), 3);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker);

        // Verify cleanup results
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tracker_guard.len(), 2);
            assert!(tracker_guard.contains_key("meeting_1_session_1_alice"));
            assert!(!tracker_guard.contains_key("meeting_1_session_2_bob")); // Should be cleaned up
            assert!(tracker_guard.contains_key("meeting_2_session_3_charlie"));
        }
    }

    #[test]
    fn test_remove_session_metrics() {
        let session_info = SessionInfo {
            session_id: "test_session".to_string(),
            meeting_id: "test_meeting".to_string(),
            reporting_user_id: "test_peer".to_string(),
            last_seen: Instant::now(),
            to_peers: HashSet::new(),
            peer_ids: HashSet::new(),
            display_name: "test_user".to_string(),
            active_servers: HashSet::new(),
            client_info_labels: None,
            last_network_type: None,
            received_layer_peers: HashSet::new(),
            tier_transition_labels: HashSet::new(),
        };

        // This test verifies that remove_session_metrics doesn't panic
        // In a real environment, this would interact with Prometheus metrics
        remove_session_metrics(&session_info);
    }

    #[test]
    fn test_session_tracker_concurrent_access() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let tracker_clone = tracker.clone();

        // Simulate concurrent access (though this is simplified since we're using Mutex)
        let handle = std::thread::spawn(move || {
            let mut tracker_guard = tracker_clone.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "concurrent_session".to_string();
            let session_info = SessionInfo {
                session_id: "session_concurrent".to_string(),
                meeting_id: "meeting_concurrent".to_string(),
                reporting_user_id: "concurrent_peer".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            tracker_guard.insert(session_key, session_info);
        });

        // Wait for the thread to complete
        handle.join().unwrap();

        // Verify the session was added
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert!(tracker_guard.contains_key("concurrent_session"));
        }
    }

    #[test]
    fn test_health_packet_with_empty_peer_stats() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Create health packet with empty peer stats
        let empty_peer_stats = std::collections::HashMap::new();
        let health_packet =
            create_test_health_packet("session_empty", "meeting_empty", "alice", empty_peer_stats);

        // Process the health packet
        let result = process_health_packet_to_metrics_pb(&health_packet, &tracker);
        assert!(result.is_ok());

        // Verify session was still tracked even with empty peer stats
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "meeting_empty_session_empty_alice".to_string();
            assert!(tracker_guard.contains_key(&session_key));
        }
    }

    #[test]
    fn test_rtt_metrics_cleanup() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Create a health packet with RTT data
        let mut hp = PbHealthPacket::new();
        hp.session_id = "sess_rtt".to_string();
        hp.meeting_id = "meet_rtt".to_string();
        hp.reporting_user_id = "alice".as_bytes().to_vec();
        hp.timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        hp.active_server_url = "wss://server.example.com".to_string();
        hp.active_server_type = "websocket".to_string();
        hp.active_server_rtt_ms = 42.5;

        // Process the packet to set RTT metrics
        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // Verify server info was tracked
        let session_key = "meet_rtt_sess_rtt_alice";
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_info = tracker_guard.get(session_key).unwrap();
            assert!(session_info.active_servers.contains(&(
                "wss://server.example.com".to_string(),
                "websocket".to_string()
            )));
        }

        // Verify RTT metrics exist (indirectly through successful processing)
        assert!(series_exists(
            "videocall_client_active_server",
            &[
                ("meeting_id", "meet_rtt"),
                ("session_id", "sess_rtt"),
                ("peer_id", "alice"),
                ("server_url", "wss://server.example.com"),
                ("server_type", "websocket")
            ]
        ));

        assert!(series_exists(
            "videocall_client_active_server_rtt_ms",
            &[
                ("meeting_id", "meet_rtt"),
                ("session_id", "sess_rtt"),
                ("peer_id", "alice"),
                ("server_url", "wss://server.example.com"),
                ("server_type", "websocket")
            ]
        ));

        // Remove session metrics
        let info = {
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(session_key).unwrap().clone()
        };
        remove_session_metrics(&info);

        // Verify RTT metrics are removed
        assert!(!series_exists(
            "videocall_client_active_server",
            &[
                ("meeting_id", "meet_rtt"),
                ("session_id", "sess_rtt"),
                ("peer_id", "alice"),
                ("server_url", "wss://server.example.com"),
                ("server_type", "websocket")
            ]
        ));

        assert!(!series_exists(
            "videocall_client_active_server_rtt_ms",
            &[
                ("meeting_id", "meet_rtt"),
                ("session_id", "sess_rtt"),
                ("peer_id", "alice"),
                ("server_url", "wss://server.example.com"),
                ("server_type", "websocket")
            ]
        ));
    }

    #[test]
    fn test_session_timeout_edge_cases() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Add session exactly at timeout boundary
        {
            let mut tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let session_key = "boundary_session".to_string();
            let mut session_info = SessionInfo {
                session_id: "session_boundary".to_string(),
                meeting_id: "meeting_boundary".to_string(),
                reporting_user_id: "boundary_peer".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
                display_name: "test_user".to_string(),
                active_servers: HashSet::new(),
                client_info_labels: None,
                last_network_type: None,
                received_layer_peers: HashSet::new(),
                tier_transition_labels: HashSet::new(),
            };
            // Set to exactly 30 seconds ago (timeout boundary)
            session_info.last_seen -= Duration::from_secs(30);
            tracker_guard.insert(session_key, session_info);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker);

        // Session should be cleaned up (>= 30 seconds is considered stale)
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tracker_guard.len(), 0);
        }
    }

    #[test]
    fn test_jwt_token_stripped_from_server_url() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // URL with ?token= (only query param)
        let mut hp = create_test_health_packet("s1", "m1", "alice", HashMap::new());
        hp.active_server_url = "wss://relay.example.com?token=eyJhbGciOi.secret".to_string();
        hp.active_server_type = "websocket".to_string();
        hp.active_server_rtt_ms = 50.0;
        // Add a peer so the packet isn't empty
        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        hp.peer_stats.insert(peer_id, ps);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // Verify the server_url label does NOT contain the token
        assert!(
            !series_exists(
                "videocall_client_active_server",
                &[(
                    "server_url",
                    "wss://relay.example.com?token=eyJhbGciOi.secret"
                )]
            ),
            "JWT token should be stripped from server_url label"
        );
        assert!(
            series_exists(
                "videocall_client_active_server",
                &[("server_url", "wss://relay.example.com")]
            ),
            "Clean URL without token should be present"
        );
    }

    #[test]
    fn test_jwt_token_stripped_with_other_params() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // URL with token among other query params
        let mut hp = create_test_health_packet("s2", "m2", "carol", HashMap::new());
        hp.active_server_url =
            "wss://relay.example.com?region=us-east&token=secret123&debug=1".to_string();
        hp.active_server_type = "webtransport".to_string();
        hp.active_server_rtt_ms = 30.0;
        let (peer_id, ps) = create_test_peer_stats("dave", true, true, 50.0, 2.0);
        hp.peer_stats.insert(peer_id, ps);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(
            series_exists(
                "videocall_client_active_server",
                &[(
                    "server_url",
                    "wss://relay.example.com?region=us-east&debug=1"
                )]
            ),
            "Token param should be stripped, other params preserved"
        );
    }

    /// After the NATS-path URL scrub in client_diagnostics.rs strips
    /// `active_server_url` to an empty string, the RTT metric must still publish.
    /// The `CLIENT_ACTIVE_SERVER` identity gauge, however, legitimately requires
    /// a non-empty URL and must remain absent.
    #[test]
    fn test_rtt_publishes_when_server_url_scrubbed_empty() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Simulate the post-scrub state: URL + server_type both zeroed, RTT set.
        let mut hp = create_test_health_packet("s_scrub", "m_scrub", "eve", HashMap::new());
        hp.active_server_url = String::new();
        hp.active_server_type = String::new();
        hp.active_server_rtt_ms = 77.25;
        let (peer_id, ps) = create_test_peer_stats("frank", true, true, 50.0, 2.0);
        hp.peer_stats.insert(peer_id, ps);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // RTT metric must be present with empty server_url / server_type labels.
        assert!(
            series_exists(
                "videocall_client_active_server_rtt_ms",
                &[
                    ("meeting_id", "m_scrub"),
                    ("session_id", "s_scrub"),
                    ("peer_id", "eve"),
                    ("server_url", ""),
                    ("server_type", ""),
                ]
            ),
            "RTT must publish even when active_server_url is empty post-scrub"
        );

        // Identity gauge must NOT be present — it requires a URL to be meaningful.
        assert!(
            !series_exists(
                "videocall_client_active_server",
                &[
                    ("meeting_id", "m_scrub"),
                    ("session_id", "s_scrub"),
                    ("peer_id", "eve"),
                ]
            ),
            "CLIENT_ACTIVE_SERVER should not publish without a non-empty URL"
        );
    }

    /// When the RTT value is zero (passthrough clients that never measured RTT),
    /// nothing is published even if server_url is also empty.
    #[test]
    fn test_rtt_not_published_when_zero_and_url_empty() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut hp = create_test_health_packet("s_zero", "m_zero", "gina", HashMap::new());
        hp.active_server_url = String::new();
        hp.active_server_type = String::new();
        hp.active_server_rtt_ms = 0.0;
        let (peer_id, ps) = create_test_peer_stats("hank", true, true, 50.0, 2.0);
        hp.peer_stats.insert(peer_id, ps);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(
            !series_exists(
                "videocall_client_active_server_rtt_ms",
                &[
                    ("meeting_id", "m_zero"),
                    ("session_id", "s_zero"),
                    ("peer_id", "gina"),
                ]
            ),
            "RTT must not publish when rtt_ms == 0.0 (passthrough client)"
        );
    }

    #[test]
    fn test_per_pair_metrics_carry_no_name_labels() {
        // #1954: per-pair series keyed only by (meeting_id, session_id, from_peer, to_peer).
        // Reverting metrics.rs + peer_labels back to 6 labels re-attaches the name labels and
        // fails the !matching_series_has_label asserts below — the mutation guard.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Build a health packet from reporter "alice_1954" observing peer "bob_1954" with a
        // real display_name set on the packet (PII that must NOT reach a per-pair label).
        // from_peer = reporting_user_id ("alice_1954"); to_peer = the peer_stats key ("bob_1954").
        let (peer_id, ps) = create_test_peer_stats("bob_1954", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s1954", "m1954", "alice_1954", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);
        hp.display_name = Some("Alice Real Name".to_string());

        assert!(process_health_packet_to_metrics_pb(&hp, &tracker).is_ok());

        let stable = [
            ("meeting_id", "m1954"),
            ("session_id", "s1954"),
            ("from_peer", "alice_1954"),
            ("to_peer", "bob_1954"),
        ];
        // videocall_peer_can_listen is set unconditionally in the publish loop, so it is
        // always present for the 4 stable labels — guards against a vacuous pass.
        assert!(
            series_exists("videocall_peer_can_listen", &stable),
            "per-pair series must be published for the 4 stable labels"
        );
        assert!(
            !matching_series_has_label("videocall_peer_can_listen", &stable, "reporter_name"),
            "per-pair series must not carry reporter_name (PII)"
        );
        assert!(
            !matching_series_has_label("videocall_peer_can_listen", &stable, "peer_name"),
            "per-pair series must not carry peer_name (PII)"
        );
    }

    #[test]
    fn test_peer_info_rename_cleanup_and_client_metric_label_shape() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut hp1 = create_test_health_packet(
            "session_peer_info",
            "meeting_peer_info",
            "peer_info_user",
            HashMap::new(),
        );
        hp1.display_name = Some("Alice One".to_string());
        let result1 = process_health_packet_to_metrics_pb(&hp1, &tracker);
        assert!(result1.is_ok());

        let mut hp2 = create_test_health_packet(
            "session_peer_info",
            "meeting_peer_info",
            "peer_info_user",
            HashMap::new(),
        );
        hp2.display_name = Some("Alice Two".to_string());
        let result2 = process_health_packet_to_metrics_pb(&hp2, &tracker);
        assert!(result2.is_ok());

        let stable_labels = [
            ("meeting_id", "meeting_peer_info"),
            ("session_id", "session_peer_info"),
            ("peer_id", "peer_info_user"),
        ];
        assert_eq!(
            matching_series_count("videocall_peer_info", &stable_labels),
            1,
            "display-name rename must leave exactly one peer_info series per stable peer key"
        );
        assert!(
            !series_exists(
                "videocall_peer_info",
                &[
                    ("meeting_id", "meeting_peer_info"),
                    ("session_id", "session_peer_info"),
                    ("peer_id", "peer_info_user"),
                    ("display_name", "Alice One"),
                ],
            ),
            "old peer_info display_name label set must be removed on rename"
        );
        assert!(
            series_exists(
                "videocall_peer_info",
                &[
                    ("meeting_id", "meeting_peer_info"),
                    ("session_id", "session_peer_info"),
                    ("peer_id", "peer_info_user"),
                    ("display_name", "Alice Two"),
                ],
            ),
            "new peer_info display_name label set must exist"
        );
        assert!(
            !matching_series_has_label(
                "videocall_client_tab_visible",
                &stable_labels,
                "display_name",
            ),
            "per-client metrics must not carry display_name labels"
        );
    }

    /// Reads the current `videocall_meeting_participants` gauge value for a meeting.
    /// Returns `None` if no series exists for that meeting (e.g. it was removed).
    fn meeting_participants_value(meeting_id: &str) -> Option<f64> {
        let families = prometheus::gather();
        for family in &families {
            if family.get_name() == "videocall_meeting_participants" {
                for metric in family.get_metric() {
                    for label in metric.get_label() {
                        if label.get_name() == "meeting_id" && label.get_value() == meeting_id {
                            return Some(metric.get_gauge().get_value());
                        }
                    }
                }
            }
        }
        None
    }

    #[test]
    fn test_meeting_participants_counts_live_sessions() {
        // Issue #1040: the gauge is derived from the authoritative session tracker
        // (distinct live sessions per meeting), NOT from one reporter's
        // peer_stats.len() + 1. With a single live reporter, the meeting has exactly
        // one tracked session, so the count is 1 even though the reporter sees peers.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // One reporter (alice) that observes 2 peers.
        let (p1, ps1) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let (p2, ps2) = create_test_peer_stats("carol", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s20", "m20", "alice", HashMap::new());
        hp.peer_stats.insert(p1, ps1);
        hp.peer_stats.insert(p2, ps2);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert_eq!(
            meeting_participants_value("m20"),
            Some(1.0),
            "one live reporting session => one participant"
        );

        // A second distinct participant (bob) reports in the same meeting.
        let mut hp2 = create_test_health_packet("s21", "m20", "bob", HashMap::new());
        let (p3, ps3) = create_test_peer_stats("alice", true, true, 50.0, 2.0);
        hp2.peer_stats.insert(p3, ps3);
        let result2 = process_health_packet_to_metrics_pb(&hp2, &tracker);
        assert!(result2.is_ok());

        assert_eq!(
            meeting_participants_value("m20"),
            Some(2.0),
            "two distinct live sessions => two participants (no per-reporter skew)"
        );
    }

    #[test]
    fn test_meeting_participants_decrements_and_removed_on_stale() {
        // Issue #1040 core bug: the gauge must decrement as sessions expire and be
        // removed entirely when the meeting empties — instead of latching a phantom
        // count forever.
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Two distinct participants report in meeting "m_leak".
        let mut hp_a = create_test_health_packet("s_a", "m_leak", "alice", HashMap::new());
        let (pa, psa) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        hp_a.peer_stats.insert(pa, psa);
        let mut hp_b = create_test_health_packet("s_b", "m_leak", "bob", HashMap::new());
        let (pb, psb) = create_test_peer_stats("alice", true, true, 50.0, 2.0);
        hp_b.peer_stats.insert(pb, psb);
        assert!(process_health_packet_to_metrics_pb(&hp_a, &tracker).is_ok());
        assert!(process_health_packet_to_metrics_pb(&hp_b, &tracker).is_ok());

        assert_eq!(
            meeting_participants_value("m_leak"),
            Some(2.0),
            "two live sessions => 2 participants"
        );

        // Expire only alice's session by backdating its last_seen past the timeout.
        {
            let mut t = tracker.lock().unwrap_or_else(|e| e.into_inner());
            for info in t.values_mut() {
                if info.reporting_user_id == "alice" {
                    info.last_seen = Instant::now() - Duration::from_secs(60);
                }
            }
        }
        cleanup_stale_sessions(&tracker);

        assert_eq!(
            meeting_participants_value("m_leak"),
            Some(1.0),
            "gauge must decrement to 1 after one session goes stale"
        );

        // Expire the remaining session — the meeting is now empty.
        {
            let mut t = tracker.lock().unwrap_or_else(|e| e.into_inner());
            for info in t.values_mut() {
                info.last_seen = Instant::now() - Duration::from_secs(60);
            }
        }
        cleanup_stale_sessions(&tracker);

        assert_eq!(
            meeting_participants_value("m_leak"),
            None,
            "gauge series must be removed once the meeting empties (no phantom participants)"
        );
    }

    #[test]
    fn test_p1_metrics_exposed() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s30", "m30", "alice", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);
        hp.send_queue_bytes = Some(1024);
        hp.packets_received_per_sec = Some(50.0);
        hp.packets_sent_per_sec = Some(45.0);
        hp.is_tab_throttled = true;

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(
            series_exists(
                "videocall_client_send_queue_bytes",
                &[("meeting_id", "m30"), ("session_id", "s30")]
            ),
            "send_queue_bytes should be exposed"
        );
        assert!(
            series_exists(
                "videocall_client_packets_received_per_sec",
                &[("meeting_id", "m30")]
            ),
            "packets_received_per_sec should be exposed"
        );
        assert!(
            series_exists(
                "videocall_client_packets_sent_per_sec",
                &[("meeting_id", "m30")]
            ),
            "packets_sent_per_sec should be exposed"
        );
        assert!(
            series_exists("videocall_client_tab_throttled", &[("meeting_id", "m30")]),
            "tab_throttled should be exposed"
        );
    }

    /// #1032: non-heap memory gauges are exported when present on the packet.
    #[test]
    fn test_nonheap_memory_metrics_exposed() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s1032", "m1032", "alice", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);
        hp.wasm_memory_bytes = Some(67_108_864); // 64 MiB WASM linear memory
        hp.agent_memory_bytes = Some(2_147_483_648); // 2 GiB total agent memory

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(
            series_exists(
                "videocall_client_wasm_memory_bytes",
                &[("meeting_id", "m1032"), ("session_id", "s1032")]
            ),
            "wasm_memory_bytes should be exposed"
        );
        assert!(
            series_exists(
                "videocall_client_agent_memory_bytes",
                &[("meeting_id", "m1032"), ("session_id", "s1032")]
            ),
            "agent_memory_bytes should be exposed"
        );
    }

    /// #1032: when the client omits the non-heap fields (API unavailable), the
    /// gauges are NOT exported — Grafana shows a gap, not a misleading zero.
    #[test]
    fn test_nonheap_memory_metrics_absent_when_omitted() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s1032b", "m1032b", "alice", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);
        // wasm_memory_bytes / agent_memory_bytes deliberately left None.

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(
            !series_exists(
                "videocall_client_agent_memory_bytes",
                &[("meeting_id", "m1032b"), ("session_id", "s1032b")]
            ),
            "agent_memory_bytes must be absent when the client omits it"
        );
    }

    #[test]
    fn test_health_reports_counter_incremented() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let before = HEALTH_REPORTS_TOTAL.get();

        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s40", "m40", "alice", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);

        let _ = process_health_packet_to_metrics_pb(&hp, &tracker);

        let after = HEALTH_REPORTS_TOTAL.get();
        assert!(
            after > before,
            "HEALTH_REPORTS_TOTAL should be incremented on each health packet"
        );
    }

    #[test]
    fn test_client_info_published_for_battery_only_metadata() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut hp = create_test_health_packet("s_battery", "m_battery", "alice", HashMap::new());
        hp.client_battery_level = Some(0.42);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        assert!(
            series_exists(
                "videocall_client_info",
                &[
                    ("meeting_id", "m_battery"),
                    ("session_id", "s_battery"),
                    ("cores", ""),
                    ("architecture", ""),
                    ("gpu_family", ""),
                    ("network_effective_type", ""),
                    ("capability_score", ""),
                ],
            ),
            "battery-only client metadata must publish CLIENT_INFO"
        );
    }

    #[test]
    fn test_battery_level_gauge_published_with_reported_value() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let mut hp = create_test_health_packet("s_batval", "m_batval", "alice", HashMap::new());
        hp.client_battery_level = Some(0.37);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        // #1392: the battery VALUE must ride on its own numeric gauge (peer_id is
        // the reporting_user_id; display_name defaults to the reporting_user_id).
        let labels = [
            ("meeting_id", "m_batval"),
            ("session_id", "s_batval"),
            ("peer_id", "alice"),
        ];
        assert_eq!(
            gauge_value("videocall_client_battery_level", &labels),
            Some(0.37),
            "the reported battery level must be published as the gauge value"
        );
    }

    #[test]
    fn test_battery_level_gauge_absent_when_not_reported() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // A packet with OTHER client metadata (so the TELEM-7 block runs) but no
        // battery level: the gauge must stay absent, not publish a misleading 0.
        let mut hp = create_test_health_packet("s_nobat", "m_nobat", "carol", HashMap::new());
        hp.client_cores = Some(8);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker);
        assert!(result.is_ok());

        let labels = [
            ("meeting_id", "m_nobat"),
            ("session_id", "s_nobat"),
            ("peer_id", "carol"),
        ];
        assert_eq!(
            gauge_value("videocall_client_battery_level", &labels),
            None,
            "an absent battery level must leave the gauge absent (not a 0)"
        );
    }

    /// Pure diff helper (issue #1092): peers we still hold series for but that are
    /// absent from the current packet are the ones to prune; peers still present
    /// (and brand-new peers not yet stored) are not.
    #[test]
    fn test_peers_to_prune_diff() {
        let mut stored = HashSet::new();
        stored.insert("alice".to_string());
        stored.insert("bob".to_string());
        stored.insert("carol".to_string());

        // Current packet only reports alice (bob + carol have left this reporter's view).
        let current: HashSet<&str> = ["alice"].into_iter().collect();
        let mut departed = peers_to_prune(&stored, &current);
        departed.sort();
        assert_eq!(departed, vec!["bob".to_string(), "carol".to_string()]);

        // All stored peers still present => nothing to prune.
        let current_all: HashSet<&str> = ["alice", "bob", "carol"].into_iter().collect();
        assert!(peers_to_prune(&stored, &current_all).is_empty());

        // Empty packet (reporter sees nobody) => every stored peer is departed.
        let empty: HashSet<&str> = HashSet::new();
        let mut all = peers_to_prune(&stored, &empty);
        all.sort();
        assert_eq!(
            all,
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()]
        );

        // A brand-new peer present in the packet but not yet stored is NOT pruned
        // (the diff only ever removes peers we already hold).
        let current_new: HashSet<&str> = ["alice", "bob", "carol", "dave"].into_iter().collect();
        assert!(peers_to_prune(&stored, &current_new).is_empty());
    }

    /// End-to-end ingest test for the per-packet prune (issue #1092):
    /// reporter alice sees {peer_a, peer_b}, then a later packet sees only
    /// {peer_a}. peer_b's per-pair series must be REMOVED while peer_a's is
    /// retained; the session must no longer track peer_b. A final packet with
    /// NO peers must prune peer_a too.
    #[test]
    fn test_departed_peer_per_pair_series_pruned_on_next_packet() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        let meeting_id = "m_prune_1092";
        let session_id = "s_prune_1092";
        let reporter = "alice_1092";

        // Packet 1: reporter sees peer_a and peer_b (both with NetEQ stats so the
        // per-pair series are written).
        let (pa, psa) = create_test_peer_stats("peer_a_1092", true, true, 150.0, 8.0);
        let (pb, psb) = create_test_peer_stats("peer_b_1092", true, true, 120.0, 4.0);
        let mut hp1 = create_test_health_packet(session_id, meeting_id, reporter, HashMap::new());
        hp1.peer_stats.insert(pa, psa);
        hp1.peer_stats.insert(pb, psb);
        assert!(process_health_packet_to_metrics_pb(&hp1, &tracker).is_ok());

        // Both per-pair series exist after packet 1.
        let labels_a = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("from_peer", reporter),
            ("to_peer", "peer_a_1092"),
        ];
        let labels_b = [
            ("meeting_id", meeting_id),
            ("session_id", session_id),
            ("from_peer", reporter),
            ("to_peer", "peer_b_1092"),
        ];
        assert!(
            series_exists("videocall_neteq_packets_awaiting_decode", &labels_a),
            "peer_a series should exist after first packet"
        );
        assert!(
            series_exists("videocall_neteq_packets_awaiting_decode", &labels_b),
            "peer_b series should exist after first packet"
        );

        // Packet 2: reporter now sees ONLY peer_a; peer_b has left its view.
        let (pa2, psa2) = create_test_peer_stats("peer_a_1092", true, true, 150.0, 8.0);
        let mut hp2 = create_test_health_packet(session_id, meeting_id, reporter, HashMap::new());
        hp2.peer_stats.insert(pa2, psa2);
        assert!(process_health_packet_to_metrics_pb(&hp2, &tracker).is_ok());

        // peer_b's per-pair series must be GONE; peer_a's must remain.
        assert!(
            !series_exists("videocall_neteq_packets_awaiting_decode", &labels_b),
            "peer_b series must be pruned once it leaves the reporter's view"
        );
        assert!(
            series_exists("videocall_neteq_packets_awaiting_decode", &labels_a),
            "peer_a series must be retained while still reported"
        );

        // Regression guard: the meeting-scoped PEER_CONNECTIONS_TOTAL{meeting, peer_b}
        // must SURVIVE the per-reporter prune. It is keyed (meeting_id, peer_id) and is
        // shared across reporters, so it is cleaned up ONLY on whole-session reap (which
        // iterates `peer_ids`). If the prune block re-adds `info.peer_ids.remove(peer_id)`,
        // peer_b drops out of every live reporter's `peer_ids` and this gauge is orphaned
        // forever — the exact frozen-gauge leak class #1092 fixes. This assertion fails
        // if that line is re-introduced.
        let conn_labels_b = [("meeting_id", meeting_id), ("peer_id", "peer_b_1092")];
        assert!(
            series_exists("videocall_peer_connections_total", &conn_labels_b),
            "PEER_CONNECTIONS_TOTAL{{meeting, peer_b}} must survive the per-reporter prune \
             (removed only on whole-session reap, which iterates peer_ids)"
        );

        // The session tracker must no longer track peer_b in the per-pair sets (so its
        // per-pair series aren't re-written), but MUST retain it in `peer_ids` so the
        // whole-session reap still removes PEER_CONNECTIONS_TOTAL{meeting, peer_b}.
        {
            let key = format!("{meeting_id}_{session_id}_{reporter}");
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let info = guard.get(&key).expect("session must still be tracked");
            assert!(
                !info.to_peers.contains("peer_b_1092"),
                "peer_b must be dropped from to_peers"
            );
            assert!(
                info.to_peers.contains("peer_a_1092"),
                "peer_a must remain in to_peers"
            );
            assert!(
                info.peer_ids.contains("peer_b_1092"),
                "peer_b must REMAIN in peer_ids so whole-session reap removes \
                 PEER_CONNECTIONS_TOTAL{{meeting, peer_b}}"
            );
        }

        // Packet 3: reporter sees NOBODY (empty peer_stats). peer_a must be pruned
        // too — this exercises the unconditional prune on the empty-peer_stats path.
        let hp3 = create_test_health_packet(session_id, meeting_id, reporter, HashMap::new());
        assert!(process_health_packet_to_metrics_pb(&hp3, &tracker).is_ok());

        assert!(
            !series_exists("videocall_neteq_packets_awaiting_decode", &labels_a),
            "peer_a series must be pruned when the reporter sees nobody (empty peer_stats)"
        );
        let session_snapshot = {
            let key = format!("{meeting_id}_{session_id}_{reporter}");
            let guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            let info = guard.get(&key).expect("session must still be tracked");
            assert!(
                info.to_peers.is_empty(),
                "all peers must be pruned after an empty packet"
            );
            // Even with every per-pair set emptied, `peer_ids` still holds the peers
            // this session ever connected to — that is what drives the meeting-scoped
            // PEER_CONNECTIONS_TOTAL cleanup at reap.
            assert!(
                info.peer_ids.contains("peer_b_1092") && info.peer_ids.contains("peer_a_1092"),
                "peer_ids must retain all ever-seen peers across per-packet prunes"
            );
            info.clone()
        };

        // PEER_CONNECTIONS_TOTAL{meeting, peer_b} is STILL present after all per-packet
        // prunes (it is removed only on whole-session reap).
        assert!(
            series_exists("videocall_peer_connections_total", &conn_labels_b),
            "PEER_CONNECTIONS_TOTAL{{meeting, peer_b}} must persist until whole-session reap"
        );

        // End-to-end reap: removing the session must now delete PEER_CONNECTIONS_TOTAL
        // for BOTH peers — proving the retained `peer_ids` actually drives that cleanup.
        // Under the #1092 prune-block bug (peer_ids.remove in the prune), peer_b would
        // have been dropped from peer_ids before this point and this gauge would leak.
        remove_session_metrics(&session_snapshot);
        assert!(
            !series_exists("videocall_peer_connections_total", &conn_labels_b),
            "PEER_CONNECTIONS_TOTAL{{meeting, peer_b}} must be removed on whole-session reap"
        );
        let conn_labels_a = [("meeting_id", meeting_id), ("peer_id", "peer_a_1092")];
        assert!(
            !series_exists("videocall_peer_connections_total", &conn_labels_a),
            "PEER_CONNECTIONS_TOTAL{{meeting, peer_a}} must be removed on whole-session reap"
        );
    }
}
