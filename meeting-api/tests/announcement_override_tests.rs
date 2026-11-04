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

//! Meeting-wide join/leave announcement overrides (issue 3168).

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures::StreamExt;
use meeting_api::db::meetings as db_meetings;
use serde_json::{json, Value};
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{
    APIResponse, CreateMeetingResponse, ListFeedResponse, ListMeetingsResponse,
    MeetingInfoResponse, ParticipantStatusResponse,
};
use videocall_meeting_types::{APIError, AnnouncementOverride};
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use AnnouncementOverride::{Off, On, Participant};

const OWNER: &str = "announce-owner@example.com";
const ATTENDEE: &str = "announce-attendee@example.com";
const ATTENDEE2: &str = "announce-attendee2@example.com";
const CO_HOST: &str = "announce-cohost@example.com";

const FIELDS: [&str; 4] = [
    "entry_message_override",
    "entry_sound_override",
    "exit_message_override",
    "exit_sound_override",
];

type Overrides = [AnnouncementOverride; 4];

macro_rules! overrides {
    ($x:expr) => {
        [
            $x.entry_message_override,
            $x.entry_sound_override,
            $x.exit_message_override,
            $x.exit_sound_override,
        ]
    };
}

fn one(field: &str, value: AnnouncementOverride) -> Value {
    let mut body = json!({});
    body[field] = json!(value);
    body
}

fn all(values: Overrides) -> Value {
    let mut body = json!({});
    for (field, value) in FIELDS.iter().zip(values) {
        body[*field] = json!(value);
    }
    body
}

async fn send(
    pool: &PgPool,
    method: &str,
    uri: &str,
    caller: &str,
    body: Option<Value>,
) -> Response {
    let builder = request_with_cookie(method, uri, caller);
    let req = match body {
        Some(json) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    build_app(pool.clone()).oneshot(req).await.unwrap()
}

async fn create(pool: &PgPool, room: &str, extra: Value) -> Response {
    cleanup_test_data(pool, room).await;
    let mut body = json!({ "meeting_id": room, "attendees": [] });
    for (k, v) in extra.as_object().expect("extra must be an object") {
        body[k] = v.clone();
    }
    send(pool, "POST", "/api/v1/meetings", OWNER, Some(body)).await
}

async fn create_ok(pool: &PgPool, room: &str, extra: Value) -> CreateMeetingResponse {
    let resp = create(pool, room, extra).await;
    assert_eq!(resp.status(), StatusCode::CREATED, "create {room}");
    response_json::<APIResponse<CreateMeetingResponse>>(resp)
        .await
        .result
}

async fn patch(pool: &PgPool, room: &str, caller: &str, body: Value) -> Response {
    let uri = format!("/api/v1/meetings/{room}");
    send(pool, "PATCH", &uri, caller, Some(body)).await
}

async fn patch_ok(pool: &PgPool, room: &str, body: Value) -> MeetingInfoResponse {
    let resp = patch(pool, room, OWNER, body.clone()).await;
    assert_eq!(resp.status(), StatusCode::OK, "PATCH {body}");
    response_json::<APIResponse<MeetingInfoResponse>>(resp)
        .await
        .result
}

async fn stored(pool: &PgPool, room: &str) -> Overrides {
    let row = db_meetings::get_by_room_id(pool, room)
        .await
        .unwrap()
        .expect("meeting row");
    overrides!(row)
}

async fn participant_status(
    pool: &PgPool,
    method: &str,
    uri: &str,
    caller: &str,
) -> ParticipantStatusResponse {
    let resp = send(pool, method, uri, caller, None).await;
    assert_eq!(resp.status(), StatusCode::OK, "{method} {uri} as {caller}");
    response_json::<APIResponse<ParticipantStatusResponse>>(resp)
        .await
        .result
}

async fn join(pool: &PgPool, room: &str, user: &str) -> ParticipantStatusResponse {
    let uri = format!("/api/v1/meetings/{room}/join");
    participant_status(pool, "POST", &uri, user).await
}

async fn my_status(pool: &PgPool, room: &str, user: &str) -> ParticipantStatusResponse {
    let uri = format!("/api/v1/meetings/{room}/status");
    participant_status(pool, "GET", &uri, user).await
}

async fn guest_request(pool: &PgPool, req: axum::http::Request<Body>) -> ParticipantStatusResponse {
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    response_json::<APIResponse<ParticipantStatusResponse>>(resp)
        .await
        .result
}

async fn join_guest(pool: &PgPool, room: &str) -> ParticipantStatusResponse {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(json!({ "display_name": "Guest" }).to_string()))
        .unwrap();
    guest_request(pool, req).await
}

async fn guest_status(pool: &PgPool, room: &str, bearer: &str) -> ParticipantStatusResponse {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/meetings/{room}/guest-status"))
        .header("Authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    guest_request(pool, req).await
}

#[tokio::test]
#[serial]
async fn a_meeting_created_without_overrides_has_none() {
    let pool = get_test_pool().await;
    let room = "announce-defaults";
    let created = create_ok(&pool, room, json!({})).await;
    assert_eq!(overrides!(created), [Participant; 4]);
    assert_eq!(stored(&pool, room).await, [Participant; 4]);

    join(&pool, room, OWNER).await;
    assert_eq!(join(&pool, room, ATTENDEE).await.status, "waiting");
    let uri = format!("/api/v1/meetings/{room}/status");
    let raw: Value = response_json(send(&pool, "GET", &uri, ATTENDEE, None).await).await;
    assert_eq!(raw["result"]["status"], "waiting", "{raw}");
    let status = raw["result"].as_object().expect("status object");
    let on_wire: Vec<&String> = status.keys().filter(|k| k.ends_with("_override")).collect();
    assert!(on_wire.is_empty(), "{on_wire:?}");
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn explicit_overrides_are_stored_and_returned_by_every_meeting_read() {
    let pool = get_test_pool().await;
    for (room, values) in [
        ("announce-create-a", [On, Off, Participant, On]),
        ("announce-create-b", [Off, On, On, Participant]),
    ] {
        let created = create_ok(&pool, room, all(values)).await;
        assert_eq!(overrides!(created), values, "{room} create response");
        assert_eq!(stored(&pool, room).await, values, "{room} stored");

        let resp = send(
            &pool,
            "GET",
            &format!("/api/v1/meetings/{room}"),
            OWNER,
            None,
        )
        .await;
        let info: APIResponse<MeetingInfoResponse> = response_json(resp).await;
        assert_eq!(overrides!(info.result), values, "{room} meeting info");

        for uri in [
            "/api/v1/meetings".to_string(),
            format!("/api/v1/meetings?q={room}"),
        ] {
            let resp = send(&pool, "GET", &uri, OWNER, None).await;
            let list: APIResponse<ListMeetingsResponse> = response_json(resp).await;
            let summary = list.result.meetings.iter().find(|m| m.meeting_id == room);
            let summary = summary.unwrap_or_else(|| panic!("{uri} must list {room}"));
            assert_eq!(overrides!(summary), values, "{room} in {uri}");
        }

        let resp = send(&pool, "GET", "/api/v1/meetings/feed", OWNER, None).await;
        let feed: APIResponse<ListFeedResponse> = response_json(resp).await;
        let row = feed.result.meetings.iter().find(|m| m.meeting_id == room);
        assert_eq!(overrides!(row.expect("in feed")), values, "{room} feed");

        for attempt in ["end", "repeated end"] {
            let uri = format!("/api/v1/meetings/{room}/end");
            let resp = send(&pool, "POST", &uri, OWNER, None).await;
            assert_eq!(resp.status(), StatusCode::OK, "{room} {attempt}");
            let ended: APIResponse<MeetingInfoResponse> = response_json(resp).await;
            assert_eq!(ended.result.state, "ended");
            assert_eq!(overrides!(ended.result), values, "{room} {attempt}");
        }
    }
    cleanup_test_data(&pool, "announce-create-a").await;
    cleanup_test_data(&pool, "announce-create-b").await;
}

#[tokio::test]
#[serial]
async fn patch_moves_one_override_through_on_off_and_back_to_participant() {
    let pool = get_test_pool().await;
    let room = "announce-patch-cycle";
    let initial = [On, Off, On, Off];
    create_ok(&pool, room, all(initial)).await;

    let mut expected = initial;
    for (i, field) in FIELDS.iter().enumerate() {
        for value in [On, Off, Participant, initial[i]] {
            let info = patch_ok(&pool, room, one(field, value)).await;
            expected[i] = value;
            assert_eq!(overrides!(info), expected, "{field}={value:?} response");
            assert_eq!(
                stored(&pool, room).await,
                expected,
                "{field}={value:?} stored"
            );
        }
    }
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn an_unknown_override_value_is_rejected_without_a_write() {
    let pool = get_test_pool().await;
    let room = "announce-unknown";
    let body = json!({ "entry_sound_override": "chime_only" });

    let resp = create(&pool, room, body.clone()).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .is_none());

    create_ok(&pool, room, all([On; 4])).await;
    let resp = patch(&pool, room, OWNER, body).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(stored(&pool, room).await, [On; 4]);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn every_join_and_status_response_carries_the_overrides() {
    let pool = get_test_pool().await;
    let values = [On, Off, Off, On];

    let a = "announce-paths-a";
    let mut extra = all(values);
    extra["allow_guests"] = json!(true);
    create_ok(&pool, a, extra).await;

    let early = join(&pool, a, ATTENDEE).await;
    assert_eq!(early.status, "waiting_for_meeting");
    assert_eq!(overrides!(early), values, "join before the host");

    let host = join(&pool, a, OWNER).await;
    assert!(host.is_host);
    assert_eq!(overrides!(host), values, "host join");

    let attendee = join(&pool, a, ATTENDEE2).await;
    assert_eq!(attendee.status, "waiting");
    assert_eq!(overrides!(attendee), values, "attendee join");
    assert_eq!(
        overrides!(my_status(&pool, a, ATTENDEE2).await),
        values,
        "attendee status"
    );

    let guest = join_guest(&pool, a).await;
    assert!(guest.is_guest);
    assert_eq!(overrides!(guest), values, "guest join");
    let bearer = guest
        .room_token
        .clone()
        .or_else(|| guest.observer_token.clone())
        .expect("a guest join returns a credential");
    assert_eq!(
        overrides!(guest_status(&pool, a, &bearer).await),
        values,
        "guest status"
    );

    patch_ok(&pool, a, one("exit_sound_override", Participant)).await;
    assert_eq!(
        overrides!(my_status(&pool, a, ATTENDEE2).await),
        [On, Off, Off, Participant],
        "attendee status after the host's PATCH"
    );
    assert_eq!(
        overrides!(guest_status(&pool, a, &bearer).await),
        [On, Off, Off, Participant],
        "guest status after the host's PATCH"
    );

    let b = "announce-paths-b";
    let mut extra = all(values);
    extra["waiting_room_enabled"] = json!(false);
    extra["co_hosts"] = json!([CO_HOST]);
    create_ok(&pool, b, extra).await;

    let activating = join(&pool, b, ATTENDEE).await;
    assert_eq!(activating.status, "admitted");
    assert!(!activating.is_host);
    assert_eq!(overrides!(activating), values, "join that activates");

    let co_host = join(&pool, b, CO_HOST).await;
    assert!(co_host.is_host);
    assert_eq!(overrides!(co_host), values, "co-host join");

    cleanup_test_data(&pool, a).await;
    cleanup_test_data(&pool, b).await;
}

#[tokio::test]
#[serial]
async fn a_non_host_participant_cannot_patch_an_override() {
    let pool = get_test_pool().await;
    let room = "announce-non-host";
    create_ok(&pool, room, json!({ "waiting_room_enabled": false })).await;
    let attendee = join(&pool, room, ATTENDEE).await;
    assert_eq!(attendee.status, "admitted");
    assert!(!attendee.is_host);

    for field in FIELDS {
        let resp = patch(&pool, room, ATTENDEE, one(field, Off)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{field}");
        let err: APIResponse<APIError> = response_json(resp).await;
        assert_eq!(err.result.code, "NOT_HOST", "{field}");
    }
    assert_eq!(stored(&pool, room).await, [Participant; 4]);

    patch_ok(&pool, room, one("entry_message_override", Off)).await;
    assert_eq!(
        stored(&pool, room).await,
        [Off, Participant, Participant, Participant]
    );
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn an_override_only_patch_tells_the_room_to_refetch() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping announcement override publish test");
        return;
    };
    let pool = get_test_pool().await;
    let room = "announce-publish";
    create_ok(&pool, room, json!({})).await;
    let mut system = nats
        .subscribe(format!("room.{room}.system"))
        .await
        .expect("subscribe");

    let req = request_with_cookie("PATCH", &format!("/api/v1/meetings/{room}"), OWNER)
        .header("Content-Type", "application/json")
        .body(Body::from(one("exit_sound_override", Off).to_string()))
        .unwrap();
    let resp = build_app_on_nats(pool.clone(), nats.clone())
        .oneshot(req)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let mut settings_updated = 0;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, system.next()).await {
        let wrapper = <PacketWrapper as protobuf::Message>::parse_from_bytes(&msg.payload)
            .expect("packet wrapper");
        let packet = <MeetingPacket as protobuf::Message>::parse_from_bytes(&wrapper.data)
            .expect("meeting packet");
        if packet.event_type == MeetingEventType::MEETING_SETTINGS_UPDATED.into() {
            settings_updated += 1;
        }
    }
    assert_eq!(settings_updated, 1);
    cleanup_test_data(&pool, room).await;
}
