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

//! Connection tracking module for server-side metrics

use crate::metrics::{
    SERVER_CONNECTIONS_ACTIVE, SERVER_CONNECTION_DURATION_SECONDS, SERVER_CONNECTION_EVENTS_TOTAL,
    SERVER_DATA_BYTES_TOTAL, SERVER_RECONNECTIONS_TOTAL,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, info};

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
    pub protocol: String,
    pub start_time: Instant,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

/// Global connection tracker for server metrics
pub struct ConnectionTracker {
    connections: Arc<Mutex<HashMap<String, ConnectionInfo>>>,
    customer_connections: Arc<Mutex<HashMap<String, u32>>>, // email -> connection_count
}

impl Default for ConnectionTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionTracker {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            customer_connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Track a new connection start
    pub fn connection_started(
        &self,
        session_id: String,
        customer_email: String,
        meeting_id: String,
        protocol: String,
    ) {
        let info = ConnectionInfo {
            session_id: session_id.clone(),
            customer_email: customer_email.clone(),
            meeting_id: meeting_id.clone(),
            protocol: protocol.clone(),
            start_time: Instant::now(),
            bytes_sent: 0,
            bytes_received: 0,
        };

        // Store connection info
        {
            let mut connections = self.connections.lock().unwrap();
            connections.insert(session_id.clone(), info);
        }

        // Update customer connection count and check for reconnections
        {
            let mut customer_conns = self.customer_connections.lock().unwrap();
            let customer_key = format!("{customer_email}:{meeting_id}");
            let count = customer_conns.entry(customer_key.clone()).or_insert(0);
            *count += 1;

            // If this isn't the first connection, it's a reconnection
            if *count > 1 {
                info!(
                    "Reconnection detected for customer {} in meeting {}",
                    customer_email, meeting_id
                );
                SERVER_RECONNECTIONS_TOTAL
                    .with_label_values(&[&protocol, &customer_email, &meeting_id])
                    .inc();
            }
        }

        // Update active connections metric
        SERVER_CONNECTIONS_ACTIVE
            .with_label_values(&[&protocol, &customer_email, &meeting_id, &session_id])
            .set(1.0);

        // Track connection event
        SERVER_CONNECTION_EVENTS_TOTAL
            .with_label_values(&["connected", &protocol, &customer_email, &meeting_id])
            .inc();

        info!(
            "Connection started: session={}, customer={}, meeting={}, protocol={}",
            session_id, customer_email, meeting_id, protocol
        );
    }

    /// Track connection end and record duration
    pub fn connection_ended(&self, session_id: &str) {
        let connection_info = {
            let mut connections = self.connections.lock().unwrap();
            connections.remove(session_id)
        };

        if let Some(info) = connection_info {
            let duration = info.start_time.elapsed().as_secs_f64();

            // Record duration histogram
            SERVER_CONNECTION_DURATION_SECONDS.observe(duration);

            // Clear active connection metric
            SERVER_CONNECTIONS_ACTIVE
                .with_label_values(&[
                    &info.protocol,
                    &info.customer_email,
                    &info.meeting_id,
                    &info.session_id,
                ])
                .set(0.0);

            // Track disconnection event
            SERVER_CONNECTION_EVENTS_TOTAL
                .with_label_values(&[
                    "disconnected",
                    &info.protocol,
                    &info.customer_email,
                    &info.meeting_id,
                ])
                .inc();

            info!(
                "Connection ended: session={}, customer={}, meeting={}, protocol={}, duration={}s, sent={}bytes, received={}bytes",
                info.session_id, info.customer_email, info.meeting_id, info.protocol,
                duration, info.bytes_sent, info.bytes_received
            );
        } else {
            debug!("Connection end tracked for unknown session: {}", session_id);
        }
    }

    /// Track data sent through a connection
    pub fn track_data_sent(&self, session_id: &str, bytes: u64) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(info) = connections.get_mut(session_id) {
            info.bytes_sent += bytes;

            // Update cumulative bytes metric
            SERVER_DATA_BYTES_TOTAL
                .with_label_values(&[
                    "sent",
                    &info.protocol,
                    &info.customer_email,
                    &info.meeting_id,
                    &info.session_id,
                ])
                .add(bytes as f64);
        }
    }

    /// Track data received through a connection
    pub fn track_data_received(&self, session_id: &str, bytes: u64) {
        let mut connections = self.connections.lock().unwrap();
        if let Some(info) = connections.get_mut(session_id) {
            info.bytes_received += bytes;

            // Update cumulative bytes metric
            SERVER_DATA_BYTES_TOTAL
                .with_label_values(&[
                    "received",
                    &info.protocol,
                    &info.customer_email,
                    &info.meeting_id,
                    &info.session_id,
                ])
                .add(bytes as f64);
        }
    }

    /// Get current connection count for debugging
    pub fn get_connection_count(&self) -> usize {
        self.connections.lock().unwrap().len()
    }
}

lazy_static::lazy_static! {
    /// Global instance for server-wide connection tracking
    pub static ref GLOBAL_TRACKER: ConnectionTracker = ConnectionTracker::new();
}
