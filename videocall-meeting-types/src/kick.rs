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

//! Host-kick revocation contract shared by meeting-api and the relay (#2934).

use serde::{Deserialize, Serialize};

/// meeting-api -> every relay: close and refuse a kicked participant's
/// sessions whose room token the kick revoked.
pub const PARTICIPANT_KICKED_SUBJECT: &str = "internal.participant_kicked";

/// `exp` leeway the relay's token validator applies (jsonwebtoken's default).
pub const JWT_EXP_LEEWAY_SECS: i64 = 60;

/// Longest room-token lifetime meeting-api accepts for `TOKEN_TTL_SECS`, so a
/// revocation always outlives every token minted before it.
pub const MAX_ROOM_TOKEN_TTL_SECS: i64 = 7 * 24 * 3600;

/// Allowance for meeting-api/relay clock skew and a mint in flight at the kick.
pub const KICK_CLOCK_SKEW_ALLOWANCE_SECS: i64 = 300;

/// How long a revocation outlives its kick.
pub const KICK_DENY_WINDOW_SECS: i64 =
    MAX_ROOM_TOKEN_TTL_SECS + JWT_EXP_LEEWAY_SECS + KICK_CLOCK_SKEW_ALLOWANCE_SECS;

/// Seconds past the kick through which `iat` is still refused: `kicked_at` is
/// stamped on the database clock, `iat` on a meeting-api replica's clock.
pub const KICK_IAT_MARGIN_SECS: i64 = 1;

/// Payload on [`PARTICIPANT_KICKED_SUBJECT`], in Unix seconds.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ParticipantKickedPayload {
    pub room_id: String,
    pub user_id: String,
    pub kicked_at: i64,
    /// A token whose `iat` is at or before this is refused.
    pub revoke_iat_through: i64,
    /// The relay may forget the revocation after this.
    pub deny_until: i64,
}

impl ParticipantKickedPayload {
    /// The revocation for a kick stamped at `kicked_at`, kept until `deny_until`.
    pub fn new(room_id: &str, user_id: &str, kicked_at: i64, deny_until: i64) -> Self {
        Self {
            room_id: room_id.to_string(),
            user_id: user_id.to_string(),
            kicked_at,
            revoke_iat_through: kicked_at.saturating_add(KICK_IAT_MARGIN_SECS),
            deny_until,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deny_window_outlives_the_longest_accepted_token() {
        assert_eq!(KICK_DENY_WINDOW_SECS, 7 * 24 * 3600 + 60 + 300);
    }

    #[test]
    fn the_revocation_reaches_one_second_past_the_kick() {
        let p = ParticipantKickedPayload::new("r", "u", 5, 9);
        assert_eq!((p.kicked_at, p.revoke_iat_through, p.deny_until), (5, 6, 9));
    }

    #[test]
    fn payload_wire_format() {
        let wire =
            r#"{"room_id":"r","user_id":"u","kicked_at":5,"revoke_iat_through":6,"deny_until":9}"#;
        let payload: ParticipantKickedPayload = serde_json::from_str(wire).expect("payload");
        assert_eq!(payload, ParticipantKickedPayload::new("r", "u", 5, 9));
        assert_eq!(serde_json::to_string(&payload).expect("serialize"), wire);
    }
}
