use actix_web::{web, App, HttpResponse, HttpServer, Result};
use async_nats::{Client, Message};
use futures::StreamExt;
use protobuf::Message as PbMessage;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::task;
use tracing::{debug, error, info};
use videocall_types::protos::server_connection_packet::{
    EventType, ServerConnectionPacket as PbServerConnectionPacket,
};

use prometheus::{Encoder, TextEncoder};

// Import shared Prometheus metrics
use sec_api::metrics::{
    SERVER_CONNECTIONS_ACTIVE, SERVER_CONNECTION_EVENTS_TOTAL, SERVER_DATA_BYTES_TOTAL,
    SERVER_PROTOCOL_CONNECTIONS, SERVER_UNIQUE_USERS_ACTIVE,
};

use prometheus::{register_gauge, Gauge};
use std::sync::OnceLock;

// Startup timestamp metric to track service restarts
static METRICS_SERVICE_STARTUP_TIMESTAMP: OnceLock<Gauge> = OnceLock::new();

fn get_startup_timestamp_metric() -> &'static Gauge {
    METRICS_SERVICE_STARTUP_TIMESTAMP.get_or_init(|| {
        register_gauge!(
            "videocall_metrics_service_startup_timestamp_seconds",
            "Unix timestamp when the metrics service was last started"
        )
        .expect("Failed to create startup timestamp metric")
    })
}

// Connection with timestamp for proper cleanup
#[derive(Debug, Clone)]
struct ConnectionData {
    value: f64,
    last_seen: Instant,
}

// Complete state snapshot approach
#[derive(Debug, Clone)]
struct ServerSnapshot {
    last_seen: Instant,
    // Raw metrics snapshot from this server with per-connection timestamps
    connections: HashMap<String, ConnectionData>, // key: "session_id_protocol_customer_meeting_server_region"
    unique_users: HashMap<String, ConnectionData>, // key: "customer@meeting_region"
    data_bytes: HashMap<String, ConnectionData>, // key: "sent/received_session_id_protocol_customer_meeting_server_region"
}

type ServerSnapshots = Arc<Mutex<HashMap<String, ServerSnapshot>>>; // server_id -> snapshot

async fn metrics_handler(snapshots: web::Data<ServerSnapshots>) -> Result<HttpResponse> {
    // Aggregate fresh snapshots on-demand (stateless!)
    aggregate_server_snapshots(&snapshots);

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

/// Aggregate fresh server snapshots into Prometheus metrics (stateless!)
fn aggregate_server_snapshots(snapshots: &ServerSnapshots) {
    use std::time::Duration;
    let snapshots_guard = snapshots.lock().unwrap();
    let now = Instant::now();
    let timeout = Duration::from_secs(30); // 30 second freshness timeout

    // Reset all metrics before aggregation
    SERVER_CONNECTIONS_ACTIVE.reset();
    SERVER_UNIQUE_USERS_ACTIVE.reset();
    SERVER_PROTOCOL_CONNECTIONS.reset();
    SERVER_DATA_BYTES_TOTAL.reset();

    debug!("Aggregating {} server snapshots", snapshots_guard.len());

    // Aggregate fresh snapshots only
    for (server_id, snapshot) in snapshots_guard.iter() {
        if now.duration_since(snapshot.last_seen) > timeout {
            debug!("Skipping stale snapshot from server: {}", server_id);
            continue; // Skip stale servers
        }

        debug!("Aggregating fresh snapshot from server: {}", server_id);

        // Clean up stale connections within this fresh server snapshot
        let conn_timeout = Duration::from_secs(10);
        let mut connections_to_remove = Vec::new();
        let mut users_to_remove = Vec::new();
        let mut data_to_remove = Vec::new();

        // Find stale connections
        for (key, conn_data) in &snapshot.connections {
            if now.duration_since(conn_data.last_seen) > conn_timeout {
                connections_to_remove.push(key.clone());
            }
        }
        for (key, conn_data) in &snapshot.unique_users {
            if now.duration_since(conn_data.last_seen) > conn_timeout {
                users_to_remove.push(key.clone());
            }
        }
        for (key, conn_data) in &snapshot.data_bytes {
            if now.duration_since(conn_data.last_seen) > conn_timeout {
                data_to_remove.push(key.clone());
            }
        }

        // Remove stale connections (this fixes the stale connection problem!)
        if !connections_to_remove.is_empty()
            || !users_to_remove.is_empty()
            || !data_to_remove.is_empty()
        {
            debug!(
                "Removing {} stale connections, {} users, {} data entries from server {}",
                connections_to_remove.len(),
                users_to_remove.len(),
                data_to_remove.len(),
                server_id
            );
        }

        // Aggregate fresh connections only
        for (key, conn_data) in &snapshot.connections {
            if !connections_to_remove.contains(key) {
                let parts: Vec<&str> = key.split('_').collect();
                if parts.len() >= 6 {
                    SERVER_CONNECTIONS_ACTIVE
                        .with_label_values(&[
                            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5],
                        ]) // session_id, protocol, customer, meeting, server, region
                        .set(conn_data.value);
                }
            }
        }

        // Aggregate fresh unique users only
        for (key, conn_data) in &snapshot.unique_users {
            if !users_to_remove.contains(key) {
                let parts: Vec<&str> = key.split('@').collect();
                if parts.len() >= 2 {
                    let parts2: Vec<&str> = parts[1].split('_').collect();
                    if parts2.len() >= 2 {
                        SERVER_UNIQUE_USERS_ACTIVE
                            .with_label_values(&[parts[0], parts2[0], parts2[1]]) // customer, meeting, region
                            .set(conn_data.value);
                    }
                }
            }
        }

        // Aggregate fresh protocol connections
        for (key, conn_data) in &snapshot.connections {
            if !connections_to_remove.contains(key) {
                let parts: Vec<&str> = key.split('_').collect();
                if parts.len() >= 6 {
                    SERVER_PROTOCOL_CONNECTIONS
                        .with_label_values(&[parts[1], parts[2], parts[3], parts[5]]) // protocol, customer, meeting, region (skipping session_id and server)
                        .set(conn_data.value);
                }
            }
        }

        // Aggregate fresh data bytes
        for (key, conn_data) in &snapshot.data_bytes {
            if !data_to_remove.contains(key) {
                let parts: Vec<&str> = key.split('_').collect();
                if parts.len() >= 7 {
                    SERVER_DATA_BYTES_TOTAL
                        .with_label_values(&[
                            parts[0], parts[1], parts[2], parts[3], parts[4], parts[5], parts[6],
                        ]) // direction, session_id, protocol, customer, meeting, server, region
                        .set(conn_data.value);
                }
            }
        }
    }

    debug!("Aggregation complete");
}

/// Update connection in server snapshot with timestamp (Hybrid Option 1!)
async fn update_connection_with_timestamp(
    packet: &PbServerConnectionPacket,
    snapshots: &ServerSnapshots,
) -> anyhow::Result<()> {
    let conn = packet
        .connection
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Missing connection metadata"))?;

    debug!(
        "Updating connection timestamp for server: {} with data from {}@{} via {}",
        conn.server_instance, conn.customer_email, conn.meeting_id, conn.protocol
    );

    let mut snapshots_guard = snapshots.lock().unwrap();
    let server_id = conn.server_instance.clone();

    // Get or create server snapshot
    let snapshot = snapshots_guard
        .entry(server_id.clone())
        .or_insert_with(|| ServerSnapshot {
            last_seen: Instant::now(),
            connections: HashMap::new(),
            unique_users: HashMap::new(),
            data_bytes: HashMap::new(),
        });

    let now = Instant::now();

    // Update server last seen
    snapshot.last_seen = now;

    // Update/add this connection with fresh timestamp (using session_id for uniqueness!)
    let connection_key = format!(
        "{}_{}_{}_{}_{}_{}",
        conn.session_id,
        conn.protocol,
        conn.customer_email,
        conn.meeting_id,
        conn.server_instance,
        conn.region
    );
    snapshot.connections.insert(
        connection_key,
        ConnectionData {
            value: 1.0,
            last_seen: now,
        },
    );

    let user_key = format!(
        "{}@{}_{}",
        conn.customer_email, conn.meeting_id, conn.region
    );
    snapshot.unique_users.insert(
        user_key,
        ConnectionData {
            value: 1.0,
            last_seen: now,
        },
    );

    // Update data transfer if present
    if let Some(data_transfer) = packet.data_transfer.as_ref() {
        if data_transfer.bytes_sent > 0 {
            let sent_key = format!(
                "sent_{}_{}_{}_{}_{}_{}",
                conn.session_id,
                conn.protocol,
                conn.customer_email,
                conn.meeting_id,
                conn.server_instance,
                conn.region
            );
            snapshot.data_bytes.insert(
                sent_key,
                ConnectionData {
                    value: data_transfer.bytes_sent as f64,
                    last_seen: now,
                },
            );
        }
        if data_transfer.bytes_received > 0 {
            let received_key = format!(
                "received_{}_{}_{}_{}_{}_{}",
                conn.session_id,
                conn.protocol,
                conn.customer_email,
                conn.meeting_id,
                conn.server_instance,
                conn.region
            );
            snapshot.data_bytes.insert(
                received_key,
                ConnectionData {
                    value: data_transfer.bytes_received as f64,
                    last_seen: now,
                },
            );
        }
    }

    debug!("Updated connection for server: {} - stale connections will be cleaned up during aggregation", server_id);
    Ok(())
}

async fn handle_server_connection_message(
    message: Message,
    snapshots: &ServerSnapshots,
) -> anyhow::Result<()> {
    let topic = &message.subject;
    debug!("Received server connection data from topic: {}", topic);

    // Parse protobuf server connection packet
    let connection_packet: PbServerConnectionPacket =
        PbServerConnectionPacket::parse_from_bytes(&message.payload)?;

    // Freshness guard: discard stale packets (30 seconds timeout)
    let now_ms: u128 = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let packet_ts_ms: u128 = connection_packet.timestamp_ms as u128;

    let is_fresh = now_ms.saturating_sub(packet_ts_ms) <= 30_000;
    if !is_fresh {
        debug!(
            "Discarded stale server connection packet on topic {}",
            topic
        );
        return Ok(());
    }

    // Process all events the same way - update server snapshot! (stateless)
    let event_type = connection_packet
        .event_type
        .enum_value()
        .unwrap_or(EventType::UNKNOWN);

    match event_type {
        EventType::CONNECTION_STARTED | EventType::DATA_TRANSFERRED => {
            debug!("Processing {:?} event for topic {}", event_type, topic);
            // Update connection with fresh timestamp (Hybrid Option 1 approach)
            update_connection_with_timestamp(&connection_packet, snapshots).await?;
        }
        EventType::CONNECTION_ENDED => {
            debug!("Connection ended - will be automatically cleaned up by timeout");
            // No action needed - stale connections will be cleaned up by timeout mechanism
        }
        EventType::UNKNOWN => {
            debug!(
                "Received unknown server connection event type for topic {}",
                topic
            );
        }
    }

    // Increment events counter
    SERVER_CONNECTION_EVENTS_TOTAL.inc();

    Ok(())
}

async fn nats_server_consumer(
    nats_client: Client,
    snapshots: ServerSnapshots,
) -> anyhow::Result<()> {
    // Subscribe to all server connection topics from all regions
    let queue_group = "metrics-server-connection-events";
    let mut subscription = nats_client
        .queue_subscribe("server.connections.>", queue_group.to_string())
        .await?;

    info!("Subscribed to NATS topic: server.connections.>");

    while let Some(message) = subscription.next().await {
        debug!(
            "Received server connection message from NATS: {}",
            message.subject
        );
        if let Err(e) = handle_server_connection_message(message, &snapshots).await {
            error!("Failed to handle server connection message: {}", e);
        }
    }

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

    info!(
        "Starting stateless snapshot metrics server on port {}",
        port
    );

    // Clean slate: Reset all server metrics on startup to prevent stale data
    info!("Resetting all server metrics to prevent stale data from previous runs");
    SERVER_CONNECTIONS_ACTIVE.reset();
    SERVER_UNIQUE_USERS_ACTIVE.reset();
    SERVER_PROTOCOL_CONNECTIONS.reset();
    SERVER_DATA_BYTES_TOTAL.reset();
    SERVER_CONNECTION_EVENTS_TOTAL.reset();

    // Record startup timestamp for debugging and dashboard filtering
    let startup_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as f64;
    get_startup_timestamp_metric().set(startup_time);

    info!(
        "Server metrics reset complete - startup timestamp: {}",
        startup_time
    );

    // Create shared server snapshots (stateless approach!)
    let snapshots: ServerSnapshots = Arc::new(Mutex::new(HashMap::new()));

    // Start NATS connection in background - don't block HTTP server startup!
    let nats_snapshots = snapshots.clone();
    task::spawn(async move {
        info!("Connecting to NATS at {}", nats_url);
        match async_nats::connect(&nats_url).await {
            Ok(nats_client) => {
                info!("Connected to NATS successfully");
                if let Err(e) = nats_server_consumer(nats_client, nats_snapshots).await {
                    error!("NATS server connection consumer failed: {}", e);
                }
            }
            Err(e) => {
                error!("Failed to connect to NATS: {}", e);
                error!("Metrics server will still run but won't receive NATS data");
            }
        }
    });

    // Start HTTP server immediately (don't wait for NATS)
    info!("Starting HTTP server on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(snapshots.clone()))
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
