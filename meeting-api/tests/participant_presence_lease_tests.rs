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

//! Presence as a lease: heartbeats, the connect window, the sweep, and the instance boundary.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::{meetings as db_meetings, participants as db_participants};
use meeting_api::feed_events::new_feed_channel;
use meeting_api::nats_consumers::{
    apply_participant_presence, apply_presence_heartbeat, spawn_presence_heartbeat_consumer_inner,
    sweep_presence, sweep_tick_for_test,
};
use meeting_api::nats_events::ParticipantPresencePayload;
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::presence::{
    HeartbeatSession, PresenceHeartbeat, PRESENCE_CONNECT_WINDOW_SECS,
    PRESENCE_HEARTBEAT_INTERVAL_SECS, PRESENCE_HEARTBEAT_SUBJECT, PRESENCE_LEASE_SECS,
};
use videocall_meeting_types::responses::{APIResponse, ParticipantStatusResponse};

const OWNER: &str = "lease-owner@example.com";
const CO: &str = "lease-cohost@example.com";
const ATTENDEE: &str = "lease-attendee@example.com";
const S1: u64 = 0x8000_0000_0000_0101;
const S2: u64 = 0x8000_0000_0000_0102;
const S3: u64 = 0x8000_0000_0000_0103;

async fn send(pool: &PgPool, method: &str, uri: &str, caller: &str, body: serde_json::Value) {
    let req = request_with_cookie(method, uri, caller)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{method} {uri} as {caller}");
}

async fn create(pool: &PgPool, room: &str, extra: serde_json::Value) {
    cleanup_test_data(pool, room).await;
    // Force the watermark fresh so tests default to healthy/lease semantics.
    db_participants::force_heartbeat_watermark_fresh_for_test(pool)
        .await
        .expect("seed heartbeat watermark");
    let mut body = serde_json::json!({ "meeting_id": room, "attendees": [] });
    for (k, v) in extra.as_object().expect("extra must be an object") {
        body[k] = v.clone();
    }
    let req = request_with_cookie("POST", "/api/v1/meetings", OWNER)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

async fn join(pool: &PgPool, room: &str, user: &str) {
    send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room}/join"),
        user,
        serde_json::json!({}),
    )
    .await;
}

async fn meeting(pool: &PgPool, room: &str) -> db_meetings::MeetingRow {
    db_meetings::get_by_room_id(pool, room)
        .await
        .unwrap()
        .expect("meeting exists")
}

async fn row(pool: &PgPool, room: &str, user: &str) -> db_participants::ParticipantRow {
    db_participants::get_status(pool, meeting(pool, room).await.id, user)
        .await
        .unwrap()
        .expect("participant row exists")
}

async fn present_count(pool: &PgPool, room: &str) -> i64 {
    db_participants::count_admitted(pool, meeting(pool, room).await.id, true)
        .await
        .unwrap()
}

/// Force the global heartbeat watermark far into the past, so
/// `presence_healthy` reads `false` regardless of the local NATS connection
/// state passed alongside it. See the exposure-window note on `create`.
async fn stale_watermark(pool: &PgPool) {
    sqlx::query(
        "UPDATE presence_heartbeat_watermark SET updated_at = NOW() - INTERVAL '999 seconds'",
    )
    .execute(pool)
    .await
    .expect("stale the heartbeat watermark");
}

/// Undo [`stale_watermark`].
async fn fresh_watermark(pool: &PgPool) {
    db_participants::force_heartbeat_watermark_fresh_for_test(pool)
        .await
        .expect("refresh the heartbeat watermark");
}

/// Set the watermark's `updated_at` to `secs` seconds ago (seeding the row if
/// absent), for tests that need a specific age rather than "very fresh" or
/// "very stale".
async fn set_watermark_age_secs(pool: &PgPool, secs: f64) {
    sqlx::query(
        "INSERT INTO presence_heartbeat_watermark (id, updated_at) \
         VALUES (TRUE, NOW() - make_interval(secs => $1)) \
         ON CONFLICT (id) DO UPDATE SET updated_at = NOW() - make_interval(secs => $1)",
    )
    .bind(secs)
    .execute(pool)
    .await
    .expect("set watermark age");
}

async fn watermark_updated_at(pool: &PgPool) -> chrono::DateTime<chrono::Utc> {
    sqlx::query_scalar("SELECT updated_at FROM presence_heartbeat_watermark WHERE id = TRUE")
        .fetch_one(pool)
        .await
        .expect("watermark row")
}

async fn presence_seen_at_of(
    pool: &PgPool,
    room: &str,
    user: &str,
) -> Option<chrono::DateTime<chrono::Utc>> {
    sqlx::query_scalar(
        "SELECT presence_seen_at FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting(pool, room).await.id)
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("presence_seen_at")
}

async fn live_session_id_of(pool: &PgPool, room: &str, user: &str) -> Option<i64> {
    sqlx::query_scalar(
        "SELECT live_session_id FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting(pool, room).await.id)
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("live_session_id")
}

/// `POST .../presence/keepalive`, authenticated by session cookie. Returns
/// the status rather than asserting it, so callers can check rejections too.
async fn keepalive_status(pool: &PgPool, room: &str, user: &str) -> StatusCode {
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room}/presence/keepalive"),
        user,
    )
    .body(Body::empty())
    .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap().status()
}

/// `POST .../presence/keepalive-guest`, authenticated by observer/room-token
/// bearer.
async fn keepalive_guest_status(pool: &PgPool, room: &str, bearer: &str) -> StatusCode {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room}/presence/keepalive-guest"))
        .header("Authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap().status()
}

/// Join as a guest into a `waiting_room_enabled: false` meeting (auto-admitted)
/// and return `(user_id, room_token)`.
async fn join_guest_admitted(pool: &PgPool, room: &str, display_name: &str) -> (String, String) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::json!({ "display_name": display_name }).to_string(),
        ))
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "guest join must succeed");
    let parsed: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    let token = parsed
        .result
        .room_token
        .clone()
        .expect("an auto-admitted guest gets a room_token");
    (parsed.result.user_id, token)
}

async fn transport(pool: &PgPool, room: &str, user: &str, session_id: u64, present: bool) {
    let (feed_tx, _feed_rx) = new_feed_channel();
    apply_participant_presence(
        pool,
        None,
        &feed_tx,
        &meeting(pool, room).await,
        &ParticipantPresencePayload {
            room_id: room.to_string(),
            user_id: user.to_string(),
            session_id,
            present,
        },
    )
    .await;
}

async fn heartbeat(pool: &PgPool, room: &str, sessions: &[(&str, u64)]) -> u64 {
    apply_presence_heartbeat(
        pool,
        &PresenceHeartbeat {
            room_id: room.to_string(),
            sessions: sessions
                .iter()
                .map(|(user_id, session_id)| HeartbeatSession {
                    user_id: user_id.to_string(),
                    session_id: *session_id,
                })
                .collect(),
        },
    )
    .await
}

/// Move every timestamp of `room` `secs` seconds into the past, as if that
/// much time went by with no relay report.
async fn travel(pool: &PgPool, room: &str, secs: u64) {
    let secs = secs as f64;
    sqlx::query(
        "UPDATE meetings SET started_at = started_at - make_interval(secs => $2) \
         WHERE room_id = $1",
    )
    .bind(room)
    .bind(secs)
    .execute(pool)
    .await
    .expect("shift meeting");
    sqlx::query(
        "UPDATE meeting_participants SET \
           admitted_at = admitted_at - make_interval(secs => $2), \
           presence_seen_at = presence_seen_at - make_interval(secs => $2), \
           left_at = left_at - make_interval(secs => $2) \
         WHERE meeting_id = (SELECT id FROM meetings WHERE room_id = $1)",
    )
    .bind(room)
    .bind(secs)
    .execute(pool)
    .await
    .expect("shift participants");
}

async fn sweep(pool: &PgPool) {
    let (feed_tx, _feed_rx) = new_feed_channel();
    sweep_presence(pool, None, &feed_tx).await.expect("sweep");
}

async fn entries(pool: &PgPool, room: &str) -> Vec<(String, bool, bool)> {
    let mut rows: Vec<(String, bool, bool)> = sqlx::query_as(
        "SELECT user_id, persistent, suspended FROM meeting_co_hosts WHERE meeting_id = $1",
    )
    .bind(meeting(pool, room).await.id)
    .fetch_all(pool)
    .await
    .expect("entries");
    rows.sort();
    rows
}

#[tokio::test]
#[serial]
async fn an_admitted_waiter_who_never_connects_does_not_block_idle() {
    let pool = get_test_pool().await;
    let room = "lease-admitted-dropped-waiter";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;
    join(&pool, room, ATTENDEE).await;
    send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/admit"),
        OWNER,
        serde_json::json!({ "user_id": ATTENDEE }),
    )
    .await;
    assert_eq!(present_count(&pool, room).await, 2);

    travel(&pool, room, PRESENCE_CONNECT_WINDOW_SECS + 1).await;
    assert_eq!(present_count(&pool, room).await, 1);
    transport(&pool, room, OWNER, S1, false).await;

    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("idle"));
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_relay_crash_ends_the_meeting_once_its_last_host_lease_runs_out() {
    let pool = get_test_pool().await;
    let room = "lease-relay-crash-ends";
    create(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    join(&pool, room, ATTENDEE).await;
    transport(&pool, room, OWNER, S1, true).await;
    transport(&pool, room, ATTENDEE, S2, true).await;

    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    assert_eq!(present_count(&pool, room).await, 0);
    sweep(&pool).await;

    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("ended"));
    assert_eq!(row(&pool, room, OWNER).await.status, "left");
    assert_eq!(row(&pool, room, ATTENDEE).await.status, "left");
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn the_owner_rejoining_a_crashed_relay_meeting_starts_a_new_instance() {
    let pool = get_test_pool().await;
    let room = "lease-relay-crash-owner-rejoins";
    create(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false, "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    join(&pool, room, CO).await;
    join(&pool, room, ATTENDEE).await;
    send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        OWNER,
        serde_json::json!({ "user_id": CO, "persist": false }),
    )
    .await;
    send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/transfer-host"),
        OWNER,
        serde_json::json!({ "user_id": ATTENDEE }),
    )
    .await;
    assert!(!row(&pool, room, OWNER).await.is_host);

    // The relay holding everyone is killed: no departures, no heartbeats.
    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));

    join(&pool, room, OWNER).await;
    assert!(
        row(&pool, room, OWNER).await.is_host,
        "the owner is host again"
    );
    assert!(!row(&pool, room, ATTENDEE).await.is_host);
    assert!(!row(&pool, room, CO).await.is_host);
    assert!(
        entries(&pool, room).await.is_empty(),
        "instance-only entries go"
    );
    assert_eq!(present_count(&pool, room).await, 1);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_presence_lease_survives_a_missed_heartbeat_but_not_a_lapse() {
    let pool = get_test_pool().await;
    let room = "lease-missed-heartbeat";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    travel(&pool, room, 2 * PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
    assert_eq!(present_count(&pool, room).await, 1);
    sweep(&pool).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");
    assert_eq!(heartbeat(&pool, room, &[(OWNER, S1)]).await, 1);

    travel(&pool, room, 2 * PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
    assert_eq!(
        present_count(&pool, room).await,
        1,
        "the heartbeat renewed it"
    );

    travel(&pool, room, PRESENCE_HEARTBEAT_INTERVAL_SECS + 1).await;
    assert_eq!(present_count(&pool, room).await, 0);
    sweep(&pool).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "left");
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("idle"));
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_heartbeat_renews_only_the_sessions_it_lists() {
    let pool = get_test_pool().await;
    let room = "lease-heartbeat-listed-only";
    create(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false, "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    join(&pool, room, CO).await;
    join(&pool, room, ATTENDEE).await;
    transport(&pool, room, OWNER, S1, true).await;
    transport(&pool, room, CO, S2, true).await;
    transport(&pool, room, ATTENDEE, S3, true).await;
    transport(&pool, room, ATTENDEE, S3, false).await;
    join(&pool, room, ATTENDEE).await;
    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;

    let renewed = heartbeat(
        &pool,
        room,
        &[(OWNER, S1), (ATTENDEE, S3), ("nobody@example.com", S2)],
    )
    .await;

    assert_eq!(renewed, 1, "only the owner's listed live session");
    assert_eq!(present_count(&pool, room).await, 1);
    sweep(&pool).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");
    assert_eq!(row(&pool, room, CO).await.status, "left");
    assert_eq!(row(&pool, room, ATTENDEE).await.status, "left");
    cleanup_test_data(&pool, room).await;
}

/// A heartbeat renews any non-tombstoned session but never adopts it as `live_session_id`.
#[tokio::test]
#[serial]
async fn a_heartbeat_renews_a_rest_reset_row_without_adopting_the_session() {
    let pool = get_test_pool().await;
    let room = "lease-heartbeat-renew-no-adopt-on-reset";
    create(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    join(&pool, room, ATTENDEE).await;

    assert_eq!(heartbeat(&pool, room, &[(ATTENDEE, S2)]).await, 1);
    assert_eq!(live_session_id_of(&pool, room, ATTENDEE).await, Some(0));

    transport(&pool, room, ATTENDEE, S2, false).await;
    assert_eq!(
        row(&pool, room, ATTENDEE).await.status,
        "admitted",
        "S2 was never recorded live, so its LEFT cannot match"
    );
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_second_tab_rejoin_does_not_get_the_still_connected_owner_swept() {
    let pool = get_test_pool().await;
    let room = "lease-second-tab-rejoin-survives";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));

    travel(&pool, room, PRESENCE_HEARTBEAT_INTERVAL_SECS - 1).await;
    join(&pool, room, OWNER).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");

    assert_eq!(heartbeat(&pool, room, &[(OWNER, S1)]).await, 1);
    assert_eq!(present_count(&pool, room).await, 1);

    for _ in 0..4 {
        travel(&pool, room, PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
        sweep(&pool).await;
        assert_eq!(row(&pool, room, OWNER).await.status, "admitted");
        heartbeat(&pool, room, &[(OWNER, S1)]).await;
    }
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));
    assert_eq!(present_count(&pool, room).await, 1);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_reconnect_with_a_lost_present_stays_present_via_heartbeat() {
    let pool = get_test_pool().await;
    let room = "lease-lost-present-reconnect-readopts";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));

    for _ in 0..4 {
        travel(&pool, room, PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
        sweep(&pool).await;
        assert_eq!(row(&pool, room, OWNER).await.status, "admitted");
        heartbeat(&pool, room, &[(OWNER, S2)]).await;
    }
    assert_eq!(
        live_session_id_of(&pool, room, OWNER).await,
        Some(S1 as i64)
    );
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));
    cleanup_test_data(&pool, room).await;
}

/// The heartbeat and LEFT consumers do not preserve relative order under backlog.
#[tokio::test]
#[serial]
async fn a_delayed_heartbeat_then_left_for_the_pre_reset_session_does_not_end_the_meeting() {
    let pool = get_test_pool().await;
    let room = "lease-delayed-heartbeat-then-left-race";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));

    join(&pool, room, OWNER).await;

    travel(&pool, room, 10 * PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
    heartbeat(&pool, room, &[(OWNER, S1)]).await;
    assert_eq!(live_session_id_of(&pool, room, OWNER).await, Some(0));

    transport(&pool, room, OWNER, S1, false).await;

    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_mismatched_heartbeat_survives_the_lease_boundary() {
    let pool = get_test_pool().await;
    let room = "lease-mismatched-heartbeat-boundary";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    travel(&pool, room, 2 * PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
    sweep(&pool).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");
    assert_eq!(heartbeat(&pool, room, &[(OWNER, S2)]).await, 1);

    travel(&pool, room, 2 * PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
    assert_eq!(
        present_count(&pool, room).await,
        1,
        "the mismatched heartbeat renewed it"
    );

    travel(&pool, room, PRESENCE_HEARTBEAT_INTERVAL_SECS + 1).await;
    assert_eq!(present_count(&pool, room).await, 0);
    sweep(&pool).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "left");
    cleanup_test_data(&pool, room).await;
}

/// A REST rejoin during the relay's reconnect grace must survive a late heartbeat and LEFT for the old session.
#[tokio::test]
#[serial]
async fn a_rest_rejoin_during_the_reconnect_grace_survives_a_late_heartbeat_and_left() {
    let pool = get_test_pool().await;
    let room = "lease-rejoin-survives-late-heartbeat-and-left";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("active"));

    // The relay starts its reconnect grace for session S1 (no LEFT sent yet).
    // A REST rejoin lands during the grace and resets live_session_id to 0.
    join(&pool, room, OWNER).await;
    assert_eq!(
        row(&pool, room, OWNER).await.status,
        "admitted",
        "the rejoin itself must not be disturbed"
    );

    // A heartbeat the relay queued before dropping S1 arrives after the
    // rejoin, still listing S1.
    heartbeat(&pool, room, &[(OWNER, S1)]).await;
    assert_eq!(live_session_id_of(&pool, room, OWNER).await, Some(0));

    // The grace expires; the relay sends the LEFT it always intended to.
    transport(&pool, room, OWNER, S1, false).await;

    assert_eq!(
        row(&pool, room, OWNER).await.status,
        "admitted",
        "the unmatched LEFT must not depart the still-connected owner"
    );
    assert_eq!(
        meeting(&pool, room).await.state.as_deref(),
        Some("active"),
        "the meeting must not end under a participant who never actually left"
    );
    cleanup_test_data(&pool, room).await;
}

/// A lagging replica applies PRESENT for a session the relay already replaced,
/// after the participant left: nothing renews that session, so it lapses.
#[tokio::test]
#[serial]
async fn a_rolled_back_live_session_expires() {
    let pool = get_test_pool().await;
    let room = "lease-rolled-back-session";
    create(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false, "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    join(&pool, room, ATTENDEE).await;
    transport(&pool, room, OWNER, S1, true).await;
    transport(&pool, room, ATTENDEE, S2, true).await;
    transport(&pool, room, ATTENDEE, S3, true).await;
    transport(&pool, room, ATTENDEE, S3, false).await;
    transport(&pool, room, ATTENDEE, S2, true).await;
    assert_eq!(row(&pool, room, ATTENDEE).await.status, "admitted");

    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    heartbeat(&pool, room, &[(OWNER, S1)]).await;

    assert_eq!(present_count(&pool, room).await, 1);
    sweep(&pool).await;
    assert_eq!(row(&pool, room, ATTENDEE).await.status, "left");
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn present_never_restores_a_row_of_a_past_instance_or_an_ended_meeting() {
    let pool = get_test_pool().await;
    let room = "lease-present-instance-guard";
    create(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    join(&pool, room, CO).await;
    transport(&pool, room, OWNER, S1, true).await;
    transport(&pool, room, CO, S2, true).await;
    transport(&pool, room, CO, S2, false).await;
    transport(&pool, room, OWNER, S1, false).await;
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("idle"));
    travel(&pool, room, 1).await;
    join(&pool, room, OWNER).await;

    // The co-host's room token from the previous instance reconnects directly.
    transport(&pool, room, CO, S3, true).await;
    let co = row(&pool, room, CO).await;
    assert_eq!(co.status, "left");
    assert!(!co.is_host);
    assert_eq!(present_count(&pool, room).await, 1);

    db_meetings::end_meeting(&pool, meeting(&pool, room).await.id)
        .await
        .expect("end");
    let pk = meeting(&pool, room).await.id;
    sqlx::query(
        "UPDATE meeting_participants SET status = 'left', left_at = NOW() \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(pk)
    .bind(OWNER)
    .execute(&pool)
    .await
    .expect("owner left the ended meeting");
    transport(&pool, room, OWNER, 0x8000_0000_0000_0199, true).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "left");
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn a_new_instance_retires_the_previous_instance_participants() {
    let pool = get_test_pool().await;
    let room = "lease-new-instance-retires";
    create(&pool, room, serde_json::json!({ "co_hosts": [CO] })).await;
    join(&pool, room, OWNER).await;
    join(&pool, room, CO).await;
    transport(&pool, room, OWNER, S1, true).await;
    transport(&pool, room, CO, S2, true).await;
    send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/end"),
        OWNER,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(row(&pool, room, CO).await.status, "admitted");
    travel(&pool, room, 1).await;

    join(&pool, room, OWNER).await;
    transport(&pool, room, CO, S3, true).await;

    let co = row(&pool, room, CO).await;
    assert_eq!(co.status, "left");
    assert!(
        !co.is_host,
        "an old instance's token must not come back as host"
    );
    assert_eq!(present_count(&pool, room).await, 1);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn admitting_into_a_meeting_nobody_is_in_starts_the_new_instance_first() {
    let pool = get_test_pool().await;
    let room = "lease-admit-order";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;
    join(&pool, room, ATTENDEE).await;
    join(&pool, room, CO).await;
    send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room}/co-hosts"),
        OWNER,
        serde_json::json!({ "user_id": CO, "persist": false }),
    )
    .await;
    transport(&pool, room, OWNER, S1, false).await;
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("idle"));

    let pk = meeting(&pool, room).await.id;
    let (admitted, activation) = db_participants::admit(&pool, pk, ATTENDEE, true)
        .await
        .expect("admit")
        .expect("the attendee was waiting");

    assert_eq!(admitted.status, "admitted");
    assert_eq!(
        activation,
        db_meetings::Activation::NewInstance { demoted: vec![] }
    );
    assert!(entries(&pool, room).await.is_empty());
    assert_eq!(row(&pool, room, CO).await.status, "waiting");
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn the_migration_retires_participants_of_meetings_that_are_not_active() {
    let pool = get_test_pool().await;
    let ended = "lease-migration-ended";
    let active = "lease-migration-active";
    // Under the OLD, per-binary `set_idle` (pre-#2702-round-3: no presence
    // check at all), a meeting can read `idle` while a participant is still
    // genuinely connected via a DIFFERENT relay replica than the one whose
    // empty view flipped the column. The migration must give that row a
    // lease, exactly like an `active` meeting's — never retire it.
    let idle = "lease-migration-idle-with-connected-user";
    for room in [ended, active, idle] {
        create(
            &pool,
            room,
            serde_json::json!({ "waiting_room_enabled": false }),
        )
        .await;
        join(&pool, room, OWNER).await;
    }
    db_meetings::end_meeting(&pool, meeting(&pool, ended).await.id)
        .await
        .expect("end");
    sqlx::query("UPDATE meetings SET state = 'idle' WHERE room_id = $1")
        .bind(idle)
        .execute(&pool)
        .await
        .expect("simulate the old per-binary idle transition");
    sqlx::query(
        "UPDATE meeting_participants SET live_session_id = NULL, presence_seen_at = NULL \
         WHERE meeting_id IN (SELECT id FROM meetings WHERE room_id = ANY($1))",
    )
    .bind(vec![ended, active, idle])
    .execute(&pool)
    .await
    .expect("pre-migration rows");

    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../dbmate/db/migrations/20260925020000_participant_presence_lease.sql"
    );
    let migration = std::fs::read_to_string(path).expect("migration file");
    let up = migration
        .split("-- migrate:down")
        .next()
        .expect("up section");
    sqlx::raw_sql(up)
        .execute(&pool)
        .await
        .expect("migration up");

    let retired = row(&pool, ended, OWNER).await;
    assert_eq!(retired.status, "left");
    assert!(retired.left_at.is_some());
    assert_eq!(row(&pool, active, OWNER).await.status, "admitted");
    assert_eq!(
        row(&pool, idle, OWNER).await.status,
        "admitted",
        "a connected user in an old-style idle meeting must not be retired"
    );
    for room in [active, idle] {
        let seen: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT presence_seen_at FROM meeting_participants \
             WHERE meeting_id = $1 AND user_id = $2",
        )
        .bind(meeting(&pool, room).await.id)
        .bind(OWNER)
        .fetch_one(&pool)
        .await
        .expect("presence_seen_at");
        assert!(
            seen.is_some(),
            "a present row in {room} gets one lease to be confirmed"
        );
    }

    let live: Option<i64> = sqlx::query_scalar(
        "INSERT INTO meeting_participants (meeting_id, user_id, status) \
         VALUES ($1, 'lease-old-replica@example.com', 'waiting') RETURNING live_session_id",
    )
    .bind(meeting(&pool, active).await.id)
    .fetch_one(&pool)
    .await
    .expect("insert as an old replica would");
    assert_eq!(live, None, "an old replica's insert gets no live session");

    for room in [ended, active, idle] {
        cleanup_test_data(&pool, room).await;
    }
}

#[tokio::test]
#[serial]
async fn heartbeats_over_nats_renew_leases() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping presence heartbeat NATS test");
        return;
    };
    let pool = get_test_pool().await;
    let room = "lease-heartbeat-nats";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;
    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    assert_eq!(present_count(&pool, room).await, 0);

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    spawn_presence_heartbeat_consumer_inner(Some(nats.clone()), pool.clone(), Some(ready_tx))
        .expect("consumer spawns with NATS");
    ready_rx.await.expect("consumer ready");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let heartbeat = PresenceHeartbeat {
        room_id: room.to_string(),
        sessions: vec![HeartbeatSession {
            user_id: OWNER.to_string(),
            session_id: S1,
        }],
    };
    nats.publish(
        PRESENCE_HEARTBEAT_SUBJECT,
        serde_json::to_vec(&heartbeat).unwrap().into(),
    )
    .await
    .expect("publish");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while present_count(&pool, room).await == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(present_count(&pool, room).await, 1);
    cleanup_test_data(&pool, room).await;
}

// ── The heartbeat watermark ──────────────────────────────────────────────
//
// A sweep — or any lease-based "nobody present" decision — trusts that a
// lapsed lease means a genuine departure. These pin that trust is withdrawn
// the moment the pipeline stops demonstrably delivering heartbeats, and
// restored the moment it resumes.

/// A stale watermark must block BOTH the sweep and the instance boundary —
/// the outage must not depart the connected owner, end their
/// `end_on_host_leave` meeting, nor read their lapsed lease as "nobody
/// present" and start a fresh instance on their own rejoin.
#[tokio::test]
#[serial]
async fn a_stale_watermark_blocks_the_sweep_and_the_instance_boundary() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping presence health-gate NATS test");
        return;
    };
    let pool = get_test_pool().await;
    let room = "lease-stale-watermark-no-sweep-no-instance";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    // Captured AFTER `travel`, which itself retroactively shifts
    // `started_at` into the past — comparing against a pre-travel value would
    // spuriously "detect" a new instance from the shift alone.
    let started_at_before = meeting(&pool, room).await.started_at;
    stale_watermark(&pool).await;

    let (feed_tx, _feed_rx) = new_feed_channel();
    assert_eq!(
        sweep_tick_for_test(&pool, &nats, &feed_tx).await,
        None,
        "an unhealthy tick must not sweep"
    );
    assert_eq!(
        row(&pool, room, OWNER).await.status,
        "admitted",
        "not swept"
    );
    assert_eq!(
        meeting(&pool, room).await.state.as_deref(),
        Some("active"),
        "not ended, not idled"
    );

    // The owner's own rejoin must not read the lapsed lease as "nobody
    // present" and start a new instance (which would demote hosts and reset
    // `started_at`) while the pipeline is unhealthy.
    join(&pool, room, OWNER).await;
    assert_eq!(
        meeting(&pool, room).await.started_at,
        started_at_before,
        "must not start a new instance while unhealthy"
    );
    assert!(
        row(&pool, room, OWNER).await.is_host,
        "must not have been demoted by a spurious new instance"
    );

    fresh_watermark(&pool).await;
    cleanup_test_data(&pool, room).await;
}

/// Once the watermark recovers, normal (strict-lease) sweeping resumes.
#[tokio::test]
#[serial]
async fn the_sweep_resumes_once_the_watermark_recovers() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping presence health-gate NATS test");
        return;
    };
    let pool = get_test_pool().await;
    let room = "lease-watermark-recovers";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    stale_watermark(&pool).await;
    let (feed_tx, _feed_rx) = new_feed_channel();
    assert_eq!(
        sweep_tick_for_test(&pool, &nats, &feed_tx).await,
        None,
        "still unhealthy: no sweep"
    );
    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");

    // Heartbeats resume reaching the database.
    fresh_watermark(&pool).await;
    assert_eq!(
        sweep_tick_for_test(&pool, &nats, &feed_tx).await,
        Some(1),
        "healthy again: the lapsed lease is swept"
    );
    assert_eq!(row(&pool, room, OWNER).await.status, "left");
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("idle"));
    cleanup_test_data(&pool, room).await;
}

/// A row the sweeper departed on a guess (the lease lapsed) is restored the
/// moment its own, still-live session is heard from again — proving the
/// sweep's guess wrong rather than compounding it. Uses the direct
/// `sweep_presence` call (always strict), matching how a real sweep tick
/// behaves once it has already passed the health gate.
#[tokio::test]
#[serial]
async fn a_falsely_swept_row_is_restored_by_its_own_sessions_heartbeat() {
    let pool = get_test_pool().await;
    let room = "lease-falsely-swept-restored-by-heartbeat";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    sweep(&pool).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "left", "swept");
    assert_eq!(meeting(&pool, room).await.state.as_deref(), Some("idle"));

    // The relay was there all along: its next heartbeat still lists S1.
    let renewed = heartbeat(&pool, room, &[(OWNER, S1)]).await;
    assert_eq!(renewed, 1, "the falsely-swept row is restored");
    let restored = row(&pool, room, OWNER).await;
    assert_eq!(restored.status, "admitted");
    assert!(restored.left_at.is_none());
    cleanup_test_data(&pool, room).await;
}

/// A relay fleet that has never sent a single heartbeat (an old relay
/// version, or brand-new meeting-api with nothing populated yet) must not
/// have its waiting-room guard read a host's connect-window-only presence as
/// "absent" and refuse joins the moment that window elapses.
#[tokio::test]
#[serial]
async fn relays_that_never_heartbeat_still_serve_the_waiting_room() {
    let pool = get_test_pool().await;
    let room = "lease-no-heartbeat-fleet-waiting-room";
    // waiting_room_enabled defaults true, admitted_can_admit defaults false;
    // end_on_host_leave=false both keeps this meeting alive when the owner's
    // connect-window presence lapses AND makes `require_present_host` true
    // in `join_as_attendee` (`!end_on_host_leave && !admitted_can_admit &&
    // waiting_room_enabled`).
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    // The owner's relay never reports PRESENT or a heartbeat at all — only
    // the REST connect window ever covered them.
    travel(&pool, room, PRESENCE_CONNECT_WINDOW_SECS + 1).await;
    stale_watermark(&pool).await;

    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room}/join"), ATTENDEE)
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::json!({}).to_string()))
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "an unhealthy pipeline must not refuse a waiting-room join as 'no host present'"
    );

    fresh_watermark(&pool).await;
    cleanup_test_data(&pool, room).await;
}

// ── NON-BLOCKING: `NOT present_sql` must not silently exclude NULL rows ─────

/// `presence_seen_at IS NULL` with a live (nonzero) session and a stale
/// `admitted_at`: before the fix, `presence_seen_at > NOW() - INTERVAL ...`
/// evaluated to SQL `NULL` (not `FALSE`), so `NOT present_sql` was also
/// `NULL` and the row was excluded from both "present" and "not present"
/// scans — the sweep could never reach it.
#[tokio::test]
#[serial]
async fn a_null_presence_seen_at_past_the_connect_window_is_still_swept() {
    let pool = get_test_pool().await;
    let room = "lease-null-presence-seen-at-sweep";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    let meeting_id = meeting(&pool, room).await.id;
    sqlx::query(
        "UPDATE meeting_participants SET presence_seen_at = NULL \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_id)
    .bind(OWNER)
    .execute(&pool)
    .await
    .expect("null out presence_seen_at");
    travel(&pool, room, PRESENCE_CONNECT_WINDOW_SECS + 1).await;

    let expired = db_participants::expired_presences(&pool, 10)
        .await
        .expect("expired_presences");
    assert!(
        expired
            .iter()
            .any(|(_, _, user_id)| user_id == OWNER),
        "a NULL presence_seen_at past the connect window must still be swept, not silently excluded"
    );

    sweep(&pool).await;
    assert_eq!(row(&pool, room, OWNER).await.status, "left");
    cleanup_test_data(&pool, room).await;
}

/// A lapsed lease can flip a still-connected co-host to `left` before anyone
/// notices. A kick must still reach that row (same current-instance predicate
/// as the heartbeat/PRESENT restore paths), fully terminate it, and make sure
/// their own session's heartbeat or reconnect cannot undo the kick.
#[tokio::test]
#[serial]
async fn kicking_a_swept_left_co_host_prevents_their_own_restore() {
    let pool = get_test_pool().await;
    let room = "lease-kick-swept-left-co-host";
    create(
        &pool,
        room,
        serde_json::json!({ "co_hosts": [CO], "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    join(&pool, room, CO).await;
    transport(&pool, room, OWNER, S1, true).await;
    transport(&pool, room, CO, S2, true).await;
    assert!(
        row(&pool, room, CO).await.is_host,
        "co-host promoted on connect"
    );

    travel(&pool, room, PRESENCE_LEASE_SECS + 1).await;
    // Renew only the owner's lease, so the sweep departs CO (a lapsed lease)
    // while OWNER stays present and eligible to call kick.
    heartbeat(&pool, room, &[(OWNER, S1)]).await;
    sweep(&pool).await;
    let swept = row(&pool, room, CO).await;
    assert_eq!(swept.status, "left", "swept, not kicked, so far");
    assert!(swept.is_host, "a sweep alone does not touch is_host");

    let pk = meeting(&pool, room).await.id;
    let outcome = db_participants::kick(&pool, pk, OWNER, CO)
        .await
        .expect("kick");
    assert_eq!(
        outcome,
        db_participants::KickOutcome::Kicked { was_host: true },
        "a swept-left co-host of the current instance must still be kickable"
    );
    let kicked = row(&pool, room, CO).await;
    assert_eq!(kicked.status, "kicked");
    assert!(!kicked.is_host);

    // Neither the kicked session's own heartbeat nor its reconnect may undo
    // the kick.
    assert_eq!(
        heartbeat(&pool, room, &[(CO, S2)]).await,
        0,
        "a kicked row must not be renewed or restored"
    );
    transport(&pool, room, CO, S2, true).await;
    let after = row(&pool, room, CO).await;
    assert_eq!(after.status, "kicked", "PRESENT must not undo a kick");
    assert!(!after.is_host);
    cleanup_test_data(&pool, room).await;
}

/// Defence in depth: the DB layer refuses a self-kick even though the route
/// already blocks it before calling in.
#[tokio::test]
#[serial]
async fn kick_refuses_a_caller_targeting_themselves() {
    let pool = get_test_pool().await;
    let room = "lease-kick-self-guard";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    let pk = meeting(&pool, room).await.id;

    assert_eq!(
        db_participants::kick(&pool, pk, OWNER, OWNER)
            .await
            .expect("kick"),
        db_participants::KickOutcome::CannotKickSelf
    );
    assert_eq!(row(&pool, room, OWNER).await.status, "admitted");
    cleanup_test_data(&pool, room).await;
}

/// `record_heartbeat` folds the watermark upsert into its own statement,
/// gated so a fresh-enough row is left alone and only a stale one is bumped.
#[tokio::test]
#[serial]
async fn record_heartbeat_refreshes_a_stale_watermark_but_not_a_fresh_one() {
    let pool = get_test_pool().await;
    let room = "lease-watermark-refresh-gate";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    // Within the gate: a heartbeat must not rewrite an already-fresh row.
    set_watermark_age_secs(&pool, 5.0).await;
    let before = watermark_updated_at(&pool).await;
    heartbeat(&pool, room, &[(OWNER, S1)]).await;
    assert_eq!(
        watermark_updated_at(&pool).await,
        before,
        "a fresh-enough watermark must not be rewritten"
    );

    // Past the gate: the next heartbeat must refresh it.
    set_watermark_age_secs(&pool, 15.0).await;
    heartbeat(&pool, room, &[(OWNER, S1)]).await;
    assert!(
        chrono::Utc::now() - watermark_updated_at(&pool).await < chrono::Duration::seconds(2),
        "a stale-past-the-gate watermark must be refreshed"
    );
    cleanup_test_data(&pool, room).await;
}

/// `AppState::presence_healthy` caches the watermark-freshness half for a
/// short TTL (2s). An unhealthy transition can therefore be masked for at
/// most that TTL, never longer: a call made immediately after the pipeline
/// goes unhealthy may still see the stale cached "healthy", but a call made
/// once the TTL has elapsed must see the transition.
#[tokio::test]
#[serial]
async fn presence_healthy_cache_cannot_mask_an_unhealthy_transition_past_its_ttl() {
    let pool = get_test_pool().await;
    let room = "lease-presence-healthy-cache-ttl";
    create(&pool, room, serde_json::json!({})).await;
    let state = build_state(pool.clone(), None, None);

    assert!(
        state.presence_healthy().await.expect("healthy"),
        "fresh watermark, no NATS client: vacuously healthy"
    );

    stale_watermark(&pool).await;
    assert!(
        state.presence_healthy().await.expect("healthy"),
        "within the cache TTL, the transition to unhealthy is not yet visible"
    );

    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    assert!(
        !state.presence_healthy().await.expect("healthy"),
        "past the cache TTL, the transition to unhealthy must be visible"
    );

    fresh_watermark(&pool).await;
    cleanup_test_data(&pool, room).await;
}

/// A user listed twice with two different sessions in one heartbeat batch is
/// ambiguous: adopting either onto a `NULL` `live_session_id` would be an
/// arbitrary, unspecified choice. `record_heartbeat` must skip that user
/// entirely rather than pick one nondeterministically.
#[tokio::test]
#[serial]
async fn a_heartbeat_listing_two_sessions_for_one_user_adopts_neither() {
    let pool = get_test_pool().await;
    let room = "lease-heartbeat-ambiguous-multi-session";
    create(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;
    join(&pool, room, OWNER).await;
    // Simulate a pre-migration row: no live session recorded yet.
    sqlx::query(
        "UPDATE meeting_participants SET live_session_id = NULL \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting(&pool, room).await.id)
    .bind(OWNER)
    .execute(&pool)
    .await
    .expect("simulate a pre-migration row");

    let renewed = heartbeat(&pool, room, &[(OWNER, S1), (OWNER, S2)]).await;
    assert_eq!(
        renewed, 0,
        "an ambiguous multi-session listing must adopt neither session"
    );
    let live: Option<i64> = sqlx::query_scalar(
        "SELECT live_session_id FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting(&pool, room).await.id)
    .bind(OWNER)
    .fetch_one(&pool)
    .await
    .expect("live_session_id");
    assert_eq!(live, None, "must stay NULL, not adopt either session");
    cleanup_test_data(&pool, room).await;
}

// ── Presence keepalive: the manual pre-join lobby has no live session ───────

/// The manual pre-join lobby admits a participant (for the owner, activates
/// the meeting) with no transport session for as long as the card is shown,
/// far longer than the 60s REST connect window. Regular keepalives must keep
/// them present past that window, keep the meeting active, and let a
/// waiting-room attendee join instead of hitting a "meeting not active"
/// rejection.
#[tokio::test]
#[serial]
async fn keepalive_keeps_a_lobby_owner_present_and_the_meeting_joinable() {
    let pool = get_test_pool().await;
    let room = "lease-keepalive-lobby-owner";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    assert_eq!(
        row(&pool, room, OWNER).await.status,
        "admitted",
        "the owner is admitted with no transport session yet"
    );

    // Three keepalives 30s apart, comfortably inside the 90s lease, spanning
    // well past the 60s connect window in total.
    for _ in 0..3 {
        travel(&pool, room, PRESENCE_HEARTBEAT_INTERVAL_SECS).await;
        assert_eq!(
            keepalive_status(&pool, room, OWNER).await,
            StatusCode::OK,
            "a keepalive for the lobby owner must succeed"
        );
    }

    sweep(&pool).await;
    assert_eq!(
        row(&pool, room, OWNER).await.status,
        "admitted",
        "keepalives must keep the lobby owner present past the connect window"
    );
    assert_eq!(
        meeting(&pool, room).await.state.as_deref(),
        Some("active"),
        "the meeting must stay active under a kept-alive lobby owner"
    );

    // A waiting-room attendee join must succeed, not hit "meeting not active".
    join(&pool, room, ATTENDEE).await;
    assert_eq!(row(&pool, room, ATTENDEE).await.status, "waiting");
    cleanup_test_data(&pool, room).await;
}

/// Control: absent any keepalive, the lobby row is still governed by the
/// connect window alone and is swept once it elapses — the fix must not make
/// every lobby row immune to sweeping.
#[tokio::test]
#[serial]
async fn without_keepalives_the_lobby_row_is_swept_after_the_connect_window() {
    let pool = get_test_pool().await;
    let room = "lease-keepalive-lobby-owner-no-keepalive";
    create(
        &pool,
        room,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room, OWNER).await;

    travel(&pool, room, PRESENCE_CONNECT_WINDOW_SECS + 1).await;
    sweep(&pool).await;
    assert_eq!(
        row(&pool, room, OWNER).await.status,
        "left",
        "without a keepalive, the connect window alone still governs the lobby row"
    );
    cleanup_test_data(&pool, room).await;
}

/// A keepalive must never touch a row whose session a relay has already
/// reported present — that is the heartbeat's job.
#[tokio::test]
#[serial]
async fn keepalive_never_touches_a_row_with_a_live_session() {
    let pool = get_test_pool().await;
    let room = "lease-keepalive-ignores-live-session";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    transport(&pool, room, OWNER, S1, true).await;

    let before = presence_seen_at_of(&pool, room, OWNER).await;
    let pk = meeting(&pool, room).await.id;
    let touched = db_participants::keepalive(&pool, pk, OWNER)
        .await
        .expect("keepalive");
    assert!(!touched, "a row with a live session must not be touched");
    assert_eq!(presence_seen_at_of(&pool, room, OWNER).await, before);
    cleanup_test_data(&pool, room).await;
}

/// A keepalive after the meeting ended must be a no-op: it never resurrects
/// a row whose meeting is terminal.
#[tokio::test]
#[serial]
async fn keepalive_after_the_meeting_ended_is_a_no_op() {
    let pool = get_test_pool().await;
    let room = "lease-keepalive-after-ended";
    create(&pool, room, serde_json::json!({})).await;
    join(&pool, room, OWNER).await;
    let pk = meeting(&pool, room).await.id;
    db_meetings::end_meeting(&pool, pk).await.expect("end");

    let before = presence_seen_at_of(&pool, room, OWNER).await;
    let touched = db_participants::keepalive(&pool, pk, OWNER)
        .await
        .expect("keepalive");
    assert!(
        !touched,
        "a keepalive after the meeting ended must be a no-op"
    );
    assert_eq!(presence_seen_at_of(&pool, room, OWNER).await, before);
    cleanup_test_data(&pool, room).await;
}

/// A guest with a valid room token can keep their own lobby row alive; a
/// forged/garbage token is rejected outright by the `GuestObserver` extractor.
#[tokio::test]
#[serial]
async fn a_guest_keepalive_works_and_a_wrong_token_is_rejected() {
    let pool = get_test_pool().await;
    let room = "lease-keepalive-guest";
    create(
        &pool,
        room,
        serde_json::json!({ "waiting_room_enabled": false, "allow_guests": true }),
    )
    .await;
    join(&pool, room, OWNER).await;

    let (guest_id, bearer) = join_guest_admitted(&pool, room, "Casual Guest").await;
    let before = presence_seen_at_of(&pool, room, &guest_id).await;

    assert_eq!(
        keepalive_guest_status(&pool, room, &bearer).await,
        StatusCode::OK,
        "a valid guest token must renew the guest's own lobby row"
    );
    assert!(
        presence_seen_at_of(&pool, room, &guest_id).await > before,
        "the guest's presence_seen_at must have been renewed"
    );

    assert_eq!(
        keepalive_guest_status(&pool, room, "not-a-real-token").await,
        StatusCode::UNAUTHORIZED,
        "a garbage bearer token must be rejected"
    );
    cleanup_test_data(&pool, room).await;
}
