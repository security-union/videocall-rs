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

//! Server-Sent Events stream for live homepage-feed updates (issue #1081).

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
};
use futures::Stream;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::auth::AuthUser;
use crate::feed_events::FeedChange;
use crate::state::AppState;

/// SSE `event:` name carried by every feed-change nudge. The frontend
/// `EventSource` client listens for exactly this name. Keep in lockstep with
/// the frontend follow-up.
const FEED_CHANGED_EVENT: &str = "feed-changed";

/// Content-free `data:` payload carried by EVERY client-facing `feed-changed`
/// SSE event. It is a fixed constant — it NEVER carries `meeting_id` or any
/// other meeting-specific field.
///
/// # Why content-free (security: no cross-tenant disclosure)
///
/// The SSE broadcast is global and unfiltered: every authenticated SSE client
/// receives every [`FeedChange`] published on this instance, regardless of
/// whether that client can see the affected meeting. Serializing the internal
/// [`FeedChange`] (which carries `meeting_id`) onto this wire would therefore
/// leak the `room_id` and activity of EVERY meeting on the platform to EVERY
/// connected client — a cross-tenant information disclosure. The nudge is
/// advisory only: the client re-fetches the per-user auth-filtered
/// `GET /api/v1/meetings`, which is what enforces visibility, so the payload
/// needs to carry nothing meeting-specific. A fixed `"refresh"` reason leaks
/// nothing identifying.
const FEED_NUDGE_DATA: &str = r#"{"reason":"refresh"}"#;

/// Keep-alive cadence. SSE keep-alive comments (`:` lines) are emitted on an
/// otherwise-idle stream every this often so intermediary proxies / load
/// balancers do not drop the connection as idle. 15s is comfortably under the
/// common 30–60s idle-timeout defaults.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// `GET /api/v1/meetings/feed/stream` — live homepage-feed change stream.
///
/// # Client contract (frontend follow-up consumes this verbatim)
///
/// - **Route:** `GET /api/v1/meetings/feed/stream`
/// - **Auth:** authenticated, same [`AuthUser`] extractor (session cookie /
///   Bearer) as `GET /api/v1/meetings`. Unauthenticated requests get `401`.
/// - **Response:** `text/event-stream` (SSE). The stream stays open until the
///   client disconnects.
/// - **Event name:** `feed-changed` (`event: feed-changed`).
/// - **Data shape:** a single, FIXED, content-free JSON line —
///   `data: {"reason":"refresh"}` — for EVERY change. The client-facing wire
///   intentionally carries NO `meeting_id` or other meeting-specific field: the
///   SSE broadcast is global/unfiltered, so leaking the affected room id to
///   every connected client would be a cross-tenant information disclosure. The
///   nudge is advisory only; clients MUST NOT trust it for authorization —
///   re-fetching the per-user feed is what enforces visibility. See
///   [`FEED_NUDGE_DATA`].
/// - **Keep-alive:** SSE comment heartbeats (`:` lines) every ~15s on an idle
///   stream, so proxies don't drop the connection.
/// - **Client behavior:** on ANY `feed-changed` event, the client debounces
///   ~300–500ms (to coalesce bursts during reconnection / admit-all storms) and
///   then re-fetches `GET /api/v1/meetings`. The nudge is content-free by
///   design: the re-fetch reuses the existing per-user auth-filtered feed
///   endpoint, so the push layer needs no per-delta authorization. A spurious
///   nudge is harmless (the client re-fetches and sees no change); the
///   guarantee is that no real change is missed.
///
/// # Lifecycle
///
/// Each connection subscribes to this instance's per-process broadcast
/// ([`AppState::feed_tx`]). The subscription is owned by the returned stream;
/// when the client disconnects, axum drops the stream, which drops the
/// [`broadcast::Receiver`], cleanly releasing the subscription (no leak).
///
/// A receiver that falls behind the broadcast buffer yields
/// [`RecvError::Lagged`]; we map it to a single generic `refresh` nudge rather
/// than erroring the stream, because the client re-fetches the whole feed on any
/// nudge — so a coalesced nudge after a lag loses no correctness. On
/// [`RecvError::Closed`] (sender gone — only at shutdown) the stream ends.
pub async fn feed_stream(
    State(state): State<AppState>,
    _auth: AuthUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.feed_tx.subscribe();
    Sse::new(feed_event_stream(rx)).keep_alive(KeepAlive::new().interval(KEEPALIVE_INTERVAL))
}

/// Adapt a [`broadcast::Receiver<FeedChange>`] into the SSE event stream.
///
/// Pulled out of the handler so the `recv` → event mapping is exercised by the
/// async wiring test without standing up an HTTP server. Ends the stream on
/// `Closed`; maps `Lagged` to a generic refresh nudge.
fn feed_event_stream(
    rx: broadcast::Receiver<FeedChange>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures::stream::unfold(rx, |mut rx| async move {
        // Each arm produces the next unfold step directly (no looping): an event
        // on `Ok`/`Lagged`, or stream-end on `Closed`. `unfold` re-invokes this
        // closure for the following item, so a `Lagged`-coalesced refresh does
        // not swallow subsequent changes.
        match rx.recv().await {
            Ok(change) => Some((Ok(feed_change_to_sse_event(&change)), rx)),
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    "feed SSE receiver lagged by {skipped} changes; emitting generic refresh"
                );
                Some((Ok(feed_change_to_sse_event(&FeedChange::refresh())), rx))
            }
            // Sender dropped (process shutdown). End the stream.
            Err(RecvError::Closed) => None,
        }
    })
}

/// Map a [`FeedChange`] to the SSE [`Event`] the client receives.
///
/// Pure and synchronous so the event-name / data-shape contract is unit-tested
/// directly. The `event` name is the fixed [`FEED_CHANGED_EVENT`]; the `data`
/// line is the fixed, content-free [`FEED_NUDGE_DATA`] nudge for EVERY change.
///
/// The `_change` contents are intentionally NOT serialized onto the client wire:
/// the SSE broadcast is global/unfiltered, so emitting the affected
/// `meeting_id` would disclose every meeting's room id and activity to every
/// connected client (cross-tenant info leak). The client re-fetches the
/// per-user auth-filtered feed on any nudge, so the payload needs to carry
/// nothing meeting-specific. The `meeting_id` still travels on the INTERNAL
/// server-to-server NATS subject (see [`crate::feed_events::publish_feed_change`]),
/// which is not client-exposed; if it is wanted for server-side debugging it
/// must be logged from there (or before this mapping), never placed on `data`.
fn feed_change_to_sse_event(_change: &FeedChange) -> Event {
    Event::default()
        .event(FEED_CHANGED_EVENT)
        .data(FEED_NUDGE_DATA)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed_events::{FeedChangeReason, FEED_BROADCAST_CAPACITY};
    use futures::StreamExt;

    /// Render a single SSE [`Event`] to its on-the-wire string by driving it
    /// through a one-shot `Sse` response body. `Event` exposes no field getters,
    /// so encoding it through the real SSE body is the only way to assert the
    /// exact `event:` / `data:` lines a browser `EventSource` would parse.
    /// `async` so it reuses the `#[tokio::test]` runtime (no nested executor).
    async fn render_event(event: Event) -> String {
        use http_body_util::BodyExt;
        let stream = futures::stream::once(async move { Ok::<_, std::convert::Infallible>(event) });
        let sse = Sse::new(stream);
        let resp: axum::response::Response = axum::response::IntoResponse::into_response(sse);
        let collected = resp.into_body().collect().await.expect("collect sse body");
        String::from_utf8(collected.to_bytes().to_vec()).expect("utf8 sse body")
    }

    #[tokio::test]
    async fn maps_change_to_named_content_free_nudge() {
        // The event NAME must be the constant the frontend listens for, and the
        // DATA must be the FIXED, content-free nudge — NOT the FeedChange JSON.
        // This fails if the event name drifts from `feed-changed` or the data
        // stops being the constant nudge.
        let change = FeedChange::new("standup-42", FeedChangeReason::Joined);
        let rendered = render_event(feed_change_to_sse_event(&change)).await;
        // axum emits the SSE wire form `event: <name>` / `data: <payload>`
        // (a space follows the colon).
        assert!(
            rendered.contains(&format!("event: {FEED_CHANGED_EVENT}")),
            "must carry the `feed-changed` event name; got: {rendered}"
        );
        assert!(
            rendered.contains(&format!("data: {FEED_NUDGE_DATA}")),
            "must carry the fixed content-free nudge as data; got: {rendered}"
        );
    }

    /// SECURITY INVARIANT (cross-tenant info disclosure, issue #1081): the
    /// client-facing SSE wire must NEVER carry a meeting's `room_id`. The SSE
    /// broadcast is global/unfiltered, so any `meeting_id` on the wire would
    /// leak every meeting's id + activity to every connected client.
    ///
    /// This test feeds a `FeedChange` with a DISTINCTIVE meeting id and asserts
    /// that id (and the `meeting_id` field name) appears NOWHERE in the rendered
    /// SSE event. It is a mutation guard: re-adding `meeting_id` to the wire
    /// (e.g. `Event::data(serde_json::to_string(change))`) makes it FAIL.
    #[tokio::test]
    async fn sse_event_does_not_leak_meeting_id() {
        let secret = "secret-room-xyz";
        let change = FeedChange::new(secret, FeedChangeReason::Joined);
        let rendered = render_event(feed_change_to_sse_event(&change)).await;
        assert!(
            !rendered.contains(secret),
            "SSE wire must NOT leak the meeting id `{secret}`; got: {rendered}"
        );
        assert!(
            !rendered.contains("meeting_id"),
            "SSE wire must NOT carry a `meeting_id` field; got: {rendered}"
        );
        // Positive control: it is still the named content-free nudge.
        assert!(
            rendered.contains(&format!("event: {FEED_CHANGED_EVENT}")),
            "must still carry the `feed-changed` event name; got: {rendered}"
        );
        assert!(
            rendered.contains(&format!("data: {FEED_NUDGE_DATA}")),
            "must still carry the fixed content-free nudge; got: {rendered}"
        );
    }

    #[tokio::test]
    async fn refresh_change_serializes_to_content_free_nudge() {
        let rendered = render_event(feed_change_to_sse_event(&FeedChange::refresh())).await;
        assert!(rendered.contains(&format!("event: {FEED_CHANGED_EVENT}")));
        assert!(
            rendered.contains(&format!("data: {FEED_NUDGE_DATA}")),
            "refresh nudge must serialize to the content-free nudge; got: {rendered}"
        );
    }

    /// End-to-end of the stream adapter on the happy path: a published change is
    /// surfaced as the next SSE item, carrying the content-free nudge (and NOT
    /// the meeting id). Fails if `feed_event_stream` stops forwarding
    /// `Ok(change)` items, or if it starts leaking the meeting id onto the wire.
    #[tokio::test]
    async fn stream_forwards_published_change() {
        let (tx, rx) = broadcast::channel::<FeedChange>(FEED_BROADCAST_CAPACITY);
        let mut stream = Box::pin(feed_event_stream(rx));

        tx.send(FeedChange::new("room-1", FeedChangeReason::Created))
            .expect("a live receiver exists");

        let item = stream.next().await.expect("stream yields an item");
        let event = item.expect("event is Ok");
        let rendered = render_event(event).await;
        assert!(
            rendered.contains(&format!("data: {FEED_NUDGE_DATA}")),
            "stream must forward the published change as the content-free nudge; got: {rendered}"
        );
        assert!(
            !rendered.contains("room-1"),
            "stream must NOT leak the meeting id onto the SSE wire; got: {rendered}"
        );
    }

    /// The `Lagged` path must surface a generic `refresh` nudge, NOT error the
    /// stream. We force a lag by overflowing a capacity-1 channel before the
    /// receiver reads. Fails if the handler maps `Lagged` to a stream error /
    /// termination instead of a refresh event.
    #[tokio::test]
    async fn stream_maps_lagged_to_refresh() {
        // Capacity 1: sending two before reading drops the oldest and the next
        // `recv` returns `Lagged`.
        let (tx, rx) = broadcast::channel::<FeedChange>(1);
        let mut stream = Box::pin(feed_event_stream(rx));

        tx.send(FeedChange::new("a", FeedChangeReason::Created))
            .unwrap();
        tx.send(FeedChange::new("b", FeedChangeReason::Joined))
            .unwrap();

        // First poll observes the lag and yields the generic refresh nudge.
        let item = stream.next().await.expect("stream yields after lag");
        let event = item.expect("lagged path yields Ok(refresh), not an error");
        let rendered = render_event(event).await;
        assert!(
            rendered.contains(&format!("data: {FEED_NUDGE_DATA}")),
            "Lagged must map to the content-free refresh nudge; got: {rendered}"
        );
    }

    /// When the sender is dropped (shutdown), the stream ends cleanly (`None`)
    /// rather than erroring — so a connection task exits gracefully.
    #[tokio::test]
    async fn stream_ends_on_closed() {
        let (tx, rx) = broadcast::channel::<FeedChange>(FEED_BROADCAST_CAPACITY);
        let mut stream = Box::pin(feed_event_stream(rx));
        drop(tx);
        assert!(
            stream.next().await.is_none(),
            "closed sender must end the stream, not error it"
        );
    }
}
