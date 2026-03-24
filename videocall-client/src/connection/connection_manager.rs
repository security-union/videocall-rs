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
    RECONNECT_BACKOFF_MULTIPLIER, RECONNECT_CONSECUTIVE_ZERO_LIMIT, RECONNECT_INITIAL_DELAY_MS,
    RECONNECT_MAX_DELAY_MS, REELECTION_CONSECUTIVE_SAMPLES, REELECTION_RTT_MULTIPLIER,
};
use crate::crypto::aes::Aes128State;
use anyhow::{anyhow, Result};
use gloo::timers::callback::Interval;
use log::{debug, error, info, warn};
use protobuf::Message;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
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

    /// Weak self-reference set by `ConnectionController` after construction.
    /// Used by the reconnection loop to call `reset_and_start_election` on the
    /// real manager instance instead of creating a throwaway one.
    manager_ref: Weak<RefCell<ConnectionManager>>,

    // --- Re-election state (RTT quality monitoring) ---
    /// The average RTT of the elected connection at the time of election.
    baseline_rtt: Option<f64>,
    /// Number of consecutive 1-Hz RTT samples that exceeded the degradation threshold.
    degradation_counter: u32,
    /// Whether a re-election is currently in progress (prevents overlapping re-elections).
    reelection_in_progress: bool,
    /// Set to `true` when the user explicitly calls `disconnect()`. Checked by
    /// the reconnection loop to prevent reconnecting after an intentional leave.
    intentionally_disconnected: Rc<Cell<bool>>,
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

        let manager = Self {
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
            manager_ref: Weak::new(),
            baseline_rtt: None,
            degradation_counter: 0,
            reelection_in_progress: false,
            intentionally_disconnected: Rc::new(Cell::new(false)),
        };

        Ok(manager)
    }

    /// Store a weak self-reference so that reconnection callbacks can access
    /// the real manager instance. Called by `ConnectionController` after construction.
    pub fn set_manager_ref(&mut self, weak: Weak<RefCell<ConnectionManager>>) {
        self.manager_ref = weak;
    }

    /// Kick off the initial server election. Must be called **after**
    /// `set_manager_ref()` so that the connection-lost callbacks capture a
    /// valid `Weak` back-reference to the owning `Rc<RefCell<ConnectionManager>>`.
    pub fn initialize(&mut self) -> Result<()> {
        self.start_election()
    }

    /// Reset all connection state and start a fresh election on the same manager
    /// instance. This preserves the shared `Rc` state (callbacks, session info,
    /// `active_connection_id`, etc.) so that inbound packet handlers, heartbeats,
    /// and the `ConnectionController` timers keep working correctly.
    ///
    /// Called by the reconnection loop instead of creating a throwaway
    /// `ConnectionManager`.
    pub fn reset_and_start_election(&mut self) -> Result<()> {
        info!("Resetting connections and starting fresh election for reconnection");

        // Drop all existing connections (stops heartbeats, closes transports).
        self.connections.clear();

        // Clear RTT measurements so the new election starts clean.
        self.rtt_measurements.clear();

        // Drain any stale RTT responses from the previous connections.
        if let Ok(mut responses) = self.rtt_responses.try_borrow_mut() {
            responses.clear();
        }

        // Clear pending session IDs from previous connections.
        if let Ok(mut pending) = self.pending_session_ids.try_borrow_mut() {
            pending.clear();
        }

        // Reset active connection — the election will set a new one.
        *self.active_connection_id.borrow_mut() = None;

        // Reset re-election monitoring state.
        self.baseline_rtt = None;
        self.degradation_counter = 0;
        self.reelection_in_progress = false;

        // Cancel any lingering timers from the previous election.
        if let ElectionState::Testing { probe_timer, .. } = &mut self.election_state {
            if let Some(timer) = probe_timer.take() {
                timer.cancel();
            }
        }

        // Start fresh election — creates new connections and begins RTT probing.
        self.start_election()
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
    /// logic runs asynchronously with exponential backoff, calling
    /// `reset_and_start_election` on the **same** manager instance so that
    /// packet pipelines, callbacks, and session state remain intact.
    fn create_connection_lost_callback(
        &self,
        connection_id: String,
        server_url: String,
    ) -> Callback<JsValue> {
        let on_state_changed = self.options.on_state_changed.clone();
        let active_connection_id = self.active_connection_id.clone();
        let reconnection_phase = self.reconnection_phase.clone();
        let manager_ref = self.manager_ref.clone();
        let election_period_ms = self.options.election_period_ms;
        let intentionally_disconnected = self.intentionally_disconnected.clone();

        Callback::from(move |error| {
            warn!("Connection {connection_id} lost: {error:?}");

            // If the user explicitly called disconnect(), do not attempt reconnection.
            if intentionally_disconnected.get() {
                info!("Connection lost after intentional disconnect — not reconnecting");
                return;
            }

            // Only react if this was the active connection.
            // After intentional disconnect(), active_connection_id is already None,
            // so this guard catches that case too (Some(id) != None → return).
            if Some(connection_id.as_str()) != active_connection_id.borrow().as_deref() {
                info!(
                    "Non-active connection lost: {connection_id}, current active: {:?}",
                    active_connection_id.borrow()
                );
                return;
            }

            *active_connection_id.borrow_mut() = None;

            {
                let phase = reconnection_phase.borrow();
                if matches!(*phase, ReconnectionPhase::Reconnecting { .. }) {
                    info!("Reconnection already in progress, ignoring duplicate connection-lost event");
                    return;
                }
            }

            *reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
                attempt: 0,
                next_delay_ms: RECONNECT_INITIAL_DELAY_MS,
            };

            on_state_changed.emit(ConnectionState::Reconnecting {
                server_url: server_url.clone(),
                attempt: 1,
            });

            info!("Active connection lost, starting automatic reconnection (unlimited retries with backoff)");

            let reconnection_phase_clone = reconnection_phase.clone();
            let active_connection_id_clone = active_connection_id.clone();
            let on_state_changed_clone = on_state_changed.clone();
            let server_url_clone = server_url.clone();
            let manager_ref_clone = manager_ref.clone();
            let intentionally_disconnected_clone = intentionally_disconnected.clone();

            wasm_bindgen_futures::spawn_local(async move {
                ConnectionManager::run_reconnection_loop(
                    reconnection_phase_clone,
                    active_connection_id_clone,
                    on_state_changed_clone,
                    server_url_clone,
                    manager_ref_clone,
                    election_period_ms,
                    intentionally_disconnected_clone,
                )
                .await;
            });
        })
    }

    /// Send RTT probe to a specific connection.
    ///
    /// RTT probes are periodic and expendable — a missed probe just means we
    /// skip one measurement. They use datagrams for lower overhead.
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

        connection.send_packet_datagram(rtt_packet);
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
        let candidates: Vec<_> = self
            .rtt_measurements
            .iter()
            .filter(|(id, _)| {
                self.connections
                    .get(*id)
                    .map(|c| c.is_connected())
                    .unwrap_or(false)
            })
            .map(|(id, m)| (id.as_str(), m))
            .collect();
        select_best_connection(&candidates)
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
    /// Uses a `Weak` reference to the real `ConnectionManager` (held by the
    /// `ConnectionController`) so that `reset_and_start_election` operates on
    /// the same instance. This ensures the new connections' inbound-media
    /// callbacks, heartbeat timers, and RTT probes all reference the same
    /// shared state used by the `ConnectionController` timers and the
    /// `VideoCallClient`'s packet pipeline.
    async fn run_reconnection_loop(
        reconnection_phase: Rc<RefCell<ReconnectionPhase>>,
        active_connection_id: Rc<RefCell<Option<String>>>,
        on_state_changed: Callback<ConnectionState>,
        last_server_url: String,
        manager_ref: Weak<RefCell<ConnectionManager>>,
        election_period_ms: u64,
        intentionally_disconnected: Rc<Cell<bool>>,
    ) {
        let mut attempt: u32 = 0;
        let mut delay_ms: u64 = RECONNECT_INITIAL_DELAY_MS;
        // Track consecutive attempts where zero servers respond. If this counter
        // reaches RECONNECT_CONSECUTIVE_ZERO_LIMIT we treat it as a likely
        // auth/server rejection and stop reconnecting immediately.
        let mut consecutive_zero_connections: u32 = 0;

        loop {
            // Check if user intentionally disconnected (e.g. left the meeting).
            if intentionally_disconnected.get() {
                info!("Reconnection loop cancelled — user disconnected intentionally");
                *reconnection_phase.borrow_mut() = ReconnectionPhase::Idle;
                return;
            }

            attempt += 1;

            info!("Reconnection attempt {} — waiting {}ms", attempt, delay_ms);

            // Update phase and emit state so the UI can show progress.
            *reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
                attempt,
                next_delay_ms: delay_ms,
            };
            on_state_changed.emit(ConnectionState::Reconnecting {
                server_url: last_server_url.clone(),
                attempt,
            });

            gloo_timers::future::sleep(std::time::Duration::from_millis(delay_ms)).await;

            // Re-check after sleep — user may have left during backoff.
            if intentionally_disconnected.get() {
                info!(
                    "Reconnection loop cancelled during backoff — user disconnected intentionally"
                );
                *reconnection_phase.borrow_mut() = ReconnectionPhase::Idle;
                return;
            }

            // Check if something else already reconnected us (e.g. re-election).
            if active_connection_id.borrow().is_some() {
                info!("Connection restored externally during reconnection wait — aborting loop");
                *reconnection_phase.borrow_mut() = ReconnectionPhase::Idle;
                return;
            }

            // Upgrade the weak reference to access the real manager.
            let manager_rc = match manager_ref.upgrade() {
                Some(rc) => rc,
                None => {
                    warn!("ConnectionManager was dropped during reconnection — aborting");
                    *reconnection_phase.borrow_mut() = ReconnectionPhase::Failed;
                    on_state_changed.emit(ConnectionState::Failed {
                        error: "Connection manager destroyed during reconnection".to_string(),
                        last_known_server: Some(last_server_url),
                    });
                    return;
                }
            };

            // Reset connections and start a fresh election on the SAME manager.
            // The borrow is scoped so it is released before the async sleep below.
            {
                match manager_rc.try_borrow_mut() {
                    Ok(mut mgr) => {
                        if let Err(e) = mgr.reset_and_start_election() {
                            warn!(
                                "Reconnection attempt {attempt} failed to reset connections: {e}"
                            );
                            // Fall through to backoff and retry.
                        }
                    }
                    Err(_) => {
                        warn!("Reconnection: could not borrow manager (busy), retrying in 200ms");
                        attempt -= 1; // undo the increment — borrow conflict is not a real attempt
                        gloo_timers::future::sleep(std::time::Duration::from_millis(200)).await;
                        continue;
                    }
                }
            }

            // Give the election period time to complete. The ConnectionController's
            // existing 200ms RTT probe timer and 100ms election-check timer will
            // drive the election automatically on the same manager instance.
            gloo_timers::future::sleep(std::time::Duration::from_millis(election_period_ms + 500))
                .await;

            // Check the result. Again scope the borrow tightly.
            // Borrow failure means unknown — treat as not-yet-connected and retry.
            let connected = manager_rc
                .try_borrow()
                .map(|mgr| mgr.is_connected())
                .unwrap_or(false);

            if connected {
                info!("Reconnection successful on attempt {attempt}");
                *reconnection_phase.borrow_mut() = ReconnectionPhase::Idle;

                // Emit the current Connected state so the UI updates.
                if let Ok(mgr) = manager_rc.try_borrow() {
                    on_state_changed.emit(mgr.get_connection_state());
                }
                return;
            }

            warn!("Reconnection attempt {attempt} failed — election did not succeed");

            // Track consecutive total failures (no server responded at all).
            // This pattern indicates auth rejection or server-side blocking
            // rather than a transient network issue.
            //
            // Check whether any connections were established during this attempt.
            // Only increment the zero-connection counter when the server truly did
            // not respond at all (likely auth rejection). If some connections were
            // made but election still failed (e.g. poor RTT), reset the counter.
            let any_connections = match manager_rc.try_borrow() {
                Ok(mgr) => Some(mgr.connections.values().any(|c| c.is_connected())),
                Err(_) => None, // borrow conflict — unknown, don't count
            };

            match any_connections {
                Some(true) => {
                    // Some servers responded — reset the zero-connection counter.
                    consecutive_zero_connections = 0;
                }
                Some(false) => {
                    consecutive_zero_connections += 1;
                }
                None => {
                    // Borrow conflict — neither increment nor reset.
                    warn!("Reconnection: could not check connection state (manager busy)");
                }
            }
            if consecutive_zero_connections >= RECONNECT_CONSECUTIVE_ZERO_LIMIT {
                error!(
                    "Reconnection aborted: {} consecutive attempts with zero successful connections \
                     — likely auth failure or server rejection",
                    consecutive_zero_connections
                );

                *reconnection_phase.borrow_mut() = ReconnectionPhase::Failed;
                on_state_changed.emit(ConnectionState::Failed {
                    error: format!(
                        "Server rejected connection ({} consecutive failures — possible auth/session error)",
                        consecutive_zero_connections
                    ),
                    last_known_server: Some(last_server_url),
                });
                return;
            }

            // Exponential backoff for next attempt.
            delay_ms = next_backoff_delay(
                delay_ms,
                RECONNECT_BACKOFF_MULTIPLIER,
                RECONNECT_MAX_DELAY_MS,
            );
        }
        // The loop only exits via `return`:
        //   (a) successful reconnection
        //   (b) intentional disconnect
        //   (c) consecutive zero-connection fast-fail (auth/server rejection)
        //   (d) manager dropped
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

    /// Begin a re-election: tear down existing connections, reset monitoring
    /// state, and create fresh connections to all servers. There is a brief
    /// disconnection period while the new election runs and selects a winner.
    pub fn start_reelection(&mut self) -> Result<()> {
        if self.reelection_in_progress {
            info!("Re-election already in progress, skipping");
            return Ok(());
        }

        info!("Starting connection quality re-election");
        self.reelection_in_progress = true;
        self.degradation_counter = 0;
        self.baseline_rtt = None;

        // Close the old active connection before creating new ones.
        // `create_all_connections` reuses the same connection IDs (ws_0, wt_0, ...),
        // so inserting into the HashMap would silently drop the old Connection.
        // We close explicitly here to ensure proper WebTransport session teardown
        // and to avoid leaking resources.
        let old_active_id = self.active_connection_id.borrow().clone();
        if let Some(ref id) = old_active_id {
            if let Some(old_conn) = self.connections.remove(id) {
                info!("Re-election: closing old active connection {id}");
                drop(old_conn);
            }
        }
        // Clear any remaining non-active connections too (from a previous election).
        self.connections.clear();
        *self.active_connection_id.borrow_mut() = None;

        // Clear stale RTT measurements so the new election starts clean.
        self.rtt_measurements.clear();

        // Drain stale RTT responses from the previous connections.
        if let Ok(mut responses) = self.rtt_responses.try_borrow_mut() {
            responses.clear();
        }

        // Clear pending session IDs — connection IDs are reused (ws_0, wt_0, …)
        // so stale entries could be incorrectly applied to new connections.
        if let Ok(mut pending) = self.pending_session_ids.try_borrow_mut() {
            pending.clear();
        }

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
    /// Used for control packets (heartbeats, RTT probes, diagnostics) that are
    /// periodic and expendable — lower overhead matters more than guaranteed
    /// delivery. Falls back to reliable stream for WebSocket connections or
    /// oversized packets.
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
        self.intentionally_disconnected.set(true);
        *self.reconnection_phase.borrow_mut() = ReconnectionPhase::Idle;
        // Must clear active_connection_id BEFORE connections.clear() so that
        // on_disconnect callbacks see None and don't start reconnection.
        *self.active_connection_id.borrow_mut() = None;
        self.connections.clear();
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

// -----------------------------------------------------------------------
// Pure helper functions extracted for testability
// -----------------------------------------------------------------------

/// Pure election algorithm: prefer WebTransport over WebSocket, lowest RTT wins.
///
/// Two-pass selection: first considers only WebTransport candidates, falling back
/// to WebSocket only if no valid WebTransport connections exist.
fn select_best_connection(
    candidates: &[(&str, &ServerRttMeasurement)],
) -> Result<(String, ServerRttMeasurement)> {
    let mut best_wt: Option<(String, ServerRttMeasurement)> = None;
    let mut best_wt_rtt = f64::INFINITY;

    let mut best_ws: Option<(String, ServerRttMeasurement)> = None;
    let mut best_ws_rtt = f64::INFINITY;

    for &(connection_id, measurement) in candidates {
        if let Some(avg_rtt) = measurement.average_rtt {
            if measurement.measurements.is_empty() {
                continue;
            }

            if measurement.is_webtransport {
                if avg_rtt < best_wt_rtt {
                    best_wt_rtt = avg_rtt;
                    best_wt = Some((connection_id.to_string(), measurement.clone()));
                }
            } else if avg_rtt < best_ws_rtt {
                best_ws_rtt = avg_rtt;
                best_ws = Some((connection_id.to_string(), measurement.clone()));
            }
        }
    }

    if let Some(best) = best_wt {
        return Ok(best);
    }

    best_ws.ok_or_else(|| anyhow!("No valid connections with RTT measurements found"))
}

/// Calculate the next backoff delay given the current delay and multiplier,
/// capped at `max_delay_ms`.
fn next_backoff_delay(current_delay_ms: u64, multiplier: f64, max_delay_ms: u64) -> u64 {
    ((current_delay_ms as f64 * multiplier) as u64).min(max_delay_ms)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adaptive_quality_constants::{
        RECONNECT_BACKOFF_MULTIPLIER, RECONNECT_CONSECUTIVE_ZERO_LIMIT, RECONNECT_INITIAL_DELAY_MS,
        RECONNECT_MAX_DELAY_MS, REELECTION_CONSECUTIVE_SAMPLES, REELECTION_RTT_MULTIPLIER,
    };

    // -----------------------------------------------------------------------
    // Helper: construct a ConnectionManager without starting an election.
    //
    // This bypasses `new()` which calls `start_election()` -> `create_all_connections()`
    // -> `Connection::connect()` which requires browser WebTransport/WebSocket APIs.
    // The resulting manager has no live connections but all the pure-logic state
    // is initialised, so we can unit-test `check_rtt_degradation`, `handle_rtt_response`,
    // `find_best_connection`, etc.
    // -----------------------------------------------------------------------
    fn make_test_manager() -> ConnectionManager {
        let options = ConnectionManagerOptions {
            websocket_urls: vec![],
            webtransport_urls: vec![],
            userid: "test-user".to_string(),
            on_inbound_media: Callback::from(|_: PacketWrapper| {}),
            on_state_changed: Callback::from(|_: ConnectionState| {}),
            peer_monitor: Callback::from(|_: ()| {}),
            election_period_ms: 3000,
        };

        ConnectionManager {
            connections: HashMap::new(),
            active_connection_id: Rc::new(RefCell::new(None)),
            rtt_measurements: HashMap::new(),
            election_state: ElectionState::Failed {
                reason: "test-init".to_string(),
                failed_at: 0.0,
            },
            rtt_reporter: None,
            rtt_probe_timer: None,
            election_timer: None,
            rtt_responses: Rc::new(RefCell::new(Vec::new())),
            options,
            aes: Rc::new(Aes128State::new(false)),
            own_session_id: Rc::new(RefCell::new(None)),
            pending_session_ids: Rc::new(RefCell::new(HashMap::new())),
            reconnection_phase: Rc::new(RefCell::new(ReconnectionPhase::Idle)),
            manager_ref: Weak::new(),
            baseline_rtt: None,
            degradation_counter: 0,
            reelection_in_progress: false,
            intentionally_disconnected: Rc::new(Cell::new(false)),
        }
    }

    /// Helper: insert a synthetic RTT measurement entry for a connection.
    fn insert_measurement(
        mgr: &mut ConnectionManager,
        conn_id: &str,
        is_webtransport: bool,
        avg_rtt: Option<f64>,
        measurements: Vec<f64>,
    ) {
        mgr.rtt_measurements.insert(
            conn_id.to_string(),
            ServerRttMeasurement {
                url: format!("https://test/{conn_id}"),
                is_webtransport,
                measurements,
                average_rtt: avg_rtt,
                connection_id: conn_id.to_string(),
                active: false,
                connected: true,
            },
        );
    }

    // ===================================================================
    // 1. ReconnectionPhase state machine
    // ===================================================================

    #[test]
    fn reconnection_phase_initial_state_is_idle() {
        let mgr = make_test_manager();
        assert_eq!(mgr.reconnection_phase(), ReconnectionPhase::Idle);
    }

    #[test]
    fn reconnection_phase_transitions_to_reconnecting() {
        let mgr = make_test_manager();
        *mgr.reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
            attempt: 1,
            next_delay_ms: RECONNECT_INITIAL_DELAY_MS,
        };
        assert_eq!(
            mgr.reconnection_phase(),
            ReconnectionPhase::Reconnecting {
                attempt: 1,
                next_delay_ms: RECONNECT_INITIAL_DELAY_MS,
            }
        );
    }

    #[test]
    fn reconnection_phase_transitions_to_failed() {
        let mgr = make_test_manager();
        *mgr.reconnection_phase.borrow_mut() = ReconnectionPhase::Failed;
        assert_eq!(mgr.reconnection_phase(), ReconnectionPhase::Failed);
    }

    #[test]
    fn reconnection_phase_round_trip_idle_reconnecting_failed() {
        let mgr = make_test_manager();

        // Start Idle
        assert_eq!(mgr.reconnection_phase(), ReconnectionPhase::Idle);

        // Transition to Reconnecting (attempt 1)
        *mgr.reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
            attempt: 1,
            next_delay_ms: 1000,
        };
        assert!(matches!(
            mgr.reconnection_phase(),
            ReconnectionPhase::Reconnecting { attempt: 1, .. }
        ));

        // Increment attempt
        *mgr.reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
            attempt: 5,
            next_delay_ms: 8000,
        };
        assert!(matches!(
            mgr.reconnection_phase(),
            ReconnectionPhase::Reconnecting { attempt: 5, .. }
        ));

        // Transition to Failed
        *mgr.reconnection_phase.borrow_mut() = ReconnectionPhase::Failed;
        assert_eq!(mgr.reconnection_phase(), ReconnectionPhase::Failed);
    }

    // ===================================================================
    // 2. Exponential backoff calculation
    // ===================================================================

    #[test]
    fn backoff_increases_exponentially() {
        let mut delay = RECONNECT_INITIAL_DELAY_MS;
        let mut delays = vec![delay];

        for _ in 0..5 {
            delay = next_backoff_delay(delay, RECONNECT_BACKOFF_MULTIPLIER, RECONNECT_MAX_DELAY_MS);
            delays.push(delay);
        }

        // With multiplier 2.0 and initial 500, max 2000:
        // 500, 1000, 2000, 2000, 2000, 2000 (capped)
        assert_eq!(delays[0], 500);
        assert_eq!(delays[1], 1000);
        assert_eq!(delays[2], 2000);
        assert_eq!(delays[3], 2000); // capped at max
        assert_eq!(delays[4], 2000);
        assert_eq!(delays[5], 2000);
    }

    #[test]
    fn backoff_is_capped_at_max_delay() {
        // Starting from a large value should be capped immediately.
        let delay = next_backoff_delay(20000, RECONNECT_BACKOFF_MULTIPLIER, RECONNECT_MAX_DELAY_MS);
        assert_eq!(delay, RECONNECT_MAX_DELAY_MS);
    }

    #[test]
    fn backoff_reaches_max_quickly() {
        // With initial=500, mult=2.0, max=2000, the cap is reached by attempt 2.
        let mut delay = RECONNECT_INITIAL_DELAY_MS;
        for _ in 0..3 {
            delay = next_backoff_delay(delay, RECONNECT_BACKOFF_MULTIPLIER, RECONNECT_MAX_DELAY_MS);
        }
        assert_eq!(delay, RECONNECT_MAX_DELAY_MS);
    }

    #[test]
    fn backoff_with_multiplier_one_stays_constant() {
        let delay = next_backoff_delay(1000, 1.0, RECONNECT_MAX_DELAY_MS);
        assert_eq!(delay, 1000);
    }

    // ===================================================================
    // 3. RTT degradation detection (check_rtt_degradation)
    // ===================================================================

    #[test]
    fn rtt_degradation_returns_false_without_baseline() {
        let mut mgr = make_test_manager();
        // No baseline set
        assert!(!mgr.check_rtt_degradation());
    }

    #[test]
    fn rtt_degradation_returns_false_with_zero_baseline() {
        let mut mgr = make_test_manager();
        mgr.baseline_rtt = Some(0.0);
        assert!(!mgr.check_rtt_degradation());
    }

    #[test]
    fn rtt_degradation_returns_false_without_active_connection() {
        let mut mgr = make_test_manager();
        mgr.baseline_rtt = Some(50.0);
        // No active connection id set
        assert!(!mgr.check_rtt_degradation());
    }

    #[test]
    fn rtt_degradation_returns_false_when_reelection_in_progress() {
        let mut mgr = make_test_manager();
        mgr.baseline_rtt = Some(50.0);
        mgr.reelection_in_progress = true;
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());
        insert_measurement(&mut mgr, "wt_0", true, Some(200.0), vec![200.0]);
        assert!(!mgr.check_rtt_degradation());
    }

    #[test]
    fn rtt_degradation_increments_counter_above_threshold() {
        let mut mgr = make_test_manager();
        let baseline = 50.0;
        mgr.baseline_rtt = Some(baseline);
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());

        // Set current RTT above threshold (baseline * REELECTION_RTT_MULTIPLIER = 100.0)
        insert_measurement(&mut mgr, "wt_0", true, Some(150.0), vec![150.0]);

        // First call: counter goes to 1, not yet at threshold
        assert!(!mgr.check_rtt_degradation());
        assert_eq!(mgr.degradation_counter, 1);
    }

    #[test]
    fn rtt_degradation_resets_counter_below_threshold() {
        let mut mgr = make_test_manager();
        let baseline = 50.0;
        mgr.baseline_rtt = Some(baseline);
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());

        // Simulate a few degraded samples
        insert_measurement(&mut mgr, "wt_0", true, Some(150.0), vec![150.0]);
        mgr.check_rtt_degradation();
        mgr.check_rtt_degradation();
        assert_eq!(mgr.degradation_counter, 2);

        // Now RTT recovers — below threshold
        mgr.rtt_measurements.get_mut("wt_0").unwrap().average_rtt = Some(80.0);
        assert!(!mgr.check_rtt_degradation());
        assert_eq!(mgr.degradation_counter, 0);
    }

    #[test]
    fn rtt_degradation_triggers_reelection_after_consecutive_threshold() {
        let mut mgr = make_test_manager();
        let baseline = 50.0;
        mgr.baseline_rtt = Some(baseline);
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());

        // Set RTT well above threshold
        insert_measurement(&mut mgr, "wt_0", true, Some(200.0), vec![200.0]);

        // Call REELECTION_CONSECUTIVE_SAMPLES - 1 times; should NOT trigger
        for _ in 0..(REELECTION_CONSECUTIVE_SAMPLES - 1) {
            assert!(!mgr.check_rtt_degradation());
        }
        assert_eq!(mgr.degradation_counter, REELECTION_CONSECUTIVE_SAMPLES - 1);

        // One more call should trigger re-election
        assert!(mgr.check_rtt_degradation());
        assert_eq!(mgr.degradation_counter, REELECTION_CONSECUTIVE_SAMPLES);
    }

    #[test]
    fn rtt_degradation_exactly_at_threshold_does_not_trigger() {
        let mut mgr = make_test_manager();
        let baseline = 50.0;
        mgr.baseline_rtt = Some(baseline);
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());

        // RTT exactly at threshold (baseline * multiplier = 100.0)
        // The check is `current_rtt > threshold`, so equal should NOT trigger.
        let threshold = baseline * REELECTION_RTT_MULTIPLIER;
        insert_measurement(&mut mgr, "wt_0", true, Some(threshold), vec![threshold]);

        for _ in 0..(REELECTION_CONSECUTIVE_SAMPLES + 2) {
            assert!(!mgr.check_rtt_degradation());
        }
        // Counter should remain 0 because samples are not strictly above threshold.
        assert_eq!(mgr.degradation_counter, 0);
    }

    #[test]
    fn rtt_degradation_intermittent_resets_counter() {
        let mut mgr = make_test_manager();
        let baseline = 50.0;
        mgr.baseline_rtt = Some(baseline);
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());

        insert_measurement(&mut mgr, "wt_0", true, Some(200.0), vec![200.0]);

        // 3 bad samples
        for _ in 0..3 {
            mgr.check_rtt_degradation();
        }
        assert_eq!(mgr.degradation_counter, 3);

        // One good sample resets
        mgr.rtt_measurements.get_mut("wt_0").unwrap().average_rtt = Some(60.0);
        mgr.check_rtt_degradation();
        assert_eq!(mgr.degradation_counter, 0);

        // Bad samples again — need full REELECTION_CONSECUTIVE_SAMPLES to trigger
        mgr.rtt_measurements.get_mut("wt_0").unwrap().average_rtt = Some(200.0);
        for _ in 0..(REELECTION_CONSECUTIVE_SAMPLES - 1) {
            assert!(!mgr.check_rtt_degradation());
        }
        assert!(mgr.check_rtt_degradation());
    }

    // ===================================================================
    // 4. disconnect() state transitions
    // ===================================================================

    #[test]
    fn disconnect_sets_active_connection_id_to_none() {
        let mut mgr = make_test_manager();
        *mgr.active_connection_id.borrow_mut() = Some("ws_0".to_string());
        mgr.election_state = ElectionState::Elected {
            connection_id: "ws_0".to_string(),
            elected_at: 0.0,
        };
        assert!(mgr.is_connected());

        mgr.disconnect().unwrap();

        assert!(mgr.active_connection_id.borrow().is_none());
    }

    #[test]
    fn disconnect_sets_intentionally_disconnected_flag() {
        let mut mgr = make_test_manager();
        assert!(!mgr.intentionally_disconnected.get());

        mgr.disconnect().unwrap();

        assert!(mgr.intentionally_disconnected.get());
    }

    #[test]
    fn disconnect_sets_reconnection_phase_to_idle() {
        let mut mgr = make_test_manager();
        *mgr.reconnection_phase.borrow_mut() = ReconnectionPhase::Reconnecting {
            attempt: 3,
            next_delay_ms: 2000,
        };

        mgr.disconnect().unwrap();

        assert_eq!(mgr.reconnection_phase(), ReconnectionPhase::Idle);
    }

    #[test]
    fn disconnect_clears_all_connections() {
        let mut mgr = make_test_manager();
        *mgr.active_connection_id.borrow_mut() = Some("ws_0".to_string());
        mgr.election_state = ElectionState::Elected {
            connection_id: "ws_0".to_string(),
            elected_at: 0.0,
        };

        mgr.disconnect().unwrap();

        assert!(mgr.connections.is_empty());
        assert!(mgr.active_connection_id.borrow().is_none());
        assert!(mgr.intentionally_disconnected.get());
        assert_eq!(mgr.reconnection_phase(), ReconnectionPhase::Idle);
    }

    #[test]
    fn active_connection_id_guard_catches_intentional_disconnect() {
        // Proves that after disconnect(), the callback's active_connection_id
        // guard (Some(id) != None) is always true, making the explicit
        // intentionally_disconnected check in the callback redundant.
        let mut mgr = make_test_manager();
        *mgr.active_connection_id.borrow_mut() = Some("ws_0".to_string());

        mgr.disconnect().unwrap();

        // For ANY connection_id, Some(id) != active_connection_id (None) is true.
        let active = mgr.active_connection_id.borrow();
        assert!(Some("ws_0") != active.as_deref());
        assert!(Some("wt_0") != active.as_deref());
        assert!(Some("anything") != active.as_deref());
    }

    #[test]
    fn intentionally_disconnected_needed_for_reconnection_loop() {
        // The reconnection loop checks active_connection_id.is_some() to detect
        // EXTERNAL reconnection. After disconnect(), active_connection_id is None,
        // which the loop would misinterpret as "not yet reconnected, keep trying."
        // The intentionally_disconnected flag disambiguates.
        let mut mgr = make_test_manager();
        *mgr.active_connection_id.borrow_mut() = Some("ws_0".to_string());

        mgr.disconnect().unwrap();

        // active_connection_id is None -- loop would see "no connection yet"
        assert!(mgr.active_connection_id.borrow().is_none());
        // But intentionally_disconnected is true -- loop should exit
        assert!(mgr.intentionally_disconnected.get());
    }

    #[test]
    fn reconnect_retries_indefinitely() {
        // There is no RECONNECT_MAX_ATTEMPTS constant -- the client retries
        // indefinitely. The only hard stop is RECONNECT_CONSECUTIVE_ZERO_LIMIT
        // (consecutive auth/server rejections). Verify the constants reflect this.
        assert_eq!(RECONNECT_INITIAL_DELAY_MS, 500);
        assert_eq!(RECONNECT_MAX_DELAY_MS, 2000);
        assert_eq!(RECONNECT_BACKOFF_MULTIPLIER, 2.0);
        // fast-fail limit is a small number so it triggers quickly on auth failures
        assert!(RECONNECT_CONSECUTIVE_ZERO_LIMIT <= 5);
    }

    // ===================================================================
    // 5. Baseline RTT tracking
    // ===================================================================

    #[test]
    fn baseline_rtt_initially_none() {
        let mgr = make_test_manager();
        assert_eq!(mgr.baseline_rtt, None);
    }

    #[test]
    fn handle_rtt_response_records_measurement() {
        let mut mgr = make_test_manager();
        insert_measurement(&mut mgr, "wt_0", true, None, vec![]);

        let media_packet = MediaPacket {
            timestamp: 1000.0,
            ..Default::default()
        };

        // RTT = reception_time - sent_timestamp = 1050 - 1000 = 50ms
        mgr.handle_rtt_response("wt_0", &media_packet, 1050.0);

        let m = mgr.rtt_measurements.get("wt_0").unwrap();
        assert_eq!(m.measurements.len(), 1);
        assert!((m.average_rtt.unwrap() - 50.0).abs() < 0.01);
    }

    #[test]
    fn handle_rtt_response_averages_multiple_samples() {
        let mut mgr = make_test_manager();
        insert_measurement(&mut mgr, "wt_0", true, None, vec![]);

        // Send 3 RTT samples: 50ms, 100ms, 150ms -> avg 100ms
        for (sent, recv) in [(1000.0, 1050.0), (2000.0, 2100.0), (3000.0, 3150.0)] {
            let pkt = MediaPacket {
                timestamp: sent,
                ..Default::default()
            };
            mgr.handle_rtt_response("wt_0", &pkt, recv);
        }

        let m = mgr.rtt_measurements.get("wt_0").unwrap();
        assert_eq!(m.measurements.len(), 3);
        assert!((m.average_rtt.unwrap() - 100.0).abs() < 0.01);
    }

    #[test]
    fn handle_rtt_response_caps_at_ten_measurements() {
        let mut mgr = make_test_manager();
        insert_measurement(&mut mgr, "wt_0", true, None, vec![]);

        // Send 15 samples
        for i in 0..15 {
            let sent = i as f64 * 1000.0;
            let recv = sent + 50.0 + i as f64; // slightly increasing RTT
            let pkt = MediaPacket {
                timestamp: sent,
                ..Default::default()
            };
            mgr.handle_rtt_response("wt_0", &pkt, recv);
        }

        let m = mgr.rtt_measurements.get("wt_0").unwrap();
        assert_eq!(m.measurements.len(), 10); // capped at 10
    }

    #[test]
    fn handle_rtt_response_ignores_unknown_connection() {
        let mut mgr = make_test_manager();
        let pkt = MediaPacket {
            timestamp: 1000.0,
            ..Default::default()
        };
        // No "unknown" entry in rtt_measurements — should not panic.
        mgr.handle_rtt_response("unknown", &pkt, 1050.0);
        assert!(!mgr.rtt_measurements.contains_key("unknown"));
    }

    // ===================================================================
    // 6. select_best_connection — pure election algorithm
    // ===================================================================
    // Tests call select_best_connection() directly with plain data.
    // No ConnectionManager, no fake state, no side doors.

    fn m(id: &str, is_wt: bool, avg: Option<f64>, samples: Vec<f64>) -> ServerRttMeasurement {
        ServerRttMeasurement {
            url: format!("https://test/{id}"),
            is_webtransport: is_wt,
            measurements: samples,
            average_rtt: avg,
            connection_id: id.to_string(),
            active: false,
            connected: false,
        }
    }

    #[test]
    fn election_no_candidates() {
        assert!(select_best_connection(&[]).is_err());
    }

    #[test]
    fn election_no_average_rtt() {
        let ws = m("ws_0", false, None, vec![]);
        assert!(select_best_connection(&[("ws_0", &ws)]).is_err());
    }

    #[test]
    fn election_empty_measurements_vec() {
        let ws = m("ws_0", false, Some(50.0), vec![]);
        assert!(select_best_connection(&[("ws_0", &ws)]).is_err());
    }

    #[test]
    fn election_single_websocket() {
        let ws = m("ws_0", false, Some(50.0), vec![50.0]);
        let (id, result) = select_best_connection(&[("ws_0", &ws)]).unwrap();
        assert_eq!(id, "ws_0");
        assert!((result.average_rtt.unwrap() - 50.0).abs() < 0.01);
    }

    #[test]
    fn election_single_webtransport() {
        let wt = m("wt_0", true, Some(30.0), vec![30.0]);
        let (id, _) = select_best_connection(&[("wt_0", &wt)]).unwrap();
        assert_eq!(id, "wt_0");
    }

    #[test]
    fn election_prefers_webtransport_over_websocket() {
        let ws = m("ws_0", false, Some(10.0), vec![10.0]); // better RTT
        let wt = m("wt_0", true, Some(80.0), vec![80.0]); // worse RTT, but WT
        let (id, _) = select_best_connection(&[("ws_0", &ws), ("wt_0", &wt)]).unwrap();
        assert_eq!(id, "wt_0");
    }

    #[test]
    fn election_falls_back_to_websocket_when_no_wt() {
        let ws0 = m("ws_0", false, Some(40.0), vec![40.0]);
        let ws1 = m("ws_1", false, Some(60.0), vec![60.0]);
        let (id, _) = select_best_connection(&[("ws_0", &ws0), ("ws_1", &ws1)]).unwrap();
        assert_eq!(id, "ws_0");
    }

    #[test]
    fn election_lowest_rtt_among_webtransport() {
        let wt0 = m("wt_0", true, Some(100.0), vec![100.0]);
        let wt1 = m("wt_1", true, Some(25.0), vec![25.0]);
        let (id, _) = select_best_connection(&[("wt_0", &wt0), ("wt_1", &wt1)]).unwrap();
        assert_eq!(id, "wt_1");
    }

    #[test]
    fn election_lowest_rtt_among_websocket() {
        let ws0 = m("ws_0", false, Some(90.0), vec![90.0]);
        let ws1 = m("ws_1", false, Some(45.0), vec![45.0]);
        let (id, _) = select_best_connection(&[("ws_0", &ws0), ("ws_1", &ws1)]).unwrap();
        assert_eq!(id, "ws_1");
    }

    #[test]
    fn election_four_backends_elects_best_wt() {
        let ws0 = m("ws_0", false, Some(20.0), vec![20.0]); // best RTT overall
        let ws1 = m("ws_1", false, Some(35.0), vec![35.0]);
        let wt0 = m("wt_0", true, Some(50.0), vec![50.0]);
        let wt1 = m("wt_1", true, Some(40.0), vec![40.0]); // best WT

        let (id, result) = select_best_connection(&[
            ("ws_0", &ws0),
            ("ws_1", &ws1),
            ("wt_0", &wt0),
            ("wt_1", &wt1),
        ])
        .unwrap();
        assert_eq!(id, "wt_1");
        assert!(result.is_webtransport);
        assert!((result.average_rtt.unwrap() - 40.0).abs() < 0.01);
    }

    #[test]
    fn election_four_backends_falls_back_to_ws_when_wt_has_no_rtt() {
        let ws0 = m("ws_0", false, Some(30.0), vec![30.0]);
        let ws1 = m("ws_1", false, Some(55.0), vec![55.0]);
        let wt0 = m("wt_0", true, None, vec![]);
        let wt1 = m("wt_1", true, None, vec![]);

        let (id, result) = select_best_connection(&[
            ("ws_0", &ws0),
            ("ws_1", &ws1),
            ("wt_0", &wt0),
            ("wt_1", &wt1),
        ])
        .unwrap();
        assert_eq!(id, "ws_0");
        assert!(!result.is_webtransport);
    }

    #[test]
    fn election_skips_candidates_without_measurements() {
        let wt0 = m("wt_0", true, Some(10.0), vec![]); // avg set but no samples → skipped
        let wt1 = m("wt_1", true, Some(80.0), vec![80.0]);
        let (id, _) = select_best_connection(&[("wt_0", &wt0), ("wt_1", &wt1)]).unwrap();
        assert_eq!(id, "wt_1");
    }

    #[test]
    fn election_mixed_validity_four_backends() {
        let ws0 = m("ws_0", false, Some(25.0), vec![25.0]);
        let ws1 = m("ws_1", false, None, vec![]);
        let wt0 = m("wt_0", true, None, vec![]);
        let wt1 = m("wt_1", true, Some(60.0), vec![60.0]);

        let (id, _) = select_best_connection(&[
            ("ws_0", &ws0),
            ("ws_1", &ws1),
            ("wt_0", &wt0),
            ("wt_1", &wt1),
        ])
        .unwrap();
        assert_eq!(id, "wt_1");
    }

    // ===================================================================
    // 7. is_connected
    // ===================================================================

    #[test]
    fn is_connected_false_when_no_active_connection() {
        let mgr = make_test_manager();
        assert!(!mgr.is_connected());
    }

    #[test]
    fn is_connected_false_when_election_not_complete() {
        let mgr = make_test_manager();
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());
        // election_state is Failed (from make_test_manager), not Elected
        assert!(!mgr.is_connected());
    }

    #[test]
    fn is_connected_true_when_elected_and_active() {
        let mut mgr = make_test_manager();
        *mgr.active_connection_id.borrow_mut() = Some("wt_0".to_string());
        mgr.election_state = ElectionState::Elected {
            connection_id: "wt_0".to_string(),
            elected_at: 0.0,
        };
        assert!(mgr.is_connected());
    }

    // ===================================================================
    // 8. ReconnectionPhase and ConnectionState enum variants
    // ===================================================================

    #[test]
    fn reconnection_phase_equality() {
        let a = ReconnectionPhase::Reconnecting {
            attempt: 3,
            next_delay_ms: 4000,
        };
        let b = ReconnectionPhase::Reconnecting {
            attempt: 3,
            next_delay_ms: 4000,
        };
        assert_eq!(a, b);

        let c = ReconnectionPhase::Reconnecting {
            attempt: 4,
            next_delay_ms: 4000,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn connection_state_variants() {
        let testing = ConnectionState::Testing {
            progress: 0.5,
            servers_tested: 2,
            total_servers: 4,
        };
        assert!(matches!(testing, ConnectionState::Testing { .. }));

        let connected = ConnectionState::Connected {
            server_url: "wss://test".to_string(),
            rtt: 42.0,
            is_webtransport: true,
        };
        assert!(matches!(connected, ConnectionState::Connected { .. }));

        let reconnecting = ConnectionState::Reconnecting {
            server_url: "wss://test".to_string(),
            attempt: 3,
        };
        assert!(matches!(reconnecting, ConnectionState::Reconnecting { .. }));

        let failed = ConnectionState::Failed {
            error: "timeout".to_string(),
            last_known_server: None,
        };
        assert!(matches!(failed, ConnectionState::Failed { .. }));
    }

    // ===================================================================
    // 9. Backoff sequence matches reconnection loop constants
    // ===================================================================

    #[test]
    fn full_backoff_sequence_matches_expected() {
        // Simulate several iterations of the reconnection loop's backoff.
        // The loop runs indefinitely, so we just verify the first N steps.
        let mut delay = RECONNECT_INITIAL_DELAY_MS;
        let mut sequence = vec![];

        for _ in 0..8 {
            sequence.push(delay);
            delay = next_backoff_delay(delay, RECONNECT_BACKOFF_MULTIPLIER, RECONNECT_MAX_DELAY_MS);
        }

        // With initial=500, mult=2.0, max=2000:
        // [500, 1000, 2000, 2000, 2000, 2000, 2000, 2000]
        assert_eq!(sequence[0], 500);
        assert_eq!(sequence[1], 1000);
        for delay in &sequence[2..] {
            assert_eq!(*delay, RECONNECT_MAX_DELAY_MS);
        }
    }

    // ===================================================================
    // 10. start_reelection guards
    // ===================================================================

    #[test]
    fn start_reelection_sets_flag() {
        let mut mgr = make_test_manager();
        assert!(!mgr.is_reelection_in_progress());

        // start_reelection calls create_all_connections which is a no-op
        // when websocket_urls and webtransport_urls are both empty.
        mgr.start_reelection().unwrap();
        assert!(mgr.is_reelection_in_progress());
        assert_eq!(mgr.degradation_counter, 0);
    }

    #[test]
    fn start_reelection_skips_when_already_in_progress() {
        let mut mgr = make_test_manager();
        mgr.reelection_in_progress = true;

        // Should return Ok without changing state.
        assert!(mgr.start_reelection().is_ok());
        assert!(mgr.is_reelection_in_progress());
    }

    // ===================================================================
    // Integration test notes
    // ===================================================================
    //
    // The following logic requires a wasm32 runtime with browser/wasm-bindgen-test
    // harness and cannot be unit tested with standard `cargo test`:
    //
    // - `run_reconnection_loop` (async, uses gloo_timers::future::sleep, Weak<RefCell<>>)
    //   -> exponential backoff timing, fast-fail after RECONNECT_CONSECUTIVE_ZERO_LIMIT
    //   -> interaction with Connection::connect and election cycle
    //
    // - `ConnectionManager::new()` and `start_election()` (call Connection::connect)
    //
    // - `complete_election()` with live connections (selects best, starts heartbeat)
    //
    // - `create_connection_lost_callback` -> spawns reconnection loop
    //
    // These should be covered by wasm-bindgen-test integration tests or E2E tests.
}
