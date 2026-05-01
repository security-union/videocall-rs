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

//! Integration tests for the display-name reconciliation policy on rejoin
//! (issue #502).
//!
//! Policy: when a participant row already exists for `(meeting_id, user_id)`
//! with a non-empty `display_name`, a follow-up join MUST NOT overwrite it.
//! Mid-meeting renames go through the rate-limited
//! `PUT /api/v1/meetings/{id}/display-name` endpoint, never through `join`.
//!
//! The bug being prevented: a user manually types "Antonio" into the
//! display-name field, joins the meeting, navigates back, then rejoins.
//! Between back-navigation and rejoin the OAuth profile fetch resolves and
//! writes a derived display name like "Tony" into localStorage, so the
//! second `join` request carries `display_name="Tony"`. Without
//! reconciliation, the DB row is silently overwritten — the meeting
//! suddenly shows the user as "Tony" even though they typed "Antonio".

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ParticipantStatusResponse};

/// Fetch the persisted `display_name` for `(room_id, email)` directly from
/// the DB so the assertion is independent of any response-shaping bugs in
/// the route handler.
async fn fetch_display_name(pool: &sqlx::PgPool, room_id: &str, email: &str) -> Option<String> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT mp.display_name \
         FROM meeting_participants mp \
         JOIN meetings m ON mp.meeting_id = m.id \
         WHERE m.room_id = $1 AND mp.user_id = $2",
    )
    .bind(room_id)
    .bind(email)
    .fetch_optional(pool)
    .await
    .expect("DB query should succeed");
    row.and_then(|(dn,)| dn)
}

/// Fetch the cached `host_display_name` from the `meetings` table so we can
/// confirm the same reconciliation policy applies to that field too.
async fn fetch_host_display_name(pool: &sqlx::PgPool, room_id: &str) -> Option<String> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT host_display_name FROM meetings WHERE room_id = $1")
            .bind(room_id)
            .fetch_optional(pool)
            .await
            .expect("DB query should succeed");
    row.and_then(|(dn,)| dn)
}

/// POST /api/v1/meetings/{room_id}/join with the given `display_name` body
/// for the supplied `email`. Returns the deserialized participant status.
async fn join_with_display_name(
    pool: &sqlx::PgPool,
    room_id: &str,
    email: &str,
    display_name: &str,
) -> ParticipantStatusResponse {
    let body = serde_json::json!({ "display_name": display_name }).to_string();
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), email)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "join with display_name=\"{display_name}\" should succeed for {email}"
    );
    let parsed: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    parsed.result
}

// ──────────────────────────────────────────────────────────────────────────
// TEST 1: Manually-typed display name survives an OAuth-derived rejoin
// ──────────────────────────────────────────────────────────────────────────
// First join: display_name="Antonio" (manually typed).
// Second join (same user_id, same meeting): display_name="Tony" (OAuth-derived).
// The DB row's display_name MUST still be "Antonio" — rejoin never silently
// renames a participant.
#[tokio::test]
#[serial]
async fn test_host_display_name_preserved_on_rejoin() {
    let pool = get_test_pool().await;
    let room_id = "issue-502-host-rejoin";
    let host_email = "tony@estrada-valdez.com";
    cleanup_test_data(&pool, room_id).await;

    // Create the meeting with the host.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host_email)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "meeting_id": room_id, "attendees": [] }))
                .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // First join: manually-typed name.
    let first = join_with_display_name(&pool, room_id, host_email, "Antonio").await;
    assert!(first.is_host, "Creator must be flagged as host");
    assert_eq!(
        fetch_display_name(&pool, room_id, host_email)
            .await
            .as_deref(),
        Some("Antonio"),
        "First join should persist the manually-typed display_name"
    );
    assert_eq!(
        fetch_host_display_name(&pool, room_id).await.as_deref(),
        Some("Antonio"),
        "First join should also seed the cached host_display_name"
    );

    // Second join: OAuth-derived name. DB row must NOT change.
    let second = join_with_display_name(&pool, room_id, host_email, "Tony").await;
    assert!(second.is_host);
    assert_eq!(
        fetch_display_name(&pool, room_id, host_email)
            .await
            .as_deref(),
        Some("Antonio"),
        "Rejoin must NOT overwrite the existing display_name"
    );
    assert_eq!(
        fetch_host_display_name(&pool, room_id).await.as_deref(),
        Some("Antonio"),
        "Rejoin must NOT overwrite the cached host_display_name either"
    );
    // The response should also reflect the reconciled value so clients
    // never see a transient "Tony" between the rejoin and the next status
    // poll.
    assert_eq!(
        second.host_display_name.as_deref(),
        Some("Antonio"),
        "Response must reflect the reconciled host_display_name (not the request's value)"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST 2: Empty display_name in the second join doesn't break reconciliation
// ──────────────────────────────────────────────────────────────────────────
// First join: display_name="Alice" (set normally).
// Second join: display_name="" (empty). Validation may reject this outright,
// but if it goes through (some clients send empty strings), the existing
// "Alice" must still survive — the reconciliation policy must NOT treat
// the request's empty value as authoritative.
//
// Note: `validate_display_name` rejects pure-whitespace and empty strings
// at the route boundary. We test the DB-level invariant by calling the DB
// function directly to confirm the SQL `COALESCE(NULLIF(...), $3)` pattern
// is correct in case future changes route around the validator.
#[tokio::test]
#[serial]
async fn test_db_empty_display_name_does_not_overwrite_existing() {
    use meeting_api::db::participants as db_participants;

    let pool = get_test_pool().await;
    let room_id = "issue-502-empty-rejoin";
    let host_email = "alice@example.com";
    cleanup_test_data(&pool, room_id).await;

    // Create the meeting and host row.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host_email)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "meeting_id": room_id, "attendees": [] }))
                .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // First join: "Alice".
    let _ = join_with_display_name(&pool, room_id, host_email, "Alice").await;
    assert_eq!(
        fetch_display_name(&pool, room_id, host_email)
            .await
            .as_deref(),
        Some("Alice")
    );

    // Look up the meeting_id (i32) so we can call upsert_host directly,
    // bypassing the route-handler validator.
    let meeting_row: (i32,) = sqlx::query_as("SELECT id FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(&pool)
        .await
        .expect("meeting should exist");

    // Direct call with display_name=Some("") simulates a future code path
    // that lets an empty string slip past validation. The SQL `NULLIF`
    // wrapper is what protects us in that case — assert it works.
    let _ = db_participants::upsert_host(&pool, meeting_row.0, host_email, Some(""))
        .await
        .expect("upsert_host with empty display_name should not fail");

    assert_eq!(
        fetch_display_name(&pool, room_id, host_email)
            .await
            .as_deref(),
        Some("Alice"),
        "Empty display_name in upsert MUST NOT overwrite existing non-empty value"
    );

    // And: a follow-up call with display_name=None must also leave it alone.
    let _ = db_participants::upsert_host(&pool, meeting_row.0, host_email, None)
        .await
        .expect("upsert_host with None display_name should not fail");
    assert_eq!(
        fetch_display_name(&pool, room_id, host_email)
            .await
            .as_deref(),
        Some("Alice"),
        "None display_name in upsert MUST NOT overwrite existing non-empty value"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST 3: First-time user with no existing row gets the requested name
// ──────────────────────────────────────────────────────────────────────────
// Regression check that the reconciliation policy doesn't break the
// legitimate first-time-set case. With no existing row, the SQL `INSERT`
// branch takes effect (the `ON CONFLICT` block is irrelevant), so the
// requested display_name lands as-is.
#[tokio::test]
#[serial]
async fn test_first_time_join_uses_requested_display_name() {
    let pool = get_test_pool().await;
    let room_id = "issue-502-first-time";
    let host_email = "newbie@example.com";
    cleanup_test_data(&pool, room_id).await;

    // Create the meeting (no participant row yet).
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host_email)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "meeting_id": room_id, "attendees": [] }))
                .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // First-time join with a fresh display_name.
    let first = join_with_display_name(&pool, room_id, host_email, "Bobby").await;
    assert!(first.is_host);
    assert_eq!(
        fetch_display_name(&pool, room_id, host_email)
            .await
            .as_deref(),
        Some("Bobby"),
        "First-time join should accept the requested display_name"
    );
    assert_eq!(
        fetch_host_display_name(&pool, room_id).await.as_deref(),
        Some("Bobby"),
        "First-time join should also seed the cached host_display_name"
    );

    cleanup_test_data(&pool, room_id).await;
}
