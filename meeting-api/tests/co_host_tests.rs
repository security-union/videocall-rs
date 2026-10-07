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

//! Integration tests for co-hosts (issue #2702). Requires `DATABASE_URL`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use futures::StreamExt;
use jsonwebtoken::{decode, DecodingKey, Validation};
use meeting_api::db::{meetings as db_meetings, participants as db_participants};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{
        APIResponse, CreateMeetingResponse, ListCoHostsResponse, MeetingInfoResponse,
        ParticipantStatusResponse,
    },
    APIError, RoomAccessTokenClaims,
};
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;

const OWNER: &str = "owner@example.com";
const CO: &str = "cohost@example.com";
const CO2: &str = "cohost2@example.com";
const ATTENDEE: &str = "attendee@example.com";

async fn send(
    pool: &sqlx::PgPool,
    method: &str,
    uri: &str,
    caller: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let app = build_app(pool.clone());
    let builder = request_with_cookie(method, uri, caller);
    let req = match body {
        Some(json) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    app.oneshot(req).await.unwrap()
}

async fn create_meeting(
    pool: &sqlx::PgPool,
    room_id: &str,
    extra: serde_json::Value,
) -> axum::response::Response {
    let mut body = serde_json::json!({ "meeting_id": room_id, "attendees": [] });
    for (k, v) in extra.as_object().expect("extra must be an object") {
        body[k] = v.clone();
    }
    send(pool, "POST", "/api/v1/meetings", OWNER, Some(body)).await
}

async fn join(pool: &sqlx::PgPool, room_id: &str, user: &str) -> axum::response::Response {
    send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        user,
        None,
    )
    .await
}

async fn join_ok(pool: &sqlx::PgPool, room_id: &str, user: &str) -> ParticipantStatusResponse {
    let resp = join(pool, room_id, user).await;
    assert_eq!(resp.status(), StatusCode::OK, "join as {user} must succeed");
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    body.result
}

async fn leave(pool: &sqlx::PgPool, room_id: &str, user: &str) {
    let resp = send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        user,
        None,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "leave as {user} must succeed"
    );
}

async fn admit(pool: &sqlx::PgPool, room_id: &str, user: &str) {
    let resp = send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        OWNER,
        Some(serde_json::json!({ "user_id": user })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "admit {user} must succeed");
}

async fn grant(
    pool: &sqlx::PgPool,
    room_id: &str,
    caller: &str,
    target: &str,
    persist: bool,
) -> axum::response::Response {
    send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/co-hosts"),
        caller,
        Some(serde_json::json!({ "user_id": target, "persist": persist })),
    )
    .await
}

async fn revoke(
    pool: &sqlx::PgPool,
    room_id: &str,
    caller: &str,
    target: &str,
) -> axum::response::Response {
    send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/co-hosts/revoke"),
        caller,
        Some(serde_json::json!({ "user_id": target })),
    )
    .await
}

async fn host_action(
    pool: &sqlx::PgPool,
    room_id: &str,
    action: &str,
    caller: &str,
    target: &str,
) -> axum::response::Response {
    send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/{action}"),
        caller,
        Some(serde_json::json!({ "user_id": target })),
    )
    .await
}

async fn meeting_pk(pool: &sqlx::PgPool, room_id: &str) -> i32 {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists")
        .id
}

async fn row(pool: &sqlx::PgPool, room_id: &str, user: &str) -> db_participants::ParticipantRow {
    db_participants::get_status(pool, meeting_pk(pool, room_id).await, user)
        .await
        .expect("get_status")
        .expect("participant row exists")
}

async fn state(pool: &sqlx::PgPool, room_id: &str) -> Option<String> {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists")
        .state
}

async fn entries(pool: &sqlx::PgPool, room_id: &str) -> Vec<(String, bool)> {
    let mut rows: Vec<(String, bool)> =
        sqlx::query_as("SELECT user_id, persistent FROM meeting_co_hosts WHERE meeting_id = $1")
            .bind(meeting_pk(pool, room_id).await)
            .fetch_all(pool)
            .await
            .expect("select co-host entries");
    rows.sort();
    rows
}

fn token_is_host(token: &str) -> bool {
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    decode::<RoomAccessTokenClaims>(
        token,
        &DecodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
        &validation,
    )
    .expect("room token must decode")
    .claims
    .is_host
}

async fn error_code(resp: axum::response::Response) -> String {
    let body: APIResponse<APIError> = response_json(resp).await;
    body.result.code
}

async fn suspended(pool: &sqlx::PgPool, room_id: &str, user: &str) -> bool {
    sqlx::query_scalar(
        "SELECT suspended FROM meeting_co_hosts WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_pk(pool, room_id).await)
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("co-host entry exists")
}

/// A relay reports `user`'s transport session present (`true`) or left.
async fn transport(pool: &sqlx::PgPool, room_id: &str, user: &str, session_id: u64, present: bool) {
    let meeting = db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists");
    let (feed_tx, _feed_rx) = meeting_api::feed_events::new_feed_channel();
    meeting_api::nats_consumers::apply_participant_presence(
        pool,
        None,
        &feed_tx,
        &meeting,
        &meeting_api::nats_events::ParticipantPresencePayload {
            room_id: room_id.to_string(),
            user_id: user.to_string(),
            session_id,
            present,
        },
    )
    .await;
}

#[tokio::test]
#[serial]
async fn owner_grants_present_participant_the_host_role() {
    let pool = get_test_pool().await;
    let room = "test-cohost-grant-present";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    let resp = grant(&pool, room, OWNER, ATTENDEE, false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ListCoHostsResponse> = response_json(resp).await;
    assert_eq!(body.result.co_hosts.len(), 1);
    let entry = &body.result.co_hosts[0];
    assert_eq!(entry.user_id, ATTENDEE);
    assert!(!entry.persistent);
    assert!(entry.is_present_host);

    assert!(row(&pool, room, ATTENDEE).await.is_host);
    let status = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}/status"),
        ATTENDEE,
        None,
    )
    .await;
    let status: APIResponse<ParticipantStatusResponse> = response_json(status).await;
    assert!(token_is_host(
        &status
            .result
            .room_token
            .expect("admitted co-host gets a token")
    ));

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn waiting_co_host_is_promoted_when_their_transport_connects_not_when_admitted() {
    let pool = get_test_pool().await;
    let room = "test-cohost-grant-waiting";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    assert_eq!(join_ok(&pool, room, ATTENDEE).await.status, "waiting");
    assert_eq!(join_ok(&pool, room, CO2).await.status, "waiting");

    for target in [ATTENDEE, CO2] {
        let resp = grant(&pool, room, OWNER, target, false).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let r = row(&pool, room, target).await;
        assert_eq!(r.status, "waiting", "grant must not admit {target}");
        assert!(!r.is_host, "grant must not promote waiting {target}");
    }

    admit(&pool, room, ATTENDEE).await;
    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/admit-all"),
        OWNER,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    for target in [ATTENDEE, CO2] {
        let r = row(&pool, room, target).await;
        assert_eq!(r.status, "admitted");
        assert!(!r.is_host, "admitting must not promote {target}");
    }

    transport(&pool, room, ATTENDEE, 11, true).await;
    transport(&pool, room, CO2, 12, true).await;
    assert!(row(&pool, room, ATTENDEE).await.is_host);
    assert!(row(&pool, room, CO2).await.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn admitting_a_co_host_who_never_connects_does_not_block_end_on_host_leave() {
    let pool = get_test_pool().await;
    let room = "test-cohost-admitted-never-connects";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    assert_eq!(join_ok(&pool, room, CO).await.status, "waiting");
    assert_eq!(
        grant(&pool, room, OWNER, CO, false).await.status(),
        StatusCode::OK
    );
    admit(&pool, room, CO).await;

    leave(&pool, room, OWNER).await;
    assert_eq!(
        state(&pool, room).await.as_deref(),
        Some("ended"),
        "a co-host admitted from the waiting room is no host until they connect"
    );

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn non_owners_cannot_manage_co_hosts() {
    let pool = get_test_pool().await;
    let room = "test-cohost-non-owner-403";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    // Granting and revoking stay owner-only for EVERYONE else, including a
    // live co-host.
    for caller in [CO, ATTENDEE] {
        let resp = grant(&pool, room, caller, ATTENDEE, true).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "grant by {caller}");
        assert_eq!(error_code(resp).await, "NOT_OWNER");
        let resp = revoke(&pool, room, caller, CO).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "revoke by {caller}");
    }
    // Listing is broader (issue #2702 round 11 addendum): a live co-host may
    // read the list (the settings page shows it), a plain participant may
    // not.
    let resp = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        CO,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "list by a live co-host");
    let list: APIResponse<ListCoHostsResponse> = response_json(resp).await;
    assert!(
        !list.result.co_hosts.is_empty(),
        "the co-host's own read must show entries, not an empty list"
    );
    let resp = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        ATTENDEE,
        None,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "list by a plain participant"
    );
    assert_eq!(error_code(resp).await, "NOT_HOST");

    assert!(!row(&pool, room, ATTENDEE).await.is_host);
    assert!(row(&pool, room, CO).await.is_host);
    assert_eq!(entries(&pool, room).await, vec![(CO.to_string(), true)]);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn owner_revokes_co_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-revoke";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    assert!(row(&pool, room, CO).await.is_host);

    let resp = revoke(&pool, room, OWNER, CO).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ListCoHostsResponse> = response_json(resp).await;
    assert!(body.result.co_hosts.is_empty());
    assert!(!row(&pool, room, CO).await.is_host);
    assert!(entries(&pool, room).await.is_empty());

    for target in [CO, ATTENDEE, "COHOST@example.com"] {
        let resp = revoke(&pool, room, OWNER, target).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "revoke {target}");
        assert_eq!(error_code(resp).await, "CO_HOST_NOT_FOUND");
    }

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn owner_can_demote_a_transfer_target_while_another_host_is_present() {
    let pool = get_test_pool().await;
    let room = "test-cohost-demote-transfer-target";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    let resp = host_action(&pool, room, "transfer-host", OWNER, ATTENDEE).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(row(&pool, room, ATTENDEE).await.is_host);

    let resp = revoke(&pool, room, OWNER, ATTENDEE).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!row(&pool, room, ATTENDEE).await.is_host);
    assert!(row(&pool, room, CO).await.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn revoking_the_last_present_host_is_refused() {
    let pool = get_test_pool().await;
    let room = "test-cohost-revoke-last-host";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "end_on_host_leave": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    let resp = revoke(&pool, room, OWNER, CO).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_eq!(error_code(resp).await, "LAST_PRESENT_HOST");
    assert!(row(&pool, room, CO).await.is_host);
    assert_eq!(entries(&pool, room).await, vec![(CO.to_string(), true)]);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn invalid_co_host_targets_are_rejected() {
    let pool = get_test_pool().await;
    let room = "test-cohost-invalid-targets";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;

    let too_long = "a".repeat(255);
    for target in ["guest:1234", OWNER, too_long.as_str(), "", "   "] {
        let resp = grant(&pool, room, OWNER, target, true).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "grant {target:?}");
    }
    let resp = revoke(&pool, room, OWNER, OWNER).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(entries(&pool, room).await.is_empty());
    assert!(row(&pool, room, OWNER).await.is_host);

    cleanup_test_data(&pool, room).await;
}

/// Issue #2702 round 10: the menu-style grant omits `persist` and now creates
/// a NEW entry as persistent (was instance-only pre-round-10). Regranting an
/// EXISTING entry without `persist` is unchanged — it keeps that entry's own
/// flag and lifts any suspension.
#[tokio::test]
#[serial]
async fn regrant_without_persist_keeps_a_saved_co_host_saved_and_lifts_the_suspension() {
    let pool = get_test_pool().await;
    let room = "test-cohost-regrant-keeps-persist";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    assert_eq!(
        host_action(&pool, room, "kick", OWNER, CO).await.status(),
        StatusCode::OK
    );
    assert!(suspended(&pool, room, CO).await);

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        OWNER,
        Some(serde_json::json!({ "user_id": CO })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(entries(&pool, room).await, vec![(CO.to_string(), true)]);
    assert!(!suspended(&pool, room, CO).await);

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        OWNER,
        Some(serde_json::json!({ "user_id": ATTENDEE, "persist": null })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        entries(&pool, room).await,
        vec![(ATTENDEE.to_string(), true), (CO.to_string(), true)],
        "a new entry granted without persist is persistent (issue #2702 round 10)"
    );

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn instance_only_grant_requires_an_active_meeting() {
    let pool = get_test_pool().await;
    let room = "test-cohost-instance-only-idle";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;

    let resp = grant(&pool, room, OWNER, CO, false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(entries(&pool, room).await.is_empty());

    let resp = grant(&pool, room, OWNER, CO, true).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(entries(&pool, room).await, vec![(CO.to_string(), true)]);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn persistent_co_host_survives_end_and_rejoins_as_host_past_the_waiting_room() {
    let pool = get_test_pool().await;
    let room = "test-cohost-persist-survives-end";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    assert_eq!(
        grant(&pool, room, OWNER, ATTENDEE, true).await.status(),
        StatusCode::OK
    );

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/end"),
        OWNER,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!row(&pool, room, ATTENDEE).await.is_host);
    assert_eq!(
        entries(&pool, room).await,
        vec![(ATTENDEE.to_string(), true)]
    );

    join_ok(&pool, room, OWNER).await;
    let rejoin = join_ok(&pool, room, ATTENDEE).await;
    assert_eq!(rejoin.status, "admitted");
    assert!(rejoin.is_host);
    assert!(token_is_host(
        &rejoin.room_token.expect("co-host is admitted with a token")
    ));
    assert!(row(&pool, room, ATTENDEE).await.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn instance_only_co_host_does_not_survive_end() {
    let pool = get_test_pool().await;
    let room = "test-cohost-instance-only-end";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    assert_eq!(
        grant(&pool, room, OWNER, ATTENDEE, false).await.status(),
        StatusCode::OK
    );

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/end"),
        OWNER,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(entries(&pool, room).await.is_empty());

    join_ok(&pool, room, OWNER).await;
    let rejoin = join_ok(&pool, room, ATTENDEE).await;
    assert_eq!(rejoin.status, "waiting");
    assert!(!rejoin.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn instance_only_co_host_is_cleared_when_the_owner_restarts_an_idle_meeting() {
    let pool = get_test_pool().await;
    let room = "test-cohost-instance-only-idle-restart";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO2], "end_on_host_leave": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    assert_eq!(
        grant(&pool, room, OWNER, ATTENDEE, false).await.status(),
        StatusCode::OK
    );
    sqlx::query("UPDATE meeting_co_hosts SET suspended = TRUE WHERE meeting_id = $1")
        .bind(meeting_pk(&pool, room).await)
        .execute(&pool)
        .await
        .expect("suspend every entry");
    leave(&pool, room, ATTENDEE).await;
    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("idle"));

    join_ok(&pool, room, OWNER).await;
    assert_eq!(entries(&pool, room).await, vec![(CO2.to_string(), true)]);
    assert!(
        !suspended(&pool, room, CO2).await,
        "a new instance lifts suspensions"
    );
    assert!(!row(&pool, room, ATTENDEE).await.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn kicked_co_host_cannot_force_an_idle_restart_to_regain_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-kick-then-spurious-idle";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "waiting_room_enabled": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    assert_eq!(
        host_action(&pool, room, "kick", OWNER, CO).await.status(),
        StatusCode::OK
    );

    // The kicked co-host connects to the relay binary nobody else uses and
    // disconnects: that relay reports its copy of the room empty.
    db_meetings::set_idle(&pool, meeting_pk(&pool, room).await, true)
        .await
        .expect("set_idle");
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    let rejoin = join_ok(&pool, room, CO).await;
    assert!(!rejoin.is_host);
    assert!(!row(&pool, room, CO).await.is_host);
    assert!(suspended(&pool, room, CO).await);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn present_co_hosts_survive_a_spurious_idle_with_suspensions_intact() {
    let pool = get_test_pool().await;
    let room = "test-cohost-spurious-idle-resume";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO, CO2], "waiting_room_enabled": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    assert!(join_ok(&pool, room, CO2).await.is_host);
    sqlx::query(
        "UPDATE meeting_co_hosts SET suspended = TRUE WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_pk(&pool, room).await)
    .bind(CO2)
    .execute(&pool)
    .await
    .expect("suspend CO2");
    let pk = meeting_pk(&pool, room).await;
    sqlx::query("UPDATE meetings SET state = 'idle' WHERE id = $1")
        .bind(pk)
        .execute(&pool)
        .await
        .expect("a spurious idle");
    let started_at = db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .unwrap()
        .started_at;

    join_ok(&pool, room, ATTENDEE).await;

    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));
    assert!(
        row(&pool, room, CO).await.is_host,
        "a present co-host keeps host"
    );
    assert!(suspended(&pool, room, CO2).await, "the suspension is kept");
    let resumed = db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.started_at, started_at, "still the same instance");

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_new_instance_demotes_and_announces_non_owner_hosts() {
    let pool = get_test_pool().await;
    let room = "test-cohost-new-instance-demotes";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "end_on_host_leave": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    leave(&pool, room, CO).await;
    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("idle"));

    let nats = maybe_nats().await;
    let mut host_changes = match &nats {
        Some(nats) => Some(
            nats.subscribe(meeting_api::nats_events::MEETING_HOST_CHANGE_SUBJECT)
                .await
                .expect("subscribe"),
        ),
        None => None,
    };
    let app = match nats.clone() {
        Some(nats) => build_app_on_nats(pool.clone(), nats),
        None => build_app(pool.clone()),
    };
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room}/join"), OWNER)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
    assert!(!row(&pool, room, CO).await.is_host);

    if let Some(sub) = host_changes.as_mut() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("the demotion must be announced within 5s")
            .expect("subscription open");
        let change: meeting_api::nats_events::MeetingHostChangePayload =
            serde_json::from_slice(&msg.payload).expect("payload");
        assert_eq!(
            (
                change.room_id.as_str(),
                change.user_id.as_str(),
                change.is_host
            ),
            (room, CO, false)
        );
    }

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn pre_designated_co_host_joins_an_active_waiting_room_meeting_as_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-predesignated-join";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;

    let joined = join_ok(&pool, room, CO).await;
    assert_eq!(joined.status, "admitted");
    assert!(joined.is_host);
    assert!(token_is_host(
        &joined.room_token.expect("co-host is admitted with a token")
    ));
    assert_eq!(join_ok(&pool, room, ATTENDEE).await.status, "waiting");

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn co_host_is_not_exempt_from_the_meeting_password() {
    let pool = get_test_pool().await;
    let room = "test-cohost-password";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "password": "s3cret-pw" }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;

    let resp = join(&pool, room, CO).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(resp).await, "MEETING_PASSWORD_REQUIRED");
    let pk = meeting_pk(&pool, room).await;
    assert!(db_participants::get_status(&pool, pk, CO)
        .await
        .expect("get_status")
        .is_none());

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/join"),
        CO,
        Some(serde_json::json!({ "password": "s3cret-pw" })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.result.is_host);

    cleanup_test_data(&pool, room).await;
}

/// Issue #2702 round 10: a co-host may start an idle waiting-room meeting
/// themselves — skipping the waiting room, exactly like the owner — rather
/// than being stuck behind it until the owner arrives.
#[tokio::test]
#[serial]
async fn saved_co_host_starts_an_idle_waiting_room_meeting_as_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-idle-wr";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;

    let joined = join_ok(&pool, room, CO).await;
    assert_eq!(joined.status, "admitted");
    assert!(joined.is_host);
    assert!(token_is_host(
        &joined.room_token.expect("co-host is admitted with a token")
    ));
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    cleanup_test_data(&pool, room).await;
}

/// Issue #2702 round 10: a saved (persistent) co-host may restart an ENDED
/// meeting — skipping the waiting room, exactly like the owner. An
/// instance-only co-host's entry is cleared on end (`reset_for_new_instance`
/// inside `end_meeting_in`), so once cleared they are a normal attendee:
/// queued behind the still-enabled waiting room, not host.
#[tokio::test]
#[serial]
async fn saved_co_host_restarts_an_ended_meeting_as_host_instance_only_one_does_not() {
    let pool = get_test_pool().await;
    let room = "test-cohost-restart-ended";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    assert_eq!(
        grant(&pool, room, OWNER, CO, true).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        grant(&pool, room, OWNER, CO2, false).await.status(),
        StatusCode::OK
    );

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/end"),
        OWNER,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(state(&pool, room).await.as_deref(), Some("ended"));
    assert!(
        entries(&pool, room).await.iter().all(|(u, _)| u != CO2),
        "the instance-only entry must be cleared on end"
    );

    let restarted = join_ok(&pool, room, CO).await;
    assert_eq!(restarted.status, "admitted");
    assert!(restarted.is_host);
    assert!(token_is_host(
        &restarted
            .room_token
            .expect("co-host is admitted with a token")
    ));
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    let plain = join_ok(&pool, room, CO2).await;
    assert_eq!(
        plain.status, "waiting",
        "an instance-only co-host cleared by the end is a normal attendee"
    );
    assert!(!plain.is_host);

    cleanup_test_data(&pool, room).await;
}

/// Issue #2702 round 10: the "ended" rejection this restart bypasses also
/// covers a meeting ended by `end_on_host_leave` — a co-host must be able to
/// restart that too, not just an explicitly-`/end`ed one.
#[tokio::test]
#[serial]
async fn saved_co_host_restarts_a_meeting_ended_by_host_leave_as_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-restart-eohl";
    cleanup_test_data(&pool, room).await;
    // end_on_host_leave and waiting_room_enabled both default true.
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    leave(&pool, room, OWNER).await;
    assert_eq!(
        state(&pool, room).await.as_deref(),
        Some("ended"),
        "the last present host leaving with end_on_host_leave must end it"
    );

    let restarted = join_ok(&pool, room, CO).await;
    assert_eq!(restarted.status, "admitted");
    assert!(restarted.is_host);
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    cleanup_test_data(&pool, room).await;
}

/// A kicked (suspended) saved co-host must not be able to restart the
/// meeting after it ends: suspension must survive End, lifting only when a
/// new instance actually starts.
#[tokio::test]
#[serial]
async fn kicked_co_host_cannot_restart_after_end() {
    let pool = get_test_pool().await;
    let room = "test-cohost-kicked-cannot-restart-after-end";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    assert_eq!(
        host_action(&pool, room, "kick", OWNER, CO).await.status(),
        StatusCode::OK
    );
    assert!(suspended(&pool, room, CO).await);

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/end"),
        OWNER,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        suspended(&pool, room, CO).await,
        "the suspension must survive End"
    );

    let rejoin = join(&pool, room, CO).await;
    assert_eq!(
        rejoin.status(),
        StatusCode::BAD_REQUEST,
        "a kicked co-host must not be able to restart the ended meeting"
    );
    assert_eq!(state(&pool, room).await.as_deref(), Some("ended"));

    cleanup_test_data(&pool, room).await;
}

/// An instance-only co-host joining an IDLE meeting must not start it: their
/// own entry would be deleted the instant the new instance starts, stranding
/// them in the waiting room of an active meeting with no host. They follow
/// the plain-participant path instead.
#[tokio::test]
#[serial]
async fn instance_only_co_host_does_not_wake_an_idle_meeting() {
    let pool = get_test_pool().await;
    let room = "test-cohost-instance-only-no-wake-idle";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    assert_eq!(
        grant(&pool, room, OWNER, CO, false).await.status(),
        StatusCode::OK
    );
    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("idle"));

    let joined = join_ok(&pool, room, CO).await;
    assert_eq!(
        joined.status, "waiting_for_meeting",
        "an instance-only co-host must not auto-activate an idle meeting"
    );
    assert!(!joined.is_host);
    assert_eq!(
        state(&pool, room).await.as_deref(),
        Some("idle"),
        "the meeting must stay idle"
    );

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn co_host_auto_activating_a_meeting_without_waiting_room_is_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-auto-activate";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "waiting_room_enabled": false }),
    )
    .await;

    let joined = join_ok(&pool, room, CO).await;
    assert_eq!(joined.status, "admitted");
    assert!(joined.is_host);
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn co_host_join_is_not_blocked_by_the_host_presence_guard() {
    let pool = get_test_pool().await;
    let room = "test-cohost-join-no-host-present";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "end_on_host_leave": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    let joined = join_ok(&pool, room, CO).await;
    assert_eq!(joined.status, "admitted");
    assert!(joined.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn attendee_join_is_allowed_while_a_co_host_is_present_without_the_owner() {
    let pool = get_test_pool().await;
    let room = "test-cohost-owner-absent-attendee-join";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "end_on_host_leave": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    leave(&pool, room, OWNER).await;

    assert_eq!(
        join_ok(&pool, room, "late@example.com").await.status,
        "waiting"
    );

    leave(&pool, room, CO).await;
    let resp = join(&pool, room, CO2).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(resp).await, "JOINING_NOT_ALLOWED");

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn co_host_leaving_while_owner_present_keeps_meeting_and_last_host_ends_it() {
    let pool = get_test_pool().await;
    let room = "test-cohost-leave-owner-present";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    leave(&pool, room, CO).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("ended"));

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn owner_leaving_while_co_host_present_keeps_meeting_and_last_host_ends_it() {
    let pool = get_test_pool().await;
    let room = "test-cohost-leave-cohost-present";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));

    leave(&pool, room, CO).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("ended"));

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn co_host_cannot_kick_the_owner_or_another_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-kick-rules";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO, CO2] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, CO2).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    for target in [OWNER, CO2] {
        let resp = host_action(&pool, room, "kick", CO, target).await;
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "co-host kicks {target}"
        );
        assert_eq!(error_code(resp).await, "NOT_OWNER");
        assert_eq!(row(&pool, room, target).await.status, "admitted");
    }

    let resp = host_action(&pool, room, "kick", CO, ATTENDEE).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(row(&pool, room, ATTENDEE).await.status, "kicked");

    let resp = host_action(&pool, room, "kick", OWNER, CO2).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the owner may kick a co-host"
    );
    assert_eq!(row(&pool, room, CO2).await.status, "kicked");

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn kicked_co_host_loses_host_and_cannot_rejoin_as_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-kick-strips-host";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);

    let resp = host_action(&pool, room, "kick", OWNER, CO).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let kicked = row(&pool, room, CO).await;
    assert_eq!(kicked.status, "kicked");
    assert!(!kicked.is_host, "a kicked host must lose the host role");

    let rejoin = join_ok(&pool, room, CO).await;
    assert_eq!(
        rejoin.status, "waiting",
        "a kicked co-host rejoins via the waiting room"
    );
    assert!(!rejoin.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn kick_of_a_user_with_no_row_is_404() {
    let pool = get_test_pool().await;
    let room = "test-cohost-kick-404";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;

    for target in [
        " owner@example.com",
        "Owner@example.com",
        "nobody@example.com",
    ] {
        let resp = host_action(&pool, room, "kick", CO, target).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "kick {target:?}");
        assert_eq!(error_code(resp).await, "PARTICIPANT_NOT_FOUND");
    }
    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn co_host_cannot_kick_a_designated_co_host_who_is_not_host_yet() {
    let pool = get_test_pool().await;
    let room = "test-cohost-kick-designated";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    assert_eq!(join_ok(&pool, room, CO2).await.status, "waiting");
    assert_eq!(
        grant(&pool, room, OWNER, CO2, false).await.status(),
        StatusCode::OK
    );
    admit(&pool, room, CO2).await;
    assert!(!row(&pool, room, CO2).await.is_host);

    let resp = host_action(&pool, room, "kick", CO, CO2).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(resp).await, "NOT_OWNER");
    assert_eq!(row(&pool, room, CO2).await.status, "admitted");
    assert!(!suspended(&pool, room, CO2).await);

    let resp = host_action(&pool, room, "kick", OWNER, CO2).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(suspended(&pool, room, CO2).await);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn participant_kicked_is_published_only_when_someone_was_kicked() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping kick publish test");
        return;
    };
    let pool = get_test_pool().await;
    let room = "test-cohost-kick-publish";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    assert_eq!(join_ok(&pool, room, ATTENDEE).await.status, "waiting");
    let mut system = nats
        .subscribe(format!("room.{room}.system"))
        .await
        .expect("subscribe");

    let kick = |target: &'static str| {
        let app = build_app_on_nats(pool.clone(), nats.clone());
        let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room}/kick"), OWNER)
            .header("Content-Type", "application/json")
            .body(Body::from(
                serde_json::json!({ "user_id": target }).to_string(),
            ))
            .unwrap();
        app.oneshot(req)
    };
    assert_eq!(kick(ATTENDEE).await.unwrap().status(), StatusCode::OK);
    admit(&pool, room, ATTENDEE).await;
    assert_eq!(kick(ATTENDEE).await.unwrap().status(), StatusCode::OK);

    let mut kicked = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, system.next()).await {
        let wrapper = <PacketWrapper as protobuf::Message>::parse_from_bytes(&msg.payload)
            .expect("packet wrapper");
        let packet = <MeetingPacket as protobuf::Message>::parse_from_bytes(&wrapper.data)
            .expect("meeting packet");
        if packet.event_type == MeetingEventType::PARTICIPANT_KICKED.into() {
            kicked.push(String::from_utf8(packet.target_user_id).expect("utf-8"));
        }
    }
    assert_eq!(
        kicked,
        vec![ATTENDEE.to_string()],
        "only the kick of the admitted participant is published"
    );

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn create_meeting_stores_co_hosts_as_persistent_entries() {
    let pool = get_test_pool().await;
    let room = "test-cohost-create";
    cleanup_test_data(&pool, room).await;

    let resp = create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO, format!(" {CO} "), CO2] }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: APIResponse<CreateMeetingResponse> = response_json(resp).await;
    assert_eq!(body.result.co_hosts, vec![CO.to_string(), CO2.to_string()]);
    let mut expected = vec![(CO.to_string(), true), (CO2.to_string(), true)];
    expected.sort();
    assert_eq!(entries(&pool, room).await, expected);

    let list = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        OWNER,
        None,
    )
    .await;
    assert_eq!(list.status(), StatusCode::OK);
    let list: APIResponse<ListCoHostsResponse> = response_json(list).await;
    assert_eq!(list.result.co_hosts.len(), 2);
    assert!(list
        .result
        .co_hosts
        .iter()
        .all(|e| e.persistent && !e.is_present_host));

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn create_meeting_rejects_invalid_co_hosts_without_creating_it() {
    let pool = get_test_pool().await;
    let room = "test-cohost-create-invalid";
    let too_many: Vec<String> = (0..101).map(|i| format!("u{i}@example.com")).collect();
    for co_hosts in [
        serde_json::json!([OWNER]),
        serde_json::json!(["guest:1234"]),
        serde_json::json!(too_many),
    ] {
        cleanup_test_data(&pool, room).await;
        let resp = create_meeting(&pool, room, serde_json::json!({ "co_hosts": co_hosts })).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(db_meetings::get_by_room_id(&pool, room)
            .await
            .expect("get_by_room_id")
            .is_none());
    }
}

#[tokio::test]
#[serial]
async fn co_host_who_transfers_host_cannot_rejoin_as_host_until_the_next_instance() {
    let pool = get_test_pool().await;
    let room = "test-cohost-transfer-suspends";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    let resp = host_action(&pool, room, "transfer-host", CO, ATTENDEE).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!row(&pool, room, CO).await.is_host);

    let rejoin = join_ok(&pool, room, CO).await;
    assert!(
        !rejoin.is_host,
        "re-joining must not mint a new host after handing the role away"
    );
    assert!(!row(&pool, room, CO).await.is_host);

    let list = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        OWNER,
        None,
    )
    .await;
    let list: APIResponse<ListCoHostsResponse> = response_json(list).await;
    let entry = list
        .result
        .co_hosts
        .iter()
        .find(|e| e.user_id == CO)
        .expect("CO entry listed");
    assert!(entry.suspended && entry.designated);

    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/end"),
        OWNER,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    join_ok(&pool, room, OWNER).await;
    let next = join_ok(&pool, room, CO).await;
    assert!(next.is_host, "the suspension lifts at the next instance");

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn list_includes_present_hosts_without_an_entry() {
    let pool = get_test_pool().await;
    let room = "test-cohost-list-undesignated";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    let resp = host_action(&pool, room, "transfer-host", OWNER, ATTENDEE).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let list = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        OWNER,
        None,
    )
    .await;
    let list: APIResponse<ListCoHostsResponse> = response_json(list).await;
    let summary: Vec<(String, bool, bool)> = list
        .result
        .co_hosts
        .iter()
        .map(|e| (e.user_id.clone(), e.designated, e.is_present_host))
        .collect();
    assert_eq!(
        summary,
        vec![
            (CO.to_string(), true, false),
            (ATTENDEE.to_string(), false, true)
        ]
    );

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn repeat_join_by_a_present_co_host_reports_no_host_change() {
    let pool = get_test_pool().await;
    let room = "test-cohost-repeat-join";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    let pk = meeting_pk(&pool, room).await;

    let (_, promoted) = db_participants::admit_as_co_host(&pool, pk, CO, None)
        .await
        .expect("admit_as_co_host")
        .expect("co-host is admitted");
    assert!(promoted, "the first join is a real false->true change");

    let (again, promoted) = db_participants::admit_as_co_host(&pool, pk, CO, None)
        .await
        .expect("admit_as_co_host")
        .expect("a present host re-joining keeps the fast path");
    assert!(again.is_host && again.status == "admitted");
    assert!(
        !promoted,
        "a repeat join must report no change, so no HOST_GRANTED is published"
    );

    let reload = join_ok(&pool, room, CO).await;
    assert_eq!(reload.status, "admitted");
    assert!(reload.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn owner_restart_of_a_meeting_a_co_host_already_started_keeps_that_host() {
    let pool = get_test_pool().await;
    let room = "test-cohost-owner-restart-race";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "waiting_room_enabled": false }),
    )
    .await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    let pk = meeting_pk(&pool, room).await;

    let (_, activation) = db_meetings::owner_join(&pool, pk, OWNER, None, true)
        .await
        .expect("owner_join");
    assert_eq!(
        activation,
        db_meetings::Activation::Unchanged,
        "the owner must not start a new instance over one that is already active"
    );
    assert!(
        row(&pool, room, CO).await.is_host,
        "the co-host who started the instance must not be silently demoted"
    );
    assert!(
        row(&pool, room, OWNER).await.is_host,
        "issue #2702 round 10: the owner's first join of an instance a \
         co-host started must make them host too"
    );

    cleanup_test_data(&pool, room).await;
}

/// Issue #2702 round 10: contrasts the owner's first join of an instance a
/// co-host started (must become host, alongside the co-host) with the owner
/// rejoining after transferring host away IN THAT SAME instance (must not
/// reclaim it — today's `admit_creator_preserve_host` rule, told apart from
/// "first join" by comparing `admitted_at` against the instance's
/// `started_at`; see `db_meetings::owner_join`).
#[tokio::test]
#[serial]
async fn owner_becomes_host_of_a_co_host_started_instance_but_not_after_transferring_it_away() {
    let pool = get_test_pool().await;
    let room = "test-owner-first-join-vs-transfer";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "waiting_room_enabled": false }),
    )
    .await;

    // CO starts the instance; the owner was never part of it yet.
    assert!(join_ok(&pool, room, CO).await.is_host);

    let owner_first_join = join_ok(&pool, room, OWNER).await;
    assert!(
        owner_first_join.is_host,
        "the owner's first join of this instance must make them host"
    );
    assert!(
        row(&pool, room, CO).await.is_host,
        "the co-host who started it keeps host"
    );

    // waiting_room_enabled is false, so the attendee is auto-admitted on join.
    join_ok(&pool, room, ATTENDEE).await;
    assert_eq!(
        host_action(&pool, room, "transfer-host", OWNER, ATTENDEE)
            .await
            .status(),
        StatusCode::OK
    );
    assert!(!row(&pool, room, OWNER).await.is_host);

    let rejoin = join_ok(&pool, room, OWNER).await;
    assert!(
        !rejoin.is_host,
        "the owner must not reclaim host after transferring it away in this instance"
    );
    assert!(!row(&pool, room, OWNER).await.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn instance_only_co_host_is_cleared_when_an_attendee_restarts_the_meeting() {
    let pool = get_test_pool().await;
    let room = "test-cohost-instance-only-auto-restart";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false, "end_on_host_leave": false }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    assert_eq!(
        grant(&pool, room, OWNER, CO, false).await.status(),
        StatusCode::OK
    );
    assert!(row(&pool, room, CO).await.is_host);
    leave(&pool, room, CO).await;
    leave(&pool, room, OWNER).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("idle"));

    join_ok(&pool, room, ATTENDEE).await;
    assert_eq!(state(&pool, room).await.as_deref(), Some("active"));
    assert!(entries(&pool, room).await.is_empty());
    assert!(!row(&pool, room, CO).await.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn transport_departure_ends_the_meeting_only_when_the_last_host_leaves() {
    let pool = get_test_pool().await;
    let room = "test-cohost-transport-departure";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    transport(&pool, room, OWNER, 21, true).await;
    transport(&pool, room, CO, 22, true).await;

    transport(&pool, room, CO, 22, false).await;
    assert_eq!(
        state(&pool, room).await.as_deref(),
        Some("active"),
        "a co-host's transport departure while the owner is present must not end it"
    );
    assert_eq!(row(&pool, room, CO).await.status, "left");

    transport(&pool, room, OWNER, 21, false).await;
    assert_eq!(
        state(&pool, room).await.as_deref(),
        Some("ended"),
        "the last present host's transport departure must end it"
    );

    assert!(
        db_participants::record_left(&pool, meeting_pk(&pool, room).await, OWNER, 21, true)
            .await
            .expect("record_left")
            .is_none(),
        "a redelivered event (another meeting-api replica) must be a no-op"
    );

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn host_role_changes_are_reported_only_when_they_happen() {
    let pool = get_test_pool().await;
    let room = "test-cohost-no-change-paths";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    let pk = meeting_pk(&pool, room).await;

    let outcome = meeting_api::db::co_hosts::grant(&pool, pk, CO, Some(true), OWNER, 100, true)
        .await
        .expect("grant");
    assert_eq!(
        outcome,
        meeting_api::db::co_hosts::GrantOutcome::Granted { promoted: false },
        "re-granting a present host is not a host change"
    );

    assert!(
        matches!(
            db_participants::kick(&pool, pk, OWNER, ATTENDEE)
                .await
                .expect("kick"),
            db_participants::KickOutcome::Kicked {
                was_host: false,
                ..
            }
        ),
        "kicking a non-host is not a host change"
    );
    let db_participants::KickOutcome::Kicked {
        was_host: true,
        kicked_at,
        deny_until,
    } = db_participants::kick(&pool, pk, OWNER, CO)
        .await
        .expect("kick")
    else {
        panic!("kicking a host is a host change");
    };
    assert_eq!(
        db_participants::kick(&pool, pk, OWNER, CO)
            .await
            .expect("kick"),
        db_participants::KickOutcome::AlreadyKicked {
            kicked_at,
            deny_until,
        },
        "a second kick changes nothing"
    );

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn turning_the_waiting_room_off_admits_co_hosts_who_are_promoted_on_connect() {
    let pool = get_test_pool().await;
    let room = "test-cohost-wr-off-promotes";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    assert_eq!(join_ok(&pool, room, CO).await.status, "waiting");
    assert_eq!(join_ok(&pool, room, ATTENDEE).await.status, "waiting");
    assert_eq!(
        grant(&pool, room, OWNER, CO, false).await.status(),
        StatusCode::OK
    );

    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        OWNER,
        Some(serde_json::json!({ "waiting_room_enabled": false })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let co = row(&pool, room, CO).await;
    assert_eq!(co.status, "admitted");
    assert!(
        !co.is_host,
        "a bulk admit must not promote before the co-host connects"
    );
    let attendee = row(&pool, room, ATTENDEE).await;
    assert_eq!(attendee.status, "admitted");

    transport(&pool, room, CO, 31, true).await;
    transport(&pool, room, ATTENDEE, 32, true).await;
    assert!(row(&pool, room, CO).await.is_host);
    assert!(!row(&pool, room, ATTENDEE).await.is_host);

    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn co_host_fast_path_follows_the_connect_rule_and_never_readmits_a_kicked_row() {
    let pool = get_test_pool().await;
    let room = "test-cohost-no-repromote";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO, CO2] })).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, CO).await;
    join_ok(&pool, room, CO2).await;
    let pk = meeting_pk(&pool, room).await;

    sqlx::query(
        "UPDATE meeting_participants SET is_host = FALSE WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(pk)
    .bind(CO)
    .execute(&pool)
    .await
    .expect("demote CO out of band");
    let (_, promoted) = db_participants::admit_as_co_host(&pool, pk, CO, None)
        .await
        .expect("admit_as_co_host")
        .expect("an admitted co-host with a live entry is promoted, as on connect");
    assert!(promoted);
    assert!(row(&pool, room, CO).await.is_host);

    let resp = host_action(&pool, room, "kick", OWNER, CO2).await;
    assert_eq!(resp.status(), StatusCode::OK);
    sqlx::query("UPDATE meeting_co_hosts SET suspended = FALSE WHERE meeting_id = $1")
        .bind(pk)
        .execute(&pool)
        .await
        .expect("lift suspension out of band");
    assert!(
        db_participants::admit_as_co_host(&pool, pk, CO2, None)
            .await
            .expect("admit_as_co_host")
            .is_none(),
        "a row kicked in this instance must not be re-admitted as host"
    );

    cleanup_test_data(&pool, room).await;
}

// ── Round 10: options authorization (issue #2702) ───────────────────────

/// A live co-host may change meeting OPTIONS via PATCH; a plain admitted
/// participant may not; and a co-host slipping a password into the same
/// request is rejected owner-only, with NOTHING applied — not even the
/// otherwise-valid toggle in the same body.
#[tokio::test]
#[serial]
async fn co_host_may_edit_options_a_plain_participant_may_not_password_stays_owner_only() {
    let pool = get_test_pool().await;
    let room = "test-cohost-patch-authz";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    // A live co-host may change OPTIONS.
    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        CO,
        Some(serde_json::json!({ "admitted_can_admit": true })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.result.admitted_can_admit);

    // A plain admitted participant (not owner, not co-host, not host) may not.
    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        ATTENDEE,
        Some(serde_json::json!({ "admitted_can_admit": false })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(resp).await, "NOT_HOST");
    assert!(
        db_meetings::get_by_room_id(&pool, room)
            .await
            .unwrap()
            .unwrap()
            .admitted_can_admit,
        "the rejected PATCH must not have changed anything"
    );

    // A co-host sending a password is rejected owner-only, with nothing
    // applied — including the otherwise-valid toggle in the same body.
    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        CO,
        Some(serde_json::json!({ "password": "s3cret-pw", "admitted_can_admit": false })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(resp).await, "NOT_OWNER");
    let row = db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .unwrap();
    assert!(row.password_hash.is_none(), "the password must not be set");
    assert!(
        row.admitted_can_admit,
        "the toggle bundled with the rejected password change must not apply either"
    );

    cleanup_test_data(&pool, room).await;
}

/// A present host with no co-host entry (a `transfer-host` target) may also
/// change meeting OPTIONS.
#[tokio::test]
#[serial]
async fn transfer_host_target_may_edit_options() {
    let pool = get_test_pool().await;
    let room = "test-transfer-target-patch";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    assert_eq!(
        host_action(&pool, room, "transfer-host", OWNER, ATTENDEE)
            .await
            .status(),
        StatusCode::OK
    );

    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        ATTENDEE,
        Some(serde_json::json!({ "admitted_can_admit": true })),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a present host (a transfer-host target with no co-host entry) may edit options"
    );

    cleanup_test_data(&pool, room).await;
}

/// A live co-host sending `remove_password: true` is still owner-only, exactly like `password: "..."`.
#[tokio::test]
#[serial]
async fn co_host_remove_password_stays_owner_only() {
    let pool = get_test_pool().await;
    let room = "test-cohost-remove-password-owner-only";
    cleanup_test_data(&pool, room).await;
    create_meeting(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "password": "s3cret-pw" }),
    )
    .await;
    join_ok(&pool, room, OWNER).await;

    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        CO,
        Some(serde_json::json!({ "remove_password": true })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(resp).await, "NOT_OWNER");
    assert!(
        db_meetings::get_by_room_id(&pool, room)
            .await
            .unwrap()
            .unwrap()
            .password_hash
            .is_some(),
        "the password must not be removed"
    );

    cleanup_test_data(&pool, room).await;
}

/// A SUSPENDED co-host may not edit options — `has_live_entry` excludes them, and they hold no cached `is_host` either.
#[tokio::test]
#[serial]
async fn suspended_co_host_may_not_edit_options() {
    let pool = get_test_pool().await;
    let room = "test-cohost-suspended-cannot-patch";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join_ok(&pool, room, OWNER).await;
    assert!(join_ok(&pool, room, CO).await.is_host);
    assert_eq!(
        host_action(&pool, room, "kick", OWNER, CO).await.status(),
        StatusCode::OK
    );
    assert!(suspended(&pool, room, CO).await);

    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        CO,
        Some(serde_json::json!({ "admitted_can_admit": true })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(error_code(resp).await, "NOT_HOST");

    cleanup_test_data(&pool, room).await;
}

/// A present-host PATCH is allowed only while the meeting is active; a cached `is_host` on an idle or ended meeting is not enough.
#[tokio::test]
#[serial]
async fn present_host_patch_requires_an_active_meeting() {
    let pool = get_test_pool().await;
    let room = "test-transfer-target-patch-inactive";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;
    assert_eq!(
        host_action(&pool, room, "transfer-host", OWNER, ATTENDEE)
            .await
            .status(),
        StatusCode::OK
    );
    transport(&pool, room, ATTENDEE, 51, true).await;

    // Force a spurious idle without disturbing ATTENDEE's genuine presence
    // (a relay reporting only its own copy of the room empty), matching
    // `present_co_hosts_survive_a_spurious_idle_with_suspensions_intact`.
    let pk = meeting_pk(&pool, room).await;
    sqlx::query("UPDATE meetings SET state = 'idle' WHERE id = $1")
        .bind(pk)
        .execute(&pool)
        .await
        .expect("force a spurious idle");
    assert_eq!(state(&pool, room).await.as_deref(), Some("idle"));
    assert!(
        row(&pool, room, ATTENDEE).await.is_host,
        "ATTENDEE is still the genuinely present host"
    );

    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        ATTENDEE,
        Some(serde_json::json!({ "admitted_can_admit": true })),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a present host must not edit options while the meeting is not active"
    );
    assert_eq!(error_code(resp).await, "NOT_HOST");

    cleanup_test_data(&pool, room).await;
}

/// `GET` reports `viewer_is_owner` / `viewer_can_edit_options` correctly for
/// the owner, a co-host, and a plain admitted participant.
#[tokio::test]
#[serial]
async fn get_meeting_reports_viewer_flags_for_owner_co_host_and_participant() {
    let pool = get_test_pool().await;
    let room = "test-get-viewer-flags";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    let resp = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/join"),
        OWNER,
        Some(serde_json::json!({ "display_name": "The Owner" })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(join_ok(&pool, room, CO).await.is_host);
    join_ok(&pool, room, ATTENDEE).await;
    admit(&pool, room, ATTENDEE).await;

    let resp = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}"),
        OWNER,
        None,
    )
    .await;
    let owner_view: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(owner_view.result.viewer_is_owner);
    assert!(owner_view.result.viewer_can_edit_options);

    let resp = send(&pool, "GET", &format!("/api/v1/meetings/{room}"), CO, None).await;
    let co_view: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(!co_view.result.viewer_is_owner);
    assert!(co_view.result.viewer_can_edit_options);
    // Round 11 addendum: a co-host viewer must also see WHO the owner is,
    // so the settings page can render an "Owner" row.
    assert_eq!(co_view.result.host_user_id.as_deref(), Some(OWNER));
    assert_eq!(
        co_view.result.host_display_name.as_deref(),
        Some("The Owner")
    );

    let resp = send(
        &pool,
        "GET",
        &format!("/api/v1/meetings/{room}"),
        ATTENDEE,
        None,
    )
    .await;
    let attendee_view: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(!attendee_view.result.viewer_is_owner);
    assert!(!attendee_view.result.viewer_can_edit_options);

    cleanup_test_data(&pool, room).await;
}

/// Co-host matching is case-insensitive: granting a mixed-case email is
/// reachable by the same identity in any case, and re-granting a different
/// case of the same email updates the one canonical (lowercase) row rather
/// than creating a second.
#[tokio::test]
#[serial]
async fn co_host_matching_is_case_insensitive() {
    let pool = get_test_pool().await;
    let room = "test-cohost-case-insensitive";
    let mixed_case = "Bob@Example.com";
    let lower_case = "bob@example.com";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;

    assert_eq!(
        grant(&pool, room, OWNER, mixed_case, true).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        entries(&pool, room).await,
        vec![(lower_case.to_string(), true)],
        "the stored entry must be canonicalized to lowercase"
    );

    // The real identity joins in yet a THIRD casing, exercising the
    // query-side normalization (not just the write-side one above).
    let joined = join_ok(&pool, room, "bOB@example.COM").await;
    assert!(
        joined.is_host,
        "a differently-cased identity must still be recognized as the granted co-host"
    );

    let resp = send(
        &pool,
        "PATCH",
        &format!("/api/v1/meetings/{room}"),
        "bOB@example.COM",
        Some(serde_json::json!({ "admitted_can_admit": true })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        grant(&pool, room, OWNER, "BOB@EXAMPLE.COM", true)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        entries(&pool, room).await,
        vec![(lower_case.to_string(), true)],
        "a different-case re-grant must not create a second entry"
    );

    cleanup_test_data(&pool, room).await;
}

/// Storage stays lowercase, but any id leaving the DB/API (list(), and the
/// NATS host-change announcement) must be the participant's real-case
/// identity, not the lowercase co-host storage key (issue #2702 round 14).
#[tokio::test]
#[serial]
async fn co_host_list_and_announce_use_the_participants_real_case() {
    let pool = get_test_pool().await;
    let room = "test-cohost-canonical-case";
    let mixed_case = "Bob@X.com";
    let lower_case = "bob@x.com";
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, serde_json::json!({})).await;
    join_ok(&pool, room, OWNER).await;
    join_ok(&pool, room, mixed_case).await;
    admit(&pool, room, mixed_case).await;

    let nats = maybe_nats().await;
    let mut host_changes = match &nats {
        Some(nats) => Some(
            nats.subscribe(meeting_api::nats_events::MEETING_HOST_CHANGE_SUBJECT)
                .await
                .expect("subscribe"),
        ),
        None => None,
    };
    let app = match nats.clone() {
        Some(nats) => build_app_on_nats(pool.clone(), nats),
        None => build_app(pool.clone()),
    };
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room}/co-hosts"), OWNER)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "user_id": lower_case, "persist": true }).to_string(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ListCoHostsResponse> = response_json(resp).await;
    assert_eq!(body.result.co_hosts.len(), 1);
    assert_eq!(
        body.result.co_hosts[0].user_id, mixed_case,
        "list() must return the participant's real case, not the lowercase storage key"
    );

    if let Some(sub) = host_changes.as_mut() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("the promotion must be announced within 5s")
            .expect("subscription open");
        let change: meeting_api::nats_events::MeetingHostChangePayload =
            serde_json::from_slice(&msg.payload).expect("payload");
        assert_eq!(
            (
                change.room_id.as_str(),
                change.user_id.as_str(),
                change.is_host
            ),
            (room, mixed_case, true),
            "the host-change announcement must target the participant's real case"
        );
    }
    assert_eq!(
        entries(&pool, room).await,
        vec![(lower_case.to_string(), true)],
        "the underlying storage stays keyed on the lowercase id"
    );

    let mut demotions = match &nats {
        Some(nats) => Some(
            nats.subscribe(meeting_api::nats_events::MEETING_HOST_CHANGE_SUBJECT)
                .await
                .expect("subscribe"),
        ),
        None => None,
    };
    let app = match nats.clone() {
        Some(nats) => build_app_on_nats(pool.clone(), nats),
        None => build_app(pool.clone()),
    };
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room}/co-hosts/revoke"),
        OWNER,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::json!({ "user_id": lower_case }).to_string(),
    ))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    if let Some(sub) = demotions.as_mut() {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("the demotion must be announced within 5s")
            .expect("subscription open");
        let change: meeting_api::nats_events::MeetingHostChangePayload =
            serde_json::from_slice(&msg.payload).expect("payload");
        assert_eq!(
            (
                change.room_id.as_str(),
                change.user_id.as_str(),
                change.is_host
            ),
            (room, mixed_case, false),
            "the revoke announcement must also target the participant's real case"
        );
    }

    assert_eq!(
        entries(&pool, room).await,
        Vec::<(String, bool)>::new(),
        "revoke deletes the entry"
    );

    cleanup_test_data(&pool, room).await;
}
