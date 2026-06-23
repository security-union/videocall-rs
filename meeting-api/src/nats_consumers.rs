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
//! WebSocket / WebTransport handler in a different process. Symmetrically, a
//! room becoming empty (last participant left a meeting that did not end)
//! drives the meeting to `state='idle'`.
//!
//! Each consumer follows the same pattern as
//! `actix-api/src/actors/chat_server.rs::started`: subscribe in a loop,
//! deserialize from JSON, dispatch to a handler, and re-subscribe on stream
//! end. The shared loop lives in [`spawn_room_state_consumer`]; each public
//! spawn function supplies its subject, a human-readable description, and the
//! per-meeting DB transition to apply. The functions are no-ops when NATS is
//! not configured.

use crate::db::meetings as db_meetings;
use crate::db::participants as db_participants;
use crate::feed_events::{FeedChange, FeedChangeReason};
use crate::nats_events::{
    ParticipantLeftPayload, MEETING_BECAME_EMPTY_SUBJECT, MEETING_ENDED_BY_HOST_SUBJECT,
    PARTICIPANT_LEFT_SUBJECT,
};
use futures::future::BoxFuture;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::broadcast;

/// Spawn the consumer for [`MEETING_ENDED_BY_HOST_SUBJECT`].
///
/// When `actix-api` broadcasts MEETING_ENDED on a host disconnect with
/// `end_on_host_leave=true`, it publishes a `MeetingEndedByHostPayload`
/// on this subject. We look the meeting up by `room_id` and set
/// `state='ended'` so the meetings list reflects the same outcome the
/// connected clients just received.
///
/// Idempotent: if the meeting is already ended (e.g. because the host
/// also clicked Hangup, or another chat_server replica racing on the
/// same broadcast) the UPDATE is a no-op at SQL level
/// (`db_meetings::end_meeting` is `UPDATE … WHERE id = $1 AND state <> 'ended'`).
///
/// Graceful degradation: when `nats` is `None`, this function returns
/// without spawning anything. The DB stays consistent only via the REST
/// `/leave` endpoint in that environment, matching the pre-fix behavior.
pub fn spawn_meeting_ended_by_host_consumer(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_meeting_ended_by_host_consumer_inner(nats, pool, feed_tx, None)
}

/// Spawn the consumer for [`MEETING_BECAME_EMPTY_SUBJECT`].
///
/// When `actix-api` detects that a room became empty (the last present
/// participant disconnected/left) for a meeting that did NOT end, it publishes
/// a `MeetingBecameEmptyPayload` on this subject. We look the meeting up by
/// `room_id` and call [`db_meetings::set_idle`], transitioning it to
/// `state='idle'` (everyone-left → idle).
///
/// Idempotent and race-safe: `set_idle` guards on `state='active'`, so it is a
/// no-op on an already-`idle` meeting (duplicate empty event, multi-replica
/// fan-out) and on an already-`ended` meeting (ended is terminal and must win
/// the end-vs-idle race). See [`db_meetings::set_idle`] for the full reasoning.
///
/// Graceful degradation: when `nats` is `None`, this returns without spawning.
pub fn spawn_meeting_became_empty_consumer(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_meeting_became_empty_consumer_inner(nats, pool, feed_tx, None)
}

/// Internal variant used by tests to eliminate the publish-before-subscribe
/// race.  `ready_tx` is signalled once the initial NATS subscription is
/// live; callers await the paired receiver before publishing test messages.
#[doc(hidden)]
pub fn spawn_meeting_ended_by_host_consumer_inner(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_room_state_consumer::<crate::nats_events::MeetingEndedByHostPayload, _>(
        nats,
        pool,
        feed_tx,
        FeedChangeReason::Ended,
        ready_tx,
        MEETING_ENDED_BY_HOST_SUBJECT,
        "host-disconnect DB-write fanout",
        |pool, meeting_id| {
            Box::pin(async move { db_meetings::end_meeting(&pool, meeting_id).await })
        },
    )
}

/// Internal variant used by tests to eliminate the publish-before-subscribe
/// race (see [`spawn_meeting_ended_by_host_consumer_inner`]).
#[doc(hidden)]
pub fn spawn_meeting_became_empty_consumer_inner(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_room_state_consumer::<crate::nats_events::MeetingBecameEmptyPayload, _>(
        nats,
        pool,
        feed_tx,
        FeedChangeReason::BecameIdle,
        ready_tx,
        MEETING_BECAME_EMPTY_SUBJECT,
        "room-empty DB-write fanout (empty->idle)",
        |pool, meeting_id| Box::pin(async move { db_meetings::set_idle(&pool, meeting_id).await }),
    )
}

/// Spawn the consumer for [`PARTICIPANT_LEFT_SUBJECT`].
///
/// When `actix-api` observes a single participant's session leave a room (a
/// transport disconnect that did NOT go through REST `/leave`, or an explicit
/// transport leave), it publishes a [`ParticipantLeftPayload`]. We look the
/// meeting up by `room_id` and mark `(meeting_id, user_id)` as `status='left',
/// left_at=NOW()` via [`db_participants::mark_left_by_disconnect`], so the
/// participant stops being counted as present (issue #1551).
///
/// Idempotent and reconnect-safe: the UPDATE only matches rows currently
/// `status IN ('admitted','waiting')`, so it is a no-op on a participant who has
/// already left, been kicked, or has no row. The relay only publishes after the
/// reconnect grace period AND when the user has no other live session, so a
/// brief disconnect+reconnect (or a multi-tab user) never marks a present
/// participant left.
///
/// Graceful degradation: when `nats` is `None`, this returns without spawning.
/// The DB then stays consistent only via the REST `/leave` endpoint, matching
/// the pre-fix behavior (an abnormal disconnect leaves a stale `admitted` row,
/// the very gap this consumer closes when NATS is configured).
pub fn spawn_participant_left_consumer(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_participant_left_consumer_inner(nats, pool, feed_tx, None)
}

/// Internal variant used by tests to eliminate the publish-before-subscribe
/// race (see [`spawn_meeting_ended_by_host_consumer_inner`]).
#[doc(hidden)]
pub fn spawn_participant_left_consumer_inner(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let nats = nats?;
    let subject = PARTICIPANT_LEFT_SUBJECT;
    let description = "participant-disconnect DB-write fanout (mark left)";
    let handle = tokio::spawn(async move {
        let mut ready_tx = ready_tx;
        loop {
            match nats.subscribe(subject).await {
                Ok(mut sub) => {
                    tracing::info!("Subscribed to {} ({})", subject, description);
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(());
                    }
                    while let Some(msg) = sub.next().await {
                        let payload =
                            match serde_json::from_slice::<ParticipantLeftPayload>(&msg.payload) {
                                Ok(p) => p,
                                Err(e) => {
                                    tracing::warn!("Dropping malformed {} payload: {e}", subject);
                                    continue;
                                }
                            };

                        // Defensive bounds, matching the room-state consumers.
                        if payload.room_id.is_empty() || payload.room_id.len() > 256 {
                            tracing::warn!(
                                "Ignoring {} with invalid room_id length: {}",
                                subject,
                                payload.room_id.len()
                            );
                            continue;
                        }
                        if payload.user_id.is_empty() || payload.user_id.len() > 256 {
                            tracing::warn!(
                                "Ignoring {} with invalid user_id length: {}",
                                subject,
                                payload.user_id.len()
                            );
                            continue;
                        }

                        match db_meetings::get_by_room_id(&pool, &payload.room_id).await {
                            Ok(Some(meeting)) => {
                                match db_participants::mark_left_by_disconnect(
                                    &pool,
                                    meeting.id,
                                    &payload.user_id,
                                )
                                .await
                                {
                                    Ok(rows) => {
                                        tracing::info!(
                                            "Applied {} for meeting {} (id={}) user {} \
                                             (rows_affected={})",
                                            subject,
                                            payload.room_id,
                                            meeting.id,
                                            payload.user_id,
                                            rows
                                        );
                                        // Nudge the local SSE clients only when a
                                        // row actually flipped to 'left' — a
                                        // duplicate/redelivered event matches zero
                                        // rows (the participant is already gone) and
                                        // changes nothing in the feed, so we skip it
                                        // to keep nudge cardinality tight. This
                                        // consumer runs on EVERY instance (fan-out,
                                        // no queue group), so feeding the LOCAL
                                        // broadcast here — rather than re-publishing
                                        // to NATS — nudges each instance's own SSE
                                        // clients exactly once and avoids an echo
                                        // loop on `internal.feed_changed`.
                                        if rows > 0 {
                                            let _ = feed_tx.send(FeedChange::new(
                                                payload.room_id.clone(),
                                                FeedChangeReason::ParticipantLeft,
                                            ));
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            "Failed to mark participant {} left for meeting {} \
                                             (id={}) on {}: {e}",
                                            payload.user_id,
                                            payload.room_id,
                                            meeting.id,
                                            subject
                                        );
                                    }
                                }
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    "Received {} for unknown room {}; ignoring",
                                    subject,
                                    payload.room_id
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "DB error looking up room {} for {} event: {e}",
                                    payload.room_id,
                                    subject
                                );
                            }
                        }
                    }
                    tracing::warn!(
                        "{} subscription stream ended, re-subscribing in 1s",
                        subject
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to subscribe to {}: {e}, retrying in 1s", subject);
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    Some(handle)
}

/// Extract the `room_id` from a deserialized internal payload.
///
/// All cross-service room-state payloads carry exactly one `room_id` field;
/// this trait lets the shared consumer loop stay generic over the concrete
/// payload type without reflection.
trait RoomIdPayload: DeserializeOwned + Send + 'static {
    fn room_id(&self) -> &str;
}

impl RoomIdPayload for crate::nats_events::MeetingEndedByHostPayload {
    fn room_id(&self) -> &str {
        &self.room_id
    }
}

impl RoomIdPayload for crate::nats_events::MeetingBecameEmptyPayload {
    fn room_id(&self) -> &str {
        &self.room_id
    }
}

/// Shared subscribe/re-subscribe/bounds loop for the internal room-state
/// consumers. Generic over the payload type `P` and the per-meeting DB action
/// `action` (which receives an owned `PgPool` clone and the resolved
/// `meeting.id`). Centralises the defensive `room_id` bounds, the
/// re-subscribe-on-stream-end behavior, and the ready-signal hook so each
/// consumer differs only in its subject and DB transition.
#[allow(clippy::too_many_arguments)]
fn spawn_room_state_consumer<P, F>(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
    reason: FeedChangeReason,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
    subject: &'static str,
    description: &'static str,
    action: F,
) -> Option<tokio::task::JoinHandle<()>>
where
    P: RoomIdPayload,
    F: Fn(PgPool, i32) -> BoxFuture<'static, Result<(), sqlx::Error>> + Send + 'static,
{
    let nats = nats?;
    let handle = tokio::spawn(async move {
        // Wrap in `Option` so `take()` can move the sender out exactly once
        // inside the loop body without violating Rust's move rules.
        let mut ready_tx = ready_tx;
        loop {
            match nats.subscribe(subject).await {
                Ok(mut sub) => {
                    tracing::info!("Subscribed to {} ({})", subject, description);
                    // Signal readiness exactly once — on the first successful
                    // subscription. Subsequent re-subscribe iterations see
                    // `None` and skip the signal.
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(());
                    }
                    while let Some(msg) = sub.next().await {
                        let payload = match serde_json::from_slice::<P>(&msg.payload) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!("Dropping malformed {} payload: {e}", subject);
                                continue;
                            }
                        };
                        let room_id = payload.room_id();

                        // Defensive bounds — payload is from a trusted peer but
                        // we still cap room_id to match the posture used
                        // elsewhere (e.g. the EvictInstance handler at
                        // chat_server.rs).
                        if room_id.is_empty() || room_id.len() > 256 {
                            tracing::warn!(
                                "Ignoring {} with invalid room_id length: {}",
                                subject,
                                room_id.len()
                            );
                            continue;
                        }

                        // Resolve room_id -> meeting.id, then apply the
                        // per-consumer DB transition. Both queries are cheap
                        // (room_id is indexed via the partial unique index on
                        // `meetings`).
                        match db_meetings::get_by_room_id(&pool, room_id).await {
                            Ok(Some(meeting)) => {
                                if let Err(e) = action(pool.clone(), meeting.id).await {
                                    tracing::error!(
                                        "Failed to apply {} for meeting {} (id={}): {e}",
                                        subject,
                                        room_id,
                                        meeting.id
                                    );
                                } else {
                                    tracing::info!(
                                        "Applied {} for meeting {} (id={})",
                                        subject,
                                        room_id,
                                        meeting.id
                                    );
                                    // Nudge local SSE clients AFTER the DB write
                                    // succeeds (additive, never on the error path).
                                    // The room-state DB actions return `()` not a
                                    // rows-affected count, and their guards make a
                                    // duplicate a SQL-level no-op (`set_idle` only
                                    // matches `state='active'`, `end_meeting` only
                                    // `state <> 'ended'`). Upstream `actix-api`
                                    // already fires these events ONCE per
                                    // transition, so a redundant nudge is rare and
                                    // harmless (the client re-fetches and sees no
                                    // change) — a spurious nudge is acceptable, a
                                    // MISSED change is not. This consumer runs on
                                    // EVERY instance (fan-out, no queue group), so
                                    // we feed the LOCAL broadcast here rather than
                                    // re-publishing to NATS: that nudges each
                                    // instance's own SSE clients exactly once and
                                    // avoids an echo loop on `internal.feed_changed`.
                                    let _ =
                                        feed_tx.send(FeedChange::new(room_id.to_string(), reason));
                                }
                            }
                            Ok(None) => {
                                // Meeting may have been hard-deleted between
                                // broadcast and event delivery. Not an error.
                                tracing::warn!(
                                    "Received {} for unknown room {}; ignoring",
                                    subject,
                                    room_id
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "DB error looking up room {} for {} event: {e}",
                                    room_id,
                                    subject
                                );
                            }
                        }
                    }
                    tracing::warn!(
                        "{} subscription stream ended, re-subscribing in 1s",
                        subject
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to subscribe to {}: {e}, retrying in 1s", subject);
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

    /// Verify the host-ended consumer correctly degrades when NATS is not
    /// configured. Uses `PgPool::connect_lazy` to satisfy the `pool` parameter
    /// without contacting a real database — when `nats` is `None`, the consumer
    /// returns `None` before the spawned task ever runs, so the lazy pool's
    /// connection is never attempted.
    #[tokio::test]
    async fn spawn_ended_returns_none_when_nats_disabled() {
        let lazy_pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://stub")
            .expect("connect_lazy should not contact the database");
        let (feed_tx, _feed_rx) = crate::feed_events::new_feed_channel();
        let handle = spawn_meeting_ended_by_host_consumer(None, lazy_pool, feed_tx);
        assert!(
            handle.is_none(),
            "spawn must return None when NATS is not configured"
        );
    }

    /// Same graceful-degradation contract for the became-empty (empty->idle)
    /// consumer.
    #[tokio::test]
    async fn spawn_became_empty_returns_none_when_nats_disabled() {
        let lazy_pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://stub")
            .expect("connect_lazy should not contact the database");
        let (feed_tx, _feed_rx) = crate::feed_events::new_feed_channel();
        let handle = spawn_meeting_became_empty_consumer(None, lazy_pool, feed_tx);
        assert!(
            handle.is_none(),
            "spawn must return None when NATS is not configured"
        );
    }

    /// Same graceful-degradation contract for the participant-left (mark-left)
    /// consumer (issue #1551).
    #[tokio::test]
    async fn spawn_participant_left_returns_none_when_nats_disabled() {
        let lazy_pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://stub")
            .expect("connect_lazy should not contact the database");
        let (feed_tx, _feed_rx) = crate::feed_events::new_feed_channel();
        let handle = spawn_participant_left_consumer(None, lazy_pool, feed_tx);
        assert!(
            handle.is_none(),
            "spawn must return None when NATS is not configured"
        );
    }
}
