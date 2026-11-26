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

//! Device-JWT authentication at meeting-api: token verification and what a
//! CLI device may do on the meeting endpoints.
//!
//! Devices are inserted with `db::devices::insert_approved_device` (the
//! browser sign-in flow is covered in `device_pages_tests.rs`); tokens are
//! signed here exactly as the CLI signs them. Run against Postgres or SQLite via `DATABASE_URL`.

mod test_helpers;

use axum::body::Body;
use axum::http::{self, Request, StatusCode};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use meeting_api::db::{self, meetings as db_meetings, DbPool};
use meeting_api::secrets::new_id;
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::Value;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::device::{fingerprint, DeviceTokenClaims, CLI_ISSUER};
use videocall_meeting_types::RoomAccessTokenClaims;

const OTHER_AUDIENCE: &str = "http://localhost:8082";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct TestDevice {
    id: String,
    key: SigningKey,
    user_id: String,
}

#[derive(Clone, Copy)]
enum Fixture {
    Approved,
    Revoked,
    IdleExpired,
    AbsExpired,
    /// Approved, last used two hours ago (eligible for a touch).
    ApprovedStale,
}

fn random_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    OsRng.fill_bytes(&mut seed);
    seed
}

fn unique_user() -> String {
    format!("dev-{}@test.local", new_id().to_lowercase())
}

async fn insert_device(pool: &DbPool, fixture: Fixture, user_id: &str) -> TestDevice {
    let key = SigningKey::from_bytes(&random_seed());
    let pk = key.verifying_key().to_bytes();
    let id = new_id();
    let now = Utc::now();
    let day = Duration::days(1);
    // (last_used_at, idle_expires_at, abs_expires_at)
    let (last_used, idle, abs) = match fixture {
        Fixture::Approved | Fixture::Revoked => (now, now + day * 90, now + day * 365),
        Fixture::ApprovedStale => (
            now - Duration::hours(2),
            now - Duration::hours(2) + day * 90,
            now + day * 365,
        ),
        Fixture::IdleExpired => (now - day * 91, now - day, now + day * 200),
        // idle <= abs is a schema CHECK, so an absolutely expired device is
        // idle-expired too.
        Fixture::AbsExpired => (
            now - Duration::hours(2),
            now - Duration::minutes(1),
            now - Duration::minutes(1),
        ),
    };
    let public_key = URL_SAFE_NO_PAD.encode(pk);
    let fingerprint = fingerprint(&pk);
    let user_name = format!("Name of {user_id}");
    db::devices::insert_approved_device(
        pool,
        &db::devices::NewDevice {
            id: &id,
            public_key: &public_key,
            fingerprint: &fingerprint,
            name: "test device",
            user_id,
            user_name: &user_name,
            requester_ip: None,
            now: now - Duration::minutes(5),
            idle_expires_at: idle,
            abs_expires_at: abs,
        },
    )
    .await
    .expect("insert device");
    sqlx::query("UPDATE devices SET last_used_at = $2 WHERE id = $1")
        .bind(&id)
        .bind(last_used)
        .execute(pool)
        .await
        .expect("set last_used_at");
    if matches!(fixture, Fixture::Revoked) {
        assert!(db::devices::revoke_by_user(pool, &id, user_id, now)
            .await
            .expect("revoke"));
    }
    TestDevice {
        id,
        key,
        user_id: user_id.to_string(),
    }
}

async fn delete_device(pool: &DbPool, device: &TestDevice) {
    let _ = sqlx::query("DELETE FROM devices WHERE id = $1")
        .bind(&device.id)
        .execute(pool)
        .await;
}

fn claims_for(device: &TestDevice, aud: &str, iat: i64, exp: i64) -> DeviceTokenClaims {
    DeviceTokenClaims {
        iss: CLI_ISSUER.to_string(),
        sub: device.id.clone(),
        aud: aud.to_string(),
        iat,
        exp,
        jti: new_id(),
    }
}

fn sign(key: &SigningKey, kid: &str, claims: &DeviceTokenClaims) -> String {
    let header = Header {
        alg: Algorithm::EdDSA,
        typ: Some("JWT".into()),
        kid: Some(kid.to_string()),
        ..Default::default()
    };
    let der = key.to_pkcs8_der().expect("pkcs8 der");
    encode(&header, claims, &EncodingKey::from_ed_der(der.as_bytes())).expect("sign")
}

/// A valid device token for meeting-api (120 s lifetime).
fn device_token(device: &TestDevice) -> String {
    let now = Utc::now().timestamp();
    sign(
        &device.key,
        &device.id,
        &claims_for(device, TEST_DEVICE_AUDIENCE, now, now + 120),
    )
}

fn bearer(method: &str, uri: &str, token: &str) -> http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
}

async fn send(pool: &DbPool, req: Request<Body>) -> axum::response::Response {
    build_app(pool.clone()).oneshot(req).await.expect("oneshot")
}

async fn get_meeting_with(pool: &DbPool, room: &str, token: &str) -> axum::response::Response {
    send(
        pool,
        bearer("GET", &format!("/api/v1/meetings/{room}"), token)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

/// Create a meeting owned by `creator` directly in the DB.
async fn create_meeting(pool: &DbPool, room: &str, creator: &str, active: bool) {
    cleanup_test_data(pool, room).await;
    let row = db_meetings::create(pool, room, creator, None, &serde_json::json!([]))
        .await
        .expect("create meeting");
    if active {
        db_meetings::activate(pool, row.id).await.expect("activate");
    }
}

async fn meeting_state(pool: &DbPool, room: &str) -> Option<String> {
    db_meetings::get_by_room_id(pool, room)
        .await
        .expect("get meeting")
        .and_then(|m| m.state)
}

fn room_token_claims(token: &str) -> RoomAccessTokenClaims {
    jsonwebtoken::dangerous::insecure_decode::<RoomAccessTokenClaims>(token)
        .expect("decode room token")
        .claims
}

fn assert_invalid_token(resp: &axum::response::Response) {
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok()),
        Some("Bearer error=\"invalid_token\"")
    );
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn valid_device_token_works_on_get_join_and_status() {
    let pool = get_test_pool().await;
    let room = "dev-auth-happy";
    let host = unique_user();
    create_meeting(&pool, room, &host, true).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;

    let resp = get_meeting_with(&pool, room, &device_token(&device)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = send(
        &pool,
        bearer(
            "POST",
            &format!("/api/v1/meetings/{room}/join"),
            &device_token(&device),
        )
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"pi"}"#))
        .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["user_id"], device.user_id.as_str());

    // Whatever the waiting-room default is, admit the device so status must
    // hand out a room token.
    let meeting = db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .expect("meeting");
    db::participants::admit(&pool, meeting.id, &device.user_id)
        .await
        .unwrap();

    let resp = send(
        &pool,
        bearer(
            "GET",
            &format!("/api/v1/meetings/{room}/status"),
            &device_token(&device),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["is_host"], false);
    assert_eq!(body["result"]["status"], "admitted");
    let token = body["result"]["room_token"]
        .as_str()
        .expect("an admitted participant gets a room token");
    assert!(!room_token_claims(token).is_host);

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

// ---------------------------------------------------------------------------
// Rejections (all 401 with WWW-Authenticate)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn rejected_device_tokens_get_401() {
    let pool = get_test_pool().await;
    let room = "dev-auth-reject";
    create_meeting(&pool, room, &unique_user(), true).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let now = Utc::now().timestamp();

    let other_key = SigningKey::from_bytes(&random_seed());
    let wrong_key = sign(
        &other_key,
        &device.id,
        &claims_for(&device, TEST_DEVICE_AUDIENCE, now, now + 120),
    );
    let expired = sign(
        &device.key,
        &device.id,
        &claims_for(&device, TEST_DEVICE_AUDIENCE, now - 400, now - 280),
    );
    let future_iat = sign(
        &device.key,
        &device.id,
        &claims_for(&device, TEST_DEVICE_AUDIENCE, now + 120, now + 240),
    );
    let too_long = sign(
        &device.key,
        &device.id,
        &claims_for(&device, TEST_DEVICE_AUDIENCE, now, now + 301),
    );
    let wrong_aud = sign(
        &device.key,
        &device.id,
        &claims_for(&device, OTHER_AUDIENCE, now, now + 120),
    );
    let unknown_kid = {
        let id = new_id();
        let mut claims = claims_for(&device, TEST_DEVICE_AUDIENCE, now, now + 120);
        claims.sub = id.clone();
        sign(&device.key, &id, &claims)
    };
    let sub_mismatch = {
        let mut claims = claims_for(&device, TEST_DEVICE_AUDIENCE, now, now + 120);
        claims.sub = new_id();
        sign(&device.key, &device.id, &claims)
    };
    let wrong_issuer = {
        let mut claims = claims_for(&device, TEST_DEVICE_AUDIENCE, now, now + 120);
        claims.iss = "someone-else".into();
        sign(&device.key, &device.id, &claims)
    };
    let malformed_kid = sign(
        &device.key,
        "short",
        &claims_for(&device, TEST_DEVICE_AUDIENCE, now, now + 120),
    );

    for (label, token) in [
        ("wrong key", wrong_key),
        ("expired", expired),
        ("iat 120 s in the future", future_iat),
        ("lifetime 301 s", too_long),
        ("different aud", wrong_aud),
        ("unknown kid", unknown_kid),
        ("kid != sub", sub_mismatch),
        ("wrong issuer", wrong_issuer),
        ("malformed kid", malformed_kid),
    ] {
        let resp = get_meeting_with(&pool, room, &token).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{label}");
        assert_invalid_token(&resp);
    }

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn devices_not_live_and_approved_get_401() {
    let pool = get_test_pool().await;
    let room = "dev-auth-states";
    create_meeting(&pool, room, &unique_user(), true).await;
    for fixture in [Fixture::Revoked, Fixture::IdleExpired, Fixture::AbsExpired] {
        let device = insert_device(&pool, fixture, &unique_user()).await;
        let resp = get_meeting_with(&pool, room, &device_token(&device)).await;
        assert_invalid_token(&resp);
        delete_device(&pool, &device).await;
    }
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn revocation_takes_effect_on_the_next_call() {
    let pool = get_test_pool().await;
    let room = "dev-auth-revoke";
    create_meeting(&pool, room, &unique_user(), true).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let resp = get_meeting_with(&pool, room, &device_token(&device)).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let revoked = db::devices::revoke_by_user(&pool, &device.id, &device.user_id, Utc::now())
        .await
        .unwrap();
    assert!(revoked);
    let resp = get_meeting_with(&pool, room, &device_token(&device)).await;
    assert_invalid_token(&resp);

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

/// Algorithm confusion: HS256 keyed with the device's public key bytes is
/// routed to the session decoder and fails there; `alg: none` cannot even
/// be parsed as a header.
#[tokio::test]
#[serial]
async fn algorithm_confusion_is_rejected() {
    let pool = get_test_pool().await;
    let room = "dev-auth-alg";
    create_meeting(&pool, room, &unique_user(), true).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let now = Utc::now().timestamp();
    let claims = claims_for(&device, TEST_DEVICE_AUDIENCE, now, now + 120);
    let pk = device.key.verifying_key().to_bytes();

    let hs256 = {
        let header = Header {
            alg: Algorithm::HS256,
            kid: Some(device.id.clone()),
            ..Default::default()
        };
        encode(&header, &claims, &EncodingKey::from_secret(&pk)).unwrap()
    };
    let hs256_b64_key = {
        let header = Header {
            alg: Algorithm::HS256,
            kid: Some(device.id.clone()),
            ..Default::default()
        };
        let secret = URL_SAFE_NO_PAD.encode(pk);
        encode(
            &header,
            &claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    };
    let none = format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(format!(
            r#"{{"alg":"none","typ":"JWT","kid":"{}"}}"#,
            device.id
        )),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    for (label, token) in [
        ("HS256 raw key", hs256),
        ("HS256 b64 key", hs256_b64_key),
        ("none", none),
    ] {
        let resp = get_meeting_with(&pool, room, &token).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{label}");
    }

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn device_token_in_session_cookie_is_rejected() {
    let pool = get_test_pool().await;
    let room = "dev-auth-cookie";
    create_meeting(&pool, room, &unique_user(), true).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let resp = send(
        &pool,
        Request::builder()
            .method("GET")
            .uri(format!("/api/v1/meetings/{room}"))
            .header("Cookie", format!("session={}", device_token(&device)))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn room_token_as_bearer_is_rejected() {
    let pool = get_test_pool().await;
    let room = "dev-auth-roomtok";
    let user = unique_user();
    create_meeting(&pool, room, &user, true).await;
    let room_token =
        meeting_api::token::generate_room_token(TEST_JWT_SECRET, 600, &user, room, true, "x")
            .unwrap();
    let resp = get_meeting_with(&pool, room, &room_token).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn device_tokens_rejected_when_audience_unset() {
    let pool = get_test_pool().await;
    let room = "dev-auth-noaud";
    create_meeting(&pool, room, &unique_user(), true).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let mut state = build_state(pool.clone());
    state.device.audience = None;
    let resp = build_app_with_state(state)
        .oneshot(
            bearer(
                "GET",
                &format!("/api/v1/meetings/{room}"),
                &device_token(&device),
            )
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_invalid_token(&resp);
    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

// ---------------------------------------------------------------------------
// Route policy
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn session_jwt_as_bearer_still_works_on_browser_routes() {
    let pool = get_test_pool().await;
    let user = unique_user();
    let jwt =
        meeting_api::token::generate_session_token(TEST_JWT_SECRET, &user, "Bee", 3600).unwrap();
    let resp = send(
        &pool,
        bearer("GET", "/profile", &jwt).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["user_id"], user.as_str());
}

#[tokio::test]
#[serial]
async fn device_token_forbidden_on_browser_only_routes() {
    let pool = get_test_pool().await;
    let room = "dev-auth-forbid";
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    // The device's own user owns the meeting, so only the credential type
    // decides the outcome.
    create_meeting(&pool, room, &device.user_id, true).await;

    let cases: Vec<(&str, String, Option<&str>)> = vec![
        ("GET", "/profile".into(), None),
        ("GET", "/session".into(), None),
        ("GET", "/api/v1/meetings".into(), None),
        (
            "POST",
            "/api/v1/meetings".into(),
            Some(r#"{"meeting_id":"dev-auth-new"}"#),
        ),
        (
            "PATCH",
            format!("/api/v1/meetings/{room}"),
            Some(r#"{"waiting_room_enabled":false}"#),
        ),
        ("DELETE", format!("/api/v1/meetings/{room}"), None),
        ("POST", format!("/api/v1/meetings/{room}/end"), None),
        ("POST", format!("/api/v1/meetings/{room}/leave"), None),
        ("GET", format!("/api/v1/meetings/{room}/participants"), None),
        ("GET", format!("/api/v1/meetings/{room}/waiting"), None),
        (
            "POST",
            format!("/api/v1/meetings/{room}/admit"),
            Some(r#"{"email":"x@y"}"#),
        ),
        ("POST", format!("/api/v1/meetings/{room}/admit-all"), None),
        (
            "POST",
            format!("/api/v1/meetings/{room}/reject"),
            Some(r#"{"email":"x@y"}"#),
        ),
    ];
    for (method, uri, body) in cases {
        let req = bearer(method, &uri, &device_token(&device))
            .header("Content-Type", "application/json")
            .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
            .unwrap();
        let resp = send(&pool, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        let body: Value = response_json(resp).await;
        assert_eq!(body["result"]["code"], "BROWSER_SESSION_REQUIRED");
    }
    // Nothing changed: the meeting is still active and nothing was created.
    assert_eq!(meeting_state(&pool, room).await.as_deref(), Some("active"));
    assert!(db_meetings::get_by_room_id(&pool, "dev-auth-new")
        .await
        .unwrap()
        .is_none());

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn cli_join_never_creates_a_meeting() {
    let pool = get_test_pool().await;
    let room = "dev-auth-missing";
    cleanup_test_data(&pool, room).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let resp = send(
        &pool,
        bearer(
            "POST",
            &format!("/api/v1/meetings/{room}/join"),
            &device_token(&device),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["code"], "MEETING_NOT_FOUND");
    assert!(db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .is_none());
    delete_device(&pool, &device).await;
}

#[tokio::test]
#[serial]
async fn cli_creator_cannot_activate_their_meeting() {
    let pool = get_test_pool().await;
    let room = "dev-auth-idle";
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    create_meeting(&pool, room, &device.user_id, false).await;
    let before = meeting_state(&pool, room).await;
    assert_ne!(before.as_deref(), Some("active"));

    let resp = send(
        &pool,
        bearer(
            "POST",
            &format!("/api/v1/meetings/{room}/join"),
            &device_token(&device),
        )
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"pi"}"#))
        .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["status"], "waiting_for_meeting");
    assert!(body["result"]["room_token"].is_null());
    assert_eq!(body["result"]["is_host"], false);
    assert_eq!(meeting_state(&pool, room).await, before);

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn cli_creator_joins_active_meeting_without_host_rights() {
    let pool = get_test_pool().await;
    let room = "dev-auth-creator";
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    create_meeting(&pool, room, &device.user_id, true).await;
    let host_name_before = db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .unwrap()
        .host_display_name;

    let resp = send(
        &pool,
        bearer(
            "POST",
            &format!("/api/v1/meetings/{room}/join"),
            &device_token(&device),
        )
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"pi camera"}"#))
        .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["status"], "admitted");
    assert_eq!(body["result"]["is_host"], false);
    let token = body["result"]["room_token"].as_str().expect("room token");
    let claims = room_token_claims(token);
    assert!(!claims.is_host);
    assert_eq!(claims.sub, device.user_id);

    // The participant row is not a host row and the host name is untouched.
    let is_host: bool = sqlx::query_scalar(
        "SELECT p.is_host FROM meeting_participants p JOIN meetings m ON m.id = p.meeting_id \
         WHERE m.room_id = $1 AND p.user_id = $2",
    )
    .bind(room)
    .bind(&device.user_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!is_host);
    let host_name_after = db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .unwrap()
        .host_display_name;
    assert_eq!(host_name_before, host_name_after);

    // Status also hands out a non-host token.
    let resp = send(
        &pool,
        bearer(
            "GET",
            &format!("/api/v1/meetings/{room}/status"),
            &device_token(&device),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["is_host"], false);
    let token = body["result"]["room_token"].as_str().expect("room token");
    assert!(!room_token_claims(token).is_host);

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn cli_leave_as_creator_is_forbidden_and_does_not_end_the_meeting() {
    let pool = get_test_pool().await;
    let room = "dev-auth-leave";
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    create_meeting(&pool, room, &device.user_id, true).await;
    let resp = send(
        &pool,
        bearer(
            "POST",
            &format!("/api/v1/meetings/{room}/leave"),
            &device_token(&device),
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(meeting_state(&pool, room).await.as_deref(), Some("active"));
    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

// ---------------------------------------------------------------------------
// last_used_at touch
// ---------------------------------------------------------------------------

async fn usage(pool: &DbPool, id: &str) -> (DateTime<Utc>, DateTime<Utc>) {
    let row = db::devices::find_device(pool, id).await.unwrap().unwrap();
    (row.last_used_at, row.idle_expires_at)
}

#[tokio::test]
#[serial]
async fn touch_updates_last_used_at_at_most_once_an_hour() {
    let pool = get_test_pool().await;
    let room = "dev-auth-touch";
    create_meeting(&pool, room, &unique_user(), true).await;

    // Used two hours ago: the first call slides both columns forward.
    let stale = insert_device(&pool, Fixture::ApprovedStale, &unique_user()).await;
    let (used_before, idle_before) = usage(&pool, &stale.id).await;
    let span = idle_before - used_before;
    let resp = get_meeting_with(&pool, room, &device_token(&stale)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (used_after, idle_after) = usage(&pool, &stale.id).await;
    assert!(used_after > used_before + Duration::minutes(110));
    assert!(idle_after > idle_before);
    // The original idle span is preserved (to within a second).
    assert!((idle_after - used_after - span).num_seconds().abs() <= 1);

    // A second call within the hour writes nothing.
    let resp = get_meeting_with(&pool, room, &device_token(&stale)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(usage(&pool, &stale.id).await, (used_after, idle_after));

    // A device used just now is not touched either.
    let fresh = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let before = usage(&pool, &fresh.id).await;
    let resp = get_meeting_with(&pool, room, &device_token(&fresh)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(usage(&pool, &fresh.id).await, before);

    delete_device(&pool, &stale).await;
    delete_device(&pool, &fresh).await;
    cleanup_test_data(&pool, room).await;
}

// ---------------------------------------------------------------------------
// oauth_requests lifetime (kept from the previous CLI work)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn stale_oauth_requests_are_rejected_and_swept() {
    use meeting_api::db::oauth as db_oauth;
    let pool = get_test_pool().await;
    let stale_state = format!("stale-{}", new_id());
    sqlx::query(
        "INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, return_to, nonce, created_at) \
         VALUES ('c', 'v', $1, NULL, NULL, $2)",
    )
    .bind(&stale_state)
    .bind(Utc::now() - Duration::seconds(db_oauth::OAUTH_REQUEST_TTL_SECS + 60))
    .execute(&pool)
    .await
    .unwrap();
    // A stale row is consumed but treated as absent.
    assert!(db_oauth::fetch_oauth_request(&pool, &stale_state)
        .await
        .unwrap()
        .is_none());

    // A fresh row survives a sweep; the stale one is removed. The sweep is
    // forced here: on the request path it is throttled per process.
    let fresh_state = format!("fresh-{}", new_id());
    let other_stale = format!("stale-{}", new_id());
    sqlx::query(
        "INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, created_at) \
         VALUES ('c', 'v', $1, $2)",
    )
    .bind(&other_stale)
    .bind(Utc::now() - Duration::hours(2))
    .execute(&pool)
    .await
    .unwrap();
    db_oauth::store_oauth_request(&pool, "c", "v", &fresh_state, None, None, None)
        .await
        .unwrap();
    db_oauth::sweep_oauth_requests(
        &pool,
        Utc::now() - Duration::seconds(db_oauth::OAUTH_REQUEST_TTL_SECS),
    )
    .await
    .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM oauth_requests WHERE csrf_state = $1")
            .bind(&other_stale)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0, "stale row swept");
    let row = db_oauth::fetch_oauth_request(&pool, &fresh_state)
        .await
        .unwrap()
        .expect("fresh row is returned");
    assert_eq!(row.pkce_verifier.as_deref(), Some("v"));
}

#[tokio::test]
#[serial]
async fn oauth_request_sweep_is_bounded() {
    use meeting_api::db::oauth as db_oauth;
    let pool = get_test_pool().await;
    let old = Utc::now() - Duration::hours(3);
    let prefix = format!("bulk-{}-", new_id());
    for i in 0..(db::SWEEP_BATCH + 20) {
        sqlx::query(
            "INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, created_at) \
             VALUES ('c', 'v', $1, $2)",
        )
        .bind(format!("{prefix}{i}"))
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    }
    let cutoff = Utc::now() - Duration::seconds(db_oauth::OAUTH_REQUEST_TTL_SECS);
    let first = db_oauth::sweep_oauth_requests(&pool, cutoff).await.unwrap();
    assert_eq!(first, db::SWEEP_BATCH as u64);
    let second = db_oauth::sweep_oauth_requests(&pool, cutoff).await.unwrap();
    assert!(second >= 20);
}

/// Rows with a NULL csrf_state can never be used. They used to occupy the
/// sweep's LIMIT forever (`NULL IN (...)` never matches); now they are
/// deleted like any other dead row.
#[tokio::test]
#[serial]
async fn oauth_sweep_deletes_null_state_rows() {
    use meeting_api::db::oauth as db_oauth;
    let pool = get_test_pool().await;
    let _ = sqlx::query("DELETE FROM oauth_requests WHERE csrf_state IS NULL")
        .execute(&pool)
        .await;
    for _ in 0..(db::SWEEP_BATCH + 10) {
        sqlx::query(
            "INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, created_at) \
             VALUES ('c', 'v', NULL, $1)",
        )
        .bind(Utc::now())
        .execute(&pool)
        .await
        .unwrap();
    }
    let old_state = format!("old-{}", new_id());
    sqlx::query(
        "INSERT INTO oauth_requests (pkce_challenge, pkce_verifier, csrf_state, created_at) \
         VALUES ('c', 'v', $1, $2)",
    )
    .bind(&old_state)
    .bind(Utc::now() - Duration::hours(2))
    .execute(&pool)
    .await
    .unwrap();

    let cutoff = Utc::now() - Duration::seconds(db_oauth::OAUTH_REQUEST_TTL_SECS);
    let mut swept = 0;
    for _ in 0..3 {
        swept += db_oauth::sweep_oauth_requests(&pool, cutoff).await.unwrap();
    }
    assert!(swept >= db::SWEEP_BATCH as u64 + 11, "swept {swept}");
    let left: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM oauth_requests WHERE csrf_state IS NULL OR csrf_state = $1",
    )
    .bind(&old_state)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

// ---------------------------------------------------------------------------
// Display names for CLI callers
// ---------------------------------------------------------------------------

async fn set_waiting_room(pool: &DbPool, room: &str, creator: &str, enabled: bool) {
    db_meetings::update_waiting_room_enabled(pool, room, creator, enabled)
        .await
        .unwrap()
        .expect("meeting");
}

async fn join_as(pool: &DbPool, req: http::request::Builder, name: Option<&str>) -> Value {
    let body = match name {
        Some(name) => serde_json::json!({ "display_name": name }).to_string(),
        None => "{}".to_string(),
    };
    let resp = send(
        pool,
        req.header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    response_json::<Value>(resp).await["result"].clone()
}

async fn status_as(pool: &DbPool, req: http::request::Builder) -> Value {
    let resp = send(pool, req.body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    response_json::<Value>(resp).await["result"].clone()
}

fn token_name(result: &Value) -> String {
    let token = result["room_token"].as_str().expect("room token");
    room_token_claims(token).display_name
}

async fn stored_name(pool: &DbPool, room: &str, user: &str) -> Option<String> {
    let meeting = db_meetings::get_by_room_id(pool, room)
        .await
        .unwrap()
        .unwrap();
    db::participants::get_status(pool, meeting.id, user)
        .await
        .unwrap()
        .and_then(|row| row.display_name)
}

/// Without --display-name a CLI shows its account name, never the user id:
/// directly from join, from status, and in the waiting-room observer token.
#[tokio::test]
#[serial]
async fn cli_without_display_name_uses_the_account_name() {
    let pool = get_test_pool().await;
    let room = "dev-auth-name-default";
    let host = unique_user();
    create_meeting(&pool, room, &host, true).await;
    set_waiting_room(&pool, room, &host, false).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let account_name = format!("Name of {}", device.user_id);
    let join_uri = format!("/api/v1/meetings/{room}/join");
    let status_uri = format!("/api/v1/meetings/{room}/status");

    let joined = join_as(
        &pool,
        bearer("POST", &join_uri, &device_token(&device)),
        None,
    )
    .await;
    assert_eq!(token_name(&joined), account_name);
    let status = status_as(&pool, bearer("GET", &status_uri, &device_token(&device))).await;
    assert_eq!(token_name(&status), account_name);

    // Waiting room: the observer token carries the account name, and so does
    // the room token once admitted.
    cleanup_test_data(&pool, room).await;
    create_meeting(&pool, room, &host, true).await;
    set_waiting_room(&pool, room, &host, true).await;
    let waiting = join_as(
        &pool,
        bearer("POST", &join_uri, &device_token(&device)),
        None,
    )
    .await;
    assert_eq!(waiting["status"], "waiting");
    let observer = waiting["observer_token"].as_str().expect("observer token");
    let observer_name = jsonwebtoken::dangerous::insecure_decode::<Value>(observer)
        .unwrap()
        .claims["display_name"]
        .clone();
    assert_eq!(observer_name, account_name.as_str());
    let meeting = db_meetings::get_by_room_id(&pool, room)
        .await
        .unwrap()
        .unwrap();
    db::participants::admit(&pool, meeting.id, &device.user_id)
        .await
        .unwrap()
        .expect("admitted");
    let status = status_as(&pool, bearer("GET", &status_uri, &device_token(&device))).await;
    assert_eq!(token_name(&status), account_name);

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

/// Browser first, then the same account's CLI with its own name: the CLI's
/// token carries the CLI name, the shared row and the browser keep theirs.
#[tokio::test]
#[serial]
async fn cli_name_never_renames_the_browser_session() {
    let pool = get_test_pool().await;
    let room = "dev-auth-name-browser-first";
    let host = unique_user();
    create_meeting(&pool, room, &host, true).await;
    set_waiting_room(&pool, room, &host, false).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let join_uri = format!("/api/v1/meetings/{room}/join");
    let status_uri = format!("/api/v1/meetings/{room}/status");

    let browser = join_as(
        &pool,
        request_with_cookie("POST", &join_uri, &device.user_id),
        Some("Browser Name"),
    )
    .await;
    assert_eq!(token_name(&browser), "Browser Name");

    let cli = join_as(
        &pool,
        bearer("POST", &join_uri, &device_token(&device)),
        Some("cli-box"),
    )
    .await;
    assert_eq!(token_name(&cli), "cli-box");
    assert_eq!(
        stored_name(&pool, room, &device.user_id).await.as_deref(),
        Some("Browser Name")
    );
    let browser_status = status_as(
        &pool,
        request_with_cookie("GET", &status_uri, &device.user_id),
    )
    .await;
    assert_eq!(token_name(&browser_status), "Browser Name");

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

/// CLI first: its name fills the empty row. The browser then joins with its
/// own name and that replaces it (browser behaviour is unchanged); a CLI
/// that gets its token from status afterwards sees the stored name.
#[tokio::test]
#[serial]
async fn cli_name_fills_an_empty_row_and_the_browser_still_wins() {
    let pool = get_test_pool().await;
    let room = "dev-auth-name-cli-first";
    let host = unique_user();
    create_meeting(&pool, room, &host, true).await;
    set_waiting_room(&pool, room, &host, false).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let join_uri = format!("/api/v1/meetings/{room}/join");
    let status_uri = format!("/api/v1/meetings/{room}/status");

    let cli = join_as(
        &pool,
        bearer("POST", &join_uri, &device_token(&device)),
        Some("cli-box"),
    )
    .await;
    assert_eq!(token_name(&cli), "cli-box");
    assert_eq!(
        stored_name(&pool, room, &device.user_id).await.as_deref(),
        Some("cli-box")
    );

    let browser = join_as(
        &pool,
        request_with_cookie("POST", &join_uri, &device.user_id),
        Some("Browser Name"),
    )
    .await;
    assert_eq!(token_name(&browser), "Browser Name");
    let browser_status = status_as(
        &pool,
        request_with_cookie("GET", &status_uri, &device.user_id),
    )
    .await;
    assert_eq!(token_name(&browser_status), "Browser Name");
    let cli_status = status_as(&pool, bearer("GET", &status_uri, &device_token(&device))).await;
    assert_eq!(token_name(&cli_status), "Browser Name");

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}

/// Room tokens for a CLI device live 300 s, so a revoked device stops
/// reaching the relay within minutes; browser room tokens keep the
/// configured TTL.
#[tokio::test]
#[serial]
async fn cli_room_tokens_are_short_lived_and_browser_tokens_unchanged() {
    let pool = get_test_pool().await;
    let room = "dev-auth-cli-token-ttl";
    let host = unique_user();
    create_meeting(&pool, room, &host, true).await;
    set_waiting_room(&pool, room, &host, false).await;
    let device = insert_device(&pool, Fixture::Approved, &unique_user()).await;
    let join_uri = format!("/api/v1/meetings/{room}/join");
    let status_uri = format!("/api/v1/meetings/{room}/status");
    let lifetime = |result: &Value| {
        let token = result["room_token"].as_str().expect("room token");
        let claims = jsonwebtoken::dangerous::insecure_decode::<Value>(token)
            .unwrap()
            .claims;
        // Room tokens carry no iat; allow a few seconds of test time.
        let remaining = claims["exp"].as_i64().unwrap() - chrono::Utc::now().timestamp();
        assert!(remaining > 0);
        (remaining + 5) / 10 * 10
    };

    let cli = join_as(
        &pool,
        bearer("POST", &join_uri, &device_token(&device)),
        None,
    )
    .await;
    assert_eq!(
        lifetime(&cli),
        meeting_api::routes::participants::CLI_ROOM_TOKEN_TTL_SECS
    );
    assert_eq!(lifetime(&cli), 300);
    let status = status_as(&pool, bearer("GET", &status_uri, &device_token(&device))).await;
    assert_eq!(lifetime(&status), 300);

    let browser = join_as(
        &pool,
        request_with_cookie("POST", &join_uri, &device.user_id),
        None,
    )
    .await;
    assert_eq!(lifetime(&browser), 600, "TEST_TOKEN_TTL");

    delete_device(&pool, &device).await;
    cleanup_test_data(&pool, room).await;
}
