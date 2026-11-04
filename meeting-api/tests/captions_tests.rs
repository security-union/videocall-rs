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

//! `POST /api/v1/meetings/{id}/captions`. Requires `DATABASE_URL`; the
//! publish tests also require `NATS_URL`.

mod captions_common;
mod test_helpers;

use axum::body::Body;
use axum::http::{header::AUTHORIZATION, Request, StatusCode};
use captions_common::*;
use meeting_api::db::participants as db_participants;
use meeting_api::state::AppState;
use meeting_api::token::{generate_observer_token, generate_room_token};
use serde_json::{json, Value};
use serial_test::serial;
use test_helpers::*;

const CO_HOST: &str = "captions-cohost@example.com";
const ATTENDEE: &str = "captions-attendee@example.com";
const GUEST: &str = "guest:captions-guest";

fn token(room: &str, user: &str) -> String {
    let guest = user.starts_with("guest:");
    generate_room_token(TEST_JWT_SECRET, 600, user, room, false, user, true, guest).unwrap()
}

fn enable() -> Value {
    json!({ "enabled": true })
}

fn renew() -> Value {
    json!({ "enabled": true, "renew": true })
}

fn revoke() -> Value {
    json!({ "enabled": false })
}

fn status(code: StatusCode, api_code: &str) -> (StatusCode, String) {
    (code, api_code.to_string())
}

async fn post_raw(
    state: &AppState,
    room: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::post(format!("/api/v1/meetings/{room}/captions"))
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        req = req.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    send(state, req.body(Body::from(body.to_string())).unwrap()).await
}

/// Status and `result.code` (empty on 204) of one captions request.
async fn post(state: &AppState, room: &str, user: &str, body: Value) -> (StatusCode, String) {
    let (status, json) = post_raw(state, room, Some(&token(room, user)), body).await;
    (
        status,
        json["result"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

fn enabled_state(pool: sqlx::PgPool, nats: Option<async_nats::Client>) -> AppState {
    AppState {
        captions_enabled: true,
        ..build_state(pool, None, nats)
    }
}

async fn set_row(state: &AppState, room: &str, user: &str, status: &str, host: bool) {
    sqlx::query(
        "INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, admitted_at) \
         SELECT id, $2, $3, $4, $5, NOW() FROM meetings WHERE room_id = $1 \
         ON CONFLICT (meeting_id, user_id) DO UPDATE SET status = EXCLUDED.status, is_host = EXCLUDED.is_host",
    )
    .bind(room)
    .bind(user)
    .bind(status)
    .bind(host)
    .bind(user.starts_with("guest:"))
    .execute(&state.db)
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn flag_off_answers_like_console_logs_not_found_before_auth() {
    let pool = get_test_pool().await;
    let room = "captions-disabled";
    let state = build_state(pool.clone(), None, None);
    create_meeting(&state, room, json!({})).await;
    let host = token(room, HOST);

    let console = Request::post(format!("/api/v1/meetings/{room}/console-logs"))
        .header(AUTHORIZATION, format!("Bearer {host}"))
        .body(Body::empty())
        .unwrap();
    let console = send(&state, console).await;
    assert_eq!(console.0, StatusCode::NOT_FOUND);
    assert_eq!(post_raw(&state, room, Some(&host), enable()).await, console);
    assert_eq!(post_raw(&state, room, None, enable()).await, console);
    let enabled = enabled_state(pool, None);
    assert_eq!(
        post_raw(&enabled, room, None, enable()).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
#[serial]
async fn only_admitted_hosts_of_the_tokens_room_may_change_captions() {
    let state = enabled_state(get_test_pool().await, None);
    let (room, other) = ("captions-authz", "captions-authz-other");
    create_meeting(&state, other, json!({})).await;
    create_meeting(&state, room, json!({ "waiting_room_enabled": false })).await;
    join(&state, room, ATTENDEE).await;
    set_row(&state, room, GUEST, "admitted", false).await;
    let not_host = status(StatusCode::FORBIDDEN, "NOT_HOST");

    let wrong_room = post_raw(&state, room, Some(&token(other, HOST)), enable()).await;
    assert_eq!(wrong_room.0, StatusCode::FORBIDDEN);
    assert_eq!(wrong_room.1["result"]["code"], "FORBIDDEN");
    let observer = generate_observer_token(TEST_JWT_SECRET, HOST, room, HOST, false).unwrap();
    assert_eq!(
        post_raw(&state, room, Some(&observer), enable()).await.0,
        StatusCode::UNAUTHORIZED
    );
    for user in ["captions-outsider@example.com", ATTENDEE, GUEST] {
        for body in [enable(), revoke()] {
            assert_eq!(post(&state, room, user, body).await, not_host, "{user}");
        }
    }
    for row in ["left", "kicked", "waiting", "rejected"] {
        let user = format!("captions-{row}@example.com");
        set_row(&state, room, &user, row, true).await;
        for body in [enable(), renew(), revoke()] {
            assert_eq!(post(&state, room, &user, body).await, not_host, "{row}");
        }
    }
    assert_eq!(captions_state(&state, room).await, (false, 0));
}

#[tokio::test]
#[serial]
async fn demoted_hosts_with_valid_tokens_are_rejected() {
    let state = enabled_state(get_test_pool().await, None);
    let room = "captions-demoted";
    let opts = json!({ "co_hosts": [CO_HOST], "waiting_room_enabled": false });
    create_meeting(&state, room, opts).await;
    join(&state, room, CO_HOST).await;
    join(&state, room, ATTENDEE).await;
    let not_host = status(StatusCode::FORBIDDEN, "NOT_HOST");
    let uri = format!("/api/v1/meetings/{room}");

    let revoke_co_host = json!({ "user_id": CO_HOST });
    as_user(
        &state,
        "POST",
        &format!("{uri}/co-hosts/revoke"),
        HOST,
        revoke_co_host,
    )
    .await;
    assert_eq!(post(&state, room, CO_HOST, enable()).await, not_host);

    let transfer = json!({ "user_id": ATTENDEE });
    as_user(
        &state,
        "POST",
        &format!("{uri}/transfer-host"),
        HOST,
        transfer,
    )
    .await;
    assert_eq!(post(&state, room, HOST, revoke()).await, not_host);
}

#[tokio::test]
#[serial]
async fn ended_missing_malformed_and_rate_limited_requests_are_refused() {
    let state = enabled_state(get_test_pool().await, None);
    let room = "captions-refused";
    create_meeting(&state, room, json!({})).await;
    assert_eq!(
        post(
            &state,
            room,
            HOST,
            json!({ "enabled": false, "renew": true })
        )
        .await,
        status(StatusCode::BAD_REQUEST, "BAD_REQUEST")
    );
    assert_eq!(
        post(&state, room, HOST, enable()).await,
        status(StatusCode::SERVICE_UNAVAILABLE, "CAPTIONS_UNAVAILABLE")
    );
    assert_eq!(captions_state(&state, room).await, (false, 0));

    as_user(
        &state,
        "POST",
        &format!("/api/v1/meetings/{room}/end"),
        HOST,
        Value::Null,
    )
    .await;
    let gone = status(StatusCode::NOT_FOUND, "MEETING_NOT_FOUND");
    for body in [enable(), renew(), revoke()] {
        assert_eq!(post(&state, room, HOST, body).await, gone);
    }

    let missing = "captions-no-such-meeting";
    cleanup_test_data(&state.db, missing).await;
    for _ in 0..10 {
        assert_eq!(post(&state, missing, HOST, renew()).await, gone);
    }
    assert_eq!(
        post(&state, missing, HOST, renew()).await,
        status(StatusCode::TOO_MANY_REQUESTS, "RATE_LIMIT_EXCEEDED")
    );
    assert_eq!(
        post(&state, missing, HOST, revoke()).await,
        gone,
        "turning captions off is not throttled"
    );
    assert_eq!(post(&state, missing, CO_HOST, renew()).await, gone);
    assert_eq!(post(&state, room, HOST, renew()).await, gone);
}

#[tokio::test]
#[serial]
async fn captions_turn_on_and_renew_only_while_the_meeting_is_active() {
    let nats = maybe_nats().await.expect("NATS_URL must be set");
    let state = enabled_state(get_test_pool().await, Some(nats.clone()));
    let room = "captions-not-active";
    let mut sub = create_subscribed(&state, &nats, room, json!({})).await;
    assert_eq!(
        post(&state, room, HOST, enable()).await,
        status(StatusCode::NO_CONTENT, "")
    );
    assert_eq!(next_payload(&mut sub).await, lease(room, HOST, 1));

    sqlx::query("UPDATE meetings SET state = 'idle' WHERE room_id = $1")
        .bind(room)
        .execute(&state.db)
        .await
        .unwrap();
    assert_eq!(
        post(&state, room, HOST, renew()).await,
        status(StatusCode::CONFLICT, "CAPTIONS_OFF")
    );
    assert_eq!(
        post(&state, room, HOST, revoke()).await,
        status(StatusCode::NO_CONTENT, "")
    );
    assert_eq!(next_payload(&mut sub).await, revoked(room, HOST, 2));
    assert_eq!(
        post(&state, room, HOST, enable()).await,
        status(StatusCode::CONFLICT, "MEETING_NOT_ACTIVE")
    );
    assert_nothing_published(&mut sub).await;
    assert_eq!(captions_state(&state, room).await, (false, 2));
}

#[tokio::test]
#[serial]
async fn hosts_enable_renew_and_revoke_with_the_current_generation() {
    let nats = maybe_nats().await.expect("NATS_URL must be set");
    let state = enabled_state(get_test_pool().await, Some(nats.clone()));
    let room = "captions-generation";
    let mut sub = create_subscribed(&state, &nats, room, json!({ "co_hosts": [CO_HOST] })).await;
    join(&state, room, CO_HOST).await;
    let ok = status(StatusCode::NO_CONTENT, "");

    for (user, body, expected) in [
        (CO_HOST, enable(), lease(room, CO_HOST, 1)),
        (HOST, enable(), lease(room, HOST, 1)),
        (HOST, renew(), lease(room, HOST, 1)),
        (CO_HOST, revoke(), revoked(room, CO_HOST, 2)),
    ] {
        assert_eq!(
            post(&state, room, user, body.clone()).await,
            ok,
            "{user} {body}"
        );
        assert_eq!(next_payload(&mut sub).await, expected, "{user} {body}");
    }
    assert_eq!(captions_state(&state, room).await, (false, 2));

    assert_eq!(
        post(&state, room, HOST, renew()).await,
        status(StatusCode::CONFLICT, "CAPTIONS_OFF")
    );
    assert_nothing_published(&mut sub).await;
    assert_eq!(captions_state(&state, room).await, (false, 2));

    assert_eq!(post(&state, room, HOST, enable()).await, ok);
    assert_eq!(next_payload(&mut sub).await, lease(room, HOST, 3));
    assert_eq!(captions_state(&state, room).await, (true, 3));
}

#[tokio::test]
#[serial]
async fn revoking_captions_already_off_still_bumps_and_publishes() {
    let nats = maybe_nats().await.expect("NATS_URL must be set");
    let state = enabled_state(get_test_pool().await, Some(nats.clone()));
    let room = "captions-revoke-off";
    let mut sub = create_subscribed(&state, &nats, room, json!({})).await;
    let ok = status(StatusCode::NO_CONTENT, "");

    // The bridge drops a revoke at or below its lease generation, so every
    // revoke must publish a generation above any lease, even with captions off.
    for generation in [1, 2] {
        assert_eq!(post(&state, room, HOST, revoke()).await, ok);
        assert_eq!(
            next_payload(&mut sub).await,
            revoked(room, HOST, generation)
        );
        assert_eq!(captions_state(&state, room).await, (false, generation));
    }
}

#[tokio::test]
#[serial]
async fn a_demotion_in_flight_wins_over_the_hosts_request() {
    let nats = maybe_nats().await.expect("NATS_URL must be set");
    let state = enabled_state(get_test_pool().await, Some(nats.clone()));
    let room = "captions-demotion-race";
    let mut sub = create_subscribed(&state, &nats, room, json!({})).await;
    let pk = meeting_pk(&state, room).await;
    set_row(&state, room, ATTENDEE, "admitted", false).await;

    // The production demotion (transfer_host: meeting row first, then the
    // participant rows) and the captions request start while a third
    // transaction holds the meeting row, so both queue on it in that order.
    let mut gate = state.db.begin().await.unwrap();
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(pk)
        .execute(&mut *gate)
        .await
        .unwrap();
    let transfer = tokio::spawn({
        let db = state.db.clone();
        async move { db_participants::transfer_host(&db, pk, HOST, ATTENDEE).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let request = tokio::spawn({
        let state = state.clone();
        async move { post(&state, room, HOST, enable()).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(!transfer.is_finished() && !request.is_finished());
    gate.commit().await.unwrap();

    let promoted = transfer
        .await
        .unwrap()
        .expect("the transfer must not deadlock with the captions request");
    assert_eq!(promoted.map(|p| p.user_id).as_deref(), Some(ATTENDEE));
    assert_eq!(
        request.await.unwrap(),
        status(StatusCode::FORBIDDEN, "NOT_HOST")
    );
    assert_nothing_published(&mut sub).await;
    assert_eq!(captions_state(&state, room).await, (false, 0));
}

#[tokio::test]
#[serial]
async fn a_non_hosts_revoke_is_refused_without_waiting_for_the_meeting_lock() {
    let state = enabled_state(get_test_pool().await, None);
    let room = "captions-revoke-lock";
    create_meeting(&state, room, json!({})).await;
    let pk = meeting_pk(&state, room).await;
    set_row(&state, room, ATTENDEE, "admitted", false).await;

    let mut gate = state.db.begin().await.unwrap();
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(pk)
        .execute(&mut *gate)
        .await
        .unwrap();
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        post(&state, room, ATTENDEE, revoke()),
    )
    .await
    .expect("a non-host's revoke must not wait for the meeting row lock");
    assert_eq!(answer, status(StatusCode::FORBIDDEN, "NOT_HOST"));
    gate.rollback().await.unwrap();
    assert_eq!(captions_state(&state, room).await, (false, 0));
}

#[tokio::test]
#[serial]
async fn oversized_bodies_and_other_methods_are_refused() {
    let state = enabled_state(get_test_pool().await, None);
    let room = "captions-oversized";
    create_meeting(&state, room, json!({})).await;
    let host = token(room, HOST);

    let padded = json!({ "enabled": true, "pad": "x".repeat(300) });
    assert_eq!(
        post_raw(&state, room, Some(&host), padded).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(captions_state(&state, room).await, (false, 0));

    let get = Request::get(format!("/api/v1/meetings/{room}/captions"))
        .header(AUTHORIZATION, format!("Bearer {host}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&state, get).await.0, StatusCode::METHOD_NOT_ALLOWED);
}
