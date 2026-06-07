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

//! Integration tests for the presence-driven everyone-left → idle consumer.
//!
//! Covers the `internal.meeting_became_empty` consumer added to
//! `meeting-api/src/nats_consumers.rs`: when `actix-api` detects a room became
//! empty (last present participant left a meeting that did NOT end), it
//! publishes a `MeetingBecameEmptyPayload`, and the consumer must transition the
//! DB row from `state='active'` to `state='idle'`.
//!
//! Mirrors the host-leave consumer test in `host_leave_db_consistency_tests.rs`:
//! gated on a live NATS connection (skips silently when `NATS_URL` is unset) and
//! a live Postgres pool via `DATABASE_URL`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::nats_events::{MeetingBecameEmptyPayload, MEETING_BECAME_EMPTY_SUBJECT};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;

/// Build an Axum router that uses the supplied NATS client.
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
        display_name_rate_limit_disabled: false,
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

/// Poll the meetings table for `room_id` until `state` equals `expected`, up to
/// 5s. Returns the observed state (may differ from `expected` on timeout).
async fn poll_state(pool: &sqlx::PgPool, room_id: &str, expected: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let row = sqlx::query_as::<_, (String,)>("SELECT state FROM meetings WHERE room_id = $1")
            .bind(room_id)
            .fetch_one(pool)
            .await
            .expect("Should fetch state");
        if row.0 == expected {
            return Some(row.0);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Final read so the caller's assertion message shows the actual value.
    sqlx::query_as::<_, (String,)>("SELECT state FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(pool)
        .await
        .ok()
        .map(|r| r.0)
}

// ──────────────────────────────────────────────────────────────────────────
// TEST: active meeting -> became-empty event -> state='idle'.
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn became_empty_consumer_marks_active_meeting_idle() {
    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping became_empty integration test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-became-empty-consumer";
    cleanup_test_data(&pool, room_id).await;

    // create -> join (host activates), mirroring production end-to-end.
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

    let row = sqlx::query_as::<_, (String,)>("SELECT state FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(&pool)
        .await
        .expect("Should fetch state");
    assert_eq!(
        row.0, "active",
        "Test precondition: meeting should be active before the simulated empty event"
    );

    // Spawn the consumer with the ready-signal variant so we know the
    // subscription is live before publishing (no publish-before-subscribe race).
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let _handle = meeting_api::nats_consumers::spawn_meeting_became_empty_consumer_inner(
        Some(nats.clone()),
        pool.clone(),
        Some(ready_tx),
    )
    .expect("Consumer should be spawned when NATS is available");

    ready_rx
        .await
        .expect("Consumer must signal subscription readiness");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Publish the synthetic room-empty payload. In production this comes from
    // chat_server's leave_rooms normal-departure path when room_members hits 0.
    let payload = MeetingBecameEmptyPayload {
        room_id: room_id.to_string(),
    };
    nats.publish(
        MEETING_BECAME_EMPTY_SUBJECT,
        serde_json::to_vec(&payload).unwrap().into(),
    )
    .await
    .expect("Should publish");

    let state = poll_state(&pool, room_id, "idle").await;
    assert_eq!(
        state.as_deref(),
        Some("idle"),
        "Consumer must transition meeting to state='idle' within 5s of receiving \
         the {MEETING_BECAME_EMPTY_SUBJECT} event"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST: ended is terminal — a became-empty event must NOT resurrect it.
//
// This is the end-vs-idle race in the "END landed first" ordering, exercised
// through the real consumer + NATS wire path (not just the unit-level set_idle
// test).
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn became_empty_consumer_does_not_resurrect_ended_meeting() {
    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping became_empty integration test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-became-empty-ended-terminal";
    cleanup_test_data(&pool, room_id).await;

    // Drive the meeting straight to 'ended' at the DB layer.
    let row = meeting_api::db::meetings::create(
        &pool,
        room_id,
        "host@example.com",
        None,
        &serde_json::json!([]),
    )
    .await
    .expect("create must succeed");
    meeting_api::db::meetings::activate(&pool, row.id)
        .await
        .expect("activate must succeed");
    meeting_api::db::meetings::end_meeting(&pool, row.id)
        .await
        .expect("end_meeting must succeed");

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let _handle = meeting_api::nats_consumers::spawn_meeting_became_empty_consumer_inner(
        Some(nats.clone()),
        pool.clone(),
        Some(ready_tx),
    )
    .expect("Consumer should be spawned when NATS is available");

    ready_rx
        .await
        .expect("Consumer must signal subscription readiness");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let payload = MeetingBecameEmptyPayload {
        room_id: room_id.to_string(),
    };
    nats.publish(
        MEETING_BECAME_EMPTY_SUBJECT,
        serde_json::to_vec(&payload).unwrap().into(),
    )
    .await
    .expect("Should publish");

    // Give the consumer time to (not) act, then assert the state is still ended.
    // We poll for 'idle' (the wrong outcome) and expect the poll to time out and
    // return the unchanged 'ended'.
    let state = poll_state(&pool, room_id, "idle").await;
    assert_eq!(
        state.as_deref(),
        Some("ended"),
        "ended is terminal — the became-empty consumer must leave it ended, got {state:?}"
    );

    cleanup_test_data(&pool, room_id).await;
}
