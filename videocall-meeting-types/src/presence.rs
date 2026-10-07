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
 */

//! Transport presence heartbeat contract and lease timings shared by the relay and meeting-api.

use serde::{Deserialize, Serialize};

pub const PRESENCE_HEARTBEAT_INTERVAL_SECS: u64 = 30;
pub const PRESENCE_LEASE_SECS: u64 = 3 * PRESENCE_HEARTBEAT_INTERVAL_SECS;
pub const PRESENCE_CONNECT_WINDOW_SECS: u64 = 60;
pub const PRESENCE_HEARTBEAT_MAX_SESSIONS: usize = 256;
pub const PRESENCE_HEARTBEAT_SUBJECT: &str = "internal.participant_presence_heartbeat";

const _: () = assert!(PRESENCE_LEASE_SECS > 2 * PRESENCE_HEARTBEAT_INTERVAL_SECS);

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatSession {
    pub user_id: String,
    pub session_id: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PresenceHeartbeat {
    pub room_id: String,
    pub sessions: Vec<HeartbeatSession>,
    /// Users holding a joined session the relay has not reported present (never
    /// activated, or an observer). Checked against host kicks only (#2934); it
    /// renews nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreported_user_ids: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_wire_format() {
        let wire = r#"{"room_id":"r","sessions":[{"user_id":"a@example.com","session_id":18446744073709551615}]}"#;
        let heartbeat: PresenceHeartbeat = serde_json::from_str(wire).expect("heartbeat");
        assert_eq!(heartbeat.sessions[0].session_id, u64::MAX);
        assert!(heartbeat.unreported_user_ids.is_empty());
        assert_eq!(serde_json::to_string(&heartbeat).expect("serialize"), wire);
    }

    #[test]
    fn unreported_users_ride_the_heartbeat_when_present() {
        let wire = r#"{"room_id":"r","sessions":[],"unreported_user_ids":["k@example.com"]}"#;
        let heartbeat: PresenceHeartbeat = serde_json::from_str(wire).expect("heartbeat");
        assert_eq!(heartbeat.unreported_user_ids, vec!["k@example.com"]);
        assert_eq!(serde_json::to_string(&heartbeat).expect("serialize"), wire);
    }
}
