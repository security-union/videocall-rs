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

//! Server-internal NATS consumers.
//!
//! These run as long-lived `tokio::spawn` tasks alongside the Axum HTTP
//! server. They listen for cross-service events that drive DB writes the
//! HTTP layer cannot observe directly — for example, a host disconnecting
//! from the media server (`actix-api`) needs to mark the meeting as
//! `state='ended'` in the DB, but the disconnect event lives on a
//! WebSocket / WebTransport handler in a different process.
//!
//! Each consumer follows the same pattern as
//! `actix-api/src/actors/chat_server.rs::started`: subscribe in a loop,
//! deserialize from JSON, dispatch to a handler, and re-subscribe on stream
//! end. The functions are no-ops when NATS is not configured.

use crate::db::meetings as db_meetings;
use crate::nats_events::{MeetingEndedByHostPayload, MEETING_ENDED_BY_HOST_SUBJECT};
use futures::StreamExt;
use sqlx::PgPool;
use std::time::Duration;

/// Spawn the consumer for [`MEETING_ENDED_BY_HOST_SUBJECT`].
///
/// When `actix-api` broadcasts MEETING_ENDED on a host disconnect with
/// `end_on_host_leave=true`, it publishes a [`MeetingEndedByHostPayload`]
/// on this subject. We look the meeting up by `room_id` and set
/// `state='ended'` so the meetings list reflects the same outcome the
/// connected clients just received.
///
/// Idempotent: if the meeting is already ended (e.g. because the host
/// also clicked Hangup, or another chat_server replica racing on the
/// same broadcast) the UPDATE is a no-op at SQL level
/// (`db_meetings::end_meeting` is `UPDATE … WHERE id = $1` and tolerates
/// being called multiple times).
///
/// Graceful degradation: when `nats` is `None`, this function returns
/// without spawning anything. The DB stays consistent only via the REST
/// `/leave` endpoint in that environment, matching the pre-fix behavior.
pub fn spawn_meeting_ended_by_host_consumer(
    nats: Option<async_nats::Client>,
    pool: PgPool,
) -> Option<tokio::task::JoinHandle<()>> {
    let nats = nats?;
    let handle = tokio::spawn(async move {
        loop {
            match nats.subscribe(MEETING_ENDED_BY_HOST_SUBJECT).await {
                Ok(mut sub) => {
                    tracing::info!(
                        "Subscribed to {} (host-disconnect DB-write fanout)",
                        MEETING_ENDED_BY_HOST_SUBJECT
                    );
                    while let Some(msg) = sub.next().await {
                        let payload =
                            match serde_json::from_slice::<MeetingEndedByHostPayload>(&msg.payload)
                            {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::warn!(
                                        "Dropping malformed {} payload: {e}",
                                        MEETING_ENDED_BY_HOST_SUBJECT
                                    );
                                    continue;
                                }
                            };

                        // Defensive bounds — payload is from a trusted
                        // peer but we still cap room_id to match the
                        // posture used elsewhere (e.g. the EvictInstance
                        // handler at chat_server.rs).
                        if payload.room_id.is_empty() || payload.room_id.len() > 256 {
                            tracing::warn!(
                                "Ignoring {} with invalid room_id length: {}",
                                MEETING_ENDED_BY_HOST_SUBJECT,
                                payload.room_id.len()
                            );
                            continue;
                        }

                        // Resolve room_id -> meeting.id, then end_meeting.
                        // Both queries are cheap (room_id is indexed via
                        // the partial unique index on `meetings`).
                        match db_meetings::get_by_room_id(&pool, &payload.room_id).await {
                            Ok(Some(meeting)) => {
                                if let Err(e) = db_meetings::end_meeting(&pool, meeting.id).await {
                                    tracing::error!(
                                        "Failed to end meeting {} (id={}) on host-leave NATS \
                                         event: {e}",
                                        payload.room_id,
                                        meeting.id
                                    );
                                } else {
                                    tracing::info!(
                                        "Marked meeting {} (id={}) as ended via host-leave \
                                         NATS event",
                                        payload.room_id,
                                        meeting.id
                                    );
                                }
                            }
                            Ok(None) => {
                                // Meeting may have been hard-deleted between
                                // broadcast and event delivery. Not an error.
                                tracing::warn!(
                                    "Received {} for unknown room {}; ignoring",
                                    MEETING_ENDED_BY_HOST_SUBJECT,
                                    payload.room_id
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "DB error looking up room {} for host-leave NATS event: {e}",
                                    payload.room_id
                                );
                            }
                        }
                    }
                    tracing::warn!(
                        "{} subscription stream ended, re-subscribing in 1s",
                        MEETING_ENDED_BY_HOST_SUBJECT
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to subscribe to {}: {e}, retrying in 1s",
                        MEETING_ENDED_BY_HOST_SUBJECT
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    Some(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the consumer correctly degrades when NATS is not configured.
    /// Uses `PgPool::connect_lazy` to satisfy the `pool` parameter without
    /// contacting a real database — when `nats` is `None`, the consumer
    /// returns `None` before the spawned task ever runs, so the lazy
    /// pool's connection is never attempted.
    #[tokio::test]
    async fn spawn_returns_none_when_nats_disabled() {
        let lazy_pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://stub")
            .expect("connect_lazy should not contact the database");
        let handle = spawn_meeting_ended_by_host_consumer(None, lazy_pool);
        assert!(
            handle.is_none(),
            "spawn must return None when NATS is not configured"
        );
    }
}
