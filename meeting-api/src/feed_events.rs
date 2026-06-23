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

//! Live homepage-feed change notifications (issue #1081).
//!
//! The homepage meeting list (`GET /api/v1/meetings`, and the richer
//! `GET /api/v1/meetings/feed`) used to be fetch-on-mount + manual refresh.
//! This module makes it update **live** via server push: whenever something
//! changes what the feed shows for any user (a meeting is created, a
//! participant is admitted/joins, a meeting goes idle/ended, a participant
//! leaves), the server emits a lightweight **"feed-changed" nudge**. The
//! browser's `EventSource` client (the frontend follow-up to this PR) re-fetches
//! the feed on each nudge after a short debounce.
//!
//! ## Why a nudge, not the feed payload
//!
//! The feed is **per-user and auth-filtered** (`list_feed_for_user` /
//! `list_by_owner` only return meetings the requesting user owns or was admitted
//! into). If we pushed the full feed over SSE we would have to re-implement that
//! per-user authorization filter inside the push layer and re-serialize the feed
//! per connected client per change — an O(connections × meetings) fan-out with a
//! duplicated, security-critical ACL. Instead we push a content-free nudge and
//! let the client re-hit the existing per-user feed endpoint, which already does
//! the authorization correctly. The SSE layer therefore needs **no** per-delta
//! authorization and **no** duplicate feed-serialization logic.
//!
//! ## Multi-instance correctness (the load-bearing design decision)
//!
//! `meeting-api` may run as **multiple horizontally-scaled instances** behind a
//! load balancer. A [`tokio::sync::broadcast`] channel is **per-process**, so an
//! SSE client connected to instance A would never see a change produced on
//! instance B. Two classes of change exist:
//!
//! 1. **NATS-consumer-driven** (`internal.meeting_became_empty`,
//!    `internal.meeting_ended_by_host`, `internal.participant_left`): the
//!    existing consumers in [`crate::nats_consumers`] use a **plain
//!    `nats.subscribe()` with NO queue group**, i.e. fan-out — every instance
//!    already receives every such event and applies the DB write.
//!
//! 2. **Local-HTTP-mutation-driven** (meeting create, admit / admit-all, the
//!    idle→active reactivation on host/attendee join, end, leave-driven end):
//!    these run **only on the single instance that handled the HTTP request**.
//!
//! A purely local broadcast would therefore silently miss class 2 on every other
//! instance — the exact "looks right, doesn't work at scale" defect. So every
//! mutation point publishes a [`FeedChange`] to a **dedicated NATS subject**
//! [`FEED_CHANGED_SUBJECT`], which **every instance subscribes to without a
//! queue group** (fan-out). Each instance's subscriber feeds the change into its
//! own local [`broadcast::Sender`], which the SSE handler ([`crate::routes::feed_stream`])
//! drains to its connected clients. This is correct for single- AND
//! multi-instance deployments.
//!
//! When NATS is **not** configured (single-instance dev / test), the mutation
//! points fall back to publishing straight into the local broadcast, so SSE
//! still works end-to-end without NATS — see [`publish_feed_change`].

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// NATS subject that carries [`FeedChange`] notifications between every
/// `meeting-api` instance. Subscribed **without a queue group** (fan-out) by
/// [`spawn_feed_change_consumer`] so all instances — and therefore all their
/// SSE clients — observe every change, regardless of which instance produced
/// it. See the module docs for the full multi-instance rationale.
pub const FEED_CHANGED_SUBJECT: &str = "internal.feed_changed";

/// Bound on the per-process broadcast channel. A nudge is tiny and SSE
/// receivers drain it immediately, so a small buffer is plenty. If a receiver
/// ever falls far enough behind to overflow this, it sees a
/// [`broadcast::error::RecvError::Lagged`] which the SSE handler maps to a
/// single generic "refresh" nudge (the client re-fetches the whole feed
/// anyway, so a coalesced nudge loses no correctness — see
/// [`crate::routes::feed_stream`]).
pub const FEED_BROADCAST_CAPACITY: usize = 256;

/// Create a feed-change broadcast channel sized at [`FEED_BROADCAST_CAPACITY`].
///
/// Returned `Sender` goes into [`AppState`] (and is cloned for the fan-out
/// subscriber); the `Receiver` is dropped by callers that only need the sender
/// (the SSE handler creates its own receiver per connection via
/// `Sender::subscribe`). A convenience so the channel capacity is defined in
/// exactly one place across the binary and every test `AppState`.
///
/// [`AppState`]: crate::state::AppState
pub fn new_feed_channel() -> (
    broadcast::Sender<FeedChange>,
    broadcast::Receiver<FeedChange>,
) {
    broadcast::channel(FEED_BROADCAST_CAPACITY)
}

/// Why the feed changed. Carried on a [`FeedChange`] so the SSE event can name
/// a `reason` (useful for client-side debugging / metrics), but the client does
/// not branch on it — any nudge triggers the same debounced re-fetch.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedChangeReason {
    /// A meeting was created.
    Created,
    /// A participant was admitted, or a host/attendee join reactivated the
    /// meeting (idle→active), changing the present-participant count / state.
    Joined,
    /// A meeting transitioned to `idle` (everyone left, meeting not ended).
    BecameIdle,
    /// A meeting transitioned to `ended`.
    Ended,
    /// A single participant left / was marked left by disconnect, changing the
    /// present-participant count.
    ParticipantLeft,
    /// Generic catch-all emitted to a lagged SSE receiver (see
    /// [`FEED_BROADCAST_CAPACITY`]). Tells the client "you may have missed some
    /// changes, just re-fetch".
    Refresh,
}

impl FeedChangeReason {
    /// Stable wire string for the SSE `data` JSON and logs. Must match the
    /// `serde(rename_all = "snake_case")` representation so the JSON the SSE
    /// handler emits and the enum stay in lockstep.
    pub fn as_str(self) -> &'static str {
        match self {
            FeedChangeReason::Created => "created",
            FeedChangeReason::Joined => "joined",
            FeedChangeReason::BecameIdle => "became_idle",
            FeedChangeReason::Ended => "ended",
            FeedChangeReason::ParticipantLeft => "participant_left",
            FeedChangeReason::Refresh => "refresh",
        }
    }
}

/// A single "the homepage feed may have changed" notification.
///
/// Intentionally minimal: the affected meeting's `room_id` (used for
/// server-to-server routing / debug logging) and a [`FeedChangeReason`]. It does
/// NOT carry the feed payload — see the module docs for why.
///
/// This type is the INTERNAL wire: it is serialized as JSON on the
/// [`FEED_CHANGED_SUBJECT`] NATS subject for fan-out between `meeting-api`
/// instances. It is NOT what reaches SSE clients — the client-facing
/// `feed-changed` event carries a fixed content-free nudge that omits
/// `meeting_id` entirely, to avoid leaking every meeting's id to every
/// connected client. See
/// [`crate::routes::feed_stream::feed_change_to_sse_event`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FeedChange {
    /// The `room_id` of the meeting whose feed entry changed. Empty for a
    /// [`FeedChangeReason::Refresh`] coalesced nudge that does not name a single
    /// meeting.
    #[serde(default)]
    pub meeting_id: String,
    /// Why the feed changed.
    pub reason: FeedChangeReason,
}

impl FeedChange {
    /// Construct a change for a specific meeting.
    pub fn new(meeting_id: impl Into<String>, reason: FeedChangeReason) -> Self {
        Self {
            meeting_id: meeting_id.into(),
            reason,
        }
    }

    /// The generic "just re-fetch" nudge emitted to lagged SSE receivers.
    pub fn refresh() -> Self {
        Self {
            meeting_id: String::new(),
            reason: FeedChangeReason::Refresh,
        }
    }
}

/// Publish a [`FeedChange`] so it reaches every connected SSE client across all
/// `meeting-api` instances.
///
/// Cross-instance delivery is via NATS [`FEED_CHANGED_SUBJECT`] (fan-out): the
/// publishing instance does NOT also push to its own local broadcast, because
/// it is itself a fan-out subscriber and would otherwise deliver the nudge
/// twice to its own SSE clients. A duplicate nudge is harmless (the client just
/// re-fetches and sees no change), but avoiding the obvious double-send keeps
/// the cardinality clean.
///
/// When NATS is **not** configured, there is by definition a single instance and
/// no fan-out subscriber, so we publish straight into the local broadcast to
/// keep SSE working in dev / test. In both cases a send error is logged and
/// swallowed — a missed nudge must never fail the originating mutation (the DB
/// write already succeeded; the client also re-fetches on its own keep-alive /
/// reconnect cadence as a backstop).
///
/// `local_tx` is this instance's broadcast sender (from [`AppState`]). It is
/// used only on the NATS-absent fallback path.
///
/// [`AppState`]: crate::state::AppState
pub async fn publish_feed_change(
    nats: Option<&async_nats::Client>,
    local_tx: &broadcast::Sender<FeedChange>,
    change: FeedChange,
) {
    match nats {
        Some(nats) => {
            let bytes = match serde_json::to_vec(&change) {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(
                        "Failed to serialize FeedChange for {} ({}): {e}",
                        change.meeting_id,
                        change.reason.as_str()
                    );
                    return;
                }
            };
            if let Err(e) = nats.publish(FEED_CHANGED_SUBJECT, bytes.into()).await {
                tracing::error!(
                    "Failed to publish {} for {} ({}): {e}",
                    FEED_CHANGED_SUBJECT,
                    change.meeting_id,
                    change.reason.as_str()
                );
            } else {
                tracing::debug!(
                    "Published {} for {} ({})",
                    FEED_CHANGED_SUBJECT,
                    change.meeting_id,
                    change.reason.as_str()
                );
            }
        }
        None => {
            // Single-instance / no-NATS: deliver straight to local SSE clients.
            // `send` errors only when there are zero receivers (no SSE clients
            // connected); that is the normal idle case, not an error.
            let _ = local_tx.send(change);
        }
    }
}

/// Spawn the long-lived fan-out subscriber that mirrors NATS
/// [`FEED_CHANGED_SUBJECT`] into this instance's local broadcast.
///
/// Mirrors the spawn/re-subscribe pattern of the room-state consumers in
/// [`crate::nats_consumers`]: plain `nats.subscribe()` (NO queue group, so every
/// instance receives every change), re-subscribe with a 1s backoff on stream
/// end, and a no-op (`None`) return when NATS is not configured. `ready_tx` is
/// signalled once the initial subscription is live (tests await it to eliminate
/// the publish-before-subscribe race).
///
/// Returns `None` when NATS is not configured — in that single-instance mode
/// [`publish_feed_change`] feeds the local broadcast directly, so SSE still
/// works without this subscriber.
pub fn spawn_feed_change_consumer(
    nats: Option<async_nats::Client>,
    local_tx: broadcast::Sender<FeedChange>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<tokio::task::JoinHandle<()>> {
    use futures::StreamExt;
    use std::time::Duration;

    let nats = nats?;
    let handle = tokio::spawn(async move {
        let mut ready_tx = ready_tx;
        loop {
            match nats.subscribe(FEED_CHANGED_SUBJECT).await {
                Ok(mut sub) => {
                    tracing::info!(
                        "Subscribed to {} (feed-change SSE fan-out)",
                        FEED_CHANGED_SUBJECT
                    );
                    if let Some(tx) = ready_tx.take() {
                        let _ = tx.send(());
                    }
                    while let Some(msg) = sub.next().await {
                        match serde_json::from_slice::<FeedChange>(&msg.payload) {
                            Ok(change) => {
                                // `send` errors only when there are no local SSE
                                // receivers — the normal idle case, not an error.
                                let _ = local_tx.send(change);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Dropping malformed {} payload: {e}",
                                    FEED_CHANGED_SUBJECT
                                );
                            }
                        }
                    }
                    tracing::warn!(
                        "{} subscription stream ended, re-subscribing in 1s",
                        FEED_CHANGED_SUBJECT
                    );
                }
                Err(e) => {
                    tracing::error!(
                        "Failed to subscribe to {}: {e}, retrying in 1s",
                        FEED_CHANGED_SUBJECT
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

    #[test]
    fn reason_wire_strings_match_serde() {
        // The INTERNAL NATS payload JSON is produced by
        // `serde_json::to_string(&change)`, while logs/metrics use
        // `reason.as_str()`. They MUST agree or the server-to-server
        // `internal.feed_changed` contract and the logs would name the same
        // event differently. (The CLIENT-facing SSE `data:` line is a fixed
        // content-free nudge and does NOT serialize the reason — see
        // `crate::routes::feed_stream::FEED_NUDGE_DATA`.) This fails if either
        // the `as_str` arm or the `rename_all` mapping drifts.
        for (reason, expected) in [
            (FeedChangeReason::Created, "created"),
            (FeedChangeReason::Joined, "joined"),
            (FeedChangeReason::BecameIdle, "became_idle"),
            (FeedChangeReason::Ended, "ended"),
            (FeedChangeReason::ParticipantLeft, "participant_left"),
            (FeedChangeReason::Refresh, "refresh"),
        ] {
            assert_eq!(reason.as_str(), expected);
            // serde of the enum (no struct wrapper) yields a quoted string.
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
        }
    }

    #[test]
    fn feed_change_json_round_trips_and_pins_wire_shape() {
        // Pin the exact JSON the INTERNAL `internal.feed_changed` NATS payload
        // carries between `meeting-api` instances, so a field rename on the
        // publisher side is caught here rather than silently breaking the
        // server-to-server fan-out (the consumer in `spawn_feed_change_consumer`
        // deserializes this shape). This is the internal wire ONLY — the
        // client-facing SSE `data:` line is a fixed content-free nudge that does
        // NOT carry `meeting_id` (see
        // `crate::routes::feed_stream::feed_change_to_sse_event`).
        let change = FeedChange::new("standup-42", FeedChangeReason::Joined);
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(json, r#"{"meeting_id":"standup-42","reason":"joined"}"#);
        let back: FeedChange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, change);
    }

    #[test]
    fn refresh_nudge_has_empty_meeting_id() {
        let change = FeedChange::refresh();
        assert_eq!(change.reason, FeedChangeReason::Refresh);
        assert!(change.meeting_id.is_empty());
        let json = serde_json::to_string(&change).unwrap();
        assert_eq!(json, r#"{"meeting_id":"","reason":"refresh"}"#);
    }

    /// A published `FeedChange` must be observed by a subscriber on the local
    /// broadcast — the core wiring the SSE handler depends on. This fails if
    /// `publish_feed_change` stops delivering to `local_tx` on the NATS-absent
    /// path (e.g. a regression that early-returns before `send`).
    #[tokio::test]
    async fn publish_feed_change_no_nats_reaches_local_subscriber() {
        let (tx, mut rx) = broadcast::channel::<FeedChange>(FEED_BROADCAST_CAPACITY);
        let expected = FeedChange::new("room-xyz", FeedChangeReason::Ended);

        publish_feed_change(None, &tx, expected.clone()).await;

        let got = rx.try_recv().expect("subscriber must observe the change");
        assert_eq!(got, expected);
    }

    /// With a receiver already attached, a second `publish_feed_change` of a
    /// different reason is delivered in order — guards against a publish that
    /// drops on a non-empty channel.
    #[tokio::test]
    async fn publish_feed_change_no_nats_preserves_order() {
        let (tx, mut rx) = broadcast::channel::<FeedChange>(FEED_BROADCAST_CAPACITY);

        publish_feed_change(None, &tx, FeedChange::new("a", FeedChangeReason::Created)).await;
        publish_feed_change(
            None,
            &tx,
            FeedChange::new("b", FeedChangeReason::BecameIdle),
        )
        .await;

        assert_eq!(
            rx.recv().await.unwrap(),
            FeedChange::new("a", FeedChangeReason::Created)
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            FeedChange::new("b", FeedChangeReason::BecameIdle)
        );
    }

    /// `spawn_feed_change_consumer` must be a no-op (return `None`) when NATS is
    /// not configured, mirroring the room-state consumers' graceful-degradation
    /// contract. In that mode `publish_feed_change` feeds the local broadcast
    /// directly, so SSE still works without this subscriber.
    #[test]
    fn spawn_consumer_returns_none_when_nats_disabled() {
        let (tx, _rx) = broadcast::channel::<FeedChange>(FEED_BROADCAST_CAPACITY);
        let handle = spawn_feed_change_consumer(None, tx, None);
        assert!(
            handle.is_none(),
            "spawn must return None when NATS is not configured"
        );
    }
}
