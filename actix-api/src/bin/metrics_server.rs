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
    // Maps peer session_id → display_name for cleanup of display_name-labeled metrics
    to_peer_display_names: HashMap<String, String>,
    // Peer IDs we have published peer connection metrics for
    peer_ids: HashSet<String>,
    // Server info we have published active server metrics for (server_url, server_type)
    active_servers: HashSet<(String, String)>,
}

type SessionTracker = Arc<Mutex<HashMap<String, SessionInfo>>>;

/// Maps session_id → display_name so we can resolve peer display names.
/// Built from incoming health packets (each carries the reporter's display_name).
type DisplayNameMap = Arc<Mutex<HashMap<String, String>>>;

// Prometheus metrics (same as existing diagnostics.rs)
// Import shared Prometheus metrics
use sec_api::metrics::{
    ACTIVE_SESSIONS_TOTAL, ADAPTIVE_AUDIO_TIER, ADAPTIVE_VIDEO_TIER, AUDIO_PACKET_LOSS_PCT,
    AUDIO_QUALITY_SCORE, CALL_QUALITY_SCORE, CLIENT_ACTIVE_SERVER, CLIENT_ACTIVE_SERVER_RTT_MS,
    CLIENT_MEMORY_TOTAL_BYTES, CLIENT_MEMORY_USED_BYTES, CLIENT_PACKETS_RECEIVED_PER_SEC,
    CLIENT_PACKETS_SENT_PER_SEC, CLIENT_SEND_QUEUE_BYTES, CLIENT_TAB_THROTTLED, CLIENT_TAB_VISIBLE,
    DATAGRAM_DROPS_TOTAL, HEALTH_REPORTS_TOTAL, KEYFRAME_REQUESTS_SENT_TOTAL, MEETING_PARTICIPANTS,
    NETEQ_ACCELERATE_OPS_PER_SEC, NETEQ_AUDIO_BUFFER_MS, NETEQ_EXPAND_OPS_PER_SEC,
    NETEQ_NORMAL_OPS_PER_SEC, NETEQ_PACKETS_AWAITING_DECODE, NETEQ_PACKETS_PER_SEC,
    NETEQ_TARGET_DELAY_MS, PEER_AUDIO_ENABLED, PEER_CAN_LISTEN, PEER_CAN_SEE,
    PEER_CONNECTIONS_TOTAL, PEER_VIDEO_ENABLED, SELF_AUDIO_ENABLED, SELF_VIDEO_ENABLED,
    VIDEO_BITRATE_KBPS, VIDEO_FPS, VIDEO_FRAMES_DROPPED, VIDEO_QUALITY_SCORE,
    WEBSOCKET_DROPS_TOTAL,
};

async fn metrics_handler(
    data: web::Data<HealthDataStore>,
    session_tracker: web::Data<SessionTracker>,
    display_name_map: web::Data<DisplayNameMap>,
) -> Result<HttpResponse> {
    drop(data.lock().unwrap_or_else(|e| e.into_inner()));

    // Clean up stale sessions before processing metrics
    cleanup_stale_sessions(&session_tracker, &display_name_map);

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

/// Clean up sessions that haven't reported in the last 30 seconds,
/// and prune display_name_map entries for sessions no longer active.
fn cleanup_stale_sessions(session_tracker: &SessionTracker, display_name_map: &DisplayNameMap) {
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

    let mut removed_session_ids = Vec::new();
    for key in to_remove {
        if let Some(session_info) = tracker.remove(&key) {
            info!(
                "Cleaning up stale session: {} (meeting: {}, peer: {})",
                session_info.session_id, session_info.meeting_id, session_info.reporting_user_id
            );
            removed_session_ids.push(session_info.session_id.clone());

            // Remove all metrics for this session
            remove_session_metrics(&session_info);
        }
    }

    // Prune display_name_map for removed sessions
    if !removed_session_ids.is_empty() {
        let active_session_ids: HashSet<&str> = tracker
            .values()
            .map(|info| info.session_id.as_str())
            .collect();
        let mut dn_map = display_name_map.lock().unwrap_or_else(|e| e.into_inner());
        dn_map.retain(|sid, _| active_session_ids.contains(sid.as_str()));
    }
}

/// Remove all Prometheus metrics for a given session
fn remove_session_metrics(session_info: &SessionInfo) {
    // Remove series for this session using precise label combinations
    let _ = ACTIVE_SESSIONS_TOTAL
        .remove_label_values(&[&session_info.meeting_id, &session_info.session_id]);

    // Remove self-reported enabled metrics for the reporting peer in this meeting
    let _ = SELF_AUDIO_ENABLED.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.reporting_user_id,
        &session_info.display_name,
    ]);
    let _ = SELF_VIDEO_ENABLED.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.reporting_user_id,
        &session_info.display_name,
    ]);

    // Remove tab visibility, throttled, memory, send queue, packet rate metrics
    let _ = CLIENT_TAB_VISIBLE.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
        &session_info.display_name,
    ]);
    let _ = CLIENT_MEMORY_USED_BYTES.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
        &session_info.display_name,
    ]);
    let _ = CLIENT_MEMORY_TOTAL_BYTES.remove_label_values(&[
        &session_info.meeting_id,
        &session_info.session_id,
        &session_info.reporting_user_id,
        &session_info.display_name,
    ]);

    // Remove send queue, packet rates, tab throttled, and receiver-side metrics
    let reporter_labels = [
        &session_info.meeting_id as &str,
        &session_info.session_id,
        &session_info.reporting_user_id,
        &session_info.display_name,
    ];
    let _ = CLIENT_SEND_QUEUE_BYTES.remove_label_values(&reporter_labels);
    let _ = CLIENT_PACKETS_RECEIVED_PER_SEC.remove_label_values(&reporter_labels);
    let _ = CLIENT_PACKETS_SENT_PER_SEC.remove_label_values(&reporter_labels);
    let _ = CLIENT_TAB_THROTTLED.remove_label_values(&reporter_labels);
    let _ = ADAPTIVE_VIDEO_TIER.remove_label_values(&reporter_labels);
    let _ = ADAPTIVE_AUDIO_TIER.remove_label_values(&reporter_labels);
    let _ = DATAGRAM_DROPS_TOTAL.remove_label_values(&reporter_labels);
    let _ = WEBSOCKET_DROPS_TOTAL.remove_label_values(&reporter_labels);
    let _ = KEYFRAME_REQUESTS_SENT_TOTAL.remove_label_values(&reporter_labels);

    // Remove active server metrics for this session
    for (server_url, server_type) in &session_info.active_servers {
        let server_labels = [
            &session_info.meeting_id,
            &session_info.session_id,
            &session_info.reporting_user_id,
            server_url.as_str(),
            server_type.as_str(),
            &session_info.display_name,
        ];
        let _ = CLIENT_ACTIVE_SERVER.remove_label_values(&server_labels);
        let _ = CLIENT_ACTIVE_SERVER_RTT_MS.remove_label_values(&server_labels);
    }

    // Remove all peer connection series we set
    for peer_id in &session_info.peer_ids {
        let _ = PEER_CONNECTIONS_TOTAL.remove_label_values(&[&session_info.meeting_id, peer_id]);
    }

    // Remove all to_peer series we set for this session
    for to_peer in &session_info.to_peers {
        let peer_dn = session_info
            .to_peer_display_names
            .get(to_peer)
            .map(|s| s.as_str())
            .unwrap_or("");

        remove_per_peer_metrics(
            &session_info.meeting_id,
            &session_info.session_id,
            &session_info.reporting_user_id,
            to_peer,
            &session_info.display_name,
            peer_dn,
        );
    }

    // Meeting participants is recomputed on next scrape; no need to force remove
    debug!(
        "Removed all series for session {} (meeting: {}, peer: {})",
        session_info.session_id, session_info.meeting_id, session_info.reporting_user_id
    );
}

/// Remove all per-peer Prometheus metrics for a specific reporter→peer pair.
/// Used both for session cleanup and for removing stale series when a peer's
/// display_name changes (e.g., from session_id to real name).
fn remove_per_peer_metrics(
    meeting_id: &str,
    session_id: &str,
    reporting_user_id: &str,
    to_peer: &str,
    reporter_display_name: &str,
    peer_display_name: &str,
) {
    let labels = [
        meeting_id,
        session_id,
        reporting_user_id,
        to_peer,
        reporter_display_name,
        peer_display_name,
    ];

    // Per-peer metrics (18 kept, 7 low-value ones removed for cardinality reduction)
    let _ = PEER_CAN_LISTEN.remove_label_values(&labels);
    let _ = PEER_CAN_SEE.remove_label_values(&labels);
    let _ = NETEQ_AUDIO_BUFFER_MS.remove_label_values(&labels);
    let _ = NETEQ_TARGET_DELAY_MS.remove_label_values(&labels);
    let _ = NETEQ_PACKETS_AWAITING_DECODE.remove_label_values(&labels);
    let _ = NETEQ_PACKETS_PER_SEC.remove_label_values(&labels);
    let _ = NETEQ_NORMAL_OPS_PER_SEC.remove_label_values(&labels);
    let _ = NETEQ_EXPAND_OPS_PER_SEC.remove_label_values(&labels);
    let _ = NETEQ_ACCELERATE_OPS_PER_SEC.remove_label_values(&labels);
    let _ = VIDEO_FPS.remove_label_values(&labels);
    let _ = VIDEO_BITRATE_KBPS.remove_label_values(&labels);
    let _ = VIDEO_FRAMES_DROPPED.remove_label_values(&labels);
    let _ = AUDIO_PACKET_LOSS_PCT.remove_label_values(&labels);
    let _ = PEER_AUDIO_ENABLED.remove_label_values(&labels);
    let _ = PEER_VIDEO_ENABLED.remove_label_values(&labels);
    let _ = AUDIO_QUALITY_SCORE.remove_label_values(&labels);
    let _ = VIDEO_QUALITY_SCORE.remove_label_values(&labels);
    let _ = CALL_QUALITY_SCORE.remove_label_values(&labels);
}

fn process_health_packet_to_metrics_pb(
    health_packet: &PbHealthPacket,
    session_tracker: &SessionTracker,
    display_name_map: &DisplayNameMap,
) -> anyhow::Result<()> {
    HEALTH_REPORTS_TOTAL.inc();

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

    // Extract reporter's display name; fall back to email if absent
    let reporter_display_name = health_packet
        .display_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(reporting_user_id)
        .to_string();

    // Register this session's display name so peers can be resolved later
    {
        let mut dn_map = display_name_map.lock().unwrap_or_else(|e| e.into_inner());
        dn_map.insert(session_id.to_string(), reporter_display_name.clone());
    }

    // Update session tracker: create entry on first packet, then refresh last_seen only.
    // Using entry().or_insert_with() preserves accumulated to_peers/peer_ids/active_servers
    // across packets. The previous tracker.insert() reset them every packet, causing a leak
    // where peers that left mid-session had their Prometheus labels written but never cleaned up.
    {
        let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
        let session_key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
        let info = tracker.entry(session_key).or_insert_with(|| SessionInfo {
            session_id: session_id.to_string(),
            meeting_id: meeting_id.to_string(),
            reporting_user_id: reporting_user_id.to_string(),
            display_name: reporter_display_name.clone(),
            last_seen: Instant::now(),
            to_peers: HashSet::new(),
            to_peer_display_names: HashMap::new(),
            peer_ids: HashSet::new(),
            active_servers: HashSet::new(),
        });
        info.last_seen = Instant::now();
        info.display_name = reporter_display_name.clone();
    }

    // Process metrics for this session
    {
        // Client-side active server info (optional)
        if !health_packet.active_server_url.is_empty() {
            // Strip JWT token from URL to prevent leaking credentials in Prometheus labels.
            // Handles both ?token=... (only param) and &token=... (among other params).
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

            let server_type = if health_packet.active_server_type.is_empty() {
                "unknown"
            } else {
                &health_packet.active_server_type
            };

            CLIENT_ACTIVE_SERVER
                .with_label_values(&[
                    meeting_id,
                    session_id,
                    reporting_user_id,
                    server_url_clean,
                    server_type,
                    reporter_display_name.as_str(),
                ])
                .set(1.0);

            if health_packet.active_server_rtt_ms != 0.0 {
                CLIENT_ACTIVE_SERVER_RTT_MS
                    .with_label_values(&[
                        meeting_id,
                        session_id,
                        reporting_user_id,
                        server_url_clean,
                        server_type,
                        reporter_display_name.as_str(),
                    ])
                    .set(health_packet.active_server_rtt_ms);
            }

            // Track server info used for cleanup
            {
                let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                let key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
                if let Some(info) = tracker.get_mut(&key) {
                    info.active_servers
                        .insert((server_url_clean.to_string(), server_type.to_string()));
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
            .with_label_values(&[
                meeting_id,
                reporting_user_id,
                reporter_display_name.as_str(),
            ])
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
            .with_label_values(&[
                meeting_id,
                reporting_user_id,
                reporter_display_name.as_str(),
            ])
            .set(if health_packet.reporting_video_enabled {
                1.0
            } else {
                0.0
            });

        // Tab visibility (HealthPacket level)
        debug!(
            "Setting CLIENT_TAB_VISIBLE for meeting={}, session={}, peer={}, value={}",
            meeting_id, session_id, reporting_user_id, health_packet.is_tab_visible
        );
        CLIENT_TAB_VISIBLE
            .with_label_values(&[
                meeting_id,
                session_id,
                reporting_user_id,
                reporter_display_name.as_str(),
            ])
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
                .with_label_values(&[
                    meeting_id,
                    session_id,
                    reporting_user_id,
                    reporter_display_name.as_str(),
                ])
                .set(mem_used as f64);
        }

        if let Some(mem_total) = health_packet.memory_total_bytes {
            debug!(
                "Setting CLIENT_MEMORY_TOTAL_BYTES for meeting={}, session={}, peer={}, value={} bytes",
                meeting_id, session_id, reporting_user_id, mem_total
            );
            CLIENT_MEMORY_TOTAL_BYTES
                .with_label_values(&[
                    meeting_id,
                    session_id,
                    reporting_user_id,
                    reporter_display_name.as_str(),
                ])
                .set(mem_total as f64);
        }

        // Communication and browser state metrics
        let reporter_labels: [&str; 4] = [
            meeting_id,
            session_id,
            reporting_user_id,
            reporter_display_name.as_str(),
        ];

        if let Some(send_queue) = health_packet.send_queue_bytes {
            CLIENT_SEND_QUEUE_BYTES
                .with_label_values(&reporter_labels)
                .set(send_queue as f64);
        }

        if let Some(rx_pps) = health_packet.packets_received_per_sec {
            CLIENT_PACKETS_RECEIVED_PER_SEC
                .with_label_values(&reporter_labels)
                .set(rx_pps);
        }

        if let Some(tx_pps) = health_packet.packets_sent_per_sec {
            CLIENT_PACKETS_SENT_PER_SEC
                .with_label_values(&reporter_labels)
                .set(tx_pps);
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
            DATAGRAM_DROPS_TOTAL
                .with_label_values(&reporter_labels)
                .set(drops as f64);
        }
        if let Some(drops) = health_packet.websocket_drops_total {
            WEBSOCKET_DROPS_TOTAL
                .with_label_values(&reporter_labels)
                .set(drops as f64);
        }
        if let Some(kf_reqs) = health_packet.keyframe_requests_sent_total {
            KEYFRAME_REQUESTS_SENT_TOTAL
                .with_label_values(&reporter_labels)
                .set(kf_reqs as f64);
        }

        // Process peer health data
        if !health_packet.peer_stats.is_empty() {
            // Snapshot display_name_map once (avoids locking per peer in the loop)
            let peer_display_names: HashMap<String, String> = {
                let dn_map = display_name_map.lock().unwrap_or_else(|e| e.into_inner());
                health_packet
                    .peer_stats
                    .keys()
                    .map(|pid| {
                        let dn = dn_map.get(pid).cloned().unwrap_or_else(|| pid.to_string());
                        (pid.clone(), dn)
                    })
                    .collect()
            };

            // Detect display_name changes and remove stale series (one tracker lock)
            {
                let mut tracker = session_tracker.lock().unwrap_or_else(|e| e.into_inner());
                let key = format!("{meeting_id}_{session_id}_{reporting_user_id}");
                if let Some(info) = tracker.get_mut(&key) {
                    for (peer_id, new_dn) in &peer_display_names {
                        if let Some(old_dn) = info.to_peer_display_names.get(peer_id) {
                            if old_dn != new_dn {
                                debug!(
                                    "Peer display name changed: {} -> {} for peer {}",
                                    old_dn, new_dn, peer_id
                                );
                                remove_per_peer_metrics(
                                    meeting_id,
                                    session_id,
                                    reporting_user_id,
                                    peer_id,
                                    reporter_display_name.as_str(),
                                    old_dn,
                                );
                            }
                        }
                        info.to_peer_display_names
                            .insert(peer_id.clone(), new_dn.clone());
                        info.to_peers.insert(peer_id.clone());
                        info.peer_ids.insert(peer_id.clone());
                    }
                }
            }

            let participants_count = health_packet.peer_stats.len();

            for (peer_id, peer_data) in &health_packet.peer_stats {
                let peer_dn = &peer_display_names[peer_id];
                let peer_labels: [&str; 6] = [
                    meeting_id,
                    session_id,
                    reporting_user_id,
                    peer_id,
                    reporter_display_name.as_str(),
                    peer_dn.as_str(),
                ];

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
                    if neteq_stats.current_buffer_size_ms != 0.0 {
                        NETEQ_AUDIO_BUFFER_MS
                            .with_label_values(&peer_labels)
                            .set(neteq_stats.current_buffer_size_ms);
                    }

                    NETEQ_TARGET_DELAY_MS
                        .with_label_values(&peer_labels)
                        .set(neteq_stats.target_delay_ms);

                    if neteq_stats.packets_awaiting_decode != 0.0 {
                        NETEQ_PACKETS_AWAITING_DECODE
                            .with_label_values(&peer_labels)
                            .set(neteq_stats.packets_awaiting_decode);
                    }

                    if neteq_stats.packets_per_sec != 0.0 {
                        NETEQ_PACKETS_PER_SEC
                            .with_label_values(&peer_labels)
                            .set(neteq_stats.packets_per_sec);
                    }

                    // Core NetEQ operation counters (high diagnostic value only)
                    if let Some(network) = neteq_stats.network.as_ref() {
                        if let Some(ops) = network.operation_counters.as_ref() {
                            NETEQ_NORMAL_OPS_PER_SEC
                                .with_label_values(&peer_labels)
                                .set(ops.normal_per_sec);
                            NETEQ_EXPAND_OPS_PER_SEC
                                .with_label_values(&peer_labels)
                                .set(ops.expand_per_sec);
                            NETEQ_ACCELERATE_OPS_PER_SEC
                                .with_label_values(&peer_labels)
                                .set(ops.accelerate_per_sec);
                        }
                    }
                }

                // Video metrics
                if let Some(video_stats) = peer_data.video_stats.as_ref() {
                    if video_stats.fps_received != 0.0 {
                        VIDEO_FPS
                            .with_label_values(&peer_labels)
                            .set(video_stats.fps_received);
                    }
                    if video_stats.bitrate_kbps != 0 {
                        VIDEO_BITRATE_KBPS
                            .with_label_values(&peer_labels)
                            .set(video_stats.bitrate_kbps as f64);
                    }
                }

                // Decode errors
                if peer_data.frames_dropped_per_sec > 0.0 {
                    VIDEO_FRAMES_DROPPED
                        .with_label_values(&peer_labels)
                        .set(peer_data.frames_dropped_per_sec);
                }

                // Audio packet loss
                if peer_data.audio_packet_loss_pct > 0.0 {
                    AUDIO_PACKET_LOSS_PCT
                        .with_label_values(&peer_labels)
                        .set(peer_data.audio_packet_loss_pct);
                }

                // Quality scores
                if let Some(score) = peer_data.audio_quality_score {
                    AUDIO_QUALITY_SCORE
                        .with_label_values(&peer_labels)
                        .set(score);
                }
                if let Some(score) = peer_data.video_quality_score {
                    VIDEO_QUALITY_SCORE
                        .with_label_values(&peer_labels)
                        .set(score);
                }
                if let Some(score) = peer_data.call_quality_score {
                    CALL_QUALITY_SCORE
                        .with_label_values(&peer_labels)
                        .set(score);
                }

                // Peer status flags
                PEER_AUDIO_ENABLED
                    .with_label_values(&peer_labels)
                    .set(if peer_data.audio_enabled { 1.0 } else { 0.0 });
                PEER_VIDEO_ENABLED
                    .with_label_values(&peer_labels)
                    .set(if peer_data.video_enabled { 1.0 } else { 0.0 });
            }

            // Update meeting participants count (peers + self)
            MEETING_PARTICIPANTS
                .with_label_values(&[meeting_id])
                .set((participants_count + 1) as f64);
        }
    }

    Ok(())
}

async fn nats_health_consumer(
    nats_client: Client,
    health_store: HealthDataStore,
    session_tracker: SessionTracker,
    display_name_map: DisplayNameMap,
) -> anyhow::Result<()> {
    // Subscribe to all health diagnostics topics from all regions
    let queue_group = "metrics-server-health-diagnostics";
    let mut subscription = nats_client
        .queue_subscribe("health.diagnostics.>", queue_group.to_string())
        .await?;

    info!("Subscribed to NATS topic: health.diagnostics.>");

    while let Some(message) = subscription.next().await {
        debug!("Received health message from NATS: {}", message.subject);
        if let Err(e) =
            handle_health_message(message, &health_store, &session_tracker, &display_name_map).await
        {
            error!("Failed to handle health message: {}", e);
        }
    }

    Ok(())
}

async fn handle_health_message(
    message: Message,
    health_store: &HealthDataStore,
    session_tracker: &SessionTracker,
    display_name_map: &DisplayNameMap,
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
        if let Err(e) =
            process_health_packet_to_metrics_pb(&health_packet, session_tracker, display_name_map)
        {
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

    // Create shared display name map (session_id → display_name)
    let display_name_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

    // Start NATS consumer in background
    let nats_store = health_store.clone();
    let nats_client_clone = nats_client.clone();
    let nats_tracker = session_tracker.clone();
    let nats_dn_map = display_name_map.clone();
    task::spawn(async move {
        if let Err(e) =
            nats_health_consumer(nats_client_clone, nats_store, nats_tracker, nats_dn_map).await
        {
            error!("NATS consumer failed: {}", e);
        }
    });

    // Start HTTP server
    info!("Starting HTTP server on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(health_store.clone()))
            .app_data(web::Data::new(session_tracker.clone()))
            .app_data(web::Data::new(display_name_map.clone()))
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
        PeerStats as PbPeerStats, VideoStats as PbVideoStats,
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
        let result = process_health_packet_to_metrics_pb(
            &hp,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
            to_peer_display_names: HashMap::new(),
            active_servers: HashSet::new(),
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
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
        cleanup_stale_sessions(&tracker, &Arc::new(Mutex::new(HashMap::new())));

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
        let result = process_health_packet_to_metrics_pb(
            &health_packet,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
        let result = process_health_packet_to_metrics_pb(
            &hp,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
        let result = process_health_packet_to_metrics_pb(
            &hp,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
                let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));
                let resp = metrics_handler(
                    web::Data::new(health_store),
                    web::Data::new(tracker_clone),
                    web::Data::new(dn_map),
                )
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
        let packet =
            create_test_health_packet(session_id, meeting_id, reporting_user_id, peer_stats);
        let result = process_health_packet_to_metrics_pb(
            &packet,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
        let result = process_health_packet_to_metrics_pb(
            &health_packet,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
        let result = process_health_packet_to_metrics_pb(
            &hp,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
            };
            tracker_guard.insert(session_key3, session_info3);
        }

        // Verify initial state
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tracker_guard.len(), 3);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker, &Arc::new(Mutex::new(HashMap::new())));

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
            to_peer_display_names: HashMap::new(),
            active_servers: HashSet::new(),
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
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
        let result = process_health_packet_to_metrics_pb(
            &health_packet,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
        let result = process_health_packet_to_metrics_pb(
            &hp,
            &tracker,
            &Arc::new(Mutex::new(HashMap::new())),
        );
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
                to_peer_display_names: HashMap::new(),
                active_servers: HashSet::new(),
            };
            // Set to exactly 30 seconds ago (timeout boundary)
            session_info.last_seen -= Duration::from_secs(30);
            tracker_guard.insert(session_key, session_info);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker, &Arc::new(Mutex::new(HashMap::new())));

        // Session should be cleaned up (>= 30 seconds is considered stale)
        {
            let tracker_guard = tracker.lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(tracker_guard.len(), 0);
        }
    }

    #[test]
    fn test_jwt_token_stripped_from_server_url() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

        // URL with ?token= (only query param)
        let mut hp = create_test_health_packet("s1", "m1", "alice", HashMap::new());
        hp.active_server_url = "wss://relay.example.com?token=eyJhbGciOi.secret".to_string();
        hp.active_server_type = "websocket".to_string();
        hp.active_server_rtt_ms = 50.0;
        // Add a peer so the packet isn't empty
        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        hp.peer_stats.insert(peer_id, ps);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker, &dn_map);
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
        let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

        // URL with token among other query params
        let mut hp = create_test_health_packet("s2", "m2", "carol", HashMap::new());
        hp.active_server_url =
            "wss://relay.example.com?region=us-east&token=secret123&debug=1".to_string();
        hp.active_server_type = "webtransport".to_string();
        hp.active_server_rtt_ms = 30.0;
        let (peer_id, ps) = create_test_peer_stats("dave", true, true, 50.0, 2.0);
        hp.peer_stats.insert(peer_id, ps);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker, &dn_map);
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

    #[test]
    fn test_display_name_resolution_removes_stale_series() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

        // First packet: reporter alice sees peer "12345" (no display name yet)
        let (peer_id, ps) = create_test_peer_stats("12345", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s10", "m10", "alice", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);
        hp.display_name = Some("Alice".to_string());
        let result = process_health_packet_to_metrics_pb(&hp, &tracker, &dn_map);
        assert!(result.is_ok());

        // Verify series exists with session_id as peer_name
        assert!(
            series_exists(
                "videocall_peer_can_listen",
                &[
                    ("meeting_id", "m10"),
                    ("to_peer", "12345"),
                    ("peer_name", "12345")
                ]
            ),
            "Should have series with session_id as peer_name"
        );

        // Now the peer sends their own health packet, populating the display_name_map
        {
            let mut map = dn_map.lock().unwrap_or_else(|e| e.into_inner());
            map.insert("12345".to_string(), "Bob".to_string());
        }

        // Second packet from alice: now "12345" resolves to "Bob"
        let (peer_id2, ps2) = create_test_peer_stats("12345", true, true, 50.0, 2.0);
        let mut hp2 = create_test_health_packet("s10", "m10", "alice", HashMap::new());
        hp2.peer_stats.insert(peer_id2, ps2);
        hp2.display_name = Some("Alice".to_string());
        let result2 = process_health_packet_to_metrics_pb(&hp2, &tracker, &dn_map);
        assert!(result2.is_ok());

        // Old series with session_id as peer_name should be removed
        assert!(
            !series_exists(
                "videocall_peer_can_listen",
                &[
                    ("meeting_id", "m10"),
                    ("to_peer", "12345"),
                    ("peer_name", "12345")
                ]
            ),
            "Stale series with session_id as peer_name should be removed"
        );

        // New series with resolved display_name should exist
        assert!(
            series_exists(
                "videocall_peer_can_listen",
                &[
                    ("meeting_id", "m10"),
                    ("to_peer", "12345"),
                    ("peer_name", "Bob")
                ]
            ),
            "New series with resolved display_name should exist"
        );
    }

    #[test]
    fn test_meeting_participants_includes_self() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

        // Reporter with 2 peers = 3 total participants
        let (p1, ps1) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let (p2, ps2) = create_test_peer_stats("carol", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s20", "m20", "alice", HashMap::new());
        hp.peer_stats.insert(p1, ps1);
        hp.peer_stats.insert(p2, ps2);

        let result = process_health_packet_to_metrics_pb(&hp, &tracker, &dn_map);
        assert!(result.is_ok());

        // Check the gauge value is 3 (2 peers + 1 self)
        let families = prometheus::gather();
        for family in &families {
            if family.get_name() == "videocall_meeting_participants" {
                for metric in family.get_metric() {
                    for label in metric.get_label() {
                        if label.get_name() == "meeting_id" && label.get_value() == "m20" {
                            assert_eq!(
                                metric.get_gauge().get_value(),
                                3.0,
                                "participants should be peers + 1 (self)"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_p1_metrics_exposed() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s30", "m30", "alice", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);
        hp.send_queue_bytes = Some(1024);
        hp.packets_received_per_sec = Some(50.0);
        hp.packets_sent_per_sec = Some(45.0);
        hp.is_tab_throttled = true;

        let result = process_health_packet_to_metrics_pb(&hp, &tracker, &dn_map);
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

    #[test]
    fn test_health_reports_counter_incremented() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

        let before = HEALTH_REPORTS_TOTAL.get();

        let (peer_id, ps) = create_test_peer_stats("bob", true, true, 50.0, 2.0);
        let mut hp = create_test_health_packet("s40", "m40", "alice", HashMap::new());
        hp.peer_stats.insert(peer_id, ps);

        let _ = process_health_packet_to_metrics_pb(&hp, &tracker, &dn_map);

        let after = HEALTH_REPORTS_TOTAL.get();
        assert!(
            after > before,
            "HEALTH_REPORTS_TOTAL should be incremented on each health packet"
        );
    }

    #[test]
    fn test_display_name_map_cleanup() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let dn_map: DisplayNameMap = Arc::new(Mutex::new(HashMap::new()));

        // Populate the display_name_map with some entries
        {
            let mut map = dn_map.lock().unwrap_or_else(|e| e.into_inner());
            map.insert("active_session".to_string(), "Alice".to_string());
            map.insert("stale_session".to_string(), "Bob".to_string());
        }

        // Only add active_session to the tracker
        {
            let mut t = tracker.lock().unwrap_or_else(|e| e.into_inner());
            t.insert(
                "m1_active_session_alice".to_string(),
                SessionInfo {
                    session_id: "active_session".to_string(),
                    meeting_id: "m1".to_string(),
                    reporting_user_id: "alice".to_string(),
                    display_name: "Alice".to_string(),
                    last_seen: Instant::now() - Duration::from_secs(60), // stale
                    to_peers: HashSet::new(),
                    to_peer_display_names: HashMap::new(),
                    peer_ids: HashSet::new(),
                    active_servers: HashSet::new(),
                },
            );
        }

        // Run cleanup — both sessions should be removed (active_session is stale too)
        cleanup_stale_sessions(&tracker, &dn_map);

        let map = dn_map.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            map.is_empty(),
            "All display_name_map entries should be cleaned since all sessions are stale"
        );
    }
}
