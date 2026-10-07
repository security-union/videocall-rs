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

//! Relay revocations for host kicks (#2934), and their re-publication while
//! a kicked user is still reported on a relay.

use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use videocall_meeting_types::kick::ParticipantKickedPayload;

use crate::db::participants::ActiveKick;
use crate::nats_events;
use crate::rate_limit::KeyedRateLimiter;

/// At most one re-publish per `(room, user)` per this interval, per replica.
pub const REASSERT_MIN_INTERVAL: Duration = Duration::from_secs(10);

static REASSERT_THROTTLE: LazyLock<KeyedRateLimiter> =
    LazyLock::new(|| KeyedRateLimiter::new(1, REASSERT_MIN_INTERVAL));

/// The revocation for a kick of `user_id` in `room_id` stamped at `kicked_at`.
pub fn payload(
    room_id: &str,
    user_id: &str,
    kicked_at: DateTime<Utc>,
    deny_until: DateTime<Utc>,
) -> ParticipantKickedPayload {
    ParticipantKickedPayload::new(
        room_id,
        user_id,
        kicked_at.timestamp(),
        deny_until.timestamp(),
    )
}

/// Re-publish each of `kicks` in `room_id`, throttled per `(room, user)`.
/// Returns how many were published.
pub async fn reassert(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    kicks: &[ActiveKick],
) -> usize {
    let mut published = 0;
    for kick in kicks {
        if !REASSERT_THROTTLE.allow(&format!("{room_id}\u{0}{}", kick.user_id)) {
            continue;
        }
        let revocation = payload(room_id, &kick.user_id, kick.kicked_at, kick.kick_deny_until);
        match nats_events::publish_kick_revocation(nats, &revocation).await {
            Ok(()) => {
                tracing::info!(
                    "Re-asserted kick of {} in room {}: a session is still reported present",
                    kick.user_id,
                    room_id
                );
                published += 1;
            }
            Err(e) => tracing::error!(
                "Failed to re-assert kick of {} in room {room_id}: {e}",
                kick.user_id
            ),
        }
    }
    published
}
