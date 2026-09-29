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

//! Integration tests for the end-meeting endpoint and meeting stats fields.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use futures::StreamExt;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, MeetingInfoResponse},
    APIError,
};
use videocall_types::protos::meeting_packet::{meeting_packet::MeetingEventType, MeetingPacket};
use videocall_types::protos::packet_wrapper::PacketWrapper;

// ── End Meeting ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_end_meeting_success() {
    let pool = get_test_pool().await;
    let room_id = "test-end-meeting-success";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
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

    // End the meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.meeting_id, room_id);
    assert_eq!(body.result.state, "ended");
    assert!(body.result.ended_at.is_some());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_end_meeting_not_owner() {
    let pool = get_test_pool().await;
    let room_id = "test-end-meeting-not-owner";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting as host.
    let app = build_app(pool.clone());
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
    let _ = app.oneshot(req).await.unwrap();

    // Non-owner tries to end it.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "other@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "NOT_OWNER");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_end_meeting_not_found() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings/nonexistent-end-test/end",
        "user@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "MEETING_NOT_FOUND");
}

#[tokio::test]
#[serial]
async fn test_end_meeting_idempotent() {
    let pool = get_test_pool().await;
    let room_id = "test-end-meeting-idempotent";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
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
    let _ = app.oneshot(req).await.unwrap();

    // End it the first time.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body1: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert_eq!(body1.result.state, "ended");
    let ended_at_1 = body1.result.ended_at;

    // End it again — should be idempotent, same state.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body2: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert_eq!(body2.result.state, "ended");
    assert_eq!(body2.result.ended_at, ended_at_1);

    cleanup_test_data(&pool, room_id).await;
}

/// Count `MEETING_ENDED` packets published within `window`.
async fn count_meeting_ended(
    system: &mut async_nats::Subscriber,
    window: std::time::Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + window;
    let mut n = 0;
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, system.next()).await {
        let wrapper = <PacketWrapper as protobuf::Message>::parse_from_bytes(&msg.payload)
            .expect("packet wrapper");
        let packet = <MeetingPacket as protobuf::Message>::parse_from_bytes(&wrapper.data)
            .expect("meeting packet");
        if packet.event_type == MeetingEventType::MEETING_ENDED.into() {
            n += 1;
        }
    }
    n
}

#[tokio::test]
#[serial]
async fn end_meeting_publishes_meeting_ended_to_the_room_once() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping MEETING_ENDED publish test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-end-meeting-publishes-meeting-ended";
    cleanup_test_data(&pool, room_id).await;
    let host = "host-end-publishes@example.com";

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "meeting_id": room_id, "attendees": [] }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::CREATED
    );

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), host)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);

    let mut system = nats
        .subscribe(format!("room.{room_id}.system"))
        .await
        .expect("subscribe to room system subject");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/end"), host)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        count_meeting_ended(&mut system, std::time::Duration::from_secs(2)).await,
        1,
        "a real /end must publish exactly one MEETING_ENDED to the room"
    );

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/end"), host)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        count_meeting_ended(&mut system, std::time::Duration::from_secs(1)).await,
        0,
        "the idempotent second /end must publish nothing"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn delete_should_broadcast_gates_on_the_soft_delete_option() {
    let pool = get_test_pool().await;
    let active_room = "test-delete-broadcast-active";
    let idle_room = "test-delete-broadcast-idle";
    cleanup_test_data(&pool, active_room).await;
    cleanup_test_data(&pool, idle_room).await;
    let host = "host-delete-broadcast@example.com";

    let row =
        meeting_api::db::meetings::create(&pool, active_room, host, None, &serde_json::json!([]))
            .await
            .expect("create must succeed");
    meeting_api::db::meetings::activate(&pool, row.id)
        .await
        .expect("activate must succeed");

    let winner = meeting_api::db::meetings::soft_delete(&pool, active_room, host)
        .await
        .expect("soft_delete must succeed");
    assert!(
        meeting_api::routes::meetings::delete_should_broadcast(&winner),
        "the winning delete of an active meeting must broadcast"
    );

    let loser = meeting_api::db::meetings::soft_delete(&pool, active_room, host)
        .await
        .expect("soft_delete must succeed even when nothing matched");
    assert!(
        loser.is_none(),
        "a second soft_delete on an already-deleted meeting must match nothing"
    );
    assert!(
        !meeting_api::routes::meetings::delete_should_broadcast(&loser),
        "the losing concurrent delete must not broadcast a duplicate"
    );

    meeting_api::db::meetings::create(&pool, idle_room, host, None, &serde_json::json!([]))
        .await
        .expect("create must succeed");
    let idle_deleted = meeting_api::db::meetings::soft_delete(&pool, idle_room, host)
        .await
        .expect("soft_delete must succeed");
    assert!(
        !meeting_api::routes::meetings::delete_should_broadcast(&idle_deleted),
        "deleting a meeting that was never activated must not broadcast"
    );

    cleanup_test_data(&pool, active_room).await;
    cleanup_test_data(&pool, idle_room).await;
}

#[tokio::test]
#[serial]
async fn delete_meeting_publishes_meeting_ended_only_for_an_active_meeting() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping delete-meeting MEETING_ENDED test");
        return;
    };
    let pool = get_test_pool().await;
    let active_room = "test-delete-meeting-publishes-active";
    let idle_room = "test-delete-meeting-publishes-idle";
    cleanup_test_data(&pool, active_room).await;
    cleanup_test_data(&pool, idle_room).await;
    let host = "host-delete-publishes@example.com";

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "meeting_id": active_room, "attendees": [] }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::CREATED
    );
    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{active_room}/join"),
        host,
    )
    .body(Body::empty())
    .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "meeting_id": idle_room, "attendees": [] }).to_string(),
        ))
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::CREATED
    );

    let mut active_system = nats
        .subscribe(format!("room.{active_room}.system"))
        .await
        .expect("subscribe to active room system subject");
    let mut idle_system = nats
        .subscribe(format!("room.{idle_room}.system"))
        .await
        .expect("subscribe to idle room system subject");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("DELETE", &format!("/api/v1/meetings/{active_room}"), host)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        count_meeting_ended(&mut active_system, std::time::Duration::from_secs(2)).await,
        1,
        "deleting an active meeting must publish exactly one MEETING_ENDED"
    );

    let app = build_app_on_nats(pool.clone(), nats.clone());
    let req = request_with_cookie("DELETE", &format!("/api/v1/meetings/{idle_room}"), host)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        count_meeting_ended(&mut idle_system, std::time::Duration::from_secs(1)).await,
        0,
        "deleting a never-activated meeting must publish nothing"
    );

    cleanup_test_data(&pool, active_room).await;
    cleanup_test_data(&pool, idle_room).await;
}

// ── Meeting Stats Fields ────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_get_meeting_includes_stats_fields() {
    let pool = get_test_pool().await;
    let room_id = "test-meeting-stats-fields";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
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
    let _ = app.oneshot(req).await.unwrap();

    // Get the meeting and verify stats fields are present.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.participant_count, 0);
    assert_eq!(body.result.waiting_count, 0);
    assert!(body.result.started_at > 0);
    assert!(body.result.ended_at.is_none());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_end_meeting_populates_ended_at() {
    let pool = get_test_pool().await;
    let room_id = "test-end-populates-ended-at";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
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
    let _ = app.oneshot(req).await.unwrap();

    // Verify ended_at is null before ending.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let before: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(before.result.ended_at.is_none());

    // End the meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify ended_at is populated after ending.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let after: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert_eq!(after.result.state, "ended");
    assert!(after.result.ended_at.is_some());
    assert!(after.result.ended_at.unwrap() > 0);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_update_meeting_returns_stats_fields() {
    let pool = get_test_pool().await;
    let room_id = "test-update-meeting-stats";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting with waiting room enabled.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": true
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Update waiting room setting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::to_string(&serde_json::json!({
            "waiting_room_enabled": false
        }))
        .unwrap(),
    ))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    // Verify stats fields are in the update response.
    assert_eq!(body.result.participant_count, 0);
    assert_eq!(body.result.waiting_count, 0);
    assert!(body.result.started_at > 0);
    assert!(!body.result.waiting_room_enabled);

    cleanup_test_data(&pool, room_id).await;
}
