use actix_web::{web, App, HttpResponse, HttpServer, Result};
use async_nats::{Client, Message};
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::task;
use tracing::{debug, error, info};

#[cfg(feature = "diagnostics")]
use prometheus::{Encoder, TextEncoder};

// Shared state for latest health data from all servers
type HealthDataStore = Arc<Mutex<HashMap<String, Value>>>;

// Prometheus metrics (same as existing diagnostics.rs)
// Import shared Prometheus metrics
#[cfg(feature = "diagnostics")]
use sec_api::metrics::{
    ACTIVE_SESSIONS_TOTAL, MEETING_PARTICIPANTS, NETEQ_AUDIO_BUFFER_MS,
    NETEQ_PACKETS_AWAITING_DECODE, PEER_CAN_LISTEN, PEER_CAN_SEE, PEER_CONNECTIONS_TOTAL,
};

#[cfg(feature = "diagnostics")]
async fn metrics_handler(data: web::Data<HealthDataStore>) -> Result<HttpResponse> {
    let health_data = data.lock().unwrap();

    // Process all stored health data and update Prometheus metrics
    for (server_key, health_packet) in health_data.iter() {
        debug!("Processing health data from {}", server_key);

        if let Err(e) = process_health_packet_to_metrics(health_packet) {
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
async fn metrics_handler(_data: web::Data<HealthDataStore>) -> Result<HttpResponse> {
    Ok(HttpResponse::Ok()
        .content_type("text/plain")
        .body("# Diagnostics feature not enabled\n"))
}

#[cfg(feature = "diagnostics")]
fn process_health_packet_to_metrics(health_packet: &Value) -> anyhow::Result<()> {
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
                    if let Some(audio_buffer_ms) =
                        neteq_stats.get("audio_buffer_ms").and_then(|v| v.as_f64())
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
                }
            }
        }

        // Set meeting participants count
        MEETING_PARTICIPANTS
            .with_label_values(&[meeting_id])
            .set(participants_count as f64);
    }

    Ok(())
}

#[cfg(not(feature = "diagnostics"))]
fn process_health_packet_to_metrics(_health_packet: &Value) -> anyhow::Result<()> {
    // No-op when diagnostics feature is disabled
    Ok(())
}

async fn nats_health_consumer(
    nats_client: Client,
    health_store: HealthDataStore,
) -> anyhow::Result<()> {
    // Subscribe to all health diagnostics topics from all regions
    let mut subscription = nats_client.subscribe("health.diagnostics.>").await?;

    info!("Subscribed to NATS topic: health.diagnostics.>");

    while let Some(message) = subscription.next().await {
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
            .route("/metrics", web::get().to(metrics_handler))
            .route(
                "/health",
                web::get().to(|| async { HttpResponse::Ok().body("OK") }),
            )
    })
    .bind(format!("0.0.0.0:{}", port))?
    .run()
    .await?;

    Ok(())
}
