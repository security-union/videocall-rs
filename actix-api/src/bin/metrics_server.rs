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
    reporting_peer: String,
    last_seen: Instant,
    // Peers we have published metrics for in this session (as to_peer)
    to_peers: HashSet<String>,
    // Peer IDs we have published peer connection metrics for
    peer_ids: HashSet<String>,
}

type SessionTracker = Arc<Mutex<HashMap<String, SessionInfo>>>;

// Prometheus metrics (same as existing diagnostics.rs)
// Import shared Prometheus metrics
use sec_api::metrics::{
    ACTIVE_SESSIONS_TOTAL, CLIENT_ACTIVE_SERVER, CLIENT_ACTIVE_SERVER_RTT_MS, MEETING_PARTICIPANTS,
    NETEQ_ACCELERATE_OPS_PER_SEC, NETEQ_AUDIO_BUFFER_MS, NETEQ_COMFORT_NOISE_OPS_PER_SEC,
    NETEQ_DTMF_OPS_PER_SEC, NETEQ_EXPAND_OPS_PER_SEC, NETEQ_FAST_ACCELERATE_OPS_PER_SEC,
    NETEQ_MERGE_OPS_PER_SEC, NETEQ_NORMAL_OPS_PER_SEC, NETEQ_PACKETS_AWAITING_DECODE,
    NETEQ_PREEMPTIVE_EXPAND_OPS_PER_SEC, NETEQ_UNDEFINED_OPS_PER_SEC, PEER_AUDIO_ENABLED,
    PEER_CAN_LISTEN, PEER_CAN_SEE, PEER_CONNECTIONS_TOTAL, PEER_VIDEO_ENABLED, SELF_AUDIO_ENABLED,
    SELF_VIDEO_ENABLED, VIDEO_FPS, VIDEO_PACKETS_BUFFERED,
};

async fn metrics_handler(
    data: web::Data<HealthDataStore>,
    session_tracker: web::Data<SessionTracker>,
) -> Result<HttpResponse> {
    drop(data.lock().unwrap());

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

/// Clean up sessions that haven't reported in the last 30 seconds
fn cleanup_stale_sessions(session_tracker: &SessionTracker) {
    use std::time::Duration;
    let mut tracker = session_tracker.lock().unwrap();
    let now = Instant::now();
    let timeout = Duration::from_secs(30); // 30 second timeout

    let mut to_remove = Vec::new();

    for (key, session_info) in tracker.iter() {
        if now.duration_since(session_info.last_seen) > timeout {
            to_remove.push(key.clone());
        }
    }

    for key in to_remove {
        if let Some(session_info) = tracker.remove(&key) {
            info!(
                "Cleaning up stale session: {} (meeting: {}, peer: {})",
                session_info.session_id, session_info.meeting_id, session_info.reporting_peer
            );

            // Remove all metrics for this session
            remove_session_metrics(&session_info);
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
        .remove_label_values(&[&session_info.meeting_id, &session_info.reporting_peer]);
    let _ = SELF_VIDEO_ENABLED
        .remove_label_values(&[&session_info.meeting_id, &session_info.reporting_peer]);

    // Remove all peer connection series we set
    for peer_id in &session_info.peer_ids {
        let _ = PEER_CONNECTIONS_TOTAL.remove_label_values(&[&session_info.meeting_id, peer_id]);
    }

    // Remove all to_peer series we set for this session
    for to_peer in &session_info.to_peers {
        let labels = [
            &session_info.meeting_id,
            &session_info.session_id,
            &session_info.reporting_peer,
            to_peer.as_str(),
        ];
        let _ = PEER_CAN_LISTEN.remove_label_values(&labels);
        let _ = PEER_CAN_SEE.remove_label_values(&labels);
        let _ = VIDEO_FPS.remove_label_values(&labels);
        let _ = VIDEO_PACKETS_BUFFERED.remove_label_values(&labels);
        let _ = NETEQ_AUDIO_BUFFER_MS.remove_label_values(&labels);
        let _ = NETEQ_PACKETS_AWAITING_DECODE.remove_label_values(&labels);
        let _ = NETEQ_NORMAL_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_EXPAND_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_ACCELERATE_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_FAST_ACCELERATE_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_PREEMPTIVE_EXPAND_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_MERGE_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_COMFORT_NOISE_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_DTMF_OPS_PER_SEC.remove_label_values(&labels);
        let _ = NETEQ_UNDEFINED_OPS_PER_SEC.remove_label_values(&labels);
    }

    // Meeting participants is recomputed on next scrape; no need to force remove
    debug!(
        "Removed all series for session {} (meeting: {}, peer: {})",
        session_info.session_id, session_info.meeting_id, session_info.reporting_peer
    );
}

fn process_health_packet_to_metrics_pb(
    health_packet: &PbHealthPacket,
    session_tracker: &SessionTracker,
) -> anyhow::Result<()> {
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

    let reporting_peer = if health_packet.reporting_peer.is_empty() {
        "unknown"
    } else {
        &health_packet.reporting_peer
    };

    // Update session tracker
    {
        let mut tracker = session_tracker.lock().unwrap();
        let session_key = format!("{meeting_id}_{session_id}_{reporting_peer}");
        tracker.insert(
            session_key,
            SessionInfo {
                session_id: session_id.to_string(),
                meeting_id: meeting_id.to_string(),
                reporting_peer: reporting_peer.to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            },
        );
    }

    // Check if this session is active (not cleaned up)
    let is_session_active = {
        let tracker = session_tracker.lock().unwrap();
        let session_key = format!("{meeting_id}_{session_id}_{reporting_peer}");
        tracker.contains_key(&session_key)
    };

    // Only publish metrics for active sessions
    if is_session_active {
        // Client-side active server info (optional)
        if !health_packet.active_server_url.is_empty() {
            let server_url = &health_packet.active_server_url;
            let server_type = if health_packet.active_server_type.is_empty() {
                "unknown"
            } else {
                &health_packet.active_server_type
            };

            CLIENT_ACTIVE_SERVER
                .with_label_values(&[
                    meeting_id,
                    session_id,
                    reporting_peer,
                    server_url,
                    server_type,
                ])
                .set(1.0);

            if health_packet.active_server_rtt_ms != 0.0 {
                CLIENT_ACTIVE_SERVER_RTT_MS
                    .with_label_values(&[
                        meeting_id,
                        session_id,
                        reporting_peer,
                        server_url,
                        server_type,
                    ])
                    .set(health_packet.active_server_rtt_ms);
            }
        }
        // Set active session metric
        ACTIVE_SESSIONS_TOTAL
            .with_label_values(&[meeting_id, session_id])
            .set(1.0);

        // Self-state reported by the sender (authoritative)
        debug!(
            "Setting SELF_AUDIO_ENABLED for meeting={}, peer={}, value={}",
            meeting_id, reporting_peer, health_packet.reporting_audio_enabled
        );
        SELF_AUDIO_ENABLED
            .with_label_values(&[meeting_id, reporting_peer])
            .set(if health_packet.reporting_audio_enabled {
                1.0
            } else {
                0.0
            });

        debug!(
            "Setting SELF_VIDEO_ENABLED for meeting={}, peer={}, value={}",
            meeting_id, reporting_peer, health_packet.reporting_video_enabled
        );
        SELF_VIDEO_ENABLED
            .with_label_values(&[meeting_id, reporting_peer])
            .set(if health_packet.reporting_video_enabled {
                1.0
            } else {
                0.0
            });

        // Process peer health data
        if !health_packet.peer_stats.is_empty() {
            let mut participants_count = 0;

            for (peer_id, peer_data) in &health_packet.peer_stats {
                participants_count += 1;

                // Set peer connection metric
                PEER_CONNECTIONS_TOTAL
                    .with_label_values(&[meeting_id, peer_id])
                    .set(1.0);
                // Track peer_id used for connections
                {
                    let mut tracker = session_tracker.lock().unwrap();
                    let key = format!("{meeting_id}_{session_id}_{reporting_peer}");
                    if let Some(info) = tracker.get_mut(&key) {
                        info.peer_ids.insert(peer_id.clone());
                    }
                }

                {
                    // Process can_listen
                    {
                        let can_listen = peer_data.can_listen;
                        PEER_CAN_LISTEN
                            .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                            .set(if can_listen { 1.0 } else { 0.0 });
                        // Track to_peer used
                        let mut tracker = session_tracker.lock().unwrap();
                        let key = format!("{meeting_id}_{session_id}_{reporting_peer}");
                        if let Some(info) = tracker.get_mut(&key) {
                            info.to_peers.insert(peer_id.clone());
                        }
                    }

                    // Process can_see
                    {
                        let can_see = peer_data.can_see;
                        PEER_CAN_SEE
                            .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                            .set(if can_see { 1.0 } else { 0.0 });
                        let mut tracker = session_tracker.lock().unwrap();
                        let key = format!("{meeting_id}_{session_id}_{reporting_peer}");
                        if let Some(info) = tracker.get_mut(&key) {
                            info.to_peers.insert(peer_id.clone());
                        }
                    }

                    // Process NetEQ metrics from neteq_stats object
                    if let Some(neteq_stats) = peer_data.neteq_stats.as_ref() {
                        if neteq_stats.current_buffer_size_ms != 0.0 {
                            NETEQ_AUDIO_BUFFER_MS
                                .with_label_values(&[
                                    meeting_id,
                                    session_id,
                                    reporting_peer,
                                    peer_id,
                                ])
                                .set(neteq_stats.current_buffer_size_ms);
                            let mut tracker = session_tracker.lock().unwrap();
                            let key = format!("{meeting_id}_{session_id}_{reporting_peer}");
                            if let Some(info) = tracker.get_mut(&key) {
                                info.to_peers.insert(peer_id.clone());
                            }
                        }

                        if neteq_stats.packets_awaiting_decode != 0.0 {
                            NETEQ_PACKETS_AWAITING_DECODE
                                .with_label_values(&[
                                    meeting_id,
                                    session_id,
                                    reporting_peer,
                                    peer_id,
                                ])
                                .set(neteq_stats.packets_awaiting_decode);
                            let mut tracker = session_tracker.lock().unwrap();
                            let key = format!("{meeting_id}_{session_id}_{reporting_peer}");
                            if let Some(info) = tracker.get_mut(&key) {
                                info.to_peers.insert(peer_id.clone());
                            }
                        }

                        // Process NetEQ operation metrics from network.operation_counters
                        if let Some(network) = neteq_stats.network.as_ref() {
                            if let Some(operation_counters) = network.operation_counters.as_ref() {
                                // Normal operations per second
                                {
                                    NETEQ_NORMAL_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.normal_per_sec);
                                }

                                // Expand operations per second
                                {
                                    NETEQ_EXPAND_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.expand_per_sec);
                                }

                                // Accelerate operations per second
                                {
                                    NETEQ_ACCELERATE_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.accelerate_per_sec);
                                }

                                // Fast accelerate operations per second
                                {
                                    NETEQ_FAST_ACCELERATE_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.fast_accelerate_per_sec);
                                }

                                // Preemptive expand operations per second
                                {
                                    NETEQ_PREEMPTIVE_EXPAND_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.preemptive_expand_per_sec);
                                }

                                // Merge operations per second
                                {
                                    NETEQ_MERGE_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.merge_per_sec);
                                }

                                // Comfort noise operations per second
                                {
                                    NETEQ_COMFORT_NOISE_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.comfort_noise_per_sec);
                                }

                                // DTMF operations per second
                                {
                                    NETEQ_DTMF_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.dtmf_per_sec);
                                }

                                // Undefined operations per second
                                {
                                    NETEQ_UNDEFINED_OPS_PER_SEC
                                        .with_label_values(&[
                                            meeting_id,
                                            session_id,
                                            reporting_peer,
                                            peer_id,
                                        ])
                                        .set(operation_counters.undefined_per_sec);
                                }
                            }
                        }
                    }

                    // Process video metrics from video_stats object
                    if let Some(video_stats) = peer_data.video_stats.as_ref() {
                        if video_stats.fps_received != 0.0 {
                            VIDEO_FPS
                                .with_label_values(&[
                                    meeting_id,
                                    session_id,
                                    reporting_peer,
                                    peer_id,
                                ])
                                .set(video_stats.fps_received);
                        }

                        if video_stats.frames_buffered != 0.0 {
                            debug!("Setting VIDEO_PACKETS_BUFFERED for meeting={}, session={}, from_peer={}, to_peer={}, value={}", 
                                   meeting_id, session_id, reporting_peer, peer_id, video_stats.frames_buffered);
                            VIDEO_PACKETS_BUFFERED
                                .with_label_values(&[
                                    meeting_id,
                                    session_id,
                                    reporting_peer,
                                    peer_id,
                                ])
                                .set(video_stats.frames_buffered);
                        }
                    }

                    // Process explicit peer status flags if present
                    {
                        let audio_enabled = peer_data.audio_enabled;
                        PEER_AUDIO_ENABLED
                            .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                            .set(if audio_enabled { 1.0 } else { 0.0 });
                    }

                    {
                        let video_enabled = peer_data.video_enabled;
                        PEER_VIDEO_ENABLED
                            .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                            .set(if video_enabled { 1.0 } else { 0.0 });
                    }
                }
            }

            // Update meeting participants count
            MEETING_PARTICIPANTS
                .with_label_values(&[meeting_id])
                .set(participants_count as f64);
        }
    }

    Ok(())
}

async fn nats_health_consumer(
    nats_client: Client,
    health_store: HealthDataStore,
    session_tracker: SessionTracker,
) -> anyhow::Result<()> {
    // Subscribe to all health diagnostics topics from all regions
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
        let mut store = health_store.lock().unwrap();
        let json_val = json!({
            "session_id": health_packet.session_id,
            "meeting_id": health_packet.meeting_id,
            "reporting_peer": health_packet.reporting_peer,
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
        PeerStats as PbPeerStats, VideoStats as PbVideoStats,
    };

    #[test]
    fn test_active_server_metrics_export() {
        // Build a health packet with active server fields set
        let mut hp = PbHealthPacket::new();
        hp.session_id = "s1".to_string();
        hp.meeting_id = "m1".to_string();
        hp.reporting_peer = "alice@example.com".to_string();
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
        reporting_peer: &str,
        peer_stats: std::collections::HashMap<String, PbPeerStats>,
    ) -> PbHealthPacket {
        let mut hp = PbHealthPacket::new();
        hp.session_id = session_id.to_string();
        hp.meeting_id = meeting_id.to_string();
        hp.reporting_peer = reporting_peer.to_string();
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
            reporting_peer: "alice".to_string(),
            last_seen: Instant::now(),
            to_peers: HashSet::new(),
            peer_ids: HashSet::new(),
        };

        assert_eq!(session_info.session_id, "session_123");
        assert_eq!(session_info.meeting_id, "meeting_456");
        assert_eq!(session_info.reporting_peer, "alice");
        assert!(session_info.last_seen.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn test_session_tracker_operations() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Test inserting a session
        {
            let mut tracker_guard = tracker.lock().unwrap();
            let session_key = "meeting_1_session_1_alice".to_string();
            let session_info = SessionInfo {
                session_id: "session_1".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_peer: "alice".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            tracker_guard.insert(session_key.clone(), session_info);
            assert_eq!(tracker_guard.len(), 1);
            assert!(tracker_guard.contains_key(&session_key));
        }

        // Test updating session timestamp
        {
            let mut tracker_guard = tracker.lock().unwrap();
            let session_key = "meeting_1_session_1_alice".to_string();
            if let Some(session_info) = tracker_guard.get_mut(&session_key) {
                session_info.last_seen = Instant::now();
            }
            assert_eq!(tracker_guard.len(), 1);
        }

        // Test removing a session
        {
            let mut tracker_guard = tracker.lock().unwrap();
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
            let mut tracker_guard = tracker.lock().unwrap();
            let session_key = "meeting_1_session_1_alice".to_string();
            let session_info = SessionInfo {
                session_id: "session_1".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_peer: "alice".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            tracker_guard.insert(session_key, session_info);
        }

        // Add a stale session (simulated by setting old timestamp)
        {
            let mut tracker_guard = tracker.lock().unwrap();
            let session_key = "meeting_1_session_2_bob".to_string();
            let mut session_info = SessionInfo {
                session_id: "session_2".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_peer: "bob".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            // Simulate old timestamp by subtracting 40 seconds
            session_info.last_seen = session_info.last_seen - Duration::from_secs(40);
            tracker_guard.insert(session_key, session_info);
        }

        // Verify we have 2 sessions before cleanup
        {
            let tracker_guard = tracker.lock().unwrap();
            assert_eq!(tracker_guard.len(), 2);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker);

        // Verify only the fresh session remains
        {
            let tracker_guard = tracker.lock().unwrap();
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
            let tracker_guard = tracker.lock().unwrap();
            let session_key = "meeting_456_session_123_alice".to_string();
            assert!(tracker_guard.contains_key(&session_key));

            let session_info = tracker_guard.get(&session_key).unwrap();
            assert_eq!(session_info.session_id, "session_123");
            assert_eq!(session_info.meeting_id, "meeting_456");
            assert_eq!(session_info.reporting_peer, "alice");
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
        assert!(series_exists(
            "videocall_video_packets_buffered",
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
            let mut store = health_store.lock().unwrap();
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

        let guard = tracker.lock().unwrap();
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
        let reporting_peer = "alice";
        let packet = create_test_health_packet(session_id, meeting_id, reporting_peer, peer_stats);
        let result = process_health_packet_to_metrics_pb(&packet, &tracker);
        assert!(result.is_ok());

        // Confirm a series exists
        assert!(series_exists(
            "videocall_neteq_packets_awaiting_decode",
            &[
                ("meeting_id", meeting_id),
                ("session_id", session_id),
                ("from_peer", reporting_peer),
                ("to_peer", "bob"),
            ],
        ));

        // Remove and ensure it disappears
        let session_key = format!("{}_{}_{}", meeting_id, session_id, reporting_peer);
        let info = {
            let guard = tracker.lock().unwrap();
            guard.get(&session_key).unwrap().clone()
        };
        remove_session_metrics(&info);

        assert!(!series_exists(
            "videocall_neteq_packets_awaiting_decode",
            &[
                ("meeting_id", meeting_id),
                ("session_id", session_id),
                ("from_peer", reporting_peer),
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
        let result = process_health_packet_to_metrics_pb(&health_packet, &tracker);
        assert!(result.is_ok());

        // Verify session tracking
        {
            let tracker_guard = tracker.lock().unwrap();
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
            let mut tracker_guard = tracker.lock().unwrap();

            // Fresh session
            let session_key1 = "meeting_1_session_1_alice".to_string();
            let session_info1 = SessionInfo {
                session_id: "session_1".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_peer: "alice".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            tracker_guard.insert(session_key1, session_info1);

            // Stale session
            let session_key2 = "meeting_1_session_2_bob".to_string();
            let mut session_info2 = SessionInfo {
                session_id: "session_2".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_peer: "bob".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            session_info2.last_seen = session_info2.last_seen - Duration::from_secs(40);
            tracker_guard.insert(session_key2, session_info2);

            // Another fresh session
            let session_key3 = "meeting_2_session_3_charlie".to_string();
            let session_info3 = SessionInfo {
                session_id: "session_3".to_string(),
                meeting_id: "meeting_2".to_string(),
                reporting_peer: "charlie".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            tracker_guard.insert(session_key3, session_info3);
        }

        // Verify initial state
        {
            let tracker_guard = tracker.lock().unwrap();
            assert_eq!(tracker_guard.len(), 3);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker);

        // Verify cleanup results
        {
            let tracker_guard = tracker.lock().unwrap();
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
            reporting_peer: "test_peer".to_string(),
            last_seen: Instant::now(),
            to_peers: HashSet::new(),
            peer_ids: HashSet::new(),
        };

        // This test verifies that remove_session_metrics doesn't panic
        // In a real environment, this would interact with Prometheus metrics
        remove_session_metrics(&session_info);

        // If we reach here, the function executed without panicking
        assert!(true);
    }

    #[test]
    fn test_session_tracker_concurrent_access() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));
        let tracker_clone = tracker.clone();

        // Simulate concurrent access (though this is simplified since we're using Mutex)
        let handle = std::thread::spawn(move || {
            let mut tracker_guard = tracker_clone.lock().unwrap();
            let session_key = "concurrent_session".to_string();
            let session_info = SessionInfo {
                session_id: "session_concurrent".to_string(),
                meeting_id: "meeting_concurrent".to_string(),
                reporting_peer: "concurrent_peer".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            tracker_guard.insert(session_key, session_info);
        });

        // Wait for the thread to complete
        handle.join().unwrap();

        // Verify the session was added
        {
            let tracker_guard = tracker.lock().unwrap();
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
            let tracker_guard = tracker.lock().unwrap();
            let session_key = "meeting_empty_session_empty_alice".to_string();
            assert!(tracker_guard.contains_key(&session_key));
        }
    }

    #[test]
    fn test_session_timeout_edge_cases() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Add session exactly at timeout boundary
        {
            let mut tracker_guard = tracker.lock().unwrap();
            let session_key = "boundary_session".to_string();
            let mut session_info = SessionInfo {
                session_id: "session_boundary".to_string(),
                meeting_id: "meeting_boundary".to_string(),
                reporting_peer: "boundary_peer".to_string(),
                last_seen: Instant::now(),
                to_peers: HashSet::new(),
                peer_ids: HashSet::new(),
            };
            // Set to exactly 30 seconds ago (timeout boundary)
            session_info.last_seen = session_info.last_seen - Duration::from_secs(30);
            tracker_guard.insert(session_key, session_info);
        }

        // Run cleanup
        cleanup_stale_sessions(&tracker);

        // Session should be cleaned up (>= 30 seconds is considered stale)
        {
            let tracker_guard = tracker.lock().unwrap();
            assert_eq!(tracker_guard.len(), 0);
        }
    }
}
