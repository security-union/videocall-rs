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
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::interval;
use tracing::{debug, error, info};
use videocall_types::protos::server_connection_packet::{
    ConnectionMetadata, DataTransferInfo, EventType, ServerConnectionPacket,
};

/// Messages for the ServerDiagnostics task
#[derive(Debug, Clone)]
pub enum TrackerMessage {
    ConnectionStarted {
        session_id: String,
        customer_email: String,
        meeting_id: String,
        protocol: String,
    },
    ConnectionEnded {
        session_id: String,
    },
    DataSent {
        session_id: String,
        bytes: u64,
    },
    DataReceived {
        session_id: String,
        bytes: u64,
    },
}

/// Data tracking helper that uses message passing
pub struct DataTracker {
    sender: mpsc::UnboundedSender<TrackerMessage>,
}

/// Tracker sender for sending messages to the ServerDiagnostics
pub type TrackerSender = mpsc::UnboundedSender<TrackerMessage>;

/// Convenience functions for sending tracker messages
pub fn send_connection_started(
    sender: &TrackerSender,
    session_id: String,
    customer_email: String,
    meeting_id: String,
    protocol: String,
) {
    let _ = sender.send(TrackerMessage::ConnectionStarted {
        session_id,
        customer_email,
        meeting_id,
        protocol,
    });
}

pub fn send_connection_ended(
    sender: &TrackerSender,
    session_id: String,
) {
    let _ = sender.send(TrackerMessage::ConnectionEnded { session_id });
}

impl DataTracker {
    pub fn new(sender: mpsc::UnboundedSender<TrackerMessage>) -> Self {
        DataTracker { sender }
    }

    /// Track received data and update metrics
    pub fn track_received(&self, session_id: &str, bytes: u64) {
        let _ = self.sender.send(TrackerMessage::DataReceived {
            session_id: session_id.to_string(),
            bytes,
        });
    }

    /// Track sent data and update metrics
    pub fn track_sent(&self, session_id: &str, bytes: u64) {
        let _ = self.sender.send(TrackerMessage::DataSent {
            session_id: session_id.to_string(),
            bytes,
        });
    }

    /// Track echo (received + sent in one call)
    pub fn track_echo(&self, session_id: &str, bytes: u64) {
        self.track_received(session_id, bytes);
        self.track_sent(session_id, bytes);
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

#[derive(Debug)]
pub struct ServerDiagnostics {
    connections: Mutex<HashMap<String, ConnectionInfo>>,
    // (customer_email, meeting_id) -> reconnection_count
    reconnections: Mutex<HashMap<(String, String), u64>>,
    nats_client: Client,
    server_instance: String,
    region: String,
    service_type: String,
}

impl ServerDiagnostics {
    pub fn new(nats_client: Client) -> Self {
        let server_instance = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("SERVER_ID"))
            .unwrap_or_else(|_| "server-unknown".to_string());
        let region = std::env::var("REGION").unwrap_or_else(|_| "us-east".to_string());
        let service_type =
            std::env::var("SERVICE_TYPE").unwrap_or_else(|_| "websocket".to_string());

        ServerDiagnostics {
            connections: Mutex::new(HashMap::new()),
            reconnections: Mutex::new(HashMap::new()),
            nats_client,
            server_instance,
            region,
            service_type,
        }
    }

    /// Create a ServerDiagnostics with message channel
    pub fn new_with_channel(
        nats_client: Client,
    ) -> (
        Self,
        mpsc::UnboundedSender<TrackerMessage>,
        mpsc::UnboundedReceiver<TrackerMessage>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let tracker = Self::new(nats_client);
        (tracker, sender, receiver)
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
        let topic = format!(
            "server.connections.{}.{}.{}",
            self.region, self.service_type, self.server_instance
        );

        match packet.write_to_bytes() {
            Ok(payload) => {
                if let Err(e) = self
                    .nats_client
                    .publish(topic.clone(), payload.into())
                    .await
                {
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

        let nats_client = self.nats_client.clone();
        let region = self.region.clone();
        let service_type = self.service_type.clone();
        let server_instance = self.server_instance.clone();

        tokio::spawn(async move {
            let topic = format!("server.connections.{region}.{service_type}.{server_instance}");

            match packet.write_to_bytes() {
                Ok(payload) => {
                    if let Err(e) = nats_client.publish(topic.clone(), payload.into()).await {
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
        });
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

            let nats_client = self.nats_client.clone();
            let region = self.region.clone();
            let service_type = self.service_type.clone();
            let server_instance = self.server_instance.clone();

            tokio::spawn(async move {
                let topic = format!("server.connections.{region}.{service_type}.{server_instance}");

                match packet.write_to_bytes() {
                    Ok(payload) => {
                        if let Err(e) = nats_client.publish(topic.clone(), payload.into()).await {
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
            });
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

    /// Run the connection tracker message processing loop
    pub async fn run_message_loop(&self, mut receiver: mpsc::UnboundedReceiver<TrackerMessage>) {
        // Start periodic reporting task
        let reporting_interval = get_reporting_interval();
        info!(
            "Starting periodic server stats reporting every {:?}",
            reporting_interval
        );

        let mut reporting_interval = interval(reporting_interval);

        loop {
            tokio::select! {
                // Handle incoming messages
                msg = receiver.recv() => {
                    if let Some(msg) = msg {
                        self.handle_message(msg).await;
                    } else {
                        // Channel closed, exit loop
                        break;
                    }
                }

                // Periodic reporting
                _ = reporting_interval.tick() => {
                    self.report_current_data_usage().await;
                }
            }
        }

        info!("Connection tracker message loop ended");
    }

    /// Handle a single tracker message
    async fn handle_message(&self, msg: TrackerMessage) {
        match msg {
            TrackerMessage::ConnectionStarted {
                session_id,
                customer_email,
                meeting_id,
                protocol,
            } => {
                self.connection_started(session_id, customer_email, meeting_id, protocol);
            }
            TrackerMessage::ConnectionEnded { session_id } => {
                self.connection_ended(&session_id);
            }
            TrackerMessage::DataSent { session_id, bytes } => {
                self.track_data_sent(&session_id, bytes);
            }
            TrackerMessage::DataReceived { session_id, bytes } => {
                self.track_data_received(&session_id, bytes);
            }
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
