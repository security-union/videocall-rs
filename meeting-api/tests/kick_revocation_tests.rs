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

//! The relay revocation a host kick publishes, and its re-assertion while a
//! kicked user is still reported present (#2934). Requires `DATABASE_URL` and
//! `NATS_URL`.

mod test_helpers;

use std::time::Duration;

use axum::body::Body;
use axum::http::StatusCode;
use futures::StreamExt;
use jsonwebtoken::{decode, DecodingKey, Validation};
use meeting_api::db::meetings as db_meetings;
use meeting_api::nats_consumers::{
    apply_participant_presence, spawn_presence_heartbeat_consumer_inner,
};
use meeting_api::nats_events::ParticipantPresencePayload;
use meeting_api::state::AppState;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::kick::{
    ParticipantKickedPayload, KICK_DENY_WINDOW_SECS, KICK_IAT_MARGIN_SECS,
    PARTICIPANT_KICKED_SUBJECT,
};
use videocall_meeting_types::presence::{
    HeartbeatSession, PresenceHeartbeat, PRESENCE_HEARTBEAT_SUBJECT,
};
use videocall_meeting_types::responses::{APIResponse, ParticipantStatusResponse};
use videocall_meeting_types::RoomAccessTokenClaims;

const HOST: &str = "kick-host@example.com";
const TARGET: &str = "kick-target@example.com";

async fn nats() -> async_nats::Client {
    maybe_nats().await.expect("NATS_URL must be set")
}

async fn send(
    state: &AppState,
    method: &str,
    uri: &str,
    caller: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let builder = request_with_cookie(method, uri, caller);
    let req = match body {
        Some(json) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    build_app_from_state(state.clone())
        .oneshot(req)
        .await
        .unwrap()
}

async fn join(state: &AppState, room: &str, user: &str) -> ParticipantStatusResponse {
    let resp = send(
        state,
        "POST",
        &format!("/api/v1/meetings/{room}/join"),
        user,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "join as {user}");
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    body.result
}

async fn kick(state: &AppState, room: &str, target: &str) -> StatusCode {
    send(
        state,
        "POST",
        &format!("/api/v1/meetings/{room}/kick"),
        HOST,
        Some(serde_json::json!({ "user_id": target })),
    )
    .await
    .status()
}

/// A started meeting with `TARGET` admitted and `waiting_room_enabled` as given.
async fn meeting_with_target(state: &AppState, room: &str, waiting_room: bool) {
    cleanup_test_data(&state.db, room).await;
    let resp = send(
        state,
        "POST",
        "/api/v1/meetings",
        HOST,
        Some(serde_json::json!({
            "meeting_id": room,
            "attendees": [],
            "waiting_room_enabled": waiting_room,
        })),
    )
    .await;
    assert!(resp.status().is_success(), "create {room}");
    join(state, room, HOST).await;
    let joined = join(state, room, TARGET).await;
    if joined.status != "admitted" {
        admit(state, room, TARGET).await;
    }
}

async fn admit(state: &AppState, room: &str, user: &str) {
    let resp = send(
        state,
        "POST",
        &format!("/api/v1/meetings/{room}/admit"),
        HOST,
        Some(serde_json::json!({ "user_id": user })),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Revocations published for `room` within `wait`.
async fn revocations(
    sub: &mut async_nats::Subscriber,
    room: &str,
    wait: Duration,
) -> Vec<ParticipantKickedPayload> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, sub.next()).await {
        let payload: ParticipantKickedPayload =
            serde_json::from_slice(&msg.payload).expect("revocation payload");
        if payload.room_id == room {
            seen.push(payload);
        }
    }
    seen
}

async fn stored_revocation(state: &AppState, room: &str, user: &str) -> (Option<i64>, Option<i64>) {
    let meeting = db_meetings::get_by_room_id(&state.db, room)
        .await
        .unwrap()
        .unwrap();
    let (kicked_at, until): (
        Option<chrono::DateTime<chrono::Utc>>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT kicked_at, kick_deny_until FROM meeting_participants \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting.id)
    .bind(user)
    .fetch_one(&state.db)
    .await
    .unwrap();
    (
        kicked_at.map(|t| t.timestamp()),
        until.map(|t| t.timestamp()),
    )
}

fn token_iat(token: &str) -> i64 {
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    decode::<RoomAccessTokenClaims>(
        token,
        &DecodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
        &validation,
    )
    .expect("room token")
    .claims
    .iat
    .expect("iat")
}

async fn next_second_after(t: i64) {
    while chrono::Utc::now().timestamp() <= t {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// MUTATION: drop the `publish_kick_revocation` call, or move it after the
/// fallible client broadcast, and this fails.
#[tokio::test]
#[serial]
async fn a_kick_publishes_the_relay_revocation_before_the_client_broadcast() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-order";
    meeting_with_target(&state, room, true).await;

    let mut all = nc.subscribe(">").await.unwrap();
    nc.flush().await.unwrap();
    let before = chrono::Utc::now().timestamp() - 2;
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    let after = chrono::Utc::now().timestamp() + 2;

    let system = format!("room.{room}.system");
    let mut order = Vec::new();
    let mut revocation = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, all.next()).await {
        if msg.subject.as_str() == PARTICIPANT_KICKED_SUBJECT {
            let payload: ParticipantKickedPayload = serde_json::from_slice(&msg.payload).unwrap();
            if payload.room_id == room {
                order.push("revocation");
                revocation = Some(payload);
            }
        } else if msg.subject.as_str() == system {
            order.push("broadcast");
        }
    }
    assert_eq!(order.first(), Some(&"revocation"), "order: {order:?}");
    let revocation = revocation.expect("revocation published");
    assert_eq!(revocation.user_id, TARGET);
    assert!((before..=after).contains(&revocation.kicked_at));
    assert_eq!(
        revocation.revoke_iat_through,
        revocation.kicked_at + KICK_IAT_MARGIN_SECS
    );
    assert_eq!(
        revocation.deny_until,
        revocation.kicked_at + KICK_DENY_WINDOW_SECS
    );
    assert_eq!(
        stored_revocation(&state, room, TARGET).await,
        (Some(revocation.kicked_at), Some(revocation.deny_until))
    );
    cleanup_test_data(&state.db, room).await;
}

/// MUTATION: return early on `AlreadyKicked` as the old `NotAdmitted` did and
/// this fails.
#[tokio::test]
#[serial]
async fn a_retried_kick_republishes_the_same_revocation() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-retry";
    meeting_with_target(&state, room, true).await;
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();

    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    let first = revocations(&mut sub, room, Duration::from_secs(1)).await;
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    let retry = revocations(&mut sub, room, Duration::from_secs(1)).await;
    assert_eq!(first.len(), 1);
    assert_eq!(
        retry, first,
        "the retry re-publishes the recorded revocation"
    );
    cleanup_test_data(&state.db, room).await;
}

/// A kick of a user who re-joined into the waiting room, or whose revocation
/// window has lapsed, publishes nothing.
#[tokio::test]
#[serial]
async fn no_revocation_for_a_rejoined_or_lapsed_kick() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-no-republish";
    meeting_with_target(&state, room, true).await;
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();

    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    assert_eq!(
        revocations(&mut sub, room, Duration::from_secs(1))
            .await
            .len(),
        1
    );
    sqlx::query(
        "UPDATE meeting_participants SET kick_deny_until = NOW() - INTERVAL '1 second' \
         WHERE user_id = $1 AND meeting_id = (SELECT id FROM meetings WHERE room_id = $2)",
    )
    .bind(TARGET)
    .bind(room)
    .execute(&state.db)
    .await
    .unwrap();
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    assert!(revocations(&mut sub, room, Duration::from_secs(1))
        .await
        .is_empty());

    assert_eq!(join(&state, room, TARGET).await.status, "waiting");
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    assert!(revocations(&mut sub, room, Duration::from_secs(1))
        .await
        .is_empty());
    cleanup_test_data(&state.db, room).await;
}

/// MUTATION: drop the `kick_rate_limiter.allow` check and this fails.
#[tokio::test]
#[serial]
async fn kicks_are_rate_limited_per_host() {
    let state = build_state(get_test_pool().await, None, None);
    let room = "test-kick-rate-limit";
    meeting_with_target(&state, room, true).await;
    for i in 0..30 {
        assert_eq!(
            kick(&state, room, &format!("absent-{i}@example.com")).await,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        kick(&state, room, TARGET).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let status: String = sqlx::query_scalar(
        "SELECT p.status FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE m.room_id = $1 AND p.user_id = $2",
    )
    .bind(room)
    .bind(TARGET)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(status, "admitted", "a rate-limited kick changes nothing");
    cleanup_test_data(&state.db, room).await;
}

/// With no waiting room a kicked attendee may re-join. A re-join authorized
/// after `revoke_iat_through` mints a token the revocation does not cover; one
/// authorized within that second is refused once and must re-join again.
#[tokio::test]
#[serial]
async fn a_rejoin_without_a_waiting_room_mints_a_token_the_revocation_does_not_cover() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-rejoin";
    meeting_with_target(&state, room, false).await;
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();

    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    let revocation = revocations(&mut sub, room, Duration::from_secs(1))
        .await
        .pop()
        .expect("revocation");
    next_second_after(revocation.revoke_iat_through).await;
    let rejoined = join(&state, room, TARGET).await;
    assert_eq!(rejoined.status, "admitted");
    assert!(token_iat(&rejoined.room_token.expect("token")) > revocation.revoke_iat_through);
    cleanup_test_data(&state.db, room).await;
}

async fn start_heartbeat_consumer(state: &AppState, nc: &async_nats::Client) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    spawn_presence_heartbeat_consumer_inner(Some(nc.clone()), state.db.clone(), Some(ready_tx))
        .expect("consumer");
    ready_rx.await.expect("consumer ready");
}

async fn heartbeat(nc: &async_nats::Client, room: &str, users: &[&str], unreported: &[&str]) {
    let hb = PresenceHeartbeat {
        room_id: room.to_string(),
        sessions: users
            .iter()
            .enumerate()
            .map(|(i, u)| HeartbeatSession {
                user_id: u.to_string(),
                session_id: 29_340 + i as u64,
            })
            .collect(),
        unreported_user_ids: unreported.iter().map(|u| u.to_string()).collect(),
    };
    nc.publish(
        PRESENCE_HEARTBEAT_SUBJECT,
        serde_json::to_vec(&hb).unwrap().into(),
    )
    .await
    .unwrap();
    nc.flush().await.unwrap();
}

/// A kicked client that stays connected on a relay that missed the kick is
/// still reported by that relay's heartbeat, which re-publishes the revocation.
///
/// MUTATION: drop the `reassert` call in the heartbeat consumer, the `kicks`
/// CTE of `record_heartbeat`, or the re-admission test in `ACTIVE_KICK`, and
/// this fails.
#[tokio::test]
#[serial]
async fn a_heartbeat_still_reporting_a_kicked_user_republishes_the_revocation() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-heartbeat";
    meeting_with_target(&state, room, true).await;
    start_heartbeat_consumer(&state, &nc).await;
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();

    heartbeat(&nc, room, &[HOST, TARGET], &[]).await;
    assert!(
        revocations(&mut sub, room, Duration::from_secs(1))
            .await
            .is_empty(),
        "nobody is kicked yet"
    );

    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    let kicked = revocations(&mut sub, room, Duration::from_secs(1)).await;
    assert_eq!(kicked.len(), 1);

    heartbeat(&nc, room, &[HOST, TARGET], &[]).await;
    assert_eq!(
        revocations(&mut sub, room, Duration::from_secs(2)).await,
        kicked,
        "the heartbeat re-asserts the recorded revocation"
    );

    assert_eq!(join(&state, room, TARGET).await.status, "waiting");
    admit(&state, room, TARGET).await;
    tokio::time::sleep(meeting_api::kick_revocation::REASSERT_MIN_INTERVAL).await;
    heartbeat(&nc, room, &[HOST, TARGET], &[]).await;
    assert!(
        revocations(&mut sub, room, Duration::from_secs(2))
            .await
            .is_empty(),
        "a re-admitted user is never re-kicked"
    );
    cleanup_test_data(&state.db, room).await;
}

/// A relay that restarted after the kick lets the held token back in and
/// reports it present; that report re-publishes the revocation.
///
/// MUTATION: drop the `active_kick` re-assertion in
/// `apply_participant_presence`, or the kick read in `record_present`, and
/// this fails.
#[tokio::test]
#[serial]
async fn a_present_report_for_a_kicked_user_republishes_the_revocation() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-present";
    meeting_with_target(&state, room, true).await;
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();

    let meeting = db_meetings::get_by_room_id(&state.db, room)
        .await
        .unwrap()
        .unwrap();
    for user in [HOST, TARGET] {
        apply_participant_presence(
            &state.db,
            Some(&nc),
            &state.feed_tx,
            &meeting,
            &ParticipantPresencePayload {
                room_id: room.to_string(),
                user_id: user.to_string(),
                session_id: 29_349,
                present: true,
            },
        )
        .await;
    }
    let seen = revocations(&mut sub, room, Duration::from_secs(1)).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].user_id, TARGET);
    cleanup_test_data(&state.db, room).await;
}

/// Leaving the waiting room after a kick must not let the pre-kick token's
/// presence report restore the old admission (which `/status` would then turn
/// into a fresh, post-kick room token).
///
/// MUTATION: drop the `kicked_at` guard from `record_present`'s `left` branch,
/// or the re-admission test in `active_kicks`, and this fails.
#[tokio::test]
#[serial]
async fn a_pre_kick_session_cannot_restore_a_kicked_user_who_left_the_waiting_room() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-left-restore";
    meeting_with_target(&state, room, true).await;
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    assert_eq!(join(&state, room, TARGET).await.status, "waiting");
    let resp = send(
        &state,
        "POST",
        &format!("/api/v1/meetings/{room}/leave"),
        TARGET,
        None,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();

    let meeting = db_meetings::get_by_room_id(&state.db, room)
        .await
        .unwrap()
        .unwrap();
    apply_participant_presence(
        &state.db,
        Some(&nc),
        &state.feed_tx,
        &meeting,
        &ParticipantPresencePayload {
            room_id: room.to_string(),
            user_id: TARGET.to_string(),
            session_id: 29_348,
            present: true,
        },
    )
    .await;
    let status: String = sqlx::query_scalar(
        "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting.id)
    .bind(TARGET)
    .fetch_one(&state.db)
    .await
    .unwrap();
    assert_eq!(status, "left");
    assert_eq!(
        revocations(&mut sub, room, Duration::from_secs(1))
            .await
            .len(),
        1
    );
    cleanup_test_data(&state.db, room).await;
}

/// A row kicked before #2934 has no recorded revocation; kicking it again
/// stamps one and publishes it.
///
/// MUTATION: skip `stamp_kick` for a `kicked` row without `kicked_at` and
/// this fails.
#[tokio::test]
#[serial]
async fn a_repeat_kick_of_a_pre_revocation_kicked_row_stamps_and_publishes_one() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-legacy";
    meeting_with_target(&state, room, true).await;
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    sqlx::query(
        "UPDATE meeting_participants SET kicked_at = NULL, kick_deny_until = NULL \
         WHERE user_id = $1 AND meeting_id = (SELECT id FROM meetings WHERE room_id = $2)",
    )
    .bind(TARGET)
    .bind(room)
    .execute(&state.db)
    .await
    .unwrap();
    assert_eq!(stored_revocation(&state, room, TARGET).await, (None, None));
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();

    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    let published = revocations(&mut sub, room, Duration::from_secs(1)).await;
    assert_eq!(published.len(), 1);
    assert_eq!(
        stored_revocation(&state, room, TARGET).await,
        (Some(published[0].kicked_at), Some(published[0].deny_until))
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT p.status FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
             WHERE m.room_id = $1 AND p.user_id = $2",
        )
        .bind(room)
        .bind(TARGET)
        .fetch_one(&state.db)
        .await
        .unwrap(),
        "kicked"
    );
    cleanup_test_data(&state.db, room).await;
}

/// A relay reports a joined session it has not activated (a client sending
/// only RTT probes) as unreported: that re-checks the kick without renewing
/// presence or stamping the heartbeat watermark.
///
/// MUTATION: drop `unreported_user_ids` from the kick check, or send an
/// unreported-only heartbeat through `record_heartbeat`, and this fails.
#[tokio::test]
#[serial]
async fn an_unreported_user_is_rechecked_for_a_kick_without_renewing_presence() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-unreported";
    meeting_with_target(&state, room, true).await;
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    start_heartbeat_consumer(&state, &nc).await;
    let mut sub = nc.subscribe(PARTICIPANT_KICKED_SUBJECT).await.unwrap();
    nc.flush().await.unwrap();
    sqlx::query("UPDATE presence_heartbeat_watermark SET updated_at = NOW() - INTERVAL '1 hour'")
        .execute(&state.db)
        .await
        .unwrap();
    let watermark = || async {
        sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT MAX(updated_at) FROM presence_heartbeat_watermark",
        )
        .fetch_one(&state.db)
        .await
        .unwrap()
    };
    let stamped = watermark().await;

    heartbeat(&nc, room, &[], &[HOST, TARGET]).await;
    let seen = revocations(&mut sub, room, Duration::from_secs(2)).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].user_id, TARGET);
    assert_eq!(
        watermark().await,
        stamped,
        "an unreported user renews nothing"
    );
    cleanup_test_data(&state.db, room).await;
}

/// A user admitted again after a kick is restored by a new session's
/// presence report once their transport dropped, like any other user.
///
/// MUTATION: make `record_present`'s kick guard (`p.admitted_at >
/// p.kicked_at`) FALSE and this fails.
#[tokio::test]
#[serial]
async fn a_user_admitted_again_after_a_kick_is_restored_by_a_new_session() {
    let nc = nats().await;
    let state = build_state(get_test_pool().await, None, Some(nc.clone()));
    let room = "test-kick-revocation-legit-restore";
    meeting_with_target(&state, room, false).await;
    assert_eq!(kick(&state, room, TARGET).await, StatusCode::OK);
    assert_eq!(join(&state, room, TARGET).await.status, "admitted");
    let meeting = db_meetings::get_by_room_id(&state.db, room)
        .await
        .unwrap()
        .unwrap();
    let status = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2",
        )
        .bind(meeting.id)
        .bind(TARGET)
        .fetch_one(&state.db)
        .await
        .unwrap()
    };
    for (session_id, present) in [(29_361, true), (29_361, false)] {
        apply_participant_presence(
            &state.db,
            Some(&nc),
            &state.feed_tx,
            &meeting,
            &ParticipantPresencePayload {
                room_id: room.to_string(),
                user_id: TARGET.to_string(),
                session_id,
                present,
            },
        )
        .await;
    }
    assert_eq!(status().await, "left");

    apply_participant_presence(
        &state.db,
        Some(&nc),
        &state.feed_tx,
        &meeting,
        &ParticipantPresencePayload {
            room_id: room.to_string(),
            user_id: TARGET.to_string(),
            session_id: 29_362,
            present: true,
        },
    )
    .await;
    assert_eq!(status().await, "admitted");
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn the_kick_revocation_migration_adds_nullable_columns_idempotently() {
    let pool = get_test_pool().await;
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../dbmate/db/migrations/20260930120000_add_participant_kick_revocation.sql"
    );
    let migration = std::fs::read_to_string(path).expect("migration file");
    let up = migration
        .split("-- migrate:down")
        .next()
        .expect("up section");
    for _ in 0..2 {
        sqlx::raw_sql(up)
            .execute(&pool)
            .await
            .expect("migration up");
    }
    let columns: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT column_name::text, data_type::text, is_nullable::text \
         FROM information_schema.columns \
         WHERE table_name = 'meeting_participants' \
           AND column_name IN ('kicked_at', 'kick_deny_until') ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec![
            (
                "kick_deny_until".to_string(),
                "timestamp with time zone".to_string(),
                "YES".to_string()
            ),
            (
                "kicked_at".to_string(),
                "timestamp with time zone".to_string(),
                "YES".to_string()
            ),
        ]
    );
}
