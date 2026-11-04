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

use serde::{Deserialize, Serialize};

/// meeting-api -> every relay: a meeting started a new instance, so room tokens
/// minted for an earlier one must no longer join.
pub const MEETING_INSTANCE_STARTED_SUBJECT: &str = "internal.meeting_instance_started";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MeetingInstanceStartedPayload {
    pub room_id: String,
    /// Unix milliseconds; equals the `meeting_started_at_ms` claim of this instance's room tokens.
    pub started_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_wire_format_is_pinned() {
        let payload = MeetingInstanceStartedPayload {
            room_id: "r".to_string(),
            started_at_ms: 5,
        };
        assert_eq!(
            serde_json::to_string(&payload).unwrap(),
            r#"{"room_id":"r","started_at_ms":5}"#
        );
        assert_eq!(
            serde_json::from_str::<MeetingInstanceStartedPayload>(
                r#"{"room_id":"r","started_at_ms":5}"#
            )
            .unwrap(),
            payload
        );
        assert_eq!(
            MEETING_INSTANCE_STARTED_SUBJECT,
            "internal.meeting_instance_started"
        );
    }
}
