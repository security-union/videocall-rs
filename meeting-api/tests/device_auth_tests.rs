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

//! Device JWTs on the meeting endpoints: which credentials get in and what
//! a CLI device may do. Claim-level token checks are unit tests in
//! `videocall_meeting_types::device::crypto`; the sign-in flow is in
//! `device_api_tests.rs`.

mod test_helpers;

use axum::body::Body;
use axum::http::{self, Request, StatusCode};
use axum::response::Response;
use chrono::{DateTime, Duration, Utc};
use meeting_api::db::{self, meetings as db_meetings, DbPool};
use meeting_api::secrets::random_base64url;
use meeting_api::state::AppState;
use serde_json::{json, Value};
use serial_test::serial;
use test_helpers::*;
use videocall_meeting_types::RoomAccessTokenClaims;

async fn create_meeting(pool: &DbPool, room: &str, creator: &str, active: bool) {
    cleanup_test_data(pool, room).await;
    let row = db_meetings::create(pool, room, creator, None, &json!([]))
        .await
        .expect("create meeting");
    if active {
        db_meetings::activate(pool, row.id).await.expect("activate");
    }
}

async fn meeting(pool: &DbPool, room: &str) -> Option<db::MeetingRow> {
    db_meetings::get_by_room_id(pool, room).await.unwrap()
}

fn room_token_claims(result: &Value) -> RoomAccessTokenClaims {
    let token = result["room_token"].as_str().expect("room token");
    jsonwebtoken::dangerous::insecure_decode::<RoomAccessTokenClaims>(token)
        .expect("decode room token")
        .claims
}

/// Assert 200 and return the envelope's `result`.
async fn ok(resp: Response) -> Value {
    assert_eq!(resp.status(), StatusCode::OK);
    response_json::<Value>(resp).await["result"].clone()
}

fn join_req(req: http::request::Builder, name: Option<&str>) -> Request<Body> {
    let body = name.map_or(json!({}), |name| json!({ "display_name": name }));
    req.header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Which credentials get in
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn rejected_credentials_get_401() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let room = "dev-auth-reject";
    create_meeting(&pool, room, &unique_user(), true).await;
    let device = insert_device(&pool, &unique_user(), Fixture::Live).await;
    let uri = format!("/api/v1/meetings/{room}");
    let get = |token: &str| bearer("GET", &uri, token).body(Body::empty()).unwrap();
    assert_eq!(
        send(&state, get(&device.token())).await.status(),
        StatusCode::OK
    );

    let now = Utc::now().timestamp();
    let own_claims = device_claims(&device.id, TEST_DEVICE_AUDIENCE, now, now + 120);
    // Keyed with the device's public key: the classic algorithm-confusion token.
    let hs256_with_kid = jsonwebtoken::encode(
        &jsonwebtoken::Header {
            kid: Some(device.id.clone()),
            ..jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256)
        },
        &own_claims,
        &jsonwebtoken::EncodingKey::from_secret(&device.key.verifying_key().to_bytes()),
    )
    .unwrap();
    let unknown = random_base64url(16);
    let room_token = meeting_api::token::generate_room_token(
        TEST_JWT_SECRET,
        600,
        &device.user_id,
        room,
        true,
        "x",
    )
    .unwrap();

    // (name, request, rejected on the device path with WWW-Authenticate)
    let mut cases: Vec<(String, Request<Body>, bool)> = vec![
        (
            "unknown kid".into(),
            get(&sign_eddsa(
                &device.key,
                Some(&unknown),
                &device_claims(&unknown, TEST_DEVICE_AUDIENCE, now, now + 120),
            )),
            true,
        ),
        ("room token as bearer".into(), get(&room_token), false),
        (
            "EdDSA without kid".into(),
            get(&sign_eddsa(&device.key, None, &own_claims)),
            false,
        ),
        (
            "HS256 with a device-shaped kid".into(),
            get(&hs256_with_kid),
            false,
        ),
        (
            "device token in the session cookie".into(),
            Request::builder()
                .uri(&uri)
                .header("Cookie", format!("session={}", device.token()))
                .body(Body::empty())
                .unwrap(),
            false,
        ),
    ];
    let user = device.user_id.as_str();
    for fixture in [Fixture::Revoked, Fixture::IdleExpired] {
        let other = insert_device(&pool, user, fixture).await;
        cases.push((format!("{fixture:?} device"), get(&other.token()), true));
    }
    for (name, req, device_path) in cases {
        let resp = send(&state, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{name}");
        if device_path {
            assert_eq!(
                resp.headers()["www-authenticate"],
                "Bearer error=\"invalid_token\"",
                "{name}"
            );
        }
    }
    let mut no_audience = state.clone();
    no_audience.device.audience = None;
    let resp = send(&no_audience, get(&device.token())).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "audience unset");

    delete_device_data(&pool, &[user]).await;
    cleanup_test_data(&pool, room).await;
}

#[tokio::test]
#[serial]
async fn device_token_forbidden_on_browser_only_routes() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let room = "dev-auth-forbid";
    let device = insert_device(&pool, &unique_user(), Fixture::Live).await;
    let sibling = insert_device(&pool, &device.user_id, Fixture::Live).await;
    // The device's own user owns the meeting, so only the credential type
    // decides the outcome.
    create_meeting(&pool, room, &device.user_id, true).await;

    // A session JWT as bearer still works on a browser-only route.
    let session =
        meeting_api::token::generate_session_token(TEST_JWT_SECRET, &device.user_id, "Bee", 3600)
            .unwrap();
    let profile = send(
        &state,
        bearer("GET", "/profile", &session)
            .body(Body::empty())
            .unwrap(),
    );
    assert_eq!(ok(profile.await).await["user_id"], device.user_id.as_str());

    let approval = json!({
        "redirect_uri": "http://127.0.0.1:53682/callback",
        "code_challenge": "A".repeat(43),
        "code_challenge_method": "S256",
        "state": "s".repeat(16),
        "public_key": public_key_of(&new_key()),
        "device_name": "next",
        "confirm": true,
    })
    .to_string();
    let email = r#"{"email":"x@y"}"#;
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
            Some(email),
        ),
        ("POST", format!("/api/v1/meetings/{room}/admit-all"), None),
        (
            "POST",
            format!("/api/v1/meetings/{room}/reject"),
            Some(email),
        ),
        (
            "POST",
            "/api/v1/devices/authorizations".into(),
            Some(&approval),
        ),
        ("GET", "/api/v1/devices".into(), None),
        ("DELETE", format!("/api/v1/devices/{}", sibling.id), None),
    ];
    for (method, uri, body) in cases {
        let req = bearer(method, &uri, &device.token())
            .header("Origin", TEST_UI_ORIGIN)
            .header("Content-Type", "application/json")
            .body(body.map(|b| Body::from(b.to_string())).unwrap_or_default())
            .unwrap();
        let resp = send(&state, req).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        let body: Value = response_json(resp).await;
        assert_eq!(body["result"]["code"], "BROWSER_SESSION_REQUIRED");
    }
    // Nothing changed.
    assert_eq!(
        meeting(&pool, room).await.unwrap().state.as_deref(),
        Some("active")
    );
    assert!(meeting(&pool, "dev-auth-new").await.is_none());
    let live = db::devices::count_live_devices(&pool, &device.user_id, Utc::now()).await;
    assert_eq!(live.unwrap(), 2);
    assert_eq!(codes_of(&pool, &device.user_id).await, 0);

    delete_device_data(&pool, &[&device.user_id]).await;
    cleanup_test_data(&pool, room).await;
}

// ---------------------------------------------------------------------------
// A CLI device never hosts
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn cli_device_never_hosts() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let room = "dev-auth-creator";
    let device = insert_device(&pool, &unique_user(), Fixture::Live).await;
    let join = || {
        join_req(
            bearer(
                "POST",
                &format!("/api/v1/meetings/{room}/join"),
                &device.token(),
            ),
            Some("pi"),
        )
    };

    // A join never creates a meeting.
    cleanup_test_data(&pool, room).await;
    let resp = send(&state, join()).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: Value = response_json(resp).await;
    assert_eq!(body["result"]["code"], "MEETING_NOT_FOUND");
    assert!(meeting(&pool, room).await.is_none());

    // The creator's idle meeting is not activated by the creator's device.
    create_meeting(&pool, room, &device.user_id, false).await;
    let before = meeting(&pool, room).await.unwrap().state;
    assert_ne!(before.as_deref(), Some("active"));
    let result = ok(send(&state, join()).await).await;
    assert_eq!(result["status"], "waiting_for_meeting");
    assert!(result["room_token"].is_null());
    assert_eq!(result["is_host"], false);
    assert_eq!(meeting(&pool, room).await.unwrap().state, before);

    // Active: admitted without host rights; the host name is untouched.
    create_meeting(&pool, room, &device.user_id, true).await;
    let host_name_before = meeting(&pool, room).await.unwrap().host_display_name;
    let result = ok(send(&state, join()).await).await;
    assert_eq!(result["status"], "admitted");
    assert_eq!(result["is_host"], false);
    let claims = room_token_claims(&result);
    assert!(!claims.is_host);
    assert_eq!(claims.sub, device.user_id);
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
    assert_eq!(
        meeting(&pool, room).await.unwrap().host_display_name,
        host_name_before
    );
    let status_uri = format!("/api/v1/meetings/{room}/status");
    let status = bearer("GET", &status_uri, &device.token());
    let result = ok(send(&state, status.body(Body::empty()).unwrap()).await).await;
    assert_eq!(result["is_host"], false);
    assert!(!room_token_claims(&result).is_host);

    // Leaving as the creator is refused and does not end the meeting.
    let leave = bearer(
        "POST",
        &format!("/api/v1/meetings/{room}/leave"),
        &device.token(),
    );
    let resp = send(&state, leave.body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        meeting(&pool, room).await.unwrap().state.as_deref(),
        Some("active")
    );

    delete_device_data(&pool, &[&device.user_id]).await;
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
    let state = build_state(pool.clone());
    let room = "dev-auth-touch";
    create_meeting(&pool, room, &unique_user(), true).await;
    let uri = format!("/api/v1/meetings/{room}");
    let get = |device: &TestDevice| {
        bearer("GET", &uri, &device.token())
            .body(Body::empty())
            .unwrap()
    };

    // Used two hours ago: the first call slides both columns, keeping the
    // idle span.
    let stale = insert_device(&pool, &unique_user(), Fixture::Stale).await;
    let (used_before, idle_before) = usage(&pool, &stale.id).await;
    ok(send(&state, get(&stale)).await).await;
    let (used_after, idle_after) = usage(&pool, &stale.id).await;
    assert!(used_after > used_before + Duration::minutes(110));
    assert!(
        ((idle_after - used_after) - (idle_before - used_before))
            .num_seconds()
            .abs()
            <= 1
    );

    // Within the hour, nothing is written.
    ok(send(&state, get(&stale)).await).await;
    assert_eq!(usage(&pool, &stale.id).await, (used_after, idle_after));
    let fresh = insert_device(&pool, &unique_user(), Fixture::Live).await;
    let before = usage(&pool, &fresh.id).await;
    ok(send(&state, get(&fresh)).await).await;
    assert_eq!(usage(&pool, &fresh.id).await, before);

    delete_device_data(&pool, &[&stale.user_id, &fresh.user_id]).await;
    cleanup_test_data(&pool, room).await;
}

// ---------------------------------------------------------------------------
// Display names and room-token lifetime for CLI callers
// ---------------------------------------------------------------------------

async fn open_meeting(state: &AppState, room: &str, waiting_room: bool) {
    let host = unique_user();
    create_meeting(&state.db, room, &host, true).await;
    db_meetings::update_waiting_room_enabled(&state.db, room, &host, waiting_room)
        .await
        .unwrap()
        .expect("meeting");
}

fn token_name(result: &Value) -> String {
    room_token_claims(result).display_name
}

/// Room tokens carry no `iat`; round the remaining lifetime to 10 s.
fn lifetime(result: &Value) -> i64 {
    (room_token_claims(result).exp - Utc::now().timestamp() + 5) / 10 * 10
}

/// Without a display name a CLI shows its account name, never the user id:
/// from join, from status, and in the waiting-room observer token.
#[tokio::test]
#[serial]
async fn cli_without_display_name_uses_the_account_name() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let room = "dev-auth-name-default";
    let device = insert_device(&pool, &unique_user(), Fixture::Live).await;
    let account_name = format!("Name of {}", device.user_id);
    let join_uri = format!("/api/v1/meetings/{room}/join");
    let status_uri = format!("/api/v1/meetings/{room}/status");
    let status = || {
        bearer("GET", &status_uri, &device.token())
            .body(Body::empty())
            .unwrap()
    };

    open_meeting(&state, room, false).await;
    let joined = ok(send(
        &state,
        join_req(bearer("POST", &join_uri, &device.token()), None),
    )
    .await)
    .await;
    assert_eq!(token_name(&joined), account_name);
    assert_eq!(
        token_name(&ok(send(&state, status()).await).await),
        account_name
    );

    open_meeting(&state, room, true).await;
    let waiting = ok(send(
        &state,
        join_req(bearer("POST", &join_uri, &device.token()), None),
    )
    .await)
    .await;
    assert_eq!(waiting["status"], "waiting");
    let observer = waiting["observer_token"].as_str().expect("observer token");
    let observer = jsonwebtoken::dangerous::insecure_decode::<Value>(observer).unwrap();
    assert_eq!(observer.claims["display_name"], account_name.as_str());
    db::participants::admit(
        &pool,
        meeting(&pool, room).await.unwrap().id,
        &device.user_id,
    )
    .await
    .unwrap()
    .expect("admitted");
    assert_eq!(
        token_name(&ok(send(&state, status()).await).await),
        account_name
    );

    delete_device_data(&pool, &[&device.user_id]).await;
    cleanup_test_data(&pool, room).await;
}

/// The same account in a browser and a CLI: a CLI name fills an empty row
/// but never renames the browser's; the browser always wins. CLI room tokens
/// live 300 s, browser ones keep the configured TTL.
#[tokio::test]
#[serial]
async fn cli_name_never_renames_the_browser_session() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let room = "dev-auth-name";
    let device = insert_device(&pool, &unique_user(), Fixture::Live).await;
    let user = device.user_id.as_str();
    let join_uri = format!("/api/v1/meetings/{room}/join");
    let status_uri = format!("/api/v1/meetings/{room}/status");
    let cli_join = || join_req(bearer("POST", &join_uri, &device.token()), Some("cli-box"));
    let browser_join = || {
        join_req(
            request_with_cookie("POST", &join_uri, user),
            Some("Browser Name"),
        )
    };
    let stored_name = || async {
        db::participants::get_status(&pool, meeting(&pool, room).await.unwrap().id, user)
            .await
            .unwrap()
            .and_then(|row| row.display_name)
    };
    open_meeting(&state, room, false).await;

    let cli = ok(send(&state, cli_join()).await).await;
    assert_eq!(token_name(&cli), "cli-box");
    assert_eq!(
        lifetime(&cli),
        meeting_api::routes::participants::CLI_ROOM_TOKEN_TTL_SECS
    );
    assert_eq!(lifetime(&cli), 300);
    assert_eq!(stored_name().await.as_deref(), Some("cli-box"));

    let browser = ok(send(&state, browser_join()).await).await;
    assert_eq!(token_name(&browser), "Browser Name");
    assert_eq!(lifetime(&browser), 600, "TEST_TOKEN_TTL");

    let cli = ok(send(&state, cli_join()).await).await;
    assert_eq!(token_name(&cli), "cli-box");
    assert_eq!(stored_name().await.as_deref(), Some("Browser Name"));

    let browser_status = request_with_cookie("GET", &status_uri, user).body(Body::empty());
    let browser_status = ok(send(&state, browser_status.unwrap()).await).await;
    assert_eq!(token_name(&browser_status), "Browser Name");
    let cli_status = bearer("GET", &status_uri, &device.token()).body(Body::empty());
    let cli_status = ok(send(&state, cli_status.unwrap()).await).await;
    assert_eq!(token_name(&cli_status), "Browser Name");
    assert_eq!(lifetime(&cli_status), 300);

    delete_device_data(&pool, &[user]).await;
    cleanup_test_data(&pool, room).await;
}
