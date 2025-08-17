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

//! Connection tracking module for server-side metrics via NATS

use async_nats::Client;
use protobuf::Message;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use videocall_types::protos::server_connection_packet::{
    ConnectionMetadata, DataTransferInfo, EventType, ServerConnectionPacket,
};

/// Data tracking helper functions to keep DRY across WebTransport/QUIC
pub struct DataTracker;

impl DataTracker {
    /// Track received data and update metrics
    pub fn track_received(session_id: &str, bytes: u64) {
        GLOBAL_TRACKER.track_data_received(session_id, bytes);
    }

    /// Track sent data and update metrics
    pub fn track_sent(session_id: &str, bytes: u64) {
        GLOBAL_TRACKER.track_data_sent(session_id, bytes);
    }

    /// Track echo (received + sent in one call)
    pub fn track_echo(session_id: &str, bytes: u64) {
        GLOBAL_TRACKER.track_data_received(session_id, bytes);
        GLOBAL_TRACKER.track_data_sent(session_id, bytes);
    }
}

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub session_id: String,
    pub customer_email: String,
    pub meeting_id: String,
    pub protocol: String, // "websocket", "webtransport", "quic"
    pub start_time: Instant,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub last_data_report: Instant,
}

// Global NATS client for server connection events
static NATS_CLIENT: OnceLock<Client> = OnceLock::new();

/// Default server stats reporting interval in seconds
const DEFAULT_REPORTING_INTERVAL_SECS: u64 = 5;

/// Get the server stats reporting interval from environment variable
fn get_reporting_interval() -> Duration {
    let interval_secs = std::env::var("SERVER_STATS_INTERVAL_SECS")
        .unwrap_or_else(|_| DEFAULT_REPORTING_INTERVAL_SECS.to_string())
        .parse::<u64>()
        .unwrap_or(DEFAULT_REPORTING_INTERVAL_SECS);

    Duration::from_secs(interval_secs)
}

#[derive(Default)]
pub struct ConnectionTracker {
    connections: Mutex<HashMap<String, ConnectionInfo>>,
    // (customer_email, meeting_id) -> reconnection_count
    reconnections: Mutex<HashMap<(String, String), u64>>,
    server_instance: String,
    region: String,
    service_type: String,
}

impl ConnectionTracker {
    pub fn new() -> Self {
        let server_instance = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("SERVER_ID"))
            .unwrap_or_else(|_| "server-unknown".to_string());
        let region = std::env::var("REGION").unwrap_or_else(|_| "us-east".to_string());
        let service_type =
            std::env::var("SERVICE_TYPE").unwrap_or_else(|_| "websocket".to_string());

        ConnectionTracker {
            connections: Mutex::new(HashMap::new()),
            reconnections: Mutex::new(HashMap::new()),
            server_instance,
            region,
            service_type,
        }
    }

    /// Initialize the global NATS client for publishing events
    pub fn init_nats_client(client: Client) -> Result<(), Client> {
        NATS_CLIENT.set(client)
    }

    /// Get current timestamp in milliseconds
    fn current_timestamp_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_millis() as u64
    }

    /// Create connection metadata
    fn create_metadata(
        &self,
        session_id: &str,
        customer_email: &str,
        meeting_id: &str,
        protocol: &str,
    ) -> ConnectionMetadata {
        let mut metadata = ConnectionMetadata::new();
        metadata.session_id = session_id.to_string();
        metadata.customer_email = customer_email.to_string();
        metadata.meeting_id = meeting_id.to_string();
        metadata.protocol = protocol.to_string();
        metadata.server_instance = self.server_instance.clone();
        metadata.region = self.region.clone();
        metadata
    }

    /// Publish event to NATS
    async fn publish_event(&self, packet: ServerConnectionPacket) {
        if let Some(client) = NATS_CLIENT.get() {
            let topic = format!(
                "server.connections.{}.{}.{}",
                self.region, self.service_type, self.server_instance
            );

            match packet.write_to_bytes() {
                Ok(payload) => {
                    if let Err(e) = client.publish(topic.clone(), payload.into()).await {
                        error!(
                            "Failed to publish server event to NATS topic {}: {}",
                            topic, e
                        );
                    } else {
                        debug!("Published server event to NATS topic: {}", topic);
                    }
                }
                Err(e) => {
                    error!("Failed to serialize server connection packet: {}", e);
                }
            }
        } else {
            warn!("NATS client not available, skipping event publication");
        }
    }

    /// Track a new connection starting
    pub fn connection_started(
        &self,
        session_id: String,
        customer_email: String,
        meeting_id: String,
        protocol: String,
    ) {
        let mut connections = self.connections.lock().unwrap();
        let mut reconnections = self.reconnections.lock().unwrap();

        let key = (customer_email.clone(), meeting_id.clone());
        let is_reconnection = reconnections.contains_key(&key);

        if is_reconnection {
            info!(
                "Reconnection detected for customer: {}, meeting: {}",
                customer_email, meeting_id
            );
        }
        let count = reconnections.get(&key).unwrap_or(&0) + 1;
        reconnections.insert(key, count);

        let now = Instant::now();
        let info = ConnectionInfo {
            session_id: session_id.clone(),
            customer_email: customer_email.clone(),
            meeting_id: meeting_id.clone(),
            protocol: protocol.clone(),
            start_time: now,
            bytes_sent: 0,
            bytes_received: 0,
            last_data_report: now,
        };
        connections.insert(session_id.clone(), info);

        debug!(
            "Connection started: session={}, customer={}, meeting={}, protocol={}",
            session_id, customer_email, meeting_id, protocol
        );

        // Publish connection started event
        let metadata = self.create_metadata(&session_id, &customer_email, &meeting_id, &protocol);
        let mut packet = ServerConnectionPacket::new();
        packet.event_type = EventType::CONNECTION_STARTED.into();
        packet.timestamp_ms = Self::current_timestamp_ms();
        packet.connection = Some(metadata).into();
        packet.is_reconnection = is_reconnection;

        if NATS_CLIENT.get().is_some() {
            tokio::spawn(async move {
                GLOBAL_TRACKER.publish_event(packet).await;
            });
        }
    }

    /// Track a connection ending
    pub fn connection_ended(&self, session_id: &str) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(info) = connections.remove(session_id) {
            let duration_ms = info.start_time.elapsed().as_millis() as u64;

            debug!(
                "Connection ended: session={}, customer={}, meeting={}, protocol={}, duration={}ms, sent={}B, received={}B",
                info.session_id, info.customer_email, info.meeting_id, info.protocol, duration_ms, info.bytes_sent, info.bytes_received
            );

            // Publish connection ended event
            let metadata = self.create_metadata(
                &info.session_id,
                &info.customer_email,
                &info.meeting_id,
                &info.protocol,
            );
            let mut packet = ServerConnectionPacket::new();
            packet.event_type = EventType::CONNECTION_ENDED.into();
            packet.timestamp_ms = Self::current_timestamp_ms();
            packet.connection = Some(metadata).into();
            packet.connection_duration_ms = duration_ms;

            // Include final data transfer stats
            let mut data_transfer = DataTransferInfo::new();
            data_transfer.bytes_sent = info.bytes_sent;
            data_transfer.bytes_received = info.bytes_received;
            packet.data_transfer = Some(data_transfer).into();

            if NATS_CLIENT.get().is_some() {
                tokio::spawn(async move {
                    GLOBAL_TRACKER.publish_event(packet).await;
                });
            }
        } else {
            debug!("Connection ended for unknown session_id: {}", session_id);
        }
    }

    /// Track data sent through a connection
    pub fn track_data_sent(&self, session_id: &str, bytes: u64) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(info) = connections.get_mut(session_id) {
            info.bytes_sent += bytes;
        }
    }

    /// Track data received through a connection
    pub fn track_data_received(&self, session_id: &str, bytes: u64) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(info) = connections.get_mut(session_id) {
            info.bytes_received += bytes;
        }
    }

    /// Get current connection count for debugging
    pub fn get_connection_count(&self) -> usize {
        self.connections.lock().unwrap().len()
    }

    /// Start periodic data reporting task (configurable via SERVER_STATS_INTERVAL_SECS env var)
    pub async fn start_periodic_reporting(&self) {
        if NATS_CLIENT.get().is_none() {
            warn!("NATS client not available, periodic reporting disabled");
            return;
        }

        let reporting_interval = get_reporting_interval();
        info!(
            "Starting periodic server stats reporting every {:?}",
            reporting_interval
        );

        let mut interval = interval(reporting_interval);
        loop {
            interval.tick().await;
            self.report_current_data_usage().await;
        }
    }

    /// Report current data usage for all active connections
    async fn report_current_data_usage(&self) {
        let connections_snapshot = {
            let connections = self.connections.lock().unwrap();
            connections.clone()
        };

        for (_session_id, info) in connections_snapshot {
            // Only report if we have new data since last report
            let should_report = info.bytes_sent > 0 || info.bytes_received > 0;

            if should_report {
                let metadata = self.create_metadata(
                    &info.session_id,
                    &info.customer_email,
                    &info.meeting_id,
                    &info.protocol,
                );
                let mut data_transfer = DataTransferInfo::new();
                data_transfer.bytes_sent = info.bytes_sent;
                data_transfer.bytes_received = info.bytes_received;

                let mut packet = ServerConnectionPacket::new();
                packet.event_type = EventType::DATA_TRANSFERRED.into();
                packet.timestamp_ms = Self::current_timestamp_ms();
                packet.connection = Some(metadata).into();
                packet.data_transfer = Some(data_transfer).into();

                self.publish_event(packet).await;
            }
        }
    }
}

lazy_static::lazy_static! {
    /// Global instance for server-wide connection tracking
    pub static ref GLOBAL_TRACKER: ConnectionTracker = ConnectionTracker::new();
}
