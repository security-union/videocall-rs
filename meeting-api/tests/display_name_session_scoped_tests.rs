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

//! REST-level integration tests for the session-scoped display-name rename
//! flow added as a follow-up to HCL issue #828.
//!
//! Two requirements pinned here:
//!
//! 1. `PUT /api/v1/meetings/{id}/display-name` MUST accept the new
//!    `session_id` field in the JSON body and respond with `200 OK`. The
//!    field is `Option<u64>` — older clients that omit it must continue to
//!    work (legacy / user-id-wide rename path).
//! 2. The DB row for the authenticated `(meeting_id, user_id)` is always
//!    updated on the persisted-default column, regardless of whether
//!    `session_id` was supplied. Per-session display names are intentionally
//!    NOT persisted; they're an in-meeting decoration that lives in
//!    `chat_server`'s `room_members` (verified in
//!    `actix-api/src/actors/chat_server.rs` unit tests).
//!
//! The wire-format guarantee that `MeetingPacket.session_id` is populated
//! when the request carries one is verified by the
//! `test_build_participant_display_name_changed_packet_*` unit tests in
//! `meeting-api/src/nats_events.rs`. NATS itself is not configured in
//! integration-test mode (`nats: None`), so the publish call short-circuits
//! and there is nothing on the wire to assert against here.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ParticipantStatusResponse};

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

/// Create the meeting and seed the host's participant row by joining.
async fn create_meeting_and_join(pool: &sqlx::PgPool, room_id: &str, host_email: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host_email)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "meeting_id": room_id, "attendees": [] }))
                .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        host_email,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::json!({ "display_name": "Initial Name" }).to_string(),
    ))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "host join must succeed");
}

// ──────────────────────────────────────────────────────────────────────────
// TEST 1: PUT with `session_id` in the body returns 200 OK
// ──────────────────────────────────────────────────────────────────────────
// The frontend agent is updating callers to include `session_id` from the
// renaming tab. The server must accept the new field and persist the
// `(meeting_id, user_id)` default — per-session decoration lives elsewhere.
#[tokio::test]
#[serial]
async fn put_display_name_accepts_session_id_field() {
    let pool = get_test_pool().await;
    let room_id = "issue-828-session-scoped-rest";
    let host_email = "session-scope@example.com";
    cleanup_test_data(&pool, room_id).await;

    create_meeting_and_join(&pool, room_id, host_email).await;

    let app = build_app(pool.clone());
    let body = serde_json::json!({
        "display_name": "Renamed Tab A",
        "session_id": 12345u64,
    });
    let req = request_with_cookie(
        "PUT",
        &format!("/api/v1/meetings/{room_id}/display-name"),
        host_email,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(body.to_string()))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let parsed: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "PUT with session_id must succeed; response={parsed:?}"
    );
    assert_eq!(
        parsed.result.display_name.as_deref(),
        Some("Renamed Tab A"),
        "response must reflect the new display name"
    );
    assert_eq!(
        fetch_display_name(&pool, room_id, host_email)
            .await
            .as_deref(),
        Some("Renamed Tab A"),
        "DB default for (meeting_id, user_id) must be updated; session_id is \
         broadcast-only metadata and does not change which row is persisted"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST 2: PUT without `session_id` field (legacy clients) still works
// ──────────────────────────────────────────────────────────────────────────
// `UpdateDisplayNameRequest.session_id` is `Option<u64>` and the field is
// `#[serde(default)]` — legacy clients that send `{"display_name": "..."}`
// must continue to receive `200 OK`.
#[tokio::test]
#[serial]
async fn put_display_name_without_session_id_is_legacy_path() {
    let pool = get_test_pool().await;
    let room_id = "issue-828-legacy-rest";
    let host_email = "legacy-scope@example.com";
    cleanup_test_data(&pool, room_id).await;

    create_meeting_and_join(&pool, room_id, host_email).await;

    // Legacy body shape: no `session_id` field at all.
    let app = build_app(pool.clone());
    let body = serde_json::json!({ "display_name": "Legacy Renamed" });
    let req = request_with_cookie(
        "PUT",
        &format!("/api/v1/meetings/{room_id}/display-name"),
        host_email,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(body.to_string()))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "legacy clients that omit `session_id` must still receive 200 OK"
    );

    cleanup_test_data(&pool, room_id).await;
}
