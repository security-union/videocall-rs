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
//! HTTP layer cannot observe directly — for example, a participant's transport
//! connecting to or leaving the media server (`actix-api`), which lives on a
//! WebSocket / WebTransport handler in a different process.
//!
//! Each consumer follows the same pattern as
//! `actix-api/src/actors/chat_server.rs::started`: subscribe in a loop,
//! deserialize from JSON, dispatch to a handler, and re-subscribe on stream
//! end. The functions are no-ops when NATS is not configured.

use crate::db::meetings as db_meetings;
use crate::db::participants as db_participants;
use crate::feed_events::{FeedChange, FeedChangeReason};
use crate::nats_events::{
    ParticipantPresencePayload, MEETING_BECAME_EMPTY_SUBJECT, MEETING_ENDED_BY_HOST_SUBJECT,
    PARTICIPANT_PRESENCE_SUBJECT,
};
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::broadcast;
use videocall_meeting_types::presence::{
    PresenceHeartbeat, PRESENCE_HEARTBEAT_INTERVAL_SECS, PRESENCE_HEARTBEAT_MAX_SESSIONS,
    PRESENCE_HEARTBEAT_SUBJECT, PRESENCE_LEASE_SECS,
};

/// Spawn the consumer for [`MEETING_ENDED_BY_HOST_SUBJECT`].
///
/// A relay predating #2702 (only present after a relay rollback) that
/// broadcast MEETING_ENDED on a host disconnect publishes a
/// `MeetingEndedByHostPayload` here; we set `state='ended'` so the meetings
/// list matches what its clients received.
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
/// Idempotent and race-safe: `set_idle` only moves an `active` meeting with
/// nobody present, so it is a no-op on an already-`idle` or `ended` meeting and
/// on one whose other relay binary still holds participants. See
/// [`db_meetings::set_idle`].
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
    // Cloned before `nats` moves into `spawn_room_state_consumer`, so the
    // per-message closure can compute presence health on its own copy.
    let nats_for_health = nats.clone();
    spawn_room_state_consumer::<crate::nats_events::MeetingBecameEmptyPayload, _>(
        nats,
        pool,
        feed_tx,
        FeedChangeReason::BecameIdle,
        ready_tx,
        MEETING_BECAME_EMPTY_SUBJECT,
        "room-empty DB-write fanout (empty->idle)",
        move |pool, meeting_id| {
            let nats_for_health = nats_for_health.clone();
            Box::pin(async move {
                let healthy = db_participants::presence_healthy(&pool, nats_for_health.as_ref())
                    .await
                    .unwrap_or(false);
                db_meetings::set_idle(&pool, meeting_id, healthy)
                    .await
                    .map(|_| ())
            })
        },
    )
}

/// Spawn the consumer for [`PARTICIPANT_PRESENCE_SUBJECT`], applying each
/// report with [`apply_participant_presence`].
///
/// Every replica subscribes (fan-out, no queue group): one subscription sees a
/// relay's reports in publish order, which a queue group would split across
/// replicas. The meeting row lock and the session guards in
/// [`db_participants::record_present`] / [`db_participants::record_left`] make
/// the second replica's application a no-op.
///
/// Graceful degradation: when `nats` is `None`, this returns without spawning.
pub fn spawn_participant_presence_consumer(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_participant_presence_consumer_inner(nats, pool, feed_tx, None)
}

/// Internal variant used by tests to eliminate the publish-before-subscribe
/// race (see [`spawn_meeting_ended_by_host_consumer_inner`]).
#[doc(hidden)]
pub fn spawn_participant_presence_consumer_inner(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let nats = nats?;
    let subject = PARTICIPANT_PRESENCE_SUBJECT;
    let handle = tokio::spawn(async move {
        let mut ready_tx = ready_tx;
        loop {
            match nats.subscribe(subject).await {
                Ok(mut sub) => {
                    tracing::info!("Subscribed to {} (participant presence)", subject);
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(());
                    }
                    while let Some(msg) = sub.next().await {
                        let report = match serde_json::from_slice::<ParticipantPresencePayload>(
                            &msg.payload,
                        ) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!("Dropping malformed {} payload: {e}", subject);
                                continue;
                            }
                        };
                        if report.room_id.is_empty()
                            || report.room_id.len() > 256
                            || report.user_id.is_empty()
                            || report.user_id.len() > 256
                            || report.session_id == 0
                        {
                            tracing::warn!(
                                "Ignoring {} with invalid fields (room_id={}, user_id={}, session_id={})",
                                subject,
                                report.room_id.len(),
                                report.user_id.len(),
                                report.session_id
                            );
                            continue;
                        }
                        match db_meetings::get_by_room_id(&pool, &report.room_id).await {
                            Ok(Some(meeting)) => {
                                apply_participant_presence(
                                    &pool,
                                    Some(&nats),
                                    &feed_tx,
                                    &meeting,
                                    &report,
                                )
                                .await;
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    "Received {} for unknown room {}; ignoring",
                                    subject,
                                    report.room_id
                                );
                            }
                            Err(e) => {
                                tracing::error!(
                                    "DB error looking up room {} for {} event: {e}",
                                    report.room_id,
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

/// Apply one relay presence report: [`db_participants::record_present`] or
/// [`db_participants::record_left`], then publish what changed — HOST_GRANTED
/// for a co-host promoted on connect, MEETING_ENDED when the last present host
/// left with `end_on_host_leave` — and nudge the local feed.
pub async fn apply_participant_presence(
    pool: &PgPool,
    nats: Option<&async_nats::Client>,
    feed_tx: &broadcast::Sender<FeedChange>,
    meeting: &db_meetings::MeetingRow,
    report: &ParticipantPresencePayload,
) {
    // A failed health check is itself treated as unhealthy — latch present
    // rather than risk fabricating "nobody present".
    let healthy = db_participants::presence_healthy(pool, nats)
        .await
        .unwrap_or(false);
    // Relay session ids are u64; the column stores the same 64 bits.
    let session_id = report.session_id as i64;
    if !report.present {
        match db_participants::record_left(pool, meeting.id, &report.user_id, session_id, healthy)
            .await
        {
            Ok(Some((_, end))) => announce_departure(nats, feed_tx, &meeting.room_id, end).await,
            Ok(None) => {}
            Err(e) => tracing::error!(
                "Failed to record {} left meeting {} (session {}): {e}",
                report.user_id,
                meeting.room_id,
                report.session_id
            ),
        }
        return;
    }
    let presence = match db_participants::record_present(
        pool,
        meeting.id,
        &report.user_id,
        session_id,
        healthy,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                "Failed to record {} present in meeting {} (session {}): {e}",
                report.user_id,
                meeting.room_id,
                report.session_id
            );
            return;
        }
    };
    if presence.promoted {
        crate::nats_events::announce_host_change(
            nats,
            &meeting.room_id,
            &report.user_id,
            meeting.creator_id.as_deref().unwrap_or_default(),
            true,
        )
        .await;
    }
    if presence.restored || presence.resumed || presence.promoted {
        let _ = feed_tx.send(FeedChange::new(
            meeting.room_id.clone(),
            FeedChangeReason::Joined,
        ));
    }
}

/// After a participant departed `room_id`: broadcast MEETING_ENDED when the
/// departure ended the meeting, and nudge the local feed.
pub async fn announce_departure(
    nats: Option<&async_nats::Client>,
    feed_tx: &broadcast::Sender<FeedChange>,
    room_id: &str,
    end: Option<db_participants::DepartureEnd>,
) {
    let reason = if end.is_some() {
        crate::nats_events::publish_meeting_ended(
            nats,
            room_id,
            crate::nats_events::HOST_LEFT_MESSAGE,
        )
        .await;
        FeedChangeReason::Ended
    } else {
        FeedChangeReason::ParticipantLeft
    };
    let _ = feed_tx.send(FeedChange::new(room_id.to_string(), reason));
}

/// Queue group of the heartbeat consumer: a heartbeat only renews leases, so
/// one replica applying it is enough.
pub const PRESENCE_HEARTBEAT_QUEUE: &str = "meeting-api-presence-heartbeat";

/// Spawn the consumer for [`PRESENCE_HEARTBEAT_SUBJECT`] (queue group
/// [`PRESENCE_HEARTBEAT_QUEUE`]), applying each with
/// [`apply_presence_heartbeat`]. No-op when NATS is not configured.
pub fn spawn_presence_heartbeat_consumer(
    nats: Option<async_nats::Client>,
    pool: PgPool,
) -> Option<tokio::task::JoinHandle<()>> {
    spawn_presence_heartbeat_consumer_inner(nats, pool, None)
}

/// Internal variant used by tests to eliminate the publish-before-subscribe
/// race (see [`spawn_meeting_ended_by_host_consumer_inner`]).
#[doc(hidden)]
pub fn spawn_presence_heartbeat_consumer_inner(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    let nats = nats?;
    let subject = PRESENCE_HEARTBEAT_SUBJECT;
    let handle = tokio::spawn(async move {
        let mut ready_tx = ready_tx;
        loop {
            match nats
                .queue_subscribe(subject, PRESENCE_HEARTBEAT_QUEUE.to_string())
                .await
            {
                Ok(sub) => {
                    tracing::info!("Subscribed to {} (presence heartbeats)", subject);
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(());
                    }
                    // Up to 8 in flight: each is an independent per-room
                    // UPDATE, so one slow write can't backlog the rest.
                    sub.for_each_concurrent(8, |msg| {
                        let pool = pool.clone();
                        async move {
                            let heartbeat =
                                match serde_json::from_slice::<PresenceHeartbeat>(&msg.payload) {
                                    Ok(h) => h,
                                    Err(e) => {
                                        tracing::warn!(
                                            "Dropping malformed {} payload: {e}",
                                            subject
                                        );
                                        return;
                                    }
                                };
                            if heartbeat.room_id.is_empty()
                                || heartbeat.room_id.len() > 256
                                || heartbeat.sessions.len() > PRESENCE_HEARTBEAT_MAX_SESSIONS
                            {
                                tracing::warn!(
                                    "Ignoring {} with invalid room_id length {} or {} sessions",
                                    subject,
                                    heartbeat.room_id.len(),
                                    heartbeat.sessions.len()
                                );
                                return;
                            }
                            apply_presence_heartbeat(&pool, &heartbeat).await;
                        }
                    })
                    .await;
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

/// Renew the presence lease of every well-formed session in `heartbeat` via
/// [`db_participants::record_heartbeat`] (which resolves the room itself, no
/// separate lookup). Returns the rows renewed.
pub async fn apply_presence_heartbeat(pool: &PgPool, heartbeat: &PresenceHeartbeat) -> u64 {
    // Relay session ids are u64; the column stores the same 64 bits.
    let sessions: Vec<(String, i64)> = heartbeat
        .sessions
        .iter()
        .filter(|s| !s.user_id.is_empty() && s.user_id.len() <= 256 && s.session_id != 0)
        .map(|s| (s.user_id.clone(), s.session_id as i64))
        .collect();
    match db_participants::record_heartbeat(pool, &heartbeat.room_id, &sessions).await {
        Ok(renewed) => renewed,
        Err(e) => {
            tracing::error!(
                "Failed to renew presence in meeting {}: {e}",
                heartbeat.room_id
            );
            0
        }
    }
}

/// How often [`spawn_presence_sweeper`] looks for lapsed presence leases.
pub const PRESENCE_SWEEP_INTERVAL: Duration = Duration::from_secs(PRESENCE_HEARTBEAT_INTERVAL_SECS);

/// Most lapsed leases one sweep departs.
const PRESENCE_SWEEP_BATCH: i64 = 500;

/// Spawn the sweeper that departs participants whose presence lease ran out —
/// a relay that crashed or was killed sends no departure — ending the meeting
/// when the last present host went. The first sweep waits one lease, so leases
/// that lapsed while no meeting-api replica consumed heartbeats are renewed
/// first. No-op when NATS is not configured: no relay reports presence then.
pub fn spawn_presence_sweeper(
    nats: Option<async_nats::Client>,
    pool: PgPool,
    feed_tx: broadcast::Sender<FeedChange>,
) -> Option<tokio::task::JoinHandle<()>> {
    let nats = nats?;
    Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(PRESENCE_LEASE_SECS)).await;
        let mut ticks = tokio::time::interval(PRESENCE_SWEEP_INTERVAL);
        loop {
            ticks.tick().await;
            sweep_tick(&pool, &nats, &feed_tx).await;
        }
    }))
}

/// One scheduler tick: sweeps only while [`presence_healthy`](db_participants::presence_healthy)
/// holds, so a heartbeat outage can't make the sweeper depart everyone it can
/// no longer hear from. `None` means skipped (including a failed health
/// check — unhealthy is the safe default). Split out so a test can drive one
/// tick synchronously instead of waiting on the real interval.
async fn sweep_tick(
    pool: &PgPool,
    nats: &async_nats::Client,
    feed_tx: &broadcast::Sender<FeedChange>,
) -> Option<usize> {
    match db_participants::presence_healthy(pool, Some(nats)).await {
        Ok(true) => match sweep_presence(pool, Some(nats), feed_tx).await {
            Ok(departed) => Some(departed),
            Err(e) => {
                tracing::error!("Presence sweep failed: {e}");
                None
            }
        },
        Ok(false) => {
            tracing::warn!(
                "Presence heartbeat pipeline unhealthy (stale watermark or NATS down); \
                 skipping this sweep tick"
            );
            None
        }
        Err(e) => {
            tracing::error!("Presence health check failed, skipping sweep tick: {e}");
            None
        }
    }
}

/// Test-only: drive one [`sweep_tick`] synchronously. Takes a live client
/// rather than `Option`: the real scheduler never ticks without one, so a
/// `None` client would test a never-reached branch instead of the gate.
#[doc(hidden)]
pub async fn sweep_tick_for_test(
    pool: &PgPool,
    nats: &async_nats::Client,
    feed_tx: &broadcast::Sender<FeedChange>,
) -> Option<usize> {
    sweep_tick(pool, nats, feed_tx).await
}

/// Advisory lock key for [`sweep_presence`], scoped to this one purpose.
const PRESENCE_SWEEP_ADVISORY_LOCK: i64 = 2_702_005;

/// One sweep: [`db_participants::depart_expired`] every participant whose
/// presence lease ran out, announcing each departure. Returns how many
/// departed. Holds a Postgres advisory lock so only one replica's tick
/// actually sweeps; a losing replica returns `Ok(0)` immediately. A single
/// row's failure is logged and skipped rather than aborting the batch.
pub async fn sweep_presence(
    pool: &PgPool,
    nats: Option<&async_nats::Client>,
    feed_tx: &broadcast::Sender<FeedChange>,
) -> Result<usize, sqlx::Error> {
    // Advisory lock/unlock are session-scoped: both must run on the same
    // physical connection.
    let mut lock_conn = pool.acquire().await?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PRESENCE_SWEEP_ADVISORY_LOCK)
        .fetch_one(&mut *lock_conn)
        .await?;
    if !acquired {
        return Ok(0);
    }
    // Unlock the same connection whether the sweep finishes, errors, or
    // panics: without `catch_unwind` here, a panic mid-sweep would unwind
    // past the unlock below, and the connection would return to the pool
    // (not closed) still holding the session-scoped advisory lock — wedging
    // every future sweep tick that draws that connection.
    let result = std::panic::AssertUnwindSafe(sweep_presence_locked(pool, nats, feed_tx))
        .catch_unwind()
        .await;
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(PRESENCE_SWEEP_ADVISORY_LOCK)
        .execute(&mut *lock_conn)
        .await;
    match result {
        Ok(departed) => departed,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

async fn sweep_presence_locked(
    pool: &PgPool,
    nats: Option<&async_nats::Client>,
    feed_tx: &broadcast::Sender<FeedChange>,
) -> Result<usize, sqlx::Error> {
    let mut departed = 0;
    for (meeting_id, room_id, user_id) in
        db_participants::expired_presences(pool, PRESENCE_SWEEP_BATCH).await?
    {
        match db_participants::depart_expired(pool, meeting_id, &user_id).await {
            Ok(Some((_, end))) => {
                tracing::info!(
                    "Presence lease of {user_id} in meeting {room_id} ran out (ended={end:?})"
                );
                announce_departure(nats, feed_tx, &room_id, end).await;
                departed += 1;
            }
            Ok(None) => {}
            Err(e) => {
                // Retried next tick either way; don't abort the batch.
                tracing::error!(
                    "Failed to depart expired presence for {user_id} in meeting {room_id}: {e}"
                );
            }
        }
    }
    Ok(departed)
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

    /// Same graceful-degradation contract for the participant presence consumer.
    #[tokio::test]
    async fn spawn_participant_presence_returns_none_when_nats_disabled() {
        let lazy_pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://stub")
            .expect("connect_lazy should not contact the database");
        let (feed_tx, _feed_rx) = crate::feed_events::new_feed_channel();
        let handle = spawn_participant_presence_consumer(None, lazy_pool, feed_tx);
        assert!(
            handle.is_none(),
            "spawn must return None when NATS is not configured"
        );
    }
}
