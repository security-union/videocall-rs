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

//! Integration tests verifying that endpoints reject unauthenticated requests.

mod test_helpers;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use meeting_api::{config::DevUser, token::decode_session_token};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::APIResponse;
use videocall_meeting_types::APIError;

/// Build a request WITHOUT the email cookie (unauthenticated).
fn unauthenticated_request(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder().method(method).uri(uri)
}

// ---------------------------------------------------------------------------
// DEV_USER auto-login integration tests
// ---------------------------------------------------------------------------

/// Production-safety invariant (failure path): when the `AppState` has no
/// `dev_user` configured — the default built by `build_app` — the auto-login
/// endpoint MUST return 404 so it is invisible to unauthenticated clients.
/// This mirrors the runtime condition where `DEV_USER` is unset or OAuth is
/// enabled: the endpoint is mounted in all builds but only activated when
/// `dev_user` is `Some`.
#[tokio::test]
#[serial]
async fn test_dev_auto_login_returns_404_when_dev_user_unset() {
    let pool = get_test_pool().await;
    let app = build_app(pool);

    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/dev/auto-login")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "/api/v1/dev/auto-login must be invisible (404) when DEV_USER is not configured"
    );
}

/// Happy path + round-trip (authorized path): when `dev_user` is `Some`,
/// calling `/api/v1/dev/auto-login` issues a redirect with a `Set-Cookie`
/// carrying a valid signed session JWT.  Re-using that cookie on a protected
/// endpoint (`GET /api/v1/meetings`) returns 200, proving the `AuthUser`
/// extractor accepts sessions minted by the auto-login handler.
///
/// Additionally the JWT's `sub` and `name` claims are verified to match the
/// configured `DEV_USER` identity, locking in the production-safety invariant
/// that auto-login cannot silently embed a different principal.
#[tokio::test]
#[serial]
async fn test_dev_auto_login_round_trip_authorizes_protected_endpoint() {
    let pool = get_test_pool().await;
    let dev_email = "dev@test.local";
    let dev_name = "Dev Test";

    // --- Step 1: call the auto-login endpoint ---
    let app = build_app_with_dev_user(
        pool.clone(),
        DevUser {
            email: dev_email.to_string(),
            name: dev_name.to_string(),
        },
    );
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/dev/auto-login")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert!(
        resp.status().is_redirection(),
        "auto-login should redirect; got {}",
        resp.status()
    );

    // --- Step 2: extract the session JWT from Set-Cookie ---
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("auto-login redirect must include Set-Cookie")
        .to_str()
        .expect("Set-Cookie value should be valid UTF-8");
    let jwt = set_cookie
        .strip_prefix("session=")
        .and_then(|rest| rest.split(';').next())
        .expect("Set-Cookie should be session=<jwt>; ...");

    // --- Step 3: verify the JWT encodes the configured DEV_USER identity ---
    let claims = decode_session_token(TEST_JWT_SECRET, jwt)
        .expect("session JWT from auto-login must be decodable with the configured secret");
    assert_eq!(
        claims.sub, dev_email,
        "JWT sub must be the configured DEV_USER email"
    );
    assert_eq!(
        claims.name, dev_name,
        "JWT name must be the configured DEV_USER display name"
    );

    // --- Step 4: reuse the cookie on a protected endpoint — must NOT be 401 ---
    // Uses plain build_app (dev_user: None) to prove the session is accepted by
    // the standard AuthUser JWT extractor, independent of dev_user in AppState.
    let app2 = build_app(pool.clone());
    let req2 = Request::builder()
        .method("GET")
        .uri("/api/v1/meetings?limit=10&offset=0")
        .header("Cookie", format!("session={jwt}"))
        .body(Body::empty())
        .unwrap();
    let resp2 = app2.oneshot(req2).await.unwrap();
    assert_eq!(
        resp2.status(),
        StatusCode::OK,
        "session cookie from dev auto-login must be accepted by protected endpoints"
    );
}

#[tokio::test]
#[serial]
async fn test_create_meeting_unauthorized() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = unauthenticated_request("POST", "/api/v1/meetings")
        .header("Content-Type", "application/json")
        .body(Body::from(
            r#"{"meeting_id":"unauthorized","attendees":[]}"#,
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "UNAUTHORIZED");
}

#[tokio::test]
#[serial]
async fn test_get_meeting_unauthorized() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = unauthenticated_request("GET", "/api/v1/meetings/some-meeting")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[serial]
async fn test_list_meetings_unauthorized() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = unauthenticated_request("GET", "/api/v1/meetings?limit=10&offset=0")
        .body(Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
