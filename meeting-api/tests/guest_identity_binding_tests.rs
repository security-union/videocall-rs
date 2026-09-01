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

//! Integration tests for guest identity binding on
//! `POST /api/v1/meetings/{meeting_id}/join-guest` (issue #2331).
//!
//! A guest's `guest:{uuid}` user_id travels to every peer in
//! `PacketWrapper.user_id`, so any co-participant can harvest it. These tests
//! pin that a harvested id is worthless without a matching guest token.
//!
//! NATS is `None` in [`build_app`]; publish calls are no-ops. Requires a live
//! Postgres via `DATABASE_URL`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use jsonwebtoken::{decode, DecodingKey, Validation};
use meeting_api::db::{meetings as db_meetings, participants as db_participants};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ParticipantStatusResponse};
use videocall_meeting_types::RoomAccessTokenClaims;

const HOST: &str = "host@example.com";

// ── Setup helpers ────────────────────────────────────────────────────────

async fn setup_active_guest_meeting(pool: &sqlx::PgPool, room_id: &str, waiting_room: bool) {
    cleanup_test_data(pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "allow_guests": true,
                "waiting_room_enabled": waiting_room
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), HOST)
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

/// `POST /join-guest`, optionally carrying a `guest_session_id` and a bearer token.
async fn join_guest(
    pool: &sqlx::PgPool,
    room_id: &str,
    display_name: &str,
    guest_session_id: Option<&str>,
    bearer: Option<&str>,
) -> (StatusCode, ParticipantStatusResponse) {
    let mut body = serde_json::json!({ "display_name": display_name });
    if let Some(id) = guest_session_id {
        body["guest_session_id"] = serde_json::Value::String(id.to_string());
    }

    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
        .header("Content-Type", "application/json");
    if let Some(token) = bearer {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let req = req
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let parsed: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    (status, parsed.result)
}

/// The credential a client persists after a join: room_token, else observer_token.
fn guest_token(resp: &ParticipantStatusResponse) -> String {
    resp.room_token
        .clone()
        .or_else(|| resp.observer_token.clone())
        .expect("a guest join must return a room_token or an observer_token")
}

async fn meeting_db_id(pool: &sqlx::PgPool, room_id: &str) -> i32 {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .unwrap()
        .expect("meeting should exist")
        .id
}

/// `GET /guest-status`, authenticated by the guest's bearer token.
async fn guest_status(
    pool: &sqlx::PgPool,
    room_id: &str,
    bearer: &str,
) -> (StatusCode, ParticipantStatusResponse) {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v1/meetings/{room_id}/guest-status"))
        .header("Authorization", format!("Bearer {bearer}"))
        .body(Body::empty())
        .unwrap();

    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let parsed: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    (status, parsed.result)
}

fn decode_token(token: &str) -> RoomAccessTokenClaims {
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    decode::<RoomAccessTokenClaims>(
        token,
        &DecodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
        &validation,
    )
    .expect("token must be a valid JWT signed with the test secret")
    .claims
}

/// Drive the row straight to `status`, standing in for the host-side
/// revocations without replaying the multi-request host flow.
async fn force_status(pool: &sqlx::PgPool, room_id: &str, user_id: &str, status: &str) {
    let db_id = meeting_db_id(pool, room_id).await;
    sqlx::query(
        "UPDATE meeting_participants SET status = $1 WHERE meeting_id = $2 AND user_id = $3",
    )
    .bind(status)
    .bind(db_id)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
}

// ── The security test ────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn harvested_guest_session_id_without_a_token_cannot_take_over_the_row() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-hijack-no-token";
    setup_active_guest_meeting(&pool, room_id, false).await;

    let (status, victim) = join_guest(&pool, room_id, "Victim", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let victim_id = victim.user_id.clone();

    // Replay the victim's user_id with no proof of ever having held it.
    let (status, attacker) = join_guest(&pool, room_id, "Attacker", Some(&victim_id), None).await;
    assert_eq!(status, StatusCode::OK);

    assert_ne!(
        attacker.user_id, victim_id,
        "an unauthenticated caller must not be granted the victim's guest identity"
    );

    // The victim's row was not taken over by the ON CONFLICT DO UPDATE.
    let meeting_db_id = meeting_db_id(&pool, room_id).await;
    let victim_row = db_participants::get_status(&pool, meeting_db_id, &victim_id)
        .await
        .unwrap()
        .expect("victim row should still exist");
    assert_eq!(victim_row.display_name.as_deref(), Some("Victim"));
    assert_eq!(victim_row.status, "admitted");
    assert_eq!(
        attacker.display_name.as_deref(),
        Some("Attacker"),
        "the attacker gets their own fresh row, not the victim's name"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn harvested_guest_session_id_cannot_demote_an_admitted_guest_to_waiting() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-hijack-wr-demote";
    setup_active_guest_meeting(&pool, room_id, true).await;

    let (_, victim) = join_guest(&pool, room_id, "Victim", None, None).await;
    let victim_id = victim.user_id.clone();
    assert_eq!(victim.status, "waiting");

    let meeting_db_id = meeting_db_id(&pool, room_id).await;
    db_participants::admit(&pool, meeting_db_id, &victim_id)
        .await
        .unwrap()
        .expect("host admits the victim");

    // Roster denial-of-service leg: the waiting-room upsert sets
    // status = 'waiting' unconditionally on conflict.
    let (_, attacker) = join_guest(&pool, room_id, "Attacker", Some(&victim_id), None).await;
    assert_ne!(attacker.user_id, victim_id);

    let victim_row = db_participants::get_status(&pool, meeting_db_id, &victim_id)
        .await
        .unwrap()
        .expect("victim row should still exist");
    assert_eq!(
        victim_row.status, "admitted",
        "an admitted guest must not be demoted to 'waiting' by someone else's join"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn first_join_without_any_token_still_works() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-first-join";
    setup_active_guest_meeting(&pool, room_id, false).await;

    let (status, guest) = join_guest(&pool, room_id, "Newcomer", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(guest.is_guest);
    assert_eq!(guest.status, "admitted");
    assert!(guest
        .user_id
        .starts_with(videocall_meeting_types::GUEST_USER_ID_PREFIX));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn rejoin_with_a_matching_guest_token_resumes_the_same_identity() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-rejoin-with-token";
    setup_active_guest_meeting(&pool, room_id, false).await;

    let (_, first) = join_guest(&pool, room_id, "Returning Guest", None, None).await;
    let guest_id = first.user_id.clone();
    let token = guest_token(&first);

    let (status, second) = join_guest(
        &pool,
        room_id,
        "Returning Guest",
        Some(&guest_id),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        second.user_id, guest_id,
        "a guest presenting their own token must land back on their own row"
    );

    // One row, not two: the rejoin updated the existing participant.
    let meeting_db_id = meeting_db_id(&pool, room_id).await;
    let guest_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1 AND is_guest = TRUE",
    )
    .bind(meeting_db_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(guest_rows, 1);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_token_whose_subject_is_someone_else_does_not_grant_that_identity() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-token-subject-mismatch";
    setup_active_guest_meeting(&pool, room_id, false).await;

    let (_, victim) = join_guest(&pool, room_id, "Victim", None, None).await;
    let victim_id = victim.user_id.clone();

    // A perfectly valid token — for the attacker's OWN identity.
    let (_, attacker_first) = join_guest(&pool, room_id, "Attacker", None, None).await;
    let attacker_token = guest_token(&attacker_first);

    let (status, attacker) = join_guest(
        &pool,
        room_id,
        "Attacker",
        Some(&victim_id),
        Some(&attacker_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(attacker.user_id, victim_id);

    let meeting_db_id = meeting_db_id(&pool, room_id).await;
    let victim_row = db_participants::get_status(&pool, meeting_db_id, &victim_id)
        .await
        .unwrap()
        .expect("victim row should still exist");
    assert_eq!(victim_row.display_name.as_deref(), Some("Victim"));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_token_issued_for_another_meeting_does_not_resume_the_identity_here() {
    let pool = get_test_pool().await;
    let room_a = "test-2331-cross-meeting-a";
    let room_b = "test-2331-cross-meeting-b";
    setup_active_guest_meeting(&pool, room_a, false).await;
    setup_active_guest_meeting(&pool, room_b, false).await;

    let (_, in_a) = join_guest(&pool, room_a, "Hopper", None, None).await;
    let guest_id = in_a.user_id.clone();
    let token_for_a = guest_token(&in_a);

    let (status, in_b) =
        join_guest(&pool, room_b, "Hopper", Some(&guest_id), Some(&token_for_a)).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(
        in_b.user_id, guest_id,
        "a token scoped to room A must not authorize an identity claim in room B"
    );

    cleanup_test_data(&pool, room_a).await;
    cleanup_test_data(&pool, room_b).await;
}

#[tokio::test]
#[serial]
async fn a_forged_token_is_ignored_rather_than_rejecting_the_join() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-forged-token";
    setup_active_guest_meeting(&pool, room_id, false).await;

    let (_, victim) = join_guest(&pool, room_id, "Victim", None, None).await;
    let victim_id = victim.user_id.clone();

    // Signed with the wrong key: not honoured, and not a 401 dead end either.
    let forged = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &serde_json::json!({
            "sub": victim_id,
            "room": room_id,
            "room_join": false,
            "is_host": false,
            "is_guest": true,
            "display_name": "Victim",
            "observer": true,
            "end_on_host_leave": true,
            "exp": chrono::Utc::now().timestamp() + 600,
            "iss": videocall_meeting_types::RoomAccessTokenClaims::ISSUER,
        }),
        &jsonwebtoken::EncodingKey::from_secret(b"not-the-servers-secret"),
    )
    .unwrap();

    let (status, attacker) =
        join_guest(&pool, room_id, "Attacker", Some(&victim_id), Some(&forged)).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(attacker.user_id, victim_id);

    cleanup_test_data(&pool, room_id).await;
}

// ── Observer-token continuity across a wait ──────────────────────────────

#[tokio::test]
#[serial]
async fn a_waiting_guest_is_re_issued_an_observer_token_on_every_poll() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-waiting-poll-remints";
    setup_active_guest_meeting(&pool, room_id, true).await;

    let (_, joined) = join_guest(&pool, room_id, "Patient Guest", None, None).await;
    assert_eq!(joined.status, "waiting");
    let guest_id = joined.user_id.clone();
    let token = guest_token(&joined);

    let (status, polled) = guest_status(&pool, room_id, &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(polled.status, "waiting");
    assert!(
        polled.room_token.is_none(),
        "a waiting guest must not be handed an admission-capable token"
    );

    let refreshed = polled
        .observer_token
        .expect("a waiting guest must be re-issued an observer token on every poll");

    let claims = decode_token(&refreshed);
    assert_eq!(claims.sub, guest_id, "must re-issue for the same identity");
    assert_eq!(claims.room, room_id, "must stay scoped to this meeting");
    assert!(claims.is_guest);
    assert!(claims.observer, "must be an observer token");
    assert!(
        !claims.room_join,
        "re-issuing must not grant admission the host has not given"
    );
    assert!(!claims.is_host);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_revoked_guest_is_not_re_issued_an_observer_token() {
    let pool = get_test_pool().await;
    let room_id = "test-2331-revoked-no-remint";
    setup_active_guest_meeting(&pool, room_id, true).await;

    let (_, joined) = join_guest(&pool, room_id, "Ejected Guest", None, None).await;
    let guest_id = joined.user_id.clone();
    let token = guest_token(&joined);

    for revoked in ["kicked", "rejected", "left"] {
        force_status(&pool, room_id, &guest_id, revoked).await;
        let (status, polled) = guest_status(&pool, room_id, &token).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(polled.status, revoked);
        assert!(
            polled.room_token.is_none(),
            "a {revoked} guest must not be handed a room token"
        );
        assert!(
            polled.observer_token.is_none(),
            "a {revoked} guest must not have their observation renewed"
        );
    }

    cleanup_test_data(&pool, room_id).await;
}
