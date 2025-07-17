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

use super::connection::Connection;
use super::webmedia::ConnectOptions;
use crate::crypto::aes::Aes128State;
use anyhow::{anyhow, Result};
use gloo::timers::callback::Interval;
use log::{debug, error, info, warn};
use protobuf::Message;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen::JsValue;
use yew::prelude::Callback;

/// RTT testing period in milliseconds
const DEFAULT_ELECTION_PERIOD_MS: u64 = 3000;

/// Interval between RTT probes in milliseconds  
const RTT_PROBE_INTERVAL_MS: u64 = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Testing {
        progress: f32,
        servers_tested: usize,
        total_servers: usize,
    },
    Connected {
        server_url: String,
        rtt: f64,
        is_webtransport: bool,
    },
    Reconnecting {
        server_url: String,
        attempt: u32,
        max_attempts: u32,
    },
    Failed {
        error: String,
        last_known_server: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ServerRttMeasurement {
    pub url: String,
    pub is_webtransport: bool,
    pub measurements: Vec<f64>,
    pub average_rtt: Option<f64>,
    pub connection_id: String,
    pub active: bool,
    pub connected: bool,
}

#[derive(Debug)]
pub enum ElectionState {
    Testing {
        start_time: f64,
        duration_ms: u64,
        probe_timer: Option<Interval>,
    },
    Elected {
        connection_id: String,
        elected_at: f64,
    },
    Reconnecting {
        connection_id: String,
        attempt: u32,
        max_attempts: u32,
        started_at: f64,
    },
    Failed {
        reason: String,
        failed_at: f64,
    },
}

#[derive(Clone, Debug)]
pub struct ConnectionManagerOptions {
    pub websocket_urls: Vec<String>,
    pub webtransport_urls: Vec<String>,
    pub userid: String,
    pub enable_webtransport: bool,
    pub on_inbound_media: Callback<PacketWrapper>,
    pub on_state_changed: Callback<ConnectionState>,
    pub peer_monitor: Callback<()>,
    pub election_period_ms: Option<u64>,
}

#[derive(Debug)]
pub struct ConnectionManager {
    connections: HashMap<String, Connection>,
    active_connection_id: Option<String>,
    rtt_measurements: HashMap<String, ServerRttMeasurement>,
    election_state: ElectionState,
    rtt_reporter: Option<Interval>,
    rtt_probe_timer: Option<Interval>,
    election_timer: Option<Interval>,
    rtt_start_times: HashMap<String, f64>, // track when RTT probes were sent
    rtt_responses: Rc<RefCell<Vec<(String, f64)>>>, // Shared queue for RTT responses: (connection_id, timestamp)
    options: ConnectionManagerOptions,
    aes: Rc<Aes128State>,
}

impl ConnectionManager {
    /// Create a new ConnectionManager and immediately start testing all connections
    pub fn new(options: ConnectionManagerOptions, aes: Rc<Aes128State>) -> Result<Self> {
        let total_servers = options.websocket_urls.len() + options.webtransport_urls.len();

        if total_servers == 0 {
            return Err(anyhow!("No servers provided for connection testing"));
        }

        info!("ConnectionManager starting with {} servers", total_servers);

        let rtt_responses = Rc::new(RefCell::new(Vec::new()));

        let mut manager = Self {
            connections: HashMap::new(),
            active_connection_id: None,
            rtt_measurements: HashMap::new(),
            election_state: ElectionState::Failed {
                reason: "Not started".to_string(),
                failed_at: js_sys::Date::now(),
            },
            rtt_reporter: None,
            rtt_probe_timer: None,
            election_timer: None,
            rtt_start_times: HashMap::new(),
            rtt_responses,
            options,
            aes,
        };

        // Immediately start creating connections and testing
        manager.start_election()?;

        Ok(manager)
    }

    /// Start the election process by creating all connections upfront
    fn start_election(&mut self) -> Result<()> {
        let election_duration = self
            .options
            .election_period_ms
            .unwrap_or(DEFAULT_ELECTION_PERIOD_MS);
        let start_time = js_sys::Date::now();

        info!("Starting connection election for {}ms", election_duration);

        // Create all connections upfront
        self.create_all_connections()?;

        // Set election state
        self.election_state = ElectionState::Testing {
            start_time,
            duration_ms: election_duration,
            probe_timer: None, // Will be set externally
        };

        // Start RTT reporting to diagnostics
        self.start_diagnostics_reporting();

        // Report initial state
        self.report_state();

        Ok(())
    }

    /// Create connections to all configured servers
    fn create_all_connections(&mut self) -> Result<()> {
        // Create WebSocket connections
        for (i, url) in self.options.websocket_urls.iter().enumerate() {
            let conn_id = format!("ws_{}", i);
            let connect_options = ConnectOptions {
                userid: self.options.userid.clone(),
                websocket_url: url.clone(),
                webtransport_url: String::new(), // Not used for WebSocket
                on_inbound_media: self.create_inbound_media_callback(conn_id.clone()),
                on_connected: self.create_connected_callback(conn_id.clone()),
                on_connection_lost: self.create_connection_lost_callback(conn_id.clone()),
                peer_monitor: self.options.peer_monitor.clone(),
            };

            match Connection::connect(false, connect_options, self.aes.clone()) {
                Ok(connection) => {
                    self.connections.insert(conn_id.clone(), connection);
                    self.rtt_measurements.insert(
                        conn_id.clone(),
                        ServerRttMeasurement {
                            url: url.clone(),
                            is_webtransport: false,
                            measurements: Vec::new(),
                            average_rtt: None,
                            connection_id: conn_id.clone(),
                            active: false,
                            connected: false,
                        },
                    );
                    debug!("Created WebSocket connection {}: {}", conn_id, url);
                }
                Err(e) => {
                    error!("Failed to create WebSocket connection to {}: {}", url, e);
                }
            }
        }

        // Create WebTransport connections
        for (i, url) in self.options.webtransport_urls.iter().enumerate() {
            let conn_id = format!("wt_{}", i);
            let connect_options = ConnectOptions {
                userid: self.options.userid.clone(),
                websocket_url: String::new(), // Not used for WebTransport
                webtransport_url: url.clone(),
                on_inbound_media: self.create_inbound_media_callback(conn_id.clone()),
                on_connected: self.create_connected_callback(conn_id.clone()),
                on_connection_lost: self.create_connection_lost_callback(conn_id.clone()),
                peer_monitor: self.options.peer_monitor.clone(),
            };

            match Connection::connect(true, connect_options, self.aes.clone()) {
                Ok(connection) => {
                    self.connections.insert(conn_id.clone(), connection);
                    self.rtt_measurements.insert(
                        conn_id.clone(),
                        ServerRttMeasurement {
                            url: url.clone(),
                            is_webtransport: true,
                            measurements: Vec::new(),
                            average_rtt: None,
                            connection_id: conn_id.clone(),
                            active: false,
                            connected: false,
                        },
                    );
                    debug!("Created WebTransport connection {}: {}", conn_id, url);
                }
                Err(e) => {
                    error!("Failed to create WebTransport connection to {}: {}", url, e);
                }
            }
        }

        info!("Created {} connections for testing", self.connections.len());

        // If only one connection was created, elect it immediately
        if self.connections.len() == 1 {
            info!("Only one connection created, electing immediately");

            // Set a very short election period for immediate election
            if let ElectionState::Testing { start_time, .. } = &mut self.election_state {
                self.election_state = ElectionState::Testing {
                    start_time: *start_time,
                    duration_ms: 100, // Very short - will be elected almost immediately
                    probe_timer: None,
                };
            }
        }

        Ok(())
    }

    /// Create callback for handling inbound media packets
    fn create_inbound_media_callback(&self, connection_id: String) -> Callback<PacketWrapper> {
        let userid = self.options.userid.clone();
        let aes = self.aes.clone();
        let on_inbound_media = self.options.on_inbound_media.clone();
        let rtt_responses = self.rtt_responses.clone();

        Callback::from(move |packet: PacketWrapper| {
            // Handle RTT responses internally
            if packet.email == userid {
                if let Ok(decrypted_data) = aes.decrypt(&packet.data) {
                    if let Ok(media_packet) = MediaPacket::parse_from_bytes(&decrypted_data) {
                        if media_packet.media_type == MediaType::RTT.into() {
                            // Extract timestamp from RTT packet
                            let timestamp = media_packet.timestamp;
                            debug!(
                                "RTT response received on connection {} with timestamp {}",
                                connection_id, timestamp
                            );

                            // Add RTT response to shared queue for processing
                            if let Ok(mut responses) = rtt_responses.try_borrow_mut() {
                                responses.push((connection_id.clone(), timestamp));
                            } else {
                                warn!("Unable to add RTT response to queue - queue is borrowed");
                            }
                            return; // Don't forward RTT packets
                        }
                    }
                }
            }

            // Forward all non-RTT packets to the main handler
            on_inbound_media.emit(packet);
        })
    }

    /// Create callback for connection established
    fn create_connected_callback(&self, connection_id: String) -> Callback<()> {
        Callback::from(move |_| {
            debug!("Connection {} established", connection_id);
            // Mark connection as connected - this will be handled externally
        })
    }

    /// Create callback for connection lost
    fn create_connection_lost_callback(&self, connection_id: String) -> Callback<JsValue> {
        Callback::from(move |error| {
            warn!("Connection {} lost: {:?}", connection_id, error);
            // Reconnection logic will be handled separately
        })
    }

    /// Send RTT probe to a specific connection
    fn send_rtt_probe(&mut self, connection_id: &str) -> Result<()> {
        let connection = self
            .connections
            .get(connection_id)
            .ok_or_else(|| anyhow!("Connection {} not found", connection_id))?;

        if !connection.is_connected() {
            return Ok(()); // Skip non-connected connections
        }

        // Update connection status
        if let Some(measurement) = self.rtt_measurements.get_mut(connection_id) {
            measurement.connected = true;
        }

        let timestamp = js_sys::Date::now();
        let rtt_packet = self.create_rtt_packet(timestamp)?;

        // Track when we sent this probe
        self.rtt_start_times
            .insert(connection_id.to_string(), timestamp);

        connection.send_packet(rtt_packet);
        debug!(
            "Sent RTT probe to {} at timestamp {}",
            connection_id, timestamp
        );
        Ok(())
    }

    /// Create an RTT probe packet
    fn create_rtt_packet(&self, timestamp: f64) -> Result<PacketWrapper> {
        let media_packet = MediaPacket {
            media_type: MediaType::RTT.into(),
            email: self.options.userid.clone(),
            timestamp,
            ..Default::default()
        };

        let data = self.aes.encrypt(&media_packet.write_to_bytes()?)?;
        Ok(PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            email: self.options.userid.clone(),
            data,
            ..Default::default()
        })
    }

    /// Handle RTT response and calculate round-trip time
    fn handle_rtt_response(&mut self, connection_id: &str, sent_timestamp: f64) {
        let current_time = js_sys::Date::now();
        let rtt = current_time - sent_timestamp;

        debug!("RTT for {}: {}ms", connection_id, rtt);

        if let Some(measurement) = self.rtt_measurements.get_mut(connection_id) {
            measurement.measurements.push(rtt);

            // Keep only recent measurements (last 10)
            if measurement.measurements.len() > 10 {
                measurement.measurements.remove(0);
            }

            // Update average
            let avg_rtt = measurement.measurements.iter().sum::<f64>()
                / measurement.measurements.len() as f64;
            measurement.average_rtt = Some(avg_rtt);
        }
    }

    /// Complete the election and select the best connection
    fn complete_election(&mut self) {
        info!("Completing connection election");

        // Stop probing
        if let ElectionState::Testing { probe_timer, .. } = &mut self.election_state {
            if let Some(timer) = probe_timer.take() {
                timer.cancel();
            }
        }

        // Find the best connection
        match self.find_best_connection() {
            Ok((connection_id, measurement)) => {
                info!(
                    "Elected connection {}: {} (avg RTT: {}ms)",
                    connection_id,
                    measurement.url,
                    measurement.average_rtt.unwrap_or(0.0)
                );

                self.active_connection_id = Some(connection_id.clone());

                // Mark as active
                if let Some(mut_measurement) = self.rtt_measurements.get_mut(&connection_id) {
                    mut_measurement.active = true;
                }

                self.election_state = ElectionState::Elected {
                    connection_id: connection_id.clone(),
                    elected_at: js_sys::Date::now(),
                };

                // Close unused connections
                self.close_unused_connections();

                // Report state
                self.report_state();
            }
            Err(e) => {
                error!("Election failed: {}", e);
                self.election_state = ElectionState::Failed {
                    reason: e.to_string(),
                    failed_at: js_sys::Date::now(),
                };
                self.report_state();
            }
        }
    }

    /// Find the connection with the best (lowest) average RTT
    fn find_best_connection(&self) -> Result<(String, ServerRttMeasurement)> {
        let mut best_connection: Option<(String, ServerRttMeasurement)> = None;
        let mut best_rtt = f64::INFINITY;

        for (connection_id, measurement) in &self.rtt_measurements {
            // Only consider connections that are actually connected
            if let Some(connection) = self.connections.get(connection_id) {
                if !connection.is_connected() {
                    continue;
                }
            }

            if let Some(avg_rtt) = measurement.average_rtt {
                if !measurement.measurements.is_empty() && avg_rtt < best_rtt {
                    best_rtt = avg_rtt;
                    best_connection = Some((connection_id.clone(), measurement.clone()));
                }
            }
        }

        best_connection.ok_or_else(|| anyhow!("No valid connections with RTT measurements found"))
    }

    /// Close all unused connections after election
    fn close_unused_connections(&mut self) {
        let active_id = self.active_connection_id.as_ref();
        let mut to_remove = Vec::new();

        for connection_id in self.connections.keys() {
            if Some(connection_id) != active_id {
                to_remove.push(connection_id.clone());
            }
        }

        for connection_id in to_remove {
            self.connections.remove(&connection_id);
            info!("Closed unused connection: {}", connection_id);
        }
    }

    /// Start reconnection process for failed active connection
    fn start_reconnection(&mut self, connection_id: String) {
        const MAX_RECONNECT_ATTEMPTS: u32 = 3;

        warn!("Starting reconnection for {}", connection_id);

        self.election_state = ElectionState::Reconnecting {
            connection_id: connection_id.clone(),
            attempt: 1,
            max_attempts: MAX_RECONNECT_ATTEMPTS,
            started_at: js_sys::Date::now(),
        };

        self.report_state();

        // TODO: Implement actual reconnection logic
        // For now, just fail after reporting state
        self.election_state = ElectionState::Failed {
            reason: "Reconnection not implemented yet".to_string(),
            failed_at: js_sys::Date::now(),
        };
        self.report_state();
    }

    /// Start 1Hz diagnostics reporting  
    fn start_diagnostics_reporting(&mut self) {
        // Note: Due to borrow checker constraints, diagnostics reporting
        // will be triggered externally through trigger_diagnostics_report()
        debug!("Diagnostics reporting initialized - will be triggered externally");
    }

    /// Process any queued RTT responses
    fn process_queued_rtt_responses(&mut self) {
        // First collect all responses to avoid borrow conflicts
        let responses_to_process: Vec<(String, f64)> =
            if let Ok(mut responses) = self.rtt_responses.try_borrow_mut() {
                responses.drain(..).collect()
            } else {
                Vec::new()
            };

        // Now process each response
        for (connection_id, _response_timestamp) in responses_to_process {
            if let Some(sent_timestamp) = self.rtt_start_times.get(&connection_id) {
                self.handle_rtt_response(&connection_id, *sent_timestamp);
                // Remove the timestamp since we've processed this response
                self.rtt_start_times.remove(&connection_id);
            } else {
                debug!(
                    "Received RTT response for {} but no sent timestamp found",
                    connection_id
                );
            }
        }
    }

    /// Trigger diagnostics reporting (to be called externally at 1Hz)
    pub fn trigger_diagnostics_report(&mut self) {
        debug!(
            "ConnectionManager::trigger_diagnostics_report called - state: {:?}",
            self.election_state
        );

        // First process any queued RTT responses
        self.process_queued_rtt_responses();

        // Then report diagnostics
        self.report_diagnostics();
    }

    /// Process RTT response packet (called externally when RTT responses are received)
    pub fn process_rtt_response(&mut self, connection_id: &str, _timestamp: f64) {
        if let Some(sent_timestamp) = self.rtt_start_times.get(connection_id) {
            self.handle_rtt_response(connection_id, *sent_timestamp);
            // Remove the timestamp since we've processed this response
            self.rtt_start_times.remove(connection_id);
        } else {
            debug!(
                "Received RTT response for {} but no sent timestamp found",
                connection_id
            );
        }
    }

    /// Report RTT metrics to diagnostics system
    fn report_diagnostics(&self) {
        debug!(
            "ConnectionManager::report_diagnostics - Active: {:?}, Election State: {:?}",
            self.active_connection_id, self.election_state
        );

        let mut metrics = Vec::new();

        // Report current election state
        match &self.election_state {
            ElectionState::Testing {
                start_time,
                duration_ms,
                ..
            } => {
                let elapsed = js_sys::Date::now() - start_time;
                let progress = (elapsed / *duration_ms as f64).min(1.0) as f32;
                metrics.push(metric!("election_state", "testing"));
                metrics.push(metric!("election_progress", progress as f64));
                metrics.push(metric!("servers_total", self.connections.len() as u64));

                // Send individual server events separately during testing
                // (Individual server metrics are sent as separate events below)
            }
            ElectionState::Elected {
                connection_id,
                elected_at,
            } => {
                metrics.push(metric!("election_state", "elected"));
                metrics.push(metric!("active_connection_id", connection_id.as_str()));
                metrics.push(metric!("elected_at", *elected_at));

                // Report active connection RTT
                if let Some(measurement) = self.rtt_measurements.get(connection_id) {
                    if let Some(avg_rtt) = measurement.average_rtt {
                        metrics.push(metric!("active_server_rtt", avg_rtt));
                        metrics.push(metric!("active_server_url", measurement.url.as_str()));
                        metrics.push(metric!(
                            "active_server_type",
                            if measurement.is_webtransport {
                                "webtransport"
                            } else {
                                "websocket"
                            }
                        ));
                    }
                }
            }

            ElectionState::Reconnecting {
                connection_id,
                attempt,
                max_attempts,
                ..
            } => {
                metrics.push(metric!("election_state", "reconnecting"));
                metrics.push(metric!("reconnect_connection_id", connection_id.as_str()));
                metrics.push(metric!("reconnect_attempt", *attempt as u64));
                metrics.push(metric!("reconnect_max_attempts", *max_attempts as u64));
            }
            ElectionState::Failed { reason, failed_at } => {
                metrics.push(metric!("election_state", "failed"));
                metrics.push(metric!("failure_reason", reason.as_str()));
                metrics.push(metric!("failed_at", *failed_at));
            }
        }

        // Send overall connection manager state
        debug!(
            "ConnectionManager: Prepared {} metrics for main event: {:?}",
            metrics.len(),
            metrics
        );
        if !metrics.is_empty() {
            let event = DiagEvent {
                subsystem: "connection_manager",
                stream_id: None,
                ts_ms: now_ms(),
                metrics,
            };

            debug!(
                "ConnectionManager: Sending main connection manager diagnostics event: {:?}",
                event
            );
            match global_sender().send(event) {
                Ok(_) => {
                    debug!("ConnectionManager: Successfully sent main connection manager diagnostics event");
                }
                Err(e) => {
                    error!(
                        "ConnectionManager: Failed to send main connection manager diagnostics: {}",
                        e
                    );
                }
            }
        } else {
            warn!("ConnectionManager: No metrics to send for main connection manager event - this might be why UI shows 'unknown'");
        }

        // Send individual server metrics as separate events
        for (connection_id, measurement) in &self.rtt_measurements {
            let connected = self
                .connections
                .get(connection_id)
                .map(|c| c.is_connected())
                .unwrap_or(false);

            let status = if measurement.active {
                "active"
            } else if connected {
                if measurement.average_rtt.is_some() {
                    "testing"
                } else {
                    "connected"
                }
            } else {
                "connecting"
            };

            let server_metrics = vec![
                metric!("server_url", measurement.url.as_str()),
                metric!(
                    "server_type",
                    if measurement.is_webtransport {
                        "webtransport"
                    } else {
                        "websocket"
                    }
                ),
                metric!("server_status", status),
                metric!("server_active", measurement.active as u64),
                metric!("server_connected", connected as u64),
                metric!("measurement_count", measurement.measurements.len() as u64),
            ];

            let mut final_metrics = server_metrics;
            if let Some(avg_rtt) = measurement.average_rtt {
                final_metrics.push(metric!("server_rtt", avg_rtt));
            }

            let event = DiagEvent {
                subsystem: "connection_manager",
                stream_id: Some(measurement.connection_id.clone()),
                ts_ms: now_ms(),
                metrics: final_metrics,
            };

            match global_sender().send(event) {
                Ok(_) => {
                    debug!(
                        "ConnectionManager: Successfully sent server diagnostics for {}",
                        measurement.connection_id
                    );
                }
                Err(e) => {
                    error!(
                        "ConnectionManager: Failed to send server diagnostics for {}: {}",
                        measurement.connection_id, e
                    );
                }
            }
        }
    }

    /// Report current state to callback
    fn report_state(&self) {
        let state = match &self.election_state {
            ElectionState::Testing {
                start_time,
                duration_ms,
                ..
            } => {
                let elapsed = js_sys::Date::now() - start_time;
                let progress = (elapsed / *duration_ms as f64).min(1.0) as f32;

                ConnectionState::Testing {
                    progress,
                    servers_tested: self.connections.len(),
                    total_servers: self.options.websocket_urls.len()
                        + self.options.webtransport_urls.len(),
                }
            }
            ElectionState::Elected { connection_id, .. } => {
                if let Some(measurement) = self.rtt_measurements.get(connection_id) {
                    ConnectionState::Connected {
                        server_url: measurement.url.clone(),
                        rtt: measurement.average_rtt.unwrap_or(0.0),
                        is_webtransport: measurement.is_webtransport,
                    }
                } else {
                    ConnectionState::Failed {
                        error: "Elected connection not found in measurements".to_string(),
                        last_known_server: None,
                    }
                }
            }
            ElectionState::Reconnecting {
                connection_id,
                attempt,
                max_attempts,
                ..
            } => {
                let server_url = self
                    .rtt_measurements
                    .get(connection_id)
                    .map(|m| m.url.clone())
                    .unwrap_or_else(|| "unknown".to_string());

                ConnectionState::Reconnecting {
                    server_url,
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                }
            }
            ElectionState::Failed { reason, .. } => ConnectionState::Failed {
                error: reason.clone(),
                last_known_server: self
                    .active_connection_id
                    .as_ref()
                    .and_then(|id| self.rtt_measurements.get(id))
                    .map(|m| m.url.clone()),
            },
        };

        self.options.on_state_changed.emit(state);
    }

    /// Send packet through active connection
    pub fn send_packet(&self, packet: PacketWrapper) -> Result<()> {
        if let Some(active_id) = &self.active_connection_id {
            if let Some(connection) = self.connections.get(active_id) {
                connection.send_packet(packet);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Set video enabled on active connection
    pub fn set_video_enabled(&self, enabled: bool) -> Result<()> {
        if let Some(active_id) = &self.active_connection_id {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_video_enabled(enabled);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Set audio enabled on active connection
    pub fn set_audio_enabled(&self, enabled: bool) -> Result<()> {
        if let Some(active_id) = &self.active_connection_id {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_audio_enabled(enabled);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Set screen enabled on active connection
    pub fn set_screen_enabled(&self, enabled: bool) -> Result<()> {
        if let Some(active_id) = &self.active_connection_id {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_screen_enabled(enabled);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Check if manager has an active connection
    pub fn is_connected(&self) -> bool {
        self.active_connection_id.is_some()
            && matches!(self.election_state, ElectionState::Elected { .. })
    }

    /// Get current RTT measurements (for debugging)
    pub fn get_rtt_measurements(&self) -> &HashMap<String, ServerRttMeasurement> {
        &self.rtt_measurements
    }

    /// Get current election state (for debugging)
    pub fn get_election_state(&self) -> &ElectionState {
        &self.election_state
    }

    /// Send RTT probes to all connected servers (can be called externally)
    pub fn send_rtt_probes(&mut self) -> Result<()> {
        for connection_id in self.connections.keys().cloned().collect::<Vec<_>>() {
            if let Err(e) = self.send_rtt_probe(&connection_id) {
                debug!("Failed to send RTT probe to {}: {}", connection_id, e);
            }
        }
        Ok(())
    }

    /// Check if election should be completed and do so if needed
    pub fn check_and_complete_election(&mut self) {
        if let ElectionState::Testing {
            start_time,
            duration_ms,
            ..
        } = &self.election_state
        {
            let elapsed = js_sys::Date::now() - start_time;
            if elapsed >= *duration_ms as f64 {
                self.complete_election();
            }
        }
    }

    /// Get current connection state for UI
    pub fn get_connection_state(&self) -> ConnectionState {
        match &self.election_state {
            ElectionState::Testing {
                start_time,
                duration_ms,
                ..
            } => {
                let elapsed = js_sys::Date::now() - start_time;
                let progress = (elapsed / *duration_ms as f64).min(1.0) as f32;

                ConnectionState::Testing {
                    progress,
                    servers_tested: self.connections.len(),
                    total_servers: self.options.websocket_urls.len()
                        + self.options.webtransport_urls.len(),
                }
            }
            ElectionState::Elected { connection_id, .. } => {
                if let Some(measurement) = self.rtt_measurements.get(connection_id) {
                    ConnectionState::Connected {
                        server_url: measurement.url.clone(),
                        rtt: measurement.average_rtt.unwrap_or(0.0),
                        is_webtransport: measurement.is_webtransport,
                    }
                } else {
                    ConnectionState::Failed {
                        error: "Elected connection not found in measurements".to_string(),
                        last_known_server: None,
                    }
                }
            }
            ElectionState::Reconnecting {
                connection_id,
                attempt,
                max_attempts,
                ..
            } => {
                let server_url = self
                    .rtt_measurements
                    .get(connection_id)
                    .map(|m| m.url.clone())
                    .unwrap_or_else(|| "unknown".to_string());

                ConnectionState::Reconnecting {
                    server_url,
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                }
            }
            ElectionState::Failed { reason, .. } => ConnectionState::Failed {
                error: reason.clone(),
                last_known_server: self
                    .active_connection_id
                    .as_ref()
                    .and_then(|id| self.rtt_measurements.get(id))
                    .map(|m| m.url.clone()),
            },
        }
    }
}

impl Drop for ConnectionManager {
    fn drop(&mut self) {
        // Clean up timers
        if let Some(reporter) = self.rtt_reporter.take() {
            reporter.cancel();
        }

        if let Some(probe_timer) = self.rtt_probe_timer.take() {
            probe_timer.cancel();
        }

        if let Some(election_timer) = self.election_timer.take() {
            election_timer.cancel();
        }

        if let ElectionState::Testing { probe_timer, .. } = &mut self.election_state {
            if let Some(timer) = probe_timer.take() {
                timer.cancel();
            }
        }
    }
}
