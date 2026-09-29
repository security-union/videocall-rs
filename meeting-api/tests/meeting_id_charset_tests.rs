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

//! Integration tests for the meeting ID character set: the two endpoints that
//! create a meeting row (`POST /api/v1/meetings` and the auto-create in
//! `POST /api/v1/meetings/{meeting_id}/join`), and the handlers that act on a
//! row whose ID fails the rule.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::{meetings as db_meetings, participants as db_participants};
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, CreateMeetingResponse, ParticipantStatusResponse},
    APIError,
};

const HOST: &str = "charset-host@example.com";
const MEMBER: &str = "charset-member@example.com";
const WAITER: &str = "charset-waiter@example.com";
const LEGACY_ID: &str = "t2832-legacy.room";

async fn meeting_row_count(pool: &PgPool, room_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(pool)
        .await
        .expect("count meetings")
}

async fn join(pool: &PgPool, path_segment: &str) -> axum::response::Response {
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{path_segment}/join"),
        HOST,
    )
    .body(Body::empty())
    .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap()
}

async fn create(pool: &PgPool, meeting_id: &str) -> axum::response::Response {
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "meeting_id": meeting_id, "attendees": [] }).to_string(),
        ))
        .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap()
}

/// `(path segment as sent, meeting ID after axum percent-decodes it)`.
const INVALID_JOIN_IDS: &[(&str, &str)] = &[
    ("t2832-victim.room", "t2832-victim.room"),
    ("t2832-a%20b", "t2832-a b"),
    ("t2832-a*b", "t2832-a*b"),
    ("t2832-a%3Eb", "t2832-a>b"),
    ("t2832-a%0Ab", "t2832-a\nb"),
    ("t2832-a%2Fb", "t2832-a/b"),
    ("t2832-a%25b", "t2832-a%b"),
    ("t2832-caf%C3%A9", "t2832-caf\u{e9}"),
    ("%2E%2E", ".."),
];

#[tokio::test]
#[serial]
async fn test_join_rejects_invalid_meeting_ids_without_creating_a_meeting() {
    let pool = get_test_pool().await;

    for (segment, decoded) in INVALID_JOIN_IDS {
        cleanup_test_data(&pool, decoded).await;

        let resp = join(&pool, segment).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "join of {decoded:?} must be rejected"
        );
        let body: APIResponse<APIError> = response_json(resp).await;
        assert!(!body.success);
        assert_eq!(body.result.code, "INVALID_MEETING_ID", "{decoded:?}");
        assert_eq!(
            meeting_row_count(&pool, decoded).await,
            0,
            "join of {decoded:?} must not auto-create a meeting"
        );

        cleanup_test_data(&pool, decoded).await;
    }
}

#[tokio::test]
#[serial]
async fn test_join_rejects_a_meeting_id_longer_than_255_bytes() {
    let pool = get_test_pool().await;
    let too_long = "t".repeat(256);
    cleanup_test_data(&pool, &too_long).await;

    let resp = join(&pool, &too_long).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "INVALID_MEETING_ID");
    assert_eq!(meeting_row_count(&pool, &too_long).await, 0);
}

#[tokio::test]
#[serial]
async fn test_join_accepts_tilde_and_hyphen_meeting_ids() {
    let pool = get_test_pool().await;

    for room_id in ["t2832~a~b", "t2832-my-meeting"] {
        cleanup_test_data(&pool, room_id).await;

        let resp = join(&pool, room_id).await;
        assert_eq!(resp.status(), StatusCode::OK, "join of {room_id:?}");
        let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
        assert!(body.result.is_host, "{room_id:?}: auto-creator is host");
        assert!(body.result.room_token.is_some(), "{room_id:?}");
        assert_eq!(meeting_row_count(&pool, room_id).await, 1, "{room_id:?}");

        cleanup_test_data(&pool, room_id).await;
    }
}

#[tokio::test]
#[serial]
async fn test_create_accepts_tilde_and_hyphen_meeting_ids() {
    let pool = get_test_pool().await;

    for room_id in ["t2832~a~b", "t2832-my-meeting"] {
        cleanup_test_data(&pool, room_id).await;

        let resp = create(&pool, room_id).await;
        assert_eq!(resp.status(), StatusCode::CREATED, "create of {room_id:?}");
        let body: APIResponse<CreateMeetingResponse> = response_json(resp).await;
        assert_eq!(body.result.meeting_id, room_id);

        cleanup_test_data(&pool, room_id).await;
    }
}

/// Skips when `NATS_URL` is unset, like the other NATS-gated suites.
#[tokio::test]
#[serial]
async fn test_invalid_room_id_is_not_published_onto_a_colliding_room_subject() {
    use futures::StreamExt;
    use meeting_api::nats_events::{publish_host_mute, publish_meeting_activated};
    use protobuf::Message;
    use videocall_types::protos::meeting_packet::MeetingPacket;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    let Ok(url) = std::env::var("NATS_URL") else {
        eprintln!("NATS_URL not set — skipping NATS subject collision test");
        return;
    };
    let nats = async_nats::connect(&url).await.expect("connect to NATS");
    let mut sub = nats
        .subscribe("room.t2832_victim_room.system")
        .await
        .expect("subscribe");
    nats.flush().await.expect("flush subscription");

    assert!(
        publish_host_mute(Some(&nats), "t2832.victim.room", "", "attacker@example.com")
            .await
            .is_err(),
        "an invalid room id must not publish a mute-all"
    );
    publish_meeting_activated(Some(&nats), "t2832.victim.room").await;
    publish_meeting_activated(Some(&nats), "t2832_victim_room").await;
    nats.flush().await.expect("flush publishes");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), sub.next())
        .await
        .expect("the valid room's event must arrive")
        .expect("subscription open");
    let wrapper = PacketWrapper::parse_from_bytes(&msg.payload).expect("PacketWrapper");
    let packet = MeetingPacket::parse_from_bytes(&wrapper.data).expect("MeetingPacket");
    assert_eq!(
        packet.room_id, "t2832_victim_room",
        "the first event on the victim's subject must be the victim's own"
    );
}

#[tokio::test]
#[serial]
async fn test_create_rejects_invalid_meeting_ids() {
    let pool = get_test_pool().await;

    for (_, room_id) in INVALID_JOIN_IDS {
        cleanup_test_data(&pool, room_id).await;

        let resp = create(&pool, room_id).await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "create of {room_id:?}"
        );
        let body: APIResponse<APIError> = response_json(resp).await;
        assert_eq!(body.result.code, "INVALID_MEETING_ID", "{room_id:?}");
        assert_eq!(meeting_row_count(&pool, room_id).await, 0, "{room_id:?}");
    }
}

/// A row the unvalidated `/join` could create: active, guests allowed, HOST as
/// host, MEMBER admitted, WAITER waiting.
async fn plant_invalid_id_meeting(pool: &PgPool, room_id: &str) -> i32 {
    cleanup_test_data(pool, room_id).await;
    let meeting = db_meetings::create_with_options(
        pool,
        room_id,
        HOST,
        None,
        &serde_json::json!([]),
        true,
        false,
        true,
        true,
        false,
        true,
    )
    .await
    .expect("plant meeting");
    db_meetings::activate(pool, meeting.id)
        .await
        .expect("activate");
    db_participants::upsert_host(pool, meeting.id, HOST, Some("Host"))
        .await
        .expect("host");
    for user in [MEMBER, WAITER] {
        db_participants::join_attendee(pool, meeting.id, user, Some(user), false, false, true)
            .await
            .expect("join attendee");
    }
    db_participants::admit(pool, meeting.id, MEMBER, true)
        .await
        .expect("admit member");
    meeting.id
}

async fn meeting_snapshot(pool: &PgPool, meeting_id: i32) -> (String, Vec<String>) {
    let meeting: String = sqlx::query_scalar(
        "SELECT concat_ws(':', state, waiting_room_enabled, allow_guests, admitted_can_admit, \
         end_on_host_leave, deleted_at IS NULL, host_display_name) FROM meetings WHERE id = $1",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await
    .expect("meeting snapshot");
    let participants: Vec<String> = sqlx::query_scalar(
        "SELECT concat_ws(':', user_id, status, is_host, display_name) FROM meeting_participants \
         WHERE meeting_id = $1 ORDER BY user_id",
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await
    .expect("participant snapshot");
    (meeting, participants)
}

async fn send(
    pool: &PgPool,
    method: &str,
    uri: &str,
    user: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let req = request_with_cookie(method, uri, user);
    let req = match body {
        Some(json) => req
            .header("Content-Type", "application/json")
            .body(Body::from(json.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap()
}

#[tokio::test]
#[serial]
async fn test_state_changing_routes_reject_an_invalid_id_row_without_touching_it() {
    use serde_json::json;
    let pool = get_test_pool().await;
    let meeting_id = plant_invalid_id_meeting(&pool, LEGACY_ID).await;
    let before = meeting_snapshot(&pool, meeting_id).await;
    let base = format!("/api/v1/meetings/{LEGACY_ID}");

    let cases = [
        ("POST", "/mute", Some(json!({ "user_id": MEMBER }))),
        ("POST", "/mute-all", None),
        ("POST", "/disable-video", Some(json!({ "user_id": MEMBER }))),
        ("POST", "/disable-video-all", None),
        ("POST", "/kick", Some(json!({ "user_id": MEMBER }))),
        ("POST", "/transfer-host", Some(json!({ "user_id": MEMBER }))),
        ("POST", "/admit", Some(json!({ "user_id": WAITER }))),
        ("POST", "/admit-all", None),
        ("POST", "/reject", Some(json!({ "user_id": WAITER }))),
        (
            "PUT",
            "/display-name",
            Some(json!({ "display_name": "Renamed" })),
        ),
        ("PATCH", "", Some(json!({ "waiting_room_enabled": false }))),
    ];
    for (method, suffix, body) in cases {
        let resp = send(&pool, method, &format!("{base}{suffix}"), HOST, body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {suffix}");
        let body: APIResponse<APIError> = response_json(resp).await;
        assert_eq!(body.result.code, "INVALID_MEETING_ID", "{method} {suffix}");
        assert_eq!(
            meeting_snapshot(&pool, meeting_id).await,
            before,
            "{method} {suffix} must not change the meeting or its participants"
        );
    }

    cleanup_test_data(&pool, LEGACY_ID).await;
}

#[tokio::test]
#[serial]
async fn test_guest_join_rejects_invalid_meeting_ids() {
    let pool = get_test_pool().await;
    let meeting_id = plant_invalid_id_meeting(&pool, LEGACY_ID).await;
    let before = meeting_snapshot(&pool, meeting_id).await;

    for room_id in [LEGACY_ID, "t2832-missing.room"] {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"display_name":"Guest"}"#))
            .unwrap();
        let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{room_id:?}");
        let body: APIResponse<APIError> = response_json(resp).await;
        assert_eq!(body.result.code, "INVALID_MEETING_ID", "{room_id:?}");
    }
    assert_eq!(meeting_snapshot(&pool, meeting_id).await, before);

    cleanup_test_data(&pool, LEGACY_ID).await;
}

#[tokio::test]
#[serial]
async fn test_an_invalid_id_row_can_still_be_viewed_left_ended_and_deleted() {
    let pool = get_test_pool().await;
    plant_invalid_id_meeting(&pool, LEGACY_ID).await;
    let base = format!("/api/v1/meetings/{LEGACY_ID}");

    for (method, uri, user) in [
        ("GET", base.clone(), HOST),
        ("POST", format!("{base}/leave"), MEMBER),
        ("POST", format!("{base}/end"), HOST),
        ("DELETE", base.clone(), HOST),
    ] {
        let resp = send(&pool, method, &uri, user, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
    }
    assert!(db_meetings::get_by_room_id(&pool, LEGACY_ID)
        .await
        .expect("lookup")
        .is_none());

    cleanup_test_data(&pool, LEGACY_ID).await;
}
