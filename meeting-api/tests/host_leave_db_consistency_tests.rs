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

//! Integration tests for the host-leave DB-consistency fix (discussion #502).
//!
//! These tests cover:
//!   1. PATCH /meetings publishes both the public client-facing
//!      `MEETING_SETTINGS_UPDATED` event AND the new server-internal
//!      `internal.meeting_settings_updated` event, so chat_server's
//!      in-memory `room_policy` cache stays fresh after a mid-meeting toggle.
//!   2. The new `internal.meeting_ended_by_host` consumer transitions the
//!      DB row to `state='ended'` exactly the way the REST POST /leave
//!      endpoint does, so back-navigation host disconnects and the legitimate
//!      hangup flow leave the meetings list in the same state.
//!
//! Both tests are gated on a live NATS connection. They skip silently when
//! `NATS_URL` is unset, matching the posture of the existing chat_server
//! integration tests.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use futures::StreamExt;
use meeting_api::nats_events::{
    MeetingEndedByHostPayload, MeetingSettingsUpdatePayload, MEETING_ENDED_BY_HOST_SUBJECT,
    MEETING_SETTINGS_UPDATE_SUBJECT,
};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;

/// Build an Axum router that uses the supplied NATS client. Mirrors
/// [`build_app`] in [`test_helpers`] but threads a NATS client into
/// `AppState` so publishers exercise the real wire format end-to-end.
fn build_app_with_nats(pool: sqlx::PgPool, nats: async_nats::Client) -> axum::Router {
    use meeting_api::{routes, state::AppState};
    let state = AppState {
        db: pool,
        jwt_secret: TEST_JWT_SECRET.to_string(),
        token_ttl_secs: 600,
        session_ttl_secs: 3600,
        oauth: None,
        jwks_cache: None,
        cookie_domain: None,
        cookie_name: "session".to_string(),
        cookie_secure: false,
        nats: Some(nats),
        service_version_urls: Vec::new(),
        http_client: reqwest::Client::new(),
        display_name_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        display_name_rate_limiter_ops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        search: None,
        allow_anonymous: true,
        dev_user: None,
    };
    routes::router().with_state(state)
}

/// Connect to NATS for tests, returning `None` if `NATS_URL` is unset.
async fn maybe_connect_nats() -> Option<async_nats::Client> {
    let url = std::env::var("NATS_URL").ok()?;
    Some(
        async_nats::connect(&url)
            .await
            .expect("Failed to connect to NATS"),
    )
}

// ──────────────────────────────────────────────────────────────────────────
// TEST 1: PATCH /meetings fires the internal cache-refresh event.
//
// This locks in the publisher half of the cache-staleness fix. Without it,
// chat_server's `room_policy` would never see toggles unless the host
// reconnected with a fresh JWT.
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn patch_meeting_publishes_internal_settings_update() {
    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping host_leave integration test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-internal-settings-update";
    cleanup_test_data(&pool, room_id).await;

    // Subscribe BEFORE creating the meeting so we don't miss events.
    let mut sub = nats
        .subscribe(MEETING_SETTINGS_UPDATE_SUBJECT)
        .await
        .expect("Should subscribe");

    // Create the meeting as host.
    let app = build_app_with_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": []
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // PATCH end_on_host_leave to false.
    let app = build_app_with_nats(pool.clone(), nats.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::to_string(&serde_json::json!({
            "end_on_host_leave": false
        }))
        .unwrap(),
    ))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Wait for the internal event. Should arrive within a second on a
    // healthy NATS bus.
    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next())
        .await
        .expect("Internal settings update event must arrive within 2s")
        .expect("Subscription stream should not end");
    let payload: MeetingSettingsUpdatePayload =
        serde_json::from_slice(&msg.payload).expect("Payload should deserialize");

    assert_eq!(payload.room_id, room_id);
    assert!(
        !payload.end_on_host_leave,
        "Internal payload must carry the post-update end_on_host_leave value"
    );
    // The other three flags should be the meeting's current values
    // (defaults from creation: waiting_room_enabled=true, admitted_can_admit=false,
    // allow_guests=false). This locks in that we send the FULL snapshot, not
    // a sparse delta.
    assert!(payload.waiting_room_enabled);
    assert!(!payload.admitted_can_admit);
    assert!(!payload.allow_guests);

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST 2: Publishing `internal.meeting_ended_by_host` transitions the DB
// row to `state='ended'`.
//
// This proves the consumer wired into `main.rs` actually writes the row.
// We start the consumer manually (the way `main.rs` does) and then
// publish the synthetic payload. Once the consumer has processed it the
// meeting state must be 'ended'.
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn meeting_ended_by_host_consumer_marks_meeting_ended() {
    use meeting_api::nats_consumers;

    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping host_leave integration test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-host-ended-consumer";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting and force it active so the state transition is
    // observable. We use the public API surface to mirror what production
    // looks like end-to-end: create -> join (host activates).
    let app = build_app_with_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": []
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let app = build_app_with_nats(pool.clone(), nats.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Sanity: meeting is currently 'active'.
    let row = sqlx::query_as::<_, (String,)>("SELECT state FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(&pool)
        .await
        .expect("Should fetch state");
    assert_eq!(
        row.0, "active",
        "Test precondition: meeting should be active before the simulated host-leave NATS event"
    );

    // Spawn the consumer (the way main.rs does it).
    let _handle =
        nats_consumers::spawn_meeting_ended_by_host_consumer(Some(nats.clone()), pool.clone())
            .expect("Consumer should be spawned when NATS is available");

    // Publish the synthetic host-leave payload. In production this comes
    // from chat_server's leave_rooms host-broadcast path.
    let payload = MeetingEndedByHostPayload {
        room_id: room_id.to_string(),
    };
    nats.publish(
        MEETING_ENDED_BY_HOST_SUBJECT,
        serde_json::to_vec(&payload).unwrap().into(),
    )
    .await
    .expect("Should publish");

    // Poll the DB until state transitions to 'ended', up to 5s. The
    // consumer is async — we can't observe the write synchronously. We
    // poll instead of sleeping a fixed duration so the test stays fast
    // on healthy environments and only waits when the consumer is slow.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut state: Option<String> = None;
    while std::time::Instant::now() < deadline {
        let row = sqlx::query_as::<_, (String,)>("SELECT state FROM meetings WHERE room_id = $1")
            .bind(room_id)
            .fetch_one(&pool)
            .await
            .expect("Should fetch state");
        if row.0 == "ended" {
            state = Some(row.0);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        state.as_deref(),
        Some("ended"),
        "Consumer must transition meeting to state='ended' within 5s of receiving \
         the {} event",
        MEETING_ENDED_BY_HOST_SUBJECT
    );

    cleanup_test_data(&pool, room_id).await;
}
