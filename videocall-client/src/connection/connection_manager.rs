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
use crate::adaptive_quality_constants::{
    RECONNECT_BACKOFF_MULTIPLIER, RECONNECT_INITIAL_DELAY_MS, RECONNECT_MAX_ATTEMPTS,
    RECONNECT_MAX_DELAY_MS, REELECTION_CONSECUTIVE_SAMPLES, REELECTION_RTT_MULTIPLIER,
};
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
use videocall_types::Callback;
use wasm_bindgen::JsValue;

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
    pub on_inbound_media: Callback<PacketWrapper>,
    pub on_state_changed: Callback<ConnectionState>,
    pub peer_monitor: Callback<()>,
    pub election_period_ms: u64,
}

/// Tracks the state of automatic reconnection after connection loss.
#[derive(Debug, Clone, PartialEq)]
pub enum ReconnectionPhase {
    /// No reconnection in progress; the connection is healthy or has not been established.
    Idle,
    /// Actively attempting to reconnect after a connection loss.
    Reconnecting { attempt: u32, next_delay_ms: u64 },
    /// All reconnection attempts exhausted; the connection is permanently failed.
    Failed,
}

#[derive(Debug)]
pub struct ConnectionManager {
    connections: HashMap<String, Connection>,
    active_connection_id: Rc<RefCell<Option<String>>>,
    rtt_measurements: HashMap<String, ServerRttMeasurement>,
    election_state: ElectionState,
    rtt_reporter: Option<Interval>,
    rtt_probe_timer: Option<Interval>,
    election_timer: Option<Interval>,
    rtt_responses: Rc<RefCell<Vec<(String, MediaPacket, f64)>>>,
    options: ConnectionManagerOptions,
    aes: Rc<Aes128State>,
    own_session_id: Rc<RefCell<Option<u64>>>,
    /// Per-connection session_ids received via SESSION_ASSIGNED before election completes.
    pending_session_ids: Rc<RefCell<HashMap<String, u64>>>,

    // --- Reconnection state ---
    reconnection_phase: Rc<RefCell<ReconnectionPhase>>,

    // --- Re-election state (RTT quality monitoring) ---
    /// The average RTT of the elected connection at the time of election.
    baseline_rtt: Option<f64>,
    /// Number of consecutive 1-Hz RTT samples that exceeded the degradation threshold.
    degradation_counter: u32,
    /// Whether a re-election is currently in progress (prevents overlapping re-elections).
    reelection_in_progress: bool,
}

impl ConnectionManager {
    /// Create a new ConnectionManager and immediately start testing all connections
    pub fn new(options: ConnectionManagerOptions, aes: Rc<Aes128State>) -> Result<Self> {
        let total_servers = options.websocket_urls.len() + options.webtransport_urls.len();

        if total_servers == 0 {
            return Err(anyhow!("No servers provided for connection testing"));
        }

        info!("ConnectionManager starting with {total_servers} servers");

        let rtt_responses = Rc::new(RefCell::new(Vec::new()));

        let mut manager = Self {
            connections: HashMap::new(),
            active_connection_id: Rc::new(RefCell::new(None)),
            rtt_measurements: HashMap::new(),
            election_state: ElectionState::Failed {
                reason: "Not started".to_string(),
                failed_at: js_sys::Date::now(),
            },
            rtt_reporter: None,
            rtt_probe_timer: None,
            election_timer: None,
            rtt_responses,
            options,
            aes,
            own_session_id: Rc::new(RefCell::new(None)),
            pending_session_ids: Rc::new(RefCell::new(HashMap::new())),
            reconnection_phase: Rc::new(RefCell::new(ReconnectionPhase::Idle)),
            baseline_rtt: None,
            degradation_counter: 0,
            reelection_in_progress: false,
        };

        // Immediately start creating connections and testing
        manager.start_election()?;

        Ok(manager)
    }

    /// Start the election process by creating all connections upfront
    fn start_election(&mut self) -> Result<()> {
        let election_duration = self.options.election_period_ms;
        let start_time = js_sys::Date::now();

        info!("Starting connection election for {election_duration}ms");

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
            let conn_id = format!("ws_{i}");
            let connect_options = ConnectOptions {
                websocket_url: url.clone(),
                webtransport_url: String::new(), // Not used for WebSocket
                on_inbound_media: self.create_inbound_media_callback(conn_id.clone()),
                on_connected: self.create_connected_callback(conn_id.clone()),
                on_connection_lost: self
                    .create_connection_lost_callback(conn_id.clone(), url.clone()),
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
                    debug!("Created WebSocket connection {conn_id}: {url}");
                }
                Err(e) => {
                    error!("Failed to create WebSocket connection to {url}: {e}");
                }
            }
        }

        // Create WebTransport connections
        for (i, url) in self.options.webtransport_urls.iter().enumerate() {
            let conn_id = format!("wt_{i}");
            let connect_options = ConnectOptions {
                websocket_url: String::new(), // Not used for WebTransport
                webtransport_url: url.clone(),
                on_inbound_media: self.create_inbound_media_callback(conn_id.clone()),
                on_connected: self.create_connected_callback(conn_id.clone()),
                on_connection_lost: self
                    .create_connection_lost_callback(conn_id.clone(), url.clone()),
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
                    debug!("Created WebTransport connection {conn_id}: {url}");
                }
                Err(e) => {
                    error!("Failed to create WebTransport connection to {url}: {e}");
                }
            }
        }

        info!("Created {} connections for testing", self.connections.len());

        // If only one connection was created, we still need to wait for it to be established
        // Don't force immediate election - let the normal process work
        if self.connections.len() == 1 {
            info!("Only one connection created, waiting for it to be established before election");
        }

        Ok(())
    }

    /// Create callback for handling inbound media packets
    fn create_inbound_media_callback(&self, connection_id: String) -> Callback<PacketWrapper> {
        let userid = self.options.userid.clone();
        let aes = self.aes.clone();
        let on_inbound_media = self.options.on_inbound_media.clone();
        let rtt_responses = self.rtt_responses.clone();
        let own_session_id = self.own_session_id.clone();
        let pending_session_ids = self.pending_session_ids.clone();
        let active_connection_id = self.active_connection_id.clone();

        Callback::from(move |packet: PacketWrapper| {
            // Intercept SESSION_ASSIGNED before anything else
            if packet.packet_type == PacketType::SESSION_ASSIGNED.into() {
                let sid = packet.session_id;
                info!(
                    "SESSION_ASSIGNED received on connection {}: {}",
                    connection_id, sid
                );

                let is_elected = active_connection_id
                    .borrow()
                    .as_deref()
                    .map(|id| id == connection_id)
                    .unwrap_or(false);

                if is_elected {
                    info!("Applying SESSION_ASSIGNED immediately (connection already elected)");
                    *own_session_id.borrow_mut() = Some(sid);
                    on_inbound_media.emit(packet);
                } else {
                    pending_session_ids
                        .borrow_mut()
                        .insert(connection_id.clone(), sid);
                }
                return;
            }

            // Handle RTT responses internally
            if packet.user_id[..] == *userid.as_bytes() {
                let reception_time = js_sys::Date::now();
                if let Ok(decrypted_data) = aes.decrypt(&packet.data) {
                    if let Ok(media_packet) = MediaPacket::parse_from_bytes(&decrypted_data) {
                        if media_packet.media_type == MediaType::RTT.into() {
                            debug!(
                                "RTT response received on connection {} at {}, sent at {}",
                                connection_id, reception_time, media_packet.timestamp
                            );
                            if let Ok(mut responses) = rtt_responses.try_borrow_mut() {
                                responses.push((
                                    connection_id.clone(),
                                    media_packet,
                                    reception_time,
                                ));
                            } else {
                                warn!("Unable to add RTT response to queue - queue is borrowed");
                            }
                            return;
                        }
                    }
                }
            }

            // Filter self-packets using session_id
            if let Some(own_id) = *own_session_id.borrow() {
                if packet.session_id != 0 && packet.session_id == own_id {
                    debug!(
                        "Rejecting packet from same session_id: {}",
                        packet.session_id
                    );
                    return;
                }
            }

            // Only forward packets from the elected connection.
            // During the election period (active_connection_id is None), all
            // connections forward packets so that RTT probes work and the
            // first SESSION_ASSIGNED can be processed.
            if let Some(ref elected_id) = *active_connection_id.borrow() {
                if *elected_id != connection_id {
                    return;
                }
            }

            on_inbound_media.emit(packet);
        })
    }

    /// Create callback for connection established
    fn create_connected_callback(&self, connection_id: String) -> Callback<()> {
        Callback::from(move |_| {
            debug!("Connection {connection_id} established");
        })
    }

    /// Create callback for connection lost.
    ///
    /// When the active connection is lost, this triggers the automatic reconnection
    /// state machine instead of simply emitting a `Failed` state. The reconnection
    /// logic runs asynchronously with exponential backoff, re-creating connections
    /// and re-running server election on each attempt.
    fn create_connection_lost_callback(
        &self,
        connection_id: String,
        server_url: String,
    ) -> Callback<JsValue> {
        let on_state_changed = self.options.on_state_changed.clone();
        let active_connection_id = self.active_connection_id.clone();
        let reconnection_phase = self.reconnection_phase.clone();

        // Capture everything needed to rebuild connections during reconnection.
        let options = self.options.clone();
        let aes = self.aes.clone();
        let own_session_id = self.own_session_id.clone();

        Callback::from(move |error| {
            warn!("Connection {connection_id} lost: {error:?}");

            // Only react if this was the active connection.
            if Some(connection_id.as_str()) != active_connection_id.borrow().as_deref() {
                info!(
                    "Non-active connection lost: {connection_id}, current active: {:?}",
                    active_connection_id.borrow()
                );
                return;
            }

            // Clear the active connection so is_connected() returns false immediately.
            *active_connection_id.borrow_mut() = None;

            // If a reconnection is already in progress, do not start another one.
            {
                let phase = reconnection_phase.borrow();
                if matches!(*phase, ReconnectionPhase::Reconnecting { .. }) {
                    info!("Reconnection already in progress, ignoring duplicate connection-lost event");
                    return;
                }
            }

            // Transition to Reconnecting and notify the UI.
            *reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
                attempt: 0,
                next_delay_ms: RECONNECT_INITIAL_DELAY_MS,
            };

            on_state_changed.emit(ConnectionState::Reconnecting {
                server_url: server_url.clone(),
                attempt: 1,
                max_attempts: RECONNECT_MAX_ATTEMPTS,
            });

            info!(
                "Active connection lost, starting automatic reconnection (max {} attempts)",
                RECONNECT_MAX_ATTEMPTS
            );

            // Launch the async reconnection loop.
            let reconnection_phase_clone = reconnection_phase.clone();
            let active_connection_id_clone = active_connection_id.clone();
            let on_state_changed_clone = on_state_changed.clone();
            let server_url_clone = server_url.clone();
            let options_clone = options.clone();
            let aes_clone = aes.clone();
            let own_session_id_clone = own_session_id.clone();

            wasm_bindgen_futures::spawn_local(async move {
                ConnectionManager::run_reconnection_loop(
                    reconnection_phase_clone,
                    active_connection_id_clone,
                    on_state_changed_clone,
                    server_url_clone,
                    options_clone,
                    aes_clone,
                    own_session_id_clone,
                )
                .await;
            });
        })
    }

    /// Send RTT probe to a specific connection
    fn send_rtt_probe(&mut self, connection_id: &str) -> Result<()> {
        let connection = self
            .connections
            .get(connection_id)
            .ok_or_else(|| anyhow!("Connection {connection_id} not found"))?;

        if !connection.is_connected() {
            return Ok(()); // Skip non-connected connections
        }

        // Update connection status
        if let Some(measurement) = self.rtt_measurements.get_mut(connection_id) {
            measurement.connected = true;
        }

        let timestamp = js_sys::Date::now();
        let rtt_packet = self.create_rtt_packet(timestamp)?;

        connection.send_packet(rtt_packet);
        debug!("Sent RTT probe to {connection_id} at timestamp {timestamp}");
        Ok(())
    }

    /// Create an RTT probe packet
    fn create_rtt_packet(&self, timestamp: f64) -> Result<PacketWrapper> {
        let media_packet = MediaPacket {
            media_type: MediaType::RTT.into(),
            user_id: self.options.userid.as_bytes().to_vec(),
            timestamp,
            ..Default::default()
        };

        let data = self.aes.encrypt(&media_packet.write_to_bytes()?)?;
        Ok(PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            user_id: self.options.userid.as_bytes().to_vec(),
            data,
            ..Default::default()
        })
    }

    /// Handle RTT response and calculate round-trip time
    fn handle_rtt_response(
        &mut self,
        connection_id: &str,
        media_packet: &MediaPacket,
        reception_time: f64,
    ) {
        let sent_timestamp = media_packet.timestamp;
        let rtt = reception_time - sent_timestamp;

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

                self.active_connection_id
                    .borrow_mut()
                    .replace(connection_id.clone());

                // Mark as active
                if let Some(mut_measurement) = self.rtt_measurements.get_mut(&connection_id) {
                    mut_measurement.active = true;
                }

                self.election_state = ElectionState::Elected {
                    connection_id: connection_id.clone(),
                    elected_at: js_sys::Date::now(),
                };

                // Apply pending session_id for the elected connection
                if let Some(sid) = self
                    .pending_session_ids
                    .borrow()
                    .get(&connection_id)
                    .copied()
                {
                    if *self.own_session_id.borrow() == Some(sid) {
                        debug!(
                            "Pending SESSION_ASSIGNED already processed for session {}, skipping",
                            sid
                        );
                    } else {
                        info!(
                            "Applying pending SESSION_ASSIGNED for elected connection {}: {}",
                            connection_id, sid
                        );
                        *self.own_session_id.borrow_mut() = Some(sid);

                        if let Some(connection) = self.connections.get(&connection_id) {
                            connection.set_session_id(sid);
                        }
                    }
                }
                self.pending_session_ids.borrow_mut().clear();

                // Start heartbeat only on the elected connection
                if let Some(connection) = self.connections.get_mut(&connection_id) {
                    connection.start_heartbeat(self.options.userid.clone());
                    info!("Started heartbeat on elected connection {}", connection_id);
                }

                // Store baseline RTT for re-election quality monitoring.
                self.baseline_rtt = measurement.average_rtt;
                self.degradation_counter = 0;
                self.reelection_in_progress = false;

                if let Some(rtt) = self.baseline_rtt {
                    info!("Baseline RTT for re-election monitoring: {rtt:.1}ms");
                }

                // Close unused connections
                self.close_unused_connections();

                // Report state
                self.report_state();
            }
            Err(e) => {
                error!("Election failed: {e}");
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
        // We run two passes: first look exclusively at WebTransport connections.
        // Only if none of them are usable do we fall back to WebSocket.

        let mut best_wt: Option<(String, ServerRttMeasurement)> = None;
        let mut best_wt_rtt = f64::INFINITY;

        let mut best_ws: Option<(String, ServerRttMeasurement)> = None;
        let mut best_ws_rtt = f64::INFINITY;

        for (connection_id, measurement) in &self.rtt_measurements {
            // Skip connections that are not yet fully established
            if let Some(conn) = self.connections.get(connection_id) {
                if !conn.is_connected() {
                    continue;
                }
            }

            if let Some(avg_rtt) = measurement.average_rtt {
                if measurement.measurements.is_empty() {
                    continue;
                }

                if measurement.is_webtransport {
                    if avg_rtt < best_wt_rtt {
                        best_wt_rtt = avg_rtt;
                        best_wt = Some((connection_id.clone(), measurement.clone()));
                    }
                } else if avg_rtt < best_ws_rtt {
                    best_ws_rtt = avg_rtt;
                    best_ws = Some((connection_id.clone(), measurement.clone()));
                }
            }
        }

        if let Some(best) = best_wt {
            return Ok(best);
        }

        best_ws.ok_or_else(|| anyhow!("No valid connections with RTT measurements found"))
    }

    /// Close all unused connections after election
    fn close_unused_connections(&mut self) {
        let active_connection_borrow = self.active_connection_id.borrow();
        let active_id = active_connection_borrow.as_deref();
        let mut to_remove = Vec::new();

        for connection_id in self.connections.keys() {
            if Some(connection_id.as_str()) != active_id {
                to_remove.push(connection_id.clone());
            }
        }

        for connection_id in to_remove {
            self.connections.remove(&connection_id);
            info!("Closed unused connection: {connection_id}");
        }
    }

    // -----------------------------------------------------------------------
    // Automatic Reconnection
    // -----------------------------------------------------------------------

    /// Asynchronous reconnection loop with exponential backoff.
    ///
    /// This is a standalone async function launched via `spawn_local` so it does not
    /// require `&mut self`. It communicates with the rest of the system through shared
    /// `Rc<RefCell<…>>` state and callbacks.
    async fn run_reconnection_loop(
        reconnection_phase: Rc<RefCell<ReconnectionPhase>>,
        active_connection_id: Rc<RefCell<Option<String>>>,
        on_state_changed: Callback<ConnectionState>,
        last_server_url: String,
        options: ConnectionManagerOptions,
        aes: Rc<Aes128State>,
        own_session_id: Rc<RefCell<Option<u64>>>,
    ) {
        let mut attempt: u32 = 0;
        let mut delay_ms: u64 = RECONNECT_INITIAL_DELAY_MS;

        loop {
            attempt += 1;
            if attempt > RECONNECT_MAX_ATTEMPTS {
                break;
            }

            info!(
                "Reconnection attempt {}/{} — waiting {}ms",
                attempt, RECONNECT_MAX_ATTEMPTS, delay_ms
            );

            // Update phase and emit state so the UI can show progress.
            *reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
                attempt,
                next_delay_ms: delay_ms,
            };
            on_state_changed.emit(ConnectionState::Reconnecting {
                server_url: last_server_url.clone(),
                attempt,
                max_attempts: RECONNECT_MAX_ATTEMPTS,
            });

            // Wait with exponential backoff.
            gloo_timers::future::sleep(std::time::Duration::from_millis(delay_ms)).await;

            // Check if something else already reconnected us (e.g. re-election).
            if active_connection_id.borrow().is_some() {
                info!("Connection restored externally during reconnection wait — aborting loop");
                *reconnection_phase.borrow_mut() = ReconnectionPhase::Idle;
                return;
            }

            // Try to create a fresh ConnectionManager and run election.
            match ConnectionManager::new(options.clone(), aes.clone()) {
                Ok(mut fresh_manager) => {
                    // Restore preserved session state.
                    if let Some(sid) = *own_session_id.borrow() {
                        fresh_manager.set_own_session_id(sid);
                    }

                    // Give the fresh manager its election period to complete.
                    let election_ms = options.election_period_ms;
                    gloo_timers::future::sleep(std::time::Duration::from_millis(election_ms + 500))
                        .await;

                    // Drive election completion manually.
                    fresh_manager.process_queued_rtt_responses();
                    fresh_manager.check_and_complete_election();

                    if fresh_manager.is_connected() {
                        info!("Reconnection successful on attempt {attempt}");

                        // Copy the elected connection id into the shared state.
                        if let Some(new_id) = fresh_manager.active_connection_id.borrow().clone() {
                            *active_connection_id.borrow_mut() = Some(new_id);
                        }

                        *reconnection_phase.borrow_mut() = ReconnectionPhase::Idle;

                        // Emit Connected state so the UI updates.
                        let state = fresh_manager.get_connection_state();
                        on_state_changed.emit(state);
                        return;
                    }

                    warn!("Reconnection attempt {attempt} failed — election did not succeed");
                }
                Err(e) => {
                    warn!("Reconnection attempt {attempt} failed to create manager: {e}");
                }
            }

            // Exponential backoff for next attempt.
            delay_ms = ((delay_ms as f64 * RECONNECT_BACKOFF_MULTIPLIER) as u64)
                .min(RECONNECT_MAX_DELAY_MS);
        }

        // All attempts exhausted.
        error!(
            "Reconnection failed after {} attempts — giving up",
            RECONNECT_MAX_ATTEMPTS
        );

        *reconnection_phase.borrow_mut() = ReconnectionPhase::Failed;
        on_state_changed.emit(ConnectionState::Failed {
            error: format!(
                "Reconnection failed after {} attempts",
                RECONNECT_MAX_ATTEMPTS
            ),
            last_known_server: Some(last_server_url),
        });
    }

    /// Returns the current reconnection phase.
    /// Used by ConnectionController and UI consumers to display reconnection status.
    #[allow(dead_code)]
    pub fn reconnection_phase(&self) -> ReconnectionPhase {
        self.reconnection_phase.borrow().clone()
    }

    // -----------------------------------------------------------------------
    // Connection Quality Re-election
    // -----------------------------------------------------------------------

    /// Called at 1 Hz (from ConnectionController) after election, to check whether
    /// the active connection's RTT has degraded enough to warrant a new election.
    ///
    /// Returns `true` if a re-election should be triggered.
    pub fn check_rtt_degradation(&mut self) -> bool {
        // Only check when we have a baseline and are in Elected state.
        let baseline = match self.baseline_rtt {
            Some(b) if b > 0.0 => b,
            _ => return false,
        };

        if self.reelection_in_progress {
            return false;
        }

        let active_id = match self.active_connection_id.borrow().clone() {
            Some(id) => id,
            None => return false,
        };

        let current_rtt = self
            .rtt_measurements
            .get(&active_id)
            .and_then(|m| m.average_rtt);

        let current_rtt = match current_rtt {
            Some(rtt) => rtt,
            None => return false,
        };

        let threshold = baseline * REELECTION_RTT_MULTIPLIER;

        if current_rtt > threshold {
            self.degradation_counter += 1;
            info!(
                "RTT degradation: current={:.1}ms baseline={:.1}ms threshold={:.1}ms (count={}/{})",
                current_rtt,
                baseline,
                threshold,
                self.degradation_counter,
                REELECTION_CONSECUTIVE_SAMPLES,
            );

            if self.degradation_counter >= REELECTION_CONSECUTIVE_SAMPLES {
                info!(
                    "RTT degradation threshold reached ({} consecutive samples) — triggering re-election",
                    REELECTION_CONSECUTIVE_SAMPLES
                );
                return true;
            }
        } else {
            // RTT is acceptable — reset counter.
            if self.degradation_counter > 0 {
                debug!(
                    "RTT recovered: current={:.1}ms baseline={:.1}ms — resetting degradation counter",
                    current_rtt, baseline
                );
                self.degradation_counter = 0;
            }
        }

        false
    }

    /// Begin a re-election: create new connections to all servers while keeping the
    /// current active connection alive. Once election completes, switch to the new
    /// best server seamlessly.
    pub fn start_reelection(&mut self) -> Result<()> {
        if self.reelection_in_progress {
            info!("Re-election already in progress, skipping");
            return Ok(());
        }

        info!("Starting connection quality re-election");
        self.reelection_in_progress = true;
        self.degradation_counter = 0;

        // Preserve the current active connection — do NOT close it yet.
        // Create fresh connections to all servers for testing.
        self.create_all_connections()?;

        // Reset election state to Testing so the normal election flow runs.
        let start_time = js_sys::Date::now();
        self.election_state = ElectionState::Testing {
            start_time,
            duration_ms: self.options.election_period_ms,
            probe_timer: None,
        };

        Ok(())
    }

    /// Returns whether a re-election is currently in progress.
    /// Used by ConnectionController and UI consumers to check re-election status.
    #[allow(dead_code)]
    pub fn is_reelection_in_progress(&self) -> bool {
        self.reelection_in_progress
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
        let responses_to_process: Vec<(String, MediaPacket, f64)> =
            if let Ok(mut responses) = self.rtt_responses.try_borrow_mut() {
                responses.drain(..).collect()
            } else {
                Vec::new()
            };

        // Now process each response
        for (connection_id, media_packet, reception_time) in responses_to_process {
            self.handle_rtt_response(&connection_id, &media_packet, reception_time);
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

    /// Report RTT metrics to diagnostics system
    fn report_diagnostics(&self) {
        debug!(
            "ConnectionManager::report_diagnostics - Active: {:?}, Election State: {:?}",
            self.active_connection_id.borrow(),
            self.election_state
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
                "ConnectionManager: Sending main connection manager diagnostics event: {event:?}"
            );
            match global_sender().try_broadcast(event) {
                Ok(_) => {
                    debug!("ConnectionManager: Successfully sent main connection manager diagnostics event");
                }
                Err(e) => {
                    error!(
                        "ConnectionManager: Failed to send main connection manager diagnostics: {e}"
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

            match global_sender().try_broadcast(event) {
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
            ElectionState::Failed { reason, .. } => ConnectionState::Failed {
                error: reason.clone(),
                last_known_server: self
                    .active_connection_id
                    .borrow()
                    .as_deref()
                    .and_then(|id| self.rtt_measurements.get(id))
                    .map(|m| m.url.clone()),
            },
        };

        self.options.on_state_changed.emit(state);
    }

    /// Send packet through active connection via reliable stream.
    pub fn send_packet(&self, packet: PacketWrapper) -> Result<()> {
        if let Some(active_id) = self.active_connection_id.borrow().as_deref() {
            if let Some(connection) = self.connections.get(active_id) {
                connection.send_packet(packet);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Send packet through active connection via datagram (unreliable, low-latency).
    ///
    /// Used for media packets (VIDEO, AUDIO, SCREEN) where low latency matters
    /// more than guaranteed delivery. Falls back to reliable stream for
    /// WebSocket connections or oversized packets.
    pub fn send_packet_datagram(&self, packet: PacketWrapper) -> Result<()> {
        if let Some(active_id) = self.active_connection_id.borrow().as_deref() {
            if let Some(connection) = self.connections.get(active_id) {
                connection.send_packet_datagram(packet);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Set video enabled on active connection
    pub fn set_video_enabled(&self, enabled: bool) -> Result<()> {
        if let Some(active_id) = self.active_connection_id.borrow().as_deref() {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_video_enabled(enabled);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Set audio enabled on active connection
    pub fn set_audio_enabled(&self, enabled: bool) -> Result<()> {
        if let Some(active_id) = self.active_connection_id.borrow().as_deref() {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_audio_enabled(enabled);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Set screen enabled on active connection
    pub fn set_screen_enabled(&self, enabled: bool) -> Result<()> {
        if let Some(active_id) = self.active_connection_id.borrow().as_deref() {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_screen_enabled(enabled);
                return Ok(());
            }
        }

        Err(anyhow!("No active connection available"))
    }

    /// Set speaking on active connection
    pub fn set_speaking(&self, speaking: bool) {
        if let Some(active_id) = self.active_connection_id.borrow().as_deref() {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_speaking(speaking);
            }
        }
    }

    /// Set own session_id for filtering self-packets and stamp outgoing heartbeats
    pub fn set_own_session_id(&self, session_id: u64) {
        *self.own_session_id.borrow_mut() = Some(session_id);

        if let Some(active_id) = self.active_connection_id.borrow().as_deref() {
            if let Some(connection) = self.connections.get(active_id) {
                connection.set_session_id(session_id);
            }
        }
        debug!("Set own_session_id to {session_id}");
    }

    /// Check if manager has an active connection
    pub fn is_connected(&self) -> bool {
        self.active_connection_id.borrow().is_some()
            && matches!(self.election_state, ElectionState::Elected { .. })
    }

    pub fn disconnect(&mut self) -> anyhow::Result<()> {
        self.connections.clear();
        self.get_connection_state();
        Ok(())
    }

    /// Get current RTT measurements (for debugging)
    pub fn get_rtt_measurements(&self) -> &HashMap<String, ServerRttMeasurement> {
        &self.rtt_measurements
    }

    /// Send RTT probes to all connected servers (can be called externally)
    pub fn send_rtt_probes(&mut self) -> Result<()> {
        for connection_id in self.connections.keys().cloned().collect::<Vec<_>>() {
            if let Err(e) = self.send_rtt_probe(&connection_id) {
                debug!("Failed to send RTT probe to {connection_id}: {e}");
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
            ElectionState::Failed { reason, .. } => ConnectionState::Failed {
                error: reason.clone(),
                last_known_server: self
                    .active_connection_id
                    .borrow()
                    .as_deref()
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
