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

//! The browser login (`/login` → IdP → `/login/callback`) with the login-CSRF
//! binding: `/login` sets a short-lived pre-login cookie whose SHA-256 is
//! stored on the OAuth request, and the callback only completes in a
//! browser presenting it. The IdP's token endpoint is a local mock; with no
//! JWKS configured meeting-api reads the ID token without verifying it, so
//! the mock can hand back a plain unsigned token. Run against Postgres or
//! SQLite via `DATABASE_URL`.

mod test_helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use meeting_api::config::OAuthConfig;
use meeting_api::db::DbPool;
use meeting_api::state::AppState;
use serial_test::serial;
use sha2::{Digest, Sha256};
use test_helpers::*;
use tower::ServiceExt;
use url::form_urlencoded;

fn sha256_hex(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", Utc::now().timestamp_nanos_opt().unwrap())
}

/// A token endpoint that answers every code with an unsigned ID token for
/// `alice@example.com`.
async fn mock_idp() -> String {
    let payload = URL_SAFE_NO_PAD
        .encode(r#"{"email":"alice@example.com","name":"Alice Example","email_verified":true}"#);
    let id_token = format!("e30.{payload}.sig");
    let body = serde_json::json!({
        "access_token": "access",
        "token_type": "Bearer",
        "id_token": id_token,
    })
    .to_string();
    let app = Router::new().route(
        "/token",
        post(move || {
            let body = body.clone();
            async move { ([("content-type", "application/json")], body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/token")
}

async fn oauth_state(pool: &DbPool) -> AppState {
    let mut state = build_state(pool.clone());
    state.oauth = Some(OAuthConfig {
        client_id: "test-client".to_string(),
        client_secret: None,
        redirect_url: "http://localhost:8081/login/callback".to_string(),
        issuer: None,
        auth_url: "https://idp.example/authorize".to_string(),
        token_url: mock_idp().await,
        jwks_url: None,
        userinfo_url: None,
        scopes: "openid email profile".to_string(),
        after_login_url: TEST_UI_ORIGIN.to_string(),
        allowed_redirect_urls: Vec::new(),
    });
    state
}

async fn send(state: &AppState, req: Request<Body>) -> Response {
    build_app_with_state(state.clone())
        .oneshot(req)
        .await
        .unwrap()
}

fn set_cookies(resp: &Response) -> Vec<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
}

fn location(resp: &Response) -> String {
    resp.headers()
        .get("location")
        .expect("Location")
        .to_str()
        .unwrap()
        .to_string()
}

async fn body_text(resp: Response) -> String {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What one `/login` produced.
struct Started {
    /// The OAuth `state` sent to the IdP.
    csrf_state: String,
    /// `name=value` of the pre-login cookie, ready for a `Cookie` header.
    cookie: String,
    /// The full `Set-Cookie` line.
    set_cookie: String,
}

async fn start_login(state: &AppState, return_to: Option<&str>, cookie: Option<&str>) -> Started {
    let uri = match return_to {
        Some(target) => format!(
            "/login?{}",
            form_urlencoded::Serializer::new(String::new())
                .append_pair("returnTo", target)
                .finish()
        ),
        None => "/login".to_string(),
    };
    let mut req = Request::builder().uri(uri);
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    let resp = send(state, req.body(Body::empty()).unwrap()).await;
    assert!(resp.status().is_redirection(), "{}", resp.status());
    let idp = url::Url::parse(&location(&resp)).unwrap();
    let csrf_state = idp
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state sent to the IdP");
    let set_cookie = set_cookies(&resp)
        .into_iter()
        .find(|c| c.contains("-login="))
        .expect("pre-login cookie");
    let cookie = set_cookie.split(';').next().unwrap().to_string();
    Started {
        csrf_state,
        cookie,
        set_cookie,
    }
}

async fn finish_login(state: &AppState, csrf_state: &str, cookie: Option<&str>) -> Response {
    let mut req = Request::builder().uri(format!(
        "/login/callback?state={}&code=the-code",
        form_urlencoded::byte_serialize(csrf_state.as_bytes()).collect::<String>()
    ));
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    send(state, req.body(Body::empty()).unwrap()).await
}

fn assert_signed_in(resp: &Response) {
    assert!(resp.status().is_redirection(), "{}", resp.status());
    let cookies = set_cookies(resp);
    assert!(
        cookies.iter().any(|c| c.starts_with("session=")),
        "session cookie set"
    );
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("session-login=;") && c.contains("Max-Age=0")),
        "pre-login cookie cleared: {cookies:?}"
    );
}

#[tokio::test]
#[serial]
async fn login_sets_a_host_only_binding_cookie_and_stores_its_hash() {
    let pool = get_test_pool().await;
    let state = oauth_state(&pool).await;
    let started = start_login(&state, None, None).await;

    let (name, value) = started.cookie.split_once('=').unwrap();
    assert_eq!(name, "session-login");
    assert_eq!(value.len(), 43);
    let attrs = started.set_cookie.to_ascii_lowercase();
    for attr in ["httponly", "samesite=lax", "path=/", "max-age=600"] {
        assert!(attrs.contains(attr), "{attr} in {attrs}");
    }
    assert!(!attrs.contains("domain="), "host-only: {attrs}");
    assert!(!attrs.contains("secure"), "COOKIE_SECURE=false: {attrs}");

    let stored: Option<String> =
        sqlx::query_scalar("SELECT browser_binding_hash FROM oauth_requests WHERE csrf_state = $1")
            .bind(&started.csrf_state)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, Some(sha256_hex(value)));

    // With COOKIE_SECURE the cookie gets the __Host- prefix and Secure.
    let mut secure = state.clone();
    secure.cookie_secure = true;
    let started = start_login(&secure, None, None).await;
    assert!(started.cookie.starts_with("__Host-session-login="));
    assert!(started.set_cookie.contains("; Secure"));
    assert!(!started.set_cookie.to_ascii_lowercase().contains("domain="));
}

#[tokio::test]
#[serial]
async fn callback_completes_only_in_the_browser_that_started_the_login() {
    let pool = get_test_pool().await;
    let state = oauth_state(&pool).await;

    // Same browser: signed in, sent to the web app.
    let started = start_login(&state, None, None).await;
    let resp = finish_login(&state, &started.csrf_state, Some(&started.cookie)).await;
    assert_signed_in(&resp);
    assert_eq!(location(&resp), TEST_UI_ORIGIN);

    // No cookie (the victim of a forwarded callback link): refused, no session.
    let started = start_login(&state, None, None).await;
    let resp = finish_login(&state, &started.csrf_state, None).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(set_cookies(&resp)
        .iter()
        .all(|c| !c.starts_with("session=")));
    assert!(body_text(resp).await.contains("different browser"));
    // The state was consumed by the failed attempt.
    let resp = finish_login(&state, &started.csrf_state, Some(&started.cookie)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(resp).await.contains("invalid OAuth state"));

    // Another browser's cookie: refused.
    let attacker = start_login(&state, None, None).await;
    let victim = start_login(&state, None, None).await;
    let resp = finish_login(&state, &attacker.csrf_state, Some(&victim.cookie)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A malformed cookie value is treated as absent.
    let started = start_login(&state, None, Some("session-login=short")).await;
    assert_ne!(started.cookie, "session-login=short");
    let resp = finish_login(&state, &started.csrf_state, Some("session-login=short")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial]
async fn a_second_tab_does_not_break_the_first_login() {
    let pool = get_test_pool().await;
    let state = oauth_state(&pool).await;
    let first = start_login(&state, None, None).await;
    // The second tab sends the cookie the first one set and keeps it, so
    // whichever tab finishes first completes.
    let second = start_login(&state, None, Some(&first.cookie)).await;
    assert_eq!(second.cookie, first.cookie);
    assert_ne!(second.csrf_state, first.csrf_state);

    let resp = finish_login(&state, &first.csrf_state, Some(&first.cookie)).await;
    assert_signed_in(&resp);
    // That success cleared the cookie; the other tab's callback, arriving
    // without it, is refused (the browser is already signed in).
    let resp = finish_login(&state, &second.csrf_state, None).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial]
async fn rows_from_before_the_binding_are_accepted_for_their_ttl_only() {
    let pool = get_test_pool().await;
    let state = oauth_state(&pool).await;
    let insert = |csrf: String, age: Duration| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO oauth_requests \
                   (pkce_challenge, pkce_verifier, csrf_state, return_to, nonce, created_at) \
                 VALUES ('c', 'v', $1, NULL, NULL, $2)",
            )
            .bind(csrf)
            .bind(Utc::now() - age)
            .execute(&pool)
            .await
            .unwrap();
        }
    };
    let fresh = unique("legacy");
    insert(fresh.clone(), Duration::minutes(1)).await;
    assert_signed_in(&finish_login(&state, &fresh, None).await);

    let stale = unique("legacy");
    insert(stale.clone(), Duration::minutes(11)).await;
    let resp = finish_login(&state, &stale, None).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[serial]
async fn return_targets_survive_the_bound_login() {
    let pool = get_test_pool().await;
    let state = oauth_state(&pool).await;
    let cases = [
        (None, TEST_UI_ORIGIN.to_string()),
        (
            Some("/meeting/abc".to_string()),
            format!("{TEST_UI_ORIGIN}/meeting/abc"),
        ),
        (
            Some("https://evil.example/x".to_string()),
            TEST_UI_ORIGIN.to_string(),
        ),
    ];
    for (return_to, expected) in cases {
        let started = start_login(&state, return_to.as_deref(), None).await;
        let resp = finish_login(&state, &started.csrf_state, Some(&started.cookie)).await;
        assert_signed_in(&resp);
        assert_eq!(location(&resp), expected, "{return_to:?}");
    }
}
