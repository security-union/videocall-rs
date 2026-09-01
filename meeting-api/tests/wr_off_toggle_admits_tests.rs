/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Issue #2262 — the bulk `UPDATE` that admits the queue when the waiting room
//! goes off emits no per-participant event, so `update_meeting_settings` must
//! report who it moved for the PATCH route to push `PARTICIPANT_ADMITTED`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::meetings as db_meetings;
use meeting_api::password::PasswordUpdate;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ParticipantStatusResponse};

const HOST: &str = "host@example.com";

async fn create_active_meeting_wr_on(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(format!(
            r#"{{"meeting_id":"{room_id}","waiting_room_enabled":true}}"#
        )))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "meeting create must succeed"
    );

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Host"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "host join must succeed");
}

async fn join_into_waiting_room(pool: &sqlx::PgPool, room_id: &str, user: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), user)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Queued"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "attendee join must succeed");

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(
        body.result.status, "waiting",
        "attendee must be queued while the waiting room is on"
    );
}

#[tokio::test]
#[serial]
async fn test_wr_off_toggle_reports_the_participants_it_admitted() {
    let pool = get_test_pool().await;
    let room_id = "wr-off-toggle-reports-admits";
    create_active_meeting_wr_on(&pool, room_id).await;

    join_into_waiting_room(&pool, room_id, "queued-a@example.com").await;
    join_into_waiting_room(&pool, room_id, "queued-b@example.com").await;

    let update = db_meetings::update_meeting_settings(
        &pool,
        room_id,
        HOST,
        Some(false),
        None,
        None,
        None,
        None,
        None,
        &PasswordUpdate::Unchanged,
    )
    .await
    .expect("settings update must not error")
    .expect("the owner's row must update");

    assert!(
        !update.row.waiting_room_enabled,
        "the toggle must have persisted"
    );

    let mut admitted = update.auto_admitted_user_ids.clone();
    admitted.sort();
    assert_eq!(
        admitted,
        vec![
            "queued-a@example.com".to_string(),
            "queued-b@example.com".to_string()
        ],
        "turning the waiting room off must report both queued participants so each \
         can be sent a PARTICIPANT_ADMITTED push"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_settings_update_that_admits_nobody_reports_nobody() {
    let pool = get_test_pool().await;
    let room_id = "wr-off-toggle-reports-none";
    create_active_meeting_wr_on(&pool, room_id).await;

    let update = db_meetings::update_meeting_settings(
        &pool,
        room_id,
        HOST,
        None,
        Some(true),
        None,
        None,
        None,
        None,
        &PasswordUpdate::Unchanged,
    )
    .await
    .expect("settings update must not error")
    .expect("the owner's row must update");

    assert!(
        update.auto_admitted_user_ids.is_empty(),
        "an update that leaves the waiting room on must admit — and report — nobody, \
         got {:?}",
        update.auto_admitted_user_ids
    );

    cleanup_test_data(&pool, room_id).await;
}
