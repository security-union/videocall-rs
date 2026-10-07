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

//! Recording lease register and stop (#2856). Requires `DATABASE_URL`; the
//! broadcast test also requires `NATS_URL`.

mod test_helpers;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use futures::StreamExt;
use meeting_api::state::AppState;
use protobuf::Message;
use serde_json::{json, Value};
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use uuid::Uuid;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::PacketWrapper;

const HOST: &str = "rec-host@example.com";

fn member(i: usize) -> String {
    format!("rec-member-{i}@example.com")
}

/// An active meeting owned by `HOST` with `HOST` and `members` admitted.
async fn meeting(pool: &PgPool, room: &str, allowed_for_all: bool, members: usize) -> i32 {
    cleanup_test_data(pool, room).await;
    let id: i32 = sqlx::query_scalar(
        "INSERT INTO meetings (room_id, creator_id, started_at, state, recording_allowed_for_all) \
         VALUES ($1, $2, NOW(), 'active', $3) RETURNING id",
    )
    .bind(room)
    .bind(HOST)
    .bind(allowed_for_all)
    .fetch_one(pool)
    .await
    .unwrap();
    add_participant(pool, id, HOST, true, false).await;
    for i in 0..members {
        add_participant(pool, id, &member(i), false, false).await;
    }
    id
}

async fn add_participant(pool: &PgPool, meeting_id: i32, user: &str, host: bool, guest: bool) {
    sqlx::query(
        "INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, admitted_at) \
         VALUES ($1, $2, 'admitted', $3, $4, NOW())",
    )
    .bind(meeting_id)
    .bind(user)
    .bind(host)
    .bind(guest)
    .execute(pool)
    .await
    .unwrap();
}

fn register_request(room: &str, user: &str, attempt: Uuid) -> Request<Body> {
    request_with_cookie("POST", &format!("/api/v1/meetings/{room}/recordings"), user)
        .header("Origin", TEST_ORIGIN)
        .header("Content-Type", "application/json")
        .body(Body::from(json!({ "attempt_id": attempt }).to_string()))
        .unwrap()
}

fn stop_request(room: &str, recording_id: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(format!(
            "/api/v1/meetings/{room}/recordings/{recording_id}/stop"
        ))
        .header("Content-Type", "text/plain")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    if status == StatusCode::NO_CONTENT {
        return (status, Value::Null);
    }
    (status, response_json(resp).await)
}

async fn register(app: &Router, room: &str, user: &str, attempt: Uuid) -> (StatusCode, Value) {
    call(app, register_request(room, user, attempt)).await
}

fn code(body: &Value) -> &str {
    body["result"]["code"].as_str().unwrap_or_default()
}

fn rid(body: &Value) -> String {
    body["result"]["recording_id"].as_str().unwrap().to_string()
}

fn secret(body: &Value) -> String {
    body["result"]["lease_secret"].as_str().unwrap().to_string()
}

async fn leases(pool: &PgPool, meeting_id: i32) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM meeting_recording_leases WHERE meeting_id = $1")
        .bind(meeting_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn version(pool: &PgPool, meeting_id: i32) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE((SELECT version FROM meeting_recording_state WHERE meeting_id = $1), 0)",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// A connection holding `meeting_id`'s `FOR UPDATE` lock until committed.
async fn hold_meeting_lock(
    pool: &PgPool,
    meeting_id: i32,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(meeting_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

#[tokio::test]
async fn register_then_stop_with_the_secret_alone() {
    let pool = get_test_pool().await;
    let room = "rec-basic";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());

    let resp = app
        .clone()
        .oneshot(register_request(room, &member(0), Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["version"], 1);
    assert_eq!(secret(&body).len(), 43);

    let (status, _) = call(&app, stop_request(room, &rid(&body), &secret(&body))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(leases(&pool, id).await, 0);
    assert_eq!(version(&pool, id).await, 2);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn settings_off_committed_while_register_waits_is_enforced() {
    let pool = get_test_pool().await;
    let room = "rec-settings-race";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());

    let mut settings = pool.begin().await.unwrap();
    sqlx::query("UPDATE meetings SET recording_allowed_for_all = FALSE WHERE id = $1")
        .bind(id)
        .execute(&mut *settings)
        .await
        .unwrap();
    let pending = tokio::spawn({
        let app = app.clone();
        async move { register(&app, room, &member(0), Uuid::new_v4()).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !pending.is_finished(),
        "register must wait for the meeting lock"
    );
    settings.commit().await.unwrap();

    let (status, body) = tokio::time::timeout(Duration::from_secs(10), pending)
        .await
        .expect("register finished")
        .unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(code(&body), "NOT_PERMITTED");
    assert_eq!(leases(&pool, id).await, 0);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn concurrent_registers_by_one_user_grant_one_lease() {
    let pool = get_test_pool().await;
    let room = "rec-double-register";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());

    let lock = hold_meeting_lock(&pool, id).await;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let (app, barrier) = (app.clone(), barrier.clone());
            tokio::spawn(async move {
                barrier.wait().await;
                register(&app, room, &member(0), Uuid::new_v4()).await
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(racers.iter().all(|r| !r.is_finished()));
    lock.commit().await.unwrap();

    let mut statuses = Vec::new();
    for racer in racers {
        let (status, _) = tokio::time::timeout(Duration::from_secs(10), racer)
            .await
            .expect("no deadlock")
            .unwrap();
        statuses.push(status);
    }
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    assert_eq!(leases(&pool, id).await, 1);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn guest_room_token_is_rejected() {
    let pool = get_test_pool().await;
    let room = "rec-guest";
    let id = meeting(&pool, room, true, 0).await;
    let guest = "guest:rec-guest-1";
    add_participant(&pool, id, guest, false, true).await;
    let token = meeting_api::token::generate_room_token(
        TEST_JWT_SECRET,
        600,
        guest,
        room,
        false,
        "Guest",
        true,
        true,
    )
    .unwrap();
    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room}/recordings"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .body(Body::from(
            json!({ "attempt_id": Uuid::new_v4() }).to_string(),
        ))
        .unwrap();
    let (status, _) = call(&build_app(pool.clone()), req).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(leases(&pool, id).await, 0);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn policy_admits_hosts_and_refuses_absent_callers_and_ended_meetings() {
    let pool = get_test_pool().await;
    let room = "rec-policy";
    let id = meeting(&pool, room, false, 3).await;
    let app = build_app(pool.clone());

    assert_eq!(
        register(&app, room, HOST, Uuid::new_v4()).await.0,
        StatusCode::OK,
        "hosts record with allow-for-all off"
    );
    let (_, body) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(code(&body), "NOT_PERMITTED");
    let counted: i32 = sqlx::query_scalar(
        "SELECT reg_window_count FROM meeting_recording_state WHERE meeting_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counted, 1, "only the host's attempt counts");

    sqlx::query("UPDATE meetings SET recording_allowed_for_all = TRUE WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    for (user, set) in [
        (member(1), "status = 'waiting'"),
        (member(2), "status = 'left', left_at = NOW()"),
    ] {
        sqlx::query(&format!(
            "UPDATE meeting_participants SET {set} WHERE meeting_id = $1 AND user_id = $2"
        ))
        .bind(id)
        .bind(&user)
        .execute(&pool)
        .await
        .unwrap();
        let (_, body) = register(&app, room, &user, Uuid::new_v4()).await;
        assert_eq!(code(&body), "NOT_ADMITTED", "{user}");
    }

    sqlx::query("UPDATE meetings SET state = 'ended' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code(&body), "MEETING_ENDED");
    let counted: i32 = sqlx::query_scalar(
        "SELECT reg_window_count FROM meeting_recording_state WHERE meeting_id = $1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(counted, 1, "refusals by policy never count");
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn register_is_limited_per_user_per_replica() {
    let pool = get_test_pool().await;
    let room = "rec-user-limit";
    meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());
    for attempt in 0..6 {
        let (status, _) = register(&app, room, &member(0), Uuid::new_v4()).await;
        assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "attempt {attempt}");
    }
    let (status, body) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code(&body), "RATE_LIMITED");
    let other_replica = build_app(pool.clone());
    let (status, _) = register(&other_replica, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::CONFLICT, "the limit is per replica");
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn kick_committed_while_register_waits_is_enforced() {
    let pool = get_test_pool().await;
    let room = "rec-kick-race";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());

    let mut kick = hold_meeting_lock(&pool, id).await;
    let pending = tokio::spawn({
        let app = app.clone();
        async move { register(&app, room, &member(0), Uuid::new_v4()).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !pending.is_finished(),
        "an admitted caller waits for the meeting lock"
    );
    sqlx::query(
        "UPDATE meeting_participants SET status = 'kicked' WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(member(0))
    .execute(&mut *kick)
    .await
    .unwrap();
    kick.commit().await.unwrap();

    let (_, body) = pending.await.unwrap();
    assert_eq!(code(&body), "NOT_ADMITTED");
    assert_eq!(leases(&pool, id).await, 0);
    let counted: Option<i32> = sqlx::query_scalar(
        "SELECT reg_window_count FROM meeting_recording_state WHERE meeting_id = $1",
    )
    .bind(id)
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(counted, None);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn lapsed_leases_are_purged_at_register() {
    let pool = get_test_pool().await;
    let room = "rec-purge";
    let id = meeting(&pool, room, true, 2).await;
    let app = build_app(pool.clone());

    register(&app, room, &member(0), Uuid::new_v4()).await;
    sqlx::query(
        "UPDATE meeting_recording_leases SET renewed_at = NOW() - INTERVAL '91 seconds' \
         WHERE meeting_id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let other_room = "rec-purge-other";
    let other = meeting(&pool, other_room, true, 1).await;
    register(&app, other_room, &member(0), Uuid::new_v4()).await;
    sqlx::query(
        "UPDATE meeting_recording_leases SET renewed_at = NOW() - INTERVAL '91 seconds' \
         WHERE meeting_id = $1",
    )
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    let (status, body) = register(&app, room, &member(1), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["result"]["version"], 2);
    assert_eq!(
        leases(&pool, other).await,
        1,
        "only the locked meeting is purged"
    );
    let holders: Vec<String> =
        sqlx::query_scalar("SELECT user_id FROM meeting_recording_leases WHERE meeting_id = $1")
            .bind(id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(holders, [member(1)]);

    sqlx::query(
        "UPDATE meeting_recording_leases SET renewed_at = NOW() - INTERVAL '91 seconds' \
         WHERE meeting_id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE meeting_recording_state SET reg_window_count = 20 WHERE meeting_id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let (status, _) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(leases(&pool, id).await, 0);
    assert_eq!(
        version(&pool, id).await,
        3,
        "a refusal that purged still bumps"
    );
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn retried_attempt_reissues_the_secret_without_a_bump() {
    let pool = get_test_pool().await;
    let room = "rec-replay";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());
    let attempt = Uuid::new_v4();

    let (_, first) = register(&app, room, &member(0), attempt).await;
    let (status, second) = register(&app, room, &member(0), attempt).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rid(&second), rid(&first));
    assert_ne!(secret(&second), secret(&first));
    assert_eq!(second["result"]["version"], first["result"]["version"]);

    call(&app, stop_request(room, &rid(&first), &secret(&first))).await;
    assert_eq!(leases(&pool, id).await, 1, "the replaced secret is dead");

    let (status, body) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "USER_CAP");

    call(&app, stop_request(room, &rid(&second), &secret(&second))).await;
    assert_eq!(leases(&pool, id).await, 0);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn expired_own_lease_is_replaced_at_register() {
    let pool = get_test_pool().await;
    let room = "rec-expired";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());

    let (_, first) = register(&app, room, &member(0), Uuid::new_v4()).await;
    sqlx::query(
        "UPDATE meeting_recording_leases SET renewed_at = NOW() - INTERVAL '91 seconds' \
         WHERE meeting_id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    let (status, second) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_ne!(rid(&second), rid(&first));
    assert_eq!(second["result"]["version"], 2);
    assert_eq!(leases(&pool, id).await, 1);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn stop_without_the_secret_changes_nothing() {
    let pool = get_test_pool().await;
    let room = "rec-stop-guard";
    let other_room = "rec-stop-guard-other";
    let id = meeting(&pool, room, true, 2).await;
    meeting(&pool, other_room, true, 0).await;
    let app = build_app(pool.clone());
    let (_, body) = register(&app, room, &member(0), Uuid::new_v4()).await;
    let (rid, secret) = (rid(&body), secret(&body));

    let lock = hold_meeting_lock(&pool, id).await;
    let misses = [
        stop_request(room, &rid, ""),
        stop_request(room, &rid, "not-the-secret-not-the-secret-not-the-secr"),
        stop_request(other_room, &rid, &secret),
        stop_request(room, &Uuid::new_v4().to_string(), &secret),
        request_with_cookie(
            "POST",
            &format!("/api/v1/meetings/{room}/recordings/{rid}/stop"),
            &member(1),
        )
        .header("Origin", TEST_ORIGIN)
        .body(Body::empty())
        .unwrap(),
    ];
    for req in misses {
        let (status, _) = tokio::time::timeout(Duration::from_secs(2), call(&app, req))
            .await
            .expect("a miss must not wait for the meeting lock");
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    lock.commit().await.unwrap();
    assert_eq!(leases(&pool, id).await, 1);
    assert_eq!(version(&pool, id).await, 1);
    cleanup_test_data(&pool, room).await;
    cleanup_test_data(&pool, other_room).await;
}

#[tokio::test]
async fn stop_rechecks_the_secret_under_the_meeting_lock() {
    let pool = get_test_pool().await;
    let room = "rec-stop-recheck";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());
    let (_, body) = register(&app, room, &member(0), Uuid::new_v4()).await;

    let mut rotate = hold_meeting_lock(&pool, id).await;
    let pending = tokio::spawn({
        let (app, req) = (app.clone(), stop_request(room, &rid(&body), &secret(&body)));
        async move { call(&app, req).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !pending.is_finished(),
        "a matching stop waits for the meeting lock"
    );
    sqlx::query("UPDATE meeting_recording_leases SET secret_hash = $2 WHERE meeting_id = $1")
        .bind(id)
        .bind(meeting_api::recording::LeaseSecret::generate().hash())
        .execute(&mut *rotate)
        .await
        .unwrap();
    rotate.commit().await.unwrap();

    assert_eq!(pending.await.unwrap().0, StatusCode::NO_CONTENT);
    assert_eq!(leases(&pool, id).await, 1);
    assert_eq!(version(&pool, id).await, 1);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn stop_is_rate_limited_per_client_address() {
    let pool = get_test_pool().await;
    let room = "rec-stop-limit";
    let id = meeting(&pool, room, true, 1).await;
    let app = build_app(pool.clone());
    let (_, body) = register(&app, room, &member(0), Uuid::new_v4()).await;

    let from_one_address = |secret: &str| {
        let mut req = stop_request(room, &rid(&body), secret);
        req.headers_mut()
            .insert("x-forwarded-for", "203.0.113.9".parse().unwrap());
        req
    };
    for _ in 0..30 {
        call(&app, from_one_address("wrong-secret-wrong-secret")).await;
    }
    assert_eq!(
        call(&app, from_one_address(&secret(&body))).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        leases(&pool, id).await,
        1,
        "31st stop from one address is ignored"
    );
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn origin_is_required_whenever_a_session_cookie_is_sent() {
    let pool = get_test_pool().await;
    let room = "rec-origin";
    let id = meeting(&pool, room, true, 3).await;
    let app = build_app(pool.clone());
    let uri = format!("/api/v1/meetings/{room}/recordings");
    let body = || Body::from(json!({ "attempt_id": Uuid::new_v4() }).to_string());
    let bearer = |user: &str| {
        let token = meeting_api::token::generate_session_token(
            TEST_JWT_SECRET,
            user,
            user,
            600,
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
        format!("Bearer {token}")
    };

    for origin in [None, Some("https://evil.test")] {
        let mut req = request_with_cookie("POST", &uri, &member(0))
            .header("Content-Type", "application/json");
        if let Some(origin) = origin {
            req = req.header("Origin", origin);
        }
        let (status, resp) = call(&app, req.body(body()).unwrap()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin:?}");
        assert_eq!(code(&resp), "BAD_ORIGIN");
    }

    let unverifiable_cookie = Request::builder()
        .method("POST")
        .uri(&uri)
        .header("Cookie", "session=not-a-jwt")
        .header("Authorization", bearer(&member(1)))
        .header("Content-Type", "application/json")
        .body(body())
        .unwrap();
    let (status, resp) = call(&app, unverifiable_cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(code(&resp), "BAD_ORIGIN");
    assert_eq!(leases(&pool, id).await, 0);

    let (status, _) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::OK, "cookie with the allowed origin");
    let bearer_only = Request::builder()
        .method("POST")
        .uri(&uri)
        .header("Authorization", bearer(&member(2)))
        .header("Content-Type", "application/json")
        .body(body())
        .unwrap();
    assert_eq!(call(&app, bearer_only).await.0, StatusCode::OK);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn meeting_cap_counts_only_active_non_host_leases() {
    let pool = get_test_pool().await;
    let room = "rec-cap";
    let id = meeting(&pool, room, true, 7).await;
    let app = build_app(pool.clone());

    for i in 0..5 {
        assert_eq!(
            register(&app, room, &member(i), Uuid::new_v4()).await.0,
            StatusCode::OK
        );
    }
    let (status, body) = register(&app, room, &member(5), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(code(&body), "MEETING_CAP");
    assert_eq!(
        register(&app, room, HOST, Uuid::new_v4()).await.0,
        StatusCode::OK,
        "hosts are exempt"
    );

    sqlx::query(
        "UPDATE meeting_recording_leases SET revoked_at = NOW() \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(member(0))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        register(&app, room, &member(5), Uuid::new_v4()).await.0,
        StatusCode::OK
    );
    sqlx::query(
        "UPDATE meeting_recording_leases SET renewed_at = NOW() - INTERVAL '91 seconds' \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(member(1))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        register(&app, room, &member(6), Uuid::new_v4()).await.0,
        StatusCode::OK
    );
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn churn_cap_spans_replicas_and_counts_refusals() {
    let pool = get_test_pool().await;
    let room = "rec-churn";
    let id = meeting(&pool, room, true, 7).await;
    let replicas = [
        build_app_from_state(build_state(pool.clone(), None, None)),
        build_app_from_state(build_state(pool.clone(), None, None)),
    ];

    let kicked = "rec-kicked@example.com";
    add_participant(&pool, id, kicked, false, false).await;
    sqlx::query("UPDATE meeting_participants SET status = 'kicked' WHERE user_id = $1")
        .bind(kicked)
        .execute(&pool)
        .await
        .unwrap();
    let lock = hold_meeting_lock(&pool, id).await;
    for (replica, outsider) in [(0, "rec-outsider@example.com"), (1, kicked)] {
        for _ in 0..5 {
            let (status, body) = tokio::time::timeout(
                Duration::from_secs(2),
                register(&replicas[replica], room, outsider, Uuid::new_v4()),
            )
            .await
            .expect("a caller who is not admitted never waits for the meeting lock");
            assert_eq!(code(&body), "NOT_ADMITTED", "{status}");
        }
    }
    lock.commit().await.unwrap();
    let mut statuses = Vec::new();
    for attempt in 0..20 {
        let (status, _) = register(
            &replicas[attempt % 2],
            room,
            &member(attempt % 7),
            Uuid::new_v4(),
        )
        .await;
        statuses.push(status);
    }
    assert!(
        !statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );
    assert_eq!(statuses.iter().filter(|s| **s == StatusCode::OK).count(), 5);

    let (status, body) = register(&replicas[0], room, &member(6), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(code(&body), "RATE_LIMITED");
    assert_eq!(
        register(&replicas[1], room, HOST, Uuid::new_v4()).await.0,
        StatusCode::OK,
        "hosts are exempt"
    );
    assert_eq!(leases(&pool, id).await, 6);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn churn_window_slides_across_the_boundary() {
    let pool = get_test_pool().await;
    let room = "rec-churn-window";
    let id = meeting(&pool, room, true, 4).await;
    let app = build_app(pool.clone());
    let set_window = |age_secs: f64, count: i32, prev: i32| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO meeting_recording_state \
                     (meeting_id, version, changed_at, reg_window_start, reg_window_count, reg_prev_count) \
                 VALUES ($1, 0, NOW(), clock_timestamp() - make_interval(secs => $2), $3, $4) \
                 ON CONFLICT (meeting_id) DO UPDATE SET reg_window_start = EXCLUDED.reg_window_start, \
                     reg_window_count = EXCLUDED.reg_window_count, reg_prev_count = EXCLUDED.reg_prev_count",
            )
            .bind(id)
            .bind(age_secs)
            .bind(count)
            .bind(prev)
            .execute(&pool)
            .await
            .unwrap();
        }
    };

    set_window(61.0, 20, 0).await;
    let (status, _) = register(&app, room, &member(0), Uuid::new_v4()).await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "a full window just closed"
    );

    set_window(121.0, 20, 20).await;
    let (status, _) = register(&app, room, &member(1), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::OK, "both buckets lapsed");

    set_window(30.0, 9, 20).await;
    let (status, _) = register(&app, room, &member(2), Uuid::new_v4()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "half the previous window still weighs"
    );
    set_window(30.0, 10, 20).await;
    let (status, _) = register(&app, room, &member(3), Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    set_window(-30.0, 9, 10).await;
    let (status, _) = register(&app, room, &member(3), Uuid::new_v4()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a window that starts ahead of the clock weighs at most 1"
    );
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
async fn broadcast_snapshot_carries_no_identity_or_secret() {
    let Some(nats) = maybe_nats().await else {
        panic!("NATS_URL must be set");
    };
    let pool = get_test_pool().await;
    let room = "rec-broadcast";
    meeting(&pool, room, true, 1).await;
    let app = build_app_from_state(AppState {
        nats: Some(nats.clone()),
        ..build_state(pool.clone(), None, None)
    });
    let mut sub = nats.subscribe(format!("room.{room}.system")).await.unwrap();
    nats.flush().await.unwrap();

    let attempt = Uuid::new_v4();
    let (_, body) = register(&app, room, &member(0), attempt).await;
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("snapshot published")
        .unwrap();
    let raw = msg.payload.to_vec();
    for needle in [
        member(0).into_bytes(),
        secret(&body).into_bytes(),
        attempt.to_string().into_bytes(),
        attempt.as_bytes().to_vec(),
    ] {
        assert!(
            !raw.windows(needle.len()).any(|w| w == needle.as_slice()),
            "{needle:?}"
        );
    }

    let wrapper = PacketWrapper::parse_from_bytes(&raw).unwrap();
    let packet = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
    assert_eq!(
        packet.event_type.enum_value(),
        Ok(MeetingEventType::RECORDING_STATE)
    );
    assert!(packet.recording_epoch > 0);
    let state = packet.recording_state.unwrap();
    assert_eq!(state.version, 1);
    assert_eq!(state.entries.len(), 1);
    assert_eq!(
        state.entries[0].recording_id,
        Uuid::parse_str(&rid(&body)).unwrap().as_bytes().to_vec()
    );

    call(&app, stop_request(room, &rid(&body), &secret(&body))).await;
    let msg = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("stop published")
        .unwrap();
    let wrapper = PacketWrapper::parse_from_bytes(&msg.payload).unwrap();
    let state = MeetingPacket::parse_from_bytes(&wrapper.data)
        .unwrap()
        .recording_state
        .unwrap();
    assert_eq!((state.version, state.entries.len()), (2, 0));
    cleanup_test_data(&pool, room).await;
}
