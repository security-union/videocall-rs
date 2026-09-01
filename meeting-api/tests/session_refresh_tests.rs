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

mod test_helpers;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use chrono::Utc;
use jsonwebtoken::{encode, EncodingKey, Header};
use meeting_api::state::AppState;
use meeting_api::token::{decode_session_token, generate_session_token, SessionTokenClaims};
use serde::Serialize;
use test_helpers::{build_app_from_state, TEST_JWT_SECRET};
use tower::ServiceExt;

const SESSION_TTL: i64 = 3600;
const REFRESH_THRESHOLD: i64 = 300;
const ABSOLUTE_MAX: i64 = 7200;

fn test_state() -> AppState {
    AppState {
        db: sqlx::PgPool::connect_lazy("postgres://localhost/unused")
            .expect("connect_lazy should not connect"),
        jwt_secret: TEST_JWT_SECRET.to_string(),
        session_jwt_secret: TEST_JWT_SECRET.to_string(),
        session_jwt_secret_previous: None,
        session_previous_secret_expires_at: 0,
        token_ttl_secs: 600,
        session_ttl_secs: SESSION_TTL,
        session_refresh_threshold_secs: REFRESH_THRESHOLD,
        session_absolute_max_secs: ABSOLUTE_MAX,
        oauth: None,
        jwks_cache: None,
        cookie_domain: None,
        cookie_name: "session".to_string(),
        cookie_secure: false,
        nats: None,
        feed_tx: meeting_api::feed_events::new_feed_channel().0,
        service_version_urls: Vec::new(),
        http_client: reqwest::Client::new(),
        display_name_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        display_name_rate_limiter_ops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        search: None,
        display_name_rate_limit_disabled: false,
        dev_user: None,
        password_gate: std::sync::Arc::new(meeting_api::password::MeetingPasswordGate::new()),
    }
}

fn session_cookie(jwt: &str) -> String {
    format!("session={jwt}")
}

fn request_with_cookie(uri: &str, jwt: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::COOKIE, session_cookie(jwt))
        .body(Body::empty())
        .unwrap()
}

fn set_cookie_values(headers: &axum::http::HeaderMap) -> Vec<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_string())
        .collect()
}

fn session_set_cookie(headers: &axum::http::HeaderMap) -> Option<String> {
    set_cookie_values(headers)
        .into_iter()
        .find(|value| value.starts_with("session="))
}

fn jwt_from_set_cookie(set_cookie: &str) -> &str {
    set_cookie
        .strip_prefix("session=")
        .and_then(|rest| rest.split(';').next())
        .expect("session Set-Cookie must contain JWT")
}

#[tokio::test]
async fn near_expiry_cookie_slides_with_later_exp() {
    let state = test_state();
    let auth_time = Utc::now().timestamp() - 60;
    let original_jwt =
        generate_session_token(TEST_JWT_SECRET, "alice@test.com", "Alice", 60, auth_time).unwrap();
    let original_claims = decode_session_token(TEST_JWT_SECRET, None, &original_jwt).unwrap();
    let app = build_app_from_state(state);

    let resp = app
        .oneshot(request_with_cookie("/profile", &original_jwt))
        .await
        .unwrap();

    let set_cookie = session_set_cookie(resp.headers()).expect("near-expiry cookie should slide");
    let refreshed_jwt = jwt_from_set_cookie(&set_cookie);
    let refreshed_claims = decode_session_token(TEST_JWT_SECRET, None, refreshed_jwt).unwrap();

    assert_eq!(refreshed_claims.sub, "alice@test.com");
    assert_eq!(refreshed_claims.name, "Alice");
    assert_eq!(refreshed_claims.auth_time, Some(auth_time));
    assert!(
        refreshed_claims.exp > original_claims.exp,
        "refreshed exp must be later than original exp"
    );
}

#[tokio::test]
async fn fresh_cookie_does_not_slide() {
    let state = test_state();
    let jwt = generate_session_token(
        TEST_JWT_SECRET,
        "fresh@test.com",
        "Fresh",
        SESSION_TTL,
        Utc::now().timestamp(),
    )
    .unwrap();
    let app = build_app_from_state(state);

    let resp = app
        .oneshot(request_with_cookie("/profile", &jwt))
        .await
        .unwrap();

    assert!(
        session_set_cookie(resp.headers()).is_none(),
        "fresh cookie must not be refreshed"
    );
}

#[tokio::test]
async fn forged_cookie_with_fresh_exp_remains_unauthorized() {
    let state = test_state();
    let jwt = generate_session_token(
        "attacker-controlled-secret",
        "attacker@test.com",
        "Attacker",
        SESSION_TTL,
        Utc::now().timestamp(),
    )
    .unwrap();
    let app = build_app_from_state(state);

    let resp = app
        .oneshot(request_with_cookie("/profile", &jwt))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unverified fresh exp may skip refresh work but must never authorize the request"
    );
    assert!(
        session_set_cookie(resp.headers()).is_none(),
        "forged cookie must never be refreshed"
    );
}

// Companion to the fresh-exp case above. A forged cookie whose unverified `exp`
// is NEAR EXPIRY (ttl 60 < REFRESH_THRESHOLD 300) passes the unverified
// threshold check and therefore ENTERS the full-verify branch of
// `decode_claims_for_refresh` — the one path the fresh-exp test never exercises.
// The HMAC verify must reject it, so the request stays unauthorized AND no
// refresh cookie is minted. Mutation sensitivity: if a re-mint ever sourced
// claims from the unverified parse instead of the verified decode, this forged
// token would produce a Set-Cookie and fail here.
#[tokio::test]
async fn forged_cookie_with_near_expiry_exp_not_refreshed() {
    let state = test_state();
    let jwt = generate_session_token(
        "attacker-controlled-secret",
        "attacker@test.com",
        "Attacker",
        60, // within REFRESH_THRESHOLD (300) → enters the verify branch
        Utc::now().timestamp(),
    )
    .unwrap();
    let app = build_app_from_state(state);

    let resp = app
        .oneshot(request_with_cookie("/profile", &jwt))
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a forged near-expiry cookie must fail the full verify and never authorize"
    );
    assert!(
        session_set_cookie(resp.headers()).is_none(),
        "a forged near-expiry cookie must NOT be re-minted (verify gates the re-mint)"
    );
}

#[tokio::test]
async fn past_absolute_cap_does_not_slide() {
    let state = test_state();
    let auth_time = Utc::now().timestamp() - ABSOLUTE_MAX - 1;
    let jwt = generate_session_token(TEST_JWT_SECRET, "capped@test.com", "Capped", 60, auth_time)
        .unwrap();
    let app = build_app_from_state(state);

    let resp = app
        .oneshot(request_with_cookie("/profile", &jwt))
        .await
        .unwrap();

    assert!(
        session_set_cookie(resp.headers()).is_none(),
        "cookie past absolute cap must not be refreshed"
    );
}

#[tokio::test]
async fn logout_with_near_expiry_cookie_only_sets_clear_cookie() {
    let state = test_state();
    let jwt = generate_session_token(
        TEST_JWT_SECRET,
        "logout@test.com",
        "Logout",
        60,
        Utc::now().timestamp(),
    )
    .unwrap();
    let app = build_app_from_state(state);

    let resp = app
        .oneshot(request_with_cookie("/logout", &jwt))
        .await
        .unwrap();
    let cookies = set_cookie_values(resp.headers());

    assert_eq!(cookies.len(), 1, "logout must emit exactly one Set-Cookie");
    assert!(
        cookies[0].starts_with("session="),
        "logout clear cookie must target the session cookie"
    );
    assert!(
        cookies[0].contains("Max-Age=0"),
        "logout must clear rather than refresh the cookie: {}",
        cookies[0]
    );
}

#[derive(Serialize)]
struct LegacySessionClaims<'a> {
    sub: &'a str,
    name: &'a str,
    exp: i64,
    iat: i64,
    iss: &'a str,
}

fn generate_legacy_session_token_without_auth_time(
    user_id: &str,
    name: &str,
    ttl_secs: i64,
    iat: i64,
) -> String {
    let claims = LegacySessionClaims {
        sub: user_id,
        name,
        exp: Utc::now().timestamp() + ttl_secs,
        iat,
        iss: SessionTokenClaims::ISSUER,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn legacy_cookie_without_auth_time_decodes_and_slides_from_iat() {
    let state = test_state();
    let iat = Utc::now().timestamp() - 60;
    let jwt = generate_legacy_session_token_without_auth_time("legacy@test.com", "Legacy", 60, iat);
    let decoded = decode_session_token(TEST_JWT_SECRET, None, &jwt).unwrap();
    assert_eq!(decoded.auth_time, None);
    let app = build_app_from_state(state);

    let resp = app
        .oneshot(request_with_cookie("/profile", &jwt))
        .await
        .unwrap();

    let set_cookie =
        session_set_cookie(resp.headers()).expect("legacy near-expiry cookie should slide");
    let refreshed_jwt = jwt_from_set_cookie(&set_cookie);
    let refreshed_claims = decode_session_token(TEST_JWT_SECRET, None, refreshed_jwt).unwrap();

    assert_eq!(refreshed_claims.sub, "legacy@test.com");
    assert_eq!(refreshed_claims.auth_time, Some(iat));
    // #2411: a re-mint must upgrade a legacy cookie, not inherit its absent `typ`.
    assert_eq!(
        refreshed_claims.typ.as_deref(),
        Some(SessionTokenClaims::TOKEN_TYPE),
        "a refreshed session must carry typ, not inherit the legacy absence"
    );
    assert_eq!(decoded.typ, None, "the inbound cookie really was legacy");
}

#[tokio::test]
async fn session_cookie_is_verified_with_the_session_secret_not_the_room_secret() {
    // #2411: with SESSION_JWT_SECRET provisioned, the room secret must not mint a session.
    const SESSION_SECRET: &str = "a-different-secret-for-session-cookies";
    let mut state = test_state();
    state.session_jwt_secret = SESSION_SECRET.to_string();
    let now = Utc::now().timestamp();

    let forged = generate_session_token(TEST_JWT_SECRET, "attacker@test.com", "A", 3600, now)
        .expect("should sign");
    let resp = build_app_from_state(state.clone())
        .oneshot(request_with_cookie("/profile", &forged))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "a session signed with the ROOM secret must not authenticate"
    );

    let legitimate = generate_session_token(SESSION_SECRET, "alice@test.com", "Alice", 3600, now)
        .expect("should sign");
    let resp = build_app_from_state(state)
        .oneshot(request_with_cookie("/profile", &legitimate))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "a session signed with the SESSION secret must authenticate"
    );
}

#[tokio::test]
async fn bearer_only_request_does_not_set_cookie() {
    let state = test_state();
    let jwt = generate_session_token(
        TEST_JWT_SECRET,
        "bearer@test.com",
        "Bearer",
        60,
        Utc::now().timestamp(),
    )
    .unwrap();
    let app = build_app_from_state(state);
    let req = Request::builder()
        .uri("/profile")
        .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();

    assert!(
        session_set_cookie(resp.headers()).is_none(),
        "Bearer-only auth must not mint a session cookie"
    );
}
