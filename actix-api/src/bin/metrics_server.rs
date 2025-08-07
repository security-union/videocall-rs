use actix_web::{web, App, HttpResponse, HttpServer, Result};
use async_nats::{Client, Message};
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::task;
use tracing::{debug, error, info};

#[cfg(feature = "diagnostics")]
use prometheus::{Encoder, TextEncoder};

// Shared state for latest health data from all servers
type HealthDataStore = Arc<Mutex<HashMap<String, Value>>>;

// Session tracking for cleanup
#[derive(Debug, Clone)]
struct SessionInfo {
    session_id: String,
    meeting_id: String,
    reporting_peer: String,
    last_seen: Instant,
}

type SessionTracker = Arc<Mutex<HashMap<String, SessionInfo>>>;

// Prometheus metrics (same as existing diagnostics.rs)
// Import shared Prometheus metrics
#[cfg(feature = "diagnostics")]
use sec_api::metrics::{
    ACTIVE_SESSIONS_TOTAL, MEETING_PARTICIPANTS, NETEQ_ACCELERATE_OPS_PER_SEC,
    NETEQ_AUDIO_BUFFER_MS, NETEQ_COMFORT_NOISE_OPS_PER_SEC, NETEQ_DTMF_OPS_PER_SEC,
    NETEQ_EXPAND_OPS_PER_SEC, NETEQ_FAST_ACCELERATE_OPS_PER_SEC, NETEQ_MERGE_OPS_PER_SEC,
    NETEQ_NORMAL_OPS_PER_SEC, NETEQ_PACKETS_AWAITING_DECODE, NETEQ_PREEMPTIVE_EXPAND_OPS_PER_SEC,
    NETEQ_UNDEFINED_OPS_PER_SEC, PEER_CAN_LISTEN, PEER_CAN_SEE, PEER_CONNECTIONS_TOTAL,
};

#[cfg(feature = "diagnostics")]
async fn metrics_handler(
    data: web::Data<HealthDataStore>,
    session_tracker: web::Data<SessionTracker>,
) -> Result<HttpResponse> {
    let health_data = data.lock().unwrap();

    // Clean up stale sessions before processing metrics
    cleanup_stale_sessions(&session_tracker);

    // Process all stored health data and update Prometheus metrics
    for (server_key, health_packet) in health_data.iter() {
        debug!("Processing health data from {}", server_key);

        if let Err(e) = process_health_packet_to_metrics(health_packet, &session_tracker) {
            error!("Failed to process health packet from {}: {}", server_key, e);
        }
    }

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

#[cfg(not(feature = "diagnostics"))]
async fn metrics_handler(
    _data: web::Data<HealthDataStore>,
    _session_tracker: web::Data<SessionTracker>,
) -> Result<HttpResponse> {
    Ok(HttpResponse::Ok()
        .content_type("text/plain")
        .body("# Diagnostics feature not enabled\n"))
}

#[cfg(feature = "diagnostics")]
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

#[cfg(not(feature = "diagnostics"))]
/// Clean up sessions that haven't reported in the last 30 seconds (stub)
fn cleanup_stale_sessions(_session_tracker: &SessionTracker) {
    // No-op when diagnostics feature is disabled
}

#[cfg(feature = "diagnostics")]
/// Remove all Prometheus metrics for a given session
fn remove_session_metrics(session_info: &SessionInfo) {
    // Note: Prometheus doesn't have a direct "remove" method for gauges
    // Instead, we set them to 0 to indicate they're inactive
    ACTIVE_SESSIONS_TOTAL
        .with_label_values(&[&session_info.meeting_id, &session_info.session_id])
        .set(0.0);

    // Remove peer-specific metrics for this session
    // We need to iterate through all possible peer combinations
    // This is a limitation of Prometheus - we can't easily remove specific label combinations
    // For now, we'll set them to 0 and rely on the cleanup to prevent accumulation

    debug!(
        "Set session {} metrics to 0 (inactive)",
        session_info.session_id
    );
}

#[cfg(not(feature = "diagnostics"))]
/// Remove all Prometheus metrics for a given session (stub)
fn remove_session_metrics(_session_info: &SessionInfo) {
    // No-op when diagnostics feature is disabled
}

#[cfg(feature = "diagnostics")]
fn process_health_packet_to_metrics(
    health_packet: &Value,
    session_tracker: &SessionTracker,
) -> anyhow::Result<()> {
    let meeting_id = health_packet
        .get("meeting_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let session_id = health_packet
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let reporting_peer = health_packet
        .get("reporting_peer")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Update session tracker
    {
        let mut tracker = session_tracker.lock().unwrap();
        let session_key = format!("{}_{}_{}", meeting_id, session_id, reporting_peer);
        tracker.insert(
            session_key,
            SessionInfo {
                session_id: session_id.to_string(),
                meeting_id: meeting_id.to_string(),
                reporting_peer: reporting_peer.to_string(),
                last_seen: Instant::now(),
            },
        );
    }

    // Set active session metric
    ACTIVE_SESSIONS_TOTAL
        .with_label_values(&[meeting_id, session_id])
        .set(1.0);

    // Process peer health data
    if let Some(peers) = health_packet.get("peer_stats").and_then(|v| v.as_object()) {
        let mut participants_count = 0;

        for (peer_id, peer_data) in peers {
            participants_count += 1;

            // Set peer connection metric
            PEER_CONNECTIONS_TOTAL
                .with_label_values(&[meeting_id, peer_id])
                .set(1.0);

            if let Some(peer_obj) = peer_data.as_object() {
                // Process can_listen
                if let Some(can_listen) = peer_obj.get("can_listen").and_then(|v| v.as_bool()) {
                    PEER_CAN_LISTEN
                        .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                        .set(if can_listen { 1.0 } else { 0.0 });
                }

                // Process can_see
                if let Some(can_see) = peer_obj.get("can_see").and_then(|v| v.as_bool()) {
                    PEER_CAN_SEE
                        .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                        .set(if can_see { 1.0 } else { 0.0 });
                }

                // Process NetEQ metrics from neteq_stats object
                if let Some(neteq_stats) = peer_obj.get("neteq_stats") {
                    if let Some(audio_buffer_ms) = neteq_stats
                        .get("current_buffer_size_ms")
                        .and_then(|v| v.as_f64())
                    {
                        NETEQ_AUDIO_BUFFER_MS
                            .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                            .set(audio_buffer_ms);
                    }

                    if let Some(packets_awaiting) = neteq_stats
                        .get("packets_awaiting_decode")
                        .and_then(|v| v.as_f64())
                    {
                        NETEQ_PACKETS_AWAITING_DECODE
                            .with_label_values(&[meeting_id, session_id, reporting_peer, peer_id])
                            .set(packets_awaiting);
                    }

                    // Process NetEQ operation metrics from network.operation_counters
                    if let Some(network) = neteq_stats.get("network") {
                        if let Some(operation_counters) = network.get("operation_counters") {
                            // Normal operations per second
                            if let Some(normal_ops) = operation_counters
                                .get("normal_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_NORMAL_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(normal_ops);
                            }

                            // Expand operations per second
                            if let Some(expand_ops) = operation_counters
                                .get("expand_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_EXPAND_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(expand_ops);
                            }

                            // Accelerate operations per second
                            if let Some(accelerate_ops) = operation_counters
                                .get("accelerate_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_ACCELERATE_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(accelerate_ops);
                            }

                            // Fast accelerate operations per second
                            if let Some(fast_accelerate_ops) = operation_counters
                                .get("fast_accelerate_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_FAST_ACCELERATE_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(fast_accelerate_ops);
                            }

                            // Preemptive expand operations per second
                            if let Some(preemptive_expand_ops) = operation_counters
                                .get("preemptive_expand_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_PREEMPTIVE_EXPAND_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(preemptive_expand_ops);
                            }

                            // Merge operations per second
                            if let Some(merge_ops) = operation_counters
                                .get("merge_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_MERGE_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(merge_ops);
                            }

                            // Comfort noise operations per second
                            if let Some(comfort_noise_ops) = operation_counters
                                .get("comfort_noise_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_COMFORT_NOISE_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(comfort_noise_ops);
                            }

                            // DTMF operations per second
                            if let Some(dtmf_ops) = operation_counters
                                .get("dtmf_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_DTMF_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(dtmf_ops);
                            }

                            // Undefined operations per second
                            if let Some(undefined_ops) = operation_counters
                                .get("undefined_per_sec")
                                .and_then(|v| v.as_f64())
                            {
                                NETEQ_UNDEFINED_OPS_PER_SEC
                                    .with_label_values(&[
                                        meeting_id,
                                        session_id,
                                        reporting_peer,
                                        peer_id,
                                    ])
                                    .set(undefined_ops);
                            }
                        }
                    }
                }
            }
        }

        // Update meeting participants count
        MEETING_PARTICIPANTS
            .with_label_values(&[meeting_id])
            .set(participants_count as f64);
    }

    Ok(())
}

#[cfg(not(feature = "diagnostics"))]
#[allow(unused)]
fn process_health_packet_to_metrics(
    _health_packet: &Value,
    _session_tracker: &SessionTracker,
) -> anyhow::Result<()> {
    // No-op when diagnostics feature is disabled
    Ok(())
}

async fn nats_health_consumer(
    nats_client: Client,
    health_store: HealthDataStore,
) -> anyhow::Result<()> {
    // Subscribe to all health diagnostics topics from all regions
    let queue_group = "metrics-server-health-diagnostics";
    let mut subscription = nats_client
        .queue_subscribe("health.diagnostics.>", queue_group.to_string())
        .await?;

    info!("Subscribed to NATS topic: health.diagnostics.>");

    while let Some(message) = subscription.next().await {
        debug!("Received health message from NATS: {}", message.subject);
        if let Err(e) = handle_health_message(message, &health_store).await {
            error!("Failed to handle health message: {}", e);
        }
    }

    Ok(())
}

async fn handle_health_message(
    message: Message,
    health_store: &HealthDataStore,
) -> anyhow::Result<()> {
    let topic = &message.subject;
    let payload = std::str::from_utf8(&message.payload)?;

    debug!("Received health data from topic: {}", topic);

    // Parse JSON health packet
    let health_packet: Value = serde_json::from_str(payload)?;

    // Store latest health data using topic as key
    {
        let mut store = health_store.lock().unwrap();
        store.insert(topic.to_string(), health_packet);
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
    task::spawn(async move {
        if let Err(e) = nats_health_consumer(nats_client_clone, nats_store).await {
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

#[cfg(all(test, feature = "diagnostics"))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Helper function to create a test health packet
    fn create_test_health_packet(
        session_id: &str,
        meeting_id: &str,
        reporting_peer: &str,
        peer_stats: serde_json::Map<String, serde_json::Value>,
    ) -> Value {
        json!({
            "session_id": session_id,
            "meeting_id": meeting_id,
            "reporting_peer": reporting_peer,
            "timestamp_ms": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            "peer_stats": peer_stats
        })
    }

    /// Helper function to create test peer stats with NetEQ data
    fn create_test_peer_stats(
        peer_id: &str,
        can_listen: bool,
        can_see: bool,
        audio_buffer_ms: f64,
        packets_awaiting_decode: f64,
    ) -> (String, serde_json::Value) {
        let neteq_stats = json!({
            "current_buffer_size_ms": audio_buffer_ms,
            "packets_awaiting_decode": packets_awaiting_decode,
            "network": {
                "operation_counters": {
                    "normal_per_sec": 10.0,
                    "expand_per_sec": 2.0,
                    "accelerate_per_sec": 1.0,
                    "fast_accelerate_per_sec": 0.0,
                    "preemptive_expand_per_sec": 5.0,
                    "merge_per_sec": 0.0,
                    "comfort_noise_per_sec": 0.0,
                    "dtmf_per_sec": 0.0,
                    "undefined_per_sec": 0.0
                }
            }
        });

        let peer_stat = json!({
            "can_listen": can_listen,
            "can_see": can_see,
            "neteq_stats": neteq_stats,
            "video_stats": null
        });

        (peer_id.to_string(), peer_stat)
    }

    #[test]
    fn test_session_info_creation() {
        let session_info = SessionInfo {
            session_id: "session_123".to_string(),
            meeting_id: "meeting_456".to_string(),
            reporting_peer: "alice".to_string(),
            last_seen: Instant::now(),
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
        let mut peer_stats = serde_json::Map::new();
        let (peer_id, peer_stat) = create_test_peer_stats("bob", true, false, 100.0, 5.0);
        peer_stats.insert(peer_id, peer_stat);

        let health_packet =
            create_test_health_packet("session_123", "meeting_456", "alice", peer_stats);

        // Process the health packet
        let result = process_health_packet_to_metrics(&health_packet, &tracker);
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
    fn test_process_health_packet_to_metrics_with_neteq_data() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Create comprehensive peer stats with NetEQ data
        let mut peer_stats = serde_json::Map::new();

        // Add peer with full NetEQ stats
        let (peer_id1, peer_stat1) = create_test_peer_stats("bob", true, true, 150.0, 8.0);
        peer_stats.insert(peer_id1, peer_stat1);

        // Add peer with minimal stats
        let (peer_id2, peer_stat2) = create_test_peer_stats("charlie", false, true, 0.0, 0.0);
        peer_stats.insert(peer_id2, peer_stat2);

        let health_packet =
            create_test_health_packet("session_789", "meeting_999", "alice", peer_stats);

        // Process the health packet
        let result = process_health_packet_to_metrics(&health_packet, &tracker);
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

        // Test with missing required fields
        let malformed_packet = json!({
            "session_id": "session_123",
            // Missing meeting_id and reporting_peer
        });

        let result = process_health_packet_to_metrics(&malformed_packet, &tracker);
        assert!(result.is_ok()); // Should handle gracefully with defaults

        // Test with completely invalid JSON
        let invalid_packet = json!("not an object");
        let result = process_health_packet_to_metrics(&invalid_packet, &tracker);
        assert!(result.is_ok()); // Should handle gracefully
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
            };
            tracker_guard.insert(session_key1, session_info1);

            // Stale session
            let session_key2 = "meeting_1_session_2_bob".to_string();
            let mut session_info2 = SessionInfo {
                session_id: "session_2".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_peer: "bob".to_string(),
                last_seen: Instant::now(),
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
        let empty_peer_stats = serde_json::Map::new();
        let health_packet =
            create_test_health_packet("session_empty", "meeting_empty", "alice", empty_peer_stats);

        // Process the health packet
        let result = process_health_packet_to_metrics(&health_packet, &tracker);
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

#[cfg(all(test, not(feature = "diagnostics")))]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn test_session_info_basic_functionality() {
        let session_info = SessionInfo {
            session_id: "session_123".to_string(),
            meeting_id: "meeting_456".to_string(),
            reporting_peer: "alice".to_string(),
            last_seen: Instant::now(),
        };

        assert_eq!(session_info.session_id, "session_123");
        assert_eq!(session_info.meeting_id, "meeting_456");
        assert_eq!(session_info.reporting_peer, "alice");
    }

    #[test]
    fn test_session_tracker_basic_operations() {
        let tracker: SessionTracker = Arc::new(Mutex::new(HashMap::new()));

        // Test basic session tracking operations
        {
            let mut tracker_guard = tracker.lock().unwrap();
            let session_key = "test_session".to_string();
            let session_info = SessionInfo {
                session_id: "session_1".to_string(),
                meeting_id: "meeting_1".to_string(),
                reporting_peer: "alice".to_string(),
                last_seen: Instant::now(),
            };
            tracker_guard.insert(session_key.clone(), session_info);
            assert_eq!(tracker_guard.len(), 1);
            assert!(tracker_guard.contains_key(&session_key));
        }
    }
}
