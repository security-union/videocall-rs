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

//! The browser half of `videocall-cli auth login` and the device JSON API:
//! `/cli/authorize`, `/cli/consent`, `/cli/code`, `POST /api/v1/devices`,
//! `/api/v1/devices/me`, `/devices`, the post-login return targets, the
//! security headers and CORS split. Run against Postgres or SQLite via
//! `DATABASE_URL`.

mod test_helpers;

use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{self, Request, StatusCode};
use axum::response::Response;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use meeting_api::db::{self, DbPool};
use meeting_api::secrets::{new_id, pkce_s256, random_base64url};
use meeting_api::state::AppState;
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::Value;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use url::form_urlencoded;
use videocall_meeting_types::device::{
    DeviceTokenClaims, RegisterDeviceResponse, RegistrationClaims, CLI_CLIENT_ID, CLI_ISSUER,
};

const AUD: &str = TEST_DEVICE_AUDIENCE;
const PORT: u16 = 53682;

// ---------------------------------------------------------------------------
// The simulated CLI
// ---------------------------------------------------------------------------

struct Cli {
    key: SigningKey,
    verifier: String,
    state: String,
    name: String,
    redirect_uri: String,
}

impl Cli {
    fn loopback() -> Self {
        Self::with_redirect(format!("http://127.0.0.1:{PORT}/callback"))
    }

    fn remote() -> Self {
        Self::with_redirect(format!("{AUD}/cli/code"))
    }

    fn with_redirect(redirect_uri: String) -> Self {
        let mut seed = [0u8; 32];
        OsRng.fill_bytes(&mut seed);
        Self {
            key: SigningKey::from_bytes(&seed),
            verifier: random_base64url(32),
            state: random_base64url(16),
            name: "laptop".to_string(),
            redirect_uri,
        }
    }

    fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.verifying_key().to_bytes())
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        vec![
            ("response_type", "code".into()),
            ("client_id", CLI_CLIENT_ID.into()),
            ("redirect_uri", self.redirect_uri.clone()),
            ("code_challenge", pkce_s256(&self.verifier)),
            ("code_challenge_method", "S256".into()),
            ("state", self.state.clone()),
            ("public_key", self.public_key()),
            ("device_name", self.name.clone()),
        ]
    }

    fn authorize_uri_with(&self, params: &[(&str, String)]) -> String {
        let query: String = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())))
            .finish();
        format!("/cli/authorize?{query}")
    }

    fn authorize_uri(&self) -> String {
        self.authorize_uri_with(&self.params())
    }

    /// The registration JWS, signed by `self.key`.
    fn registration(&self, aud: &str, public_key: &str, name: &str) -> String {
        let now = Utc::now().timestamp();
        let claims = RegistrationClaims {
            iss: CLI_ISSUER.to_string(),
            aud: aud.to_string(),
            iat: now,
            exp: now + 120,
            public_key: public_key.to_string(),
            name: name.to_string(),
        };
        let der = self.key.to_pkcs8_der().expect("pkcs8");
        encode(
            &Header::new(Algorithm::EdDSA),
            &claims,
            &EncodingKey::from_ed_der(der.as_bytes()),
        )
        .expect("sign registration")
    }

    fn exchange_body(&self, code: &str) -> Value {
        serde_json::json!({
            "code": code,
            "code_verifier": self.verifier,
            "redirect_uri": self.redirect_uri,
            "registration": self.registration(AUD, &self.public_key(), &self.name),
        })
    }

    fn device_token(&self, device_id: &str, aud: &str) -> String {
        let now = Utc::now().timestamp();
        let claims = DeviceTokenClaims {
            iss: CLI_ISSUER.to_string(),
            sub: device_id.to_string(),
            aud: aud.to_string(),
            iat: now,
            exp: now + 120,
            jti: new_id(),
        };
        let header = Header {
            alg: Algorithm::EdDSA,
            kid: Some(device_id.to_string()),
            ..Header::new(Algorithm::EdDSA)
        };
        let der = self.key.to_pkcs8_der().expect("pkcs8");
        encode(&header, &claims, &EncodingKey::from_ed_der(der.as_bytes())).expect("sign")
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn state(pool: &DbPool) -> AppState {
    let mut state = build_state(pool.clone());
    state.oauth = Some(test_oauth_config());
    state
}

async fn send(state: &AppState, req: Request<Body>) -> Response {
    build_app_with_state(state.clone())
        .oneshot(req)
        .await
        .expect("oneshot")
}

async fn body_text(resp: Response) -> String {
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

fn header(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn location(resp: &Response) -> String {
    header(resp, "location").expect("Location header")
}

fn get(uri: &str) -> http::request::Builder {
    Request::builder().method("GET").uri(uri)
}

fn unique_user() -> String {
    format!("pages-{}@test.local", new_id().to_lowercase())
}

fn cookie_get(uri: &str, user: &str) -> Request<Body> {
    request_with_cookie("GET", uri, user)
        .body(Body::empty())
        .unwrap()
}

/// A consent form POST as the browser sends it.
fn consent_post(user: Option<&str>) -> http::request::Builder {
    let builder = match user {
        Some(user) => request_with_cookie("POST", "/cli/consent", user),
        None => Request::builder().method("POST").uri("/cli/consent"),
    };
    builder
        .header("Origin", AUD)
        .header("Sec-Fetch-Site", "same-origin")
        .header("Content-Type", "application/x-www-form-urlencoded")
}

fn form(fields: &[(&str, &str)]) -> Body {
    Body::from(
        form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish(),
    )
}

async fn reset(pool: &DbPool) {
    sqlx::query("DELETE FROM cli_auth_requests")
        .execute(pool)
        .await
        .expect("reset requests");
}

async fn delete_user_devices(pool: &DbPool, user: &str) {
    let _ = sqlx::query("DELETE FROM devices WHERE user_id = $1")
        .bind(user)
        .execute(pool)
        .await;
}

/// `GET /cli/authorize` and return the request id from the redirect.
async fn authorize(state: &AppState, cli: &Cli) -> String {
    let resp = send(
        state,
        get(&cli.authorize_uri())
            .header("X-Forwarded-For", "203.0.113.7")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND, "authorize redirects");
    let loc = location(&resp);
    let id = loc
        .strip_prefix("/cli/consent?request=")
        .unwrap_or_else(|| panic!("unexpected Location {loc}"));
    assert_eq!(id.len(), 22);
    id.to_string()
}

fn extract_after<'a>(html: &'a str, marker: &str) -> &'a str {
    let start = html
        .find(marker)
        .unwrap_or_else(|| panic!("{marker} in page"))
        + marker.len();
    let end = html[start..].find('"').expect("closing quote") + start;
    &html[start..end]
}

/// Render the consent page as `user` and return the nonce.
async fn consent_nonce(state: &AppState, request_id: &str, user: &str) -> String {
    let resp = send(
        state,
        cookie_get(&format!("/cli/consent?request={request_id}"), user),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    extract_after(&html, "name=\"nonce\" value=\"").to_string()
}

async fn decide(
    state: &AppState,
    request_id: &str,
    nonce: &str,
    user: &str,
    decision: &str,
    confirm: bool,
) -> Response {
    let mut fields = vec![
        ("request", request_id),
        ("nonce", nonce),
        ("decision", decision),
    ];
    if confirm {
        fields.push(("confirm", "yes"));
    }
    send(state, consent_post(Some(user)).body(form(&fields)).unwrap()).await
}

fn query_pairs(location: &str) -> Vec<(String, String)> {
    url::Url::parse(location)
        .expect("absolute Location")
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn pair<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Full loopback approval; returns the code delivered to the CLI listener.
async fn approve_loopback(state: &AppState, cli: &Cli, user: &str) -> String {
    let id = authorize(state, cli).await;
    let nonce = consent_nonce(state, &id, user).await;
    let resp = decide(state, &id, &nonce, user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = location(&resp);
    assert!(loc.starts_with(&format!("http://127.0.0.1:{PORT}/callback?")));
    let pairs = query_pairs(&loc);
    assert_eq!(pair(&pairs, "state"), Some(cli.state.as_str()));
    pair(&pairs, "code").expect("code").to_string()
}

/// Full remote approval; returns the code shown on the page.
async fn approve_remote(state: &AppState, cli: &Cli, user: &str) -> String {
    let id = authorize(state, cli).await;
    let nonce = consent_nonce(state, &id, user).await;
    let resp = decide(state, &id, &nonce, user, "approve", true).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(header(&resp, "location").is_none());
    let html = body_text(resp).await;
    extract_after(&html, "readonly value=\"").to_string()
}

async fn exchange_json(state: &AppState, body: &Value) -> Response {
    send(
        state,
        Request::builder()
            .method("POST")
            .uri("/api/v1/devices")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn assert_grant_error(resp: Response, code: &str) {
    let status = resp.status();
    let json: Value = response_json(resp).await;
    assert_eq!(json["error"], code, "{json}");
    assert!(json["error_description"].is_string());
    assert!(status.is_client_error());
}

// ---------------------------------------------------------------------------
// /cli/authorize
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn authorize_accepts_loopback_and_remote_redirects() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    for cli in [Cli::loopback(), Cli::remote()] {
        let id = authorize(&st, &cli).await;
        let (kind, host): (String, Option<String>) = sqlx::query_as(
            "SELECT redirect_kind, redirect_host FROM cli_auth_requests WHERE id = $1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        if cli.redirect_uri.ends_with("/cli/code") {
            assert_eq!((kind.as_str(), host), ("remote", None));
        } else {
            assert_eq!(kind, "loopback");
        }
    }
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn authorize_never_redirects_on_invalid_input() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let cli = Cli::loopback();
    let mut weak = [0u8; 32];
    weak[0] = 1;
    let cases: Vec<(&str, String)> = vec![
        ("redirect_uri", "https://evil.example/cli/code".into()),
        ("redirect_uri", "http://localhost:53682/callback".into()),
        ("redirect_uri", "http://[::1]:53682/callback".into()),
        ("redirect_uri", "http://127.0.0.1:53682/callback?x=1".into()),
        ("redirect_uri", "http://127.0.0.1:80/callback".into()),
        ("redirect_uri", "http://evil.example:53682/callback".into()),
        ("redirect_uri", format!("{AUD}/cli/code/")),
        ("redirect_uri", "http://localhost:9999/cli/code".into()),
        ("redirect_uri", String::new()),
        ("response_type", "token".into()),
        ("client_id", "someone-else".into()),
        ("code_challenge_method", "plain".into()),
        ("code_challenge", "short".into()),
        ("state", "short".into()),
        ("public_key", URL_SAFE_NO_PAD.encode(weak)),
        ("public_key", "not-a-key".into()),
        ("device_name", "   ".into()),
        ("device_name", "x".repeat(65)),
    ];
    for (field, value) in cases {
        let params: Vec<(&str, String)> = cli
            .params()
            .into_iter()
            .map(|(k, v)| {
                if k == field {
                    (k, value.clone())
                } else {
                    (k, v)
                }
            })
            .collect();
        let resp = send(
            &st,
            get(&cli.authorize_uri_with(&params))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{field}={value:?}");
        assert!(header(&resp, "location").is_none(), "{field}={value:?}");
        assert!(header(&resp, "content-security-policy").is_some());
    }
    // A missing parameter and a missing query string are rejected the same way.
    let missing: Vec<(&str, String)> = cli
        .params()
        .into_iter()
        .filter(|(k, _)| *k != "state")
        .collect();
    for uri in [cli.authorize_uri_with(&missing), "/cli/authorize".into()] {
        let resp = send(&st, get(&uri).body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(header(&resp, "location").is_none());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cli_auth_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0, "nothing stored for invalid requests");
}

#[tokio::test]
#[serial]
async fn authorize_rejects_an_already_registered_key() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let cli = Cli::loopback();
    let code = approve_loopback(&st, &cli, &user).await;
    let resp = exchange_json(&st, &cli.exchange_body(&code)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = send(&st, get(&cli.authorize_uri()).body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(header(&resp, "location").is_none());
    delete_user_devices(&pool, &user).await;
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn unauthenticated_request_rows_are_capped() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let mut st = state(&pool);
    st.device.max_pending_per_ip = 2;
    st.device.max_pending = 3;
    let from = |ip: &str| {
        let cli = Cli::loopback();
        get(&cli.authorize_uri())
            .header("X-Forwarded-For", ip)
            .body(Body::empty())
            .unwrap()
    };
    // Per IP.
    for _ in 0..2 {
        assert_eq!(
            send(&st, from("198.51.100.1")).await.status(),
            StatusCode::FOUND
        );
    }
    let resp = send(&st, from("198.51.100.1")).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // Global: one more from another IP fills it, the next is refused.
    assert_eq!(
        send(&st, from("198.51.100.2")).await.status(),
        StatusCode::FOUND
    );
    let resp = send(&st, from("198.51.100.3")).await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // Expired requests do not count.
    sqlx::query("UPDATE cli_auth_requests SET expires_at = $1")
        .bind(Utc::now() - Duration::seconds(1))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(&st, from("198.51.100.1")).await.status(),
        StatusCode::FOUND
    );
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn request_sweep_is_bounded() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let old = Utc::now() - Duration::hours(3);
    for _ in 0..(db::SWEEP_BATCH + 5) {
        let pk = URL_SAFE_NO_PAD.encode([7u8; 32]);
        sqlx::query(
            "INSERT INTO cli_auth_requests (id, code_challenge, redirect_kind, client_state, \
               public_key, device_name, created_at, expires_at) \
             VALUES ($1, $2, 'remote', $3, $4, 'x', $5, $5)",
        )
        .bind(new_id())
        .bind("A".repeat(43))
        .bind("s".repeat(16))
        .bind(pk)
        .bind(old)
        .execute(&pool)
        .await
        .unwrap();
    }
    let now = Utc::now();
    let first = db::cli_auth::sweep_requests(&pool, now).await.unwrap();
    assert_eq!(first, db::SWEEP_BATCH as u64);
    let second = db::cli_auth::sweep_requests(&pool, now).await.unwrap();
    assert_eq!(second, 5);
}

#[tokio::test]
#[serial]
async fn per_client_cap_counts_only_requests_awaiting_a_decision() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let mut st = state(&pool);
    st.device.max_pending_per_ip = 2;
    st.device.max_pending = 100;
    let from = |ip: &str| {
        get(&Cli::loopback().authorize_uri())
            .header("X-Forwarded-For", ip)
            .body(Body::empty())
            .unwrap()
    };
    for _ in 0..2 {
        assert_eq!(
            send(&st, from("198.51.100.9")).await.status(),
            StatusCode::FOUND
        );
    }
    assert_eq!(
        send(&st, from("198.51.100.9")).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // Decided (approved, denied or consumed) rows no longer hold a slot,
    // for the per-client cap or the global one.
    sqlx::query(
        "UPDATE cli_auth_requests SET decided_at = $1, decision = 'denied', user_id = 'someone'",
    )
    .bind(Utc::now())
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        send(&st, from("198.51.100.9")).await.status(),
        StatusCode::FOUND
    );
    let open = db::cli_auth::count_open_requests(&pool, Utc::now(), None)
        .await
        .unwrap();
    assert_eq!(open, 1);
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn per_client_cap_skips_infrastructure_addresses_and_groups_ipv6_by_64() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let mut st = state(&pool);
    st.device.max_pending_per_ip = 2;
    st.device.max_pending = 100;
    let from = |ip: Option<&str>| {
        let mut req = get(&Cli::loopback().authorize_uri());
        if let Some(ip) = ip {
            req = req.header("X-Forwarded-For", ip);
        }
        req.body(Body::empty()).unwrap()
    };
    // An ingress hiding client addresses must not turn the per-client cap
    // into a global one: loopback, RFC 1918, CGNAT, link-local, ULA and a
    // missing address are all exempt (the global cap still applies).
    for ip in [
        Some("127.0.0.1"),
        Some("10.0.0.5"),
        Some("192.168.1.4"),
        Some("100.64.3.2"),
        Some("169.254.0.9"),
        Some("fd00::7"),
        None,
    ] {
        for _ in 0..3 {
            assert_eq!(
                send(&st, from(ip)).await.status(),
                StatusCode::FOUND,
                "{ip:?}"
            );
        }
    }
    let exempt: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM cli_auth_requests WHERE requester_cap_key IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(exempt, 21);

    // Two addresses in one IPv6 /64 share an allowance; another /64 does not.
    assert_eq!(
        send(&st, from(Some("2001:db8:5:6::1"))).await.status(),
        StatusCode::FOUND
    );
    assert_eq!(
        send(&st, from(Some("2001:db8:5:6:ffff::2"))).await.status(),
        StatusCode::FOUND
    );
    assert_eq!(
        send(&st, from(Some("2001:db8:5:6:abcd::3"))).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        send(&st, from(Some("2001:db8:5:7::1"))).await.status(),
        StatusCode::FOUND
    );

    // The global cap still counts exempt rows.
    st.device.max_pending = 24;
    assert_eq!(
        send(&st, from(Some("10.0.0.5"))).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn authorize_refuses_subresource_and_script_requests() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let cli = Cli::loopback();
    // An <img>, a fetch() and an iframe on another site: 400, nothing stored.
    for (mode, dest) in [
        (Some("no-cors"), Some("image")),
        (Some("cors"), Some("empty")),
        (Some("navigate"), Some("iframe")),
        (Some("no-cors"), None),
        (None, Some("image")),
    ] {
        let mut req = get(&cli.authorize_uri());
        if let Some(mode) = mode {
            req = req.header("Sec-Fetch-Mode", mode);
        }
        if let Some(dest) = dest {
            req = req.header("Sec-Fetch-Dest", dest);
        }
        let resp = send(&st, req.body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{mode:?} {dest:?}");
        assert!(header(&resp, "location").is_none());
    }
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cli_auth_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);

    // A top-level navigation, and a client without Fetch Metadata, are fine.
    let resp = send(
        &st,
        get(&cli.authorize_uri())
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Site", "none")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    authorize(&st, &Cli::loopback()).await;
    reset(&pool).await;
}

// ---------------------------------------------------------------------------
// /cli/consent: session, return target, CSRF
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn consent_requires_a_browser_session_cookie() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let id = authorize(&st, &Cli::loopback()).await;
    let expected_login = format!(
        "/login?{}",
        form_urlencoded::Serializer::new(String::new())
            .append_pair("returnTo", &format!("{AUD}/cli/consent?request={id}"))
            .finish()
    );
    let session =
        meeting_api::token::generate_session_token(TEST_JWT_SECRET, &user, &user, 600).unwrap();

    // No cookie, a session JWT as bearer, a device JWT as bearer: all are
    // treated as signed out and sent to /login.
    let device_cli = Cli::loopback();
    let device_jwt = device_cli.device_token(&new_id(), AUD);
    for auth in [None, Some(session.clone()), Some(device_jwt.clone())] {
        let mut req = get(&format!("/cli/consent?request={id}"));
        if let Some(token) = auth {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = send(&st, req.body(Body::empty()).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(location(&resp), expected_login);
    }

    // A POST without a cookie renders SignedOut, even with a bearer.
    let nonce = consent_nonce(&st, &id, &user).await;
    let fields = [
        ("request", id.as_str()),
        ("nonce", nonce.as_str()),
        ("decision", "approve"),
    ];
    for auth in [None, Some(session), Some(device_jwt)] {
        let mut req = consent_post(None);
        if let Some(token) = auth {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = send(&st, req.body(form(&fields)).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(body_text(resp).await.contains("signed out"));
    }
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn consent_page_shows_no_requester_address() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let id = authorize(&st, &Cli::remote()).await;
    let resp = send(
        &st,
        cookie_get(&format!("/cli/consent?request={id}"), &user),
    )
    .await;
    let html = body_text(resp).await;
    assert!(!html.contains("203.0.113.7"), "requester IP shown");
    assert!(!html.contains("Request came from"));
    assert!(!html.contains("different network"));
    reset(&pool).await;
}

fn sign_out_post(origin: &str, fields: &[(&str, &str)], user: &str) -> Request<Body> {
    request_with_cookie("POST", "/cli/sign-out", user)
        .header("Origin", origin)
        .header("Sec-Fetch-Site", "same-origin")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form(fields))
        .unwrap()
}

#[tokio::test]
#[serial]
async fn sign_out_from_the_device_pages_is_a_post_that_returns_to_the_request() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let id = authorize(&st, &Cli::loopback()).await;

    // The consent page offers a POST form, not a /logout link.
    let html = body_text(
        send(
            &st,
            cookie_get(&format!("/cli/consent?request={id}"), &user),
        )
        .await,
    )
    .await;
    assert!(!html.contains("/logout"));
    assert!(html.contains("<form method=\"post\" action=\"/cli/sign-out\""));
    assert!(html.contains(&format!("name=\"request\" value=\"{id}\"")));
    let html = body_text(send(&st, cookie_get("/devices", &user)).await).await;
    assert!(html.contains("action=\"/cli/sign-out\""));

    // Signed out, cookie cleared, a sign-in link back to this request.
    let resp = send(&st, sign_out_post(AUD, &[("request", &id)], &user)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let cleared = header(&resp, "set-cookie").expect("Set-Cookie");
    assert!(
        cleared.starts_with("session=;") && cleared.contains("Max-Age=0"),
        "{cleared}"
    );
    let html = body_text(resp).await;
    assert!(html.contains("Signed out"));
    let expected = form_urlencoded::Serializer::new(String::new())
        .append_pair("returnTo", &format!("{AUD}/cli/consent?request={id}"))
        .finish();
    assert!(
        html.contains(&format!("href=\"/login?{expected}\"")),
        "{html}"
    );

    // No or bad request id: back to /devices.
    for fields in [vec![], vec![("request", "../../evil")]] {
        let html = body_text(send(&st, sign_out_post(AUD, &fields, &user)).await).await;
        let expected = form_urlencoded::Serializer::new(String::new())
            .append_pair("returnTo", &format!("{AUD}/devices"))
            .finish();
        assert!(html.contains(&expected), "{html}");
    }

    // Cross-site POST: rejected, the cookie stays.
    let resp = send(&st, sign_out_post("https://evil.example", &[], &user)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(header(&resp, "set-cookie").is_none());
    reset(&pool).await;
}

/// Run `/login?returnTo=…` and return what was stored as the return target.
async fn stored_return_to(state: &AppState, pool: &DbPool, return_to: &str) -> Option<String> {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("returnTo", return_to)
        .finish();
    let resp = send(
        state,
        get(&format!("/login?{query}")).body(Body::empty()).unwrap(),
    )
    .await;
    assert!(resp.status().is_redirection(), "{return_to}");
    let pairs = query_pairs(&location(&resp));
    let csrf = pair(&pairs, "state").expect("state").to_string();
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT return_to FROM oauth_requests WHERE csrf_state = $1",
    )
    .bind(csrf)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[serial]
async fn login_return_target_is_a_closed_allowlist_on_the_api_origin() {
    let pool = get_test_pool().await;
    let mut st = state(&pool);
    st.oauth = Some(test_oauth_config());
    let id = "aB3_-aB3_-aB3_-aB3_-aB";
    for good in [
        format!("{AUD}/cli/consent?request={id}"),
        format!("{AUD}/devices"),
    ] {
        assert_eq!(
            stored_return_to(&st, &pool, &good).await,
            Some(good.clone())
        );
    }
    for bad in [
        format!("{AUD}/login"),
        format!("{AUD}/logout"),
        format!("{AUD}/api/v1/meetings"),
        format!("{AUD}/api/v1/devices/me"),
        format!("{AUD}//evil.example"),
        format!("{AUD}/device@evil.example"),
        format!("{AUD}/device/../x"),
        format!("{AUD}/devices/../login"),
        format!("{AUD}/devices?x=1"),
        format!("{AUD}/cli/consent?request={id}&next=//evil.example"),
        format!("{AUD}/cli/consent?request=short"),
        format!("{AUD}/cli/code"),
        format!("{AUD}/cli/authorize"),
        "//evil.example/devices".to_string(),
        "https://evil.example/devices".to_string(),
    ] {
        assert_eq!(stored_return_to(&st, &pool, &bad).await, None, "{bad}");
    }
    // The existing UI targets are unchanged.
    assert_eq!(
        stored_return_to(&st, &pool, "/meeting/abc").await,
        Some("/meeting/abc".to_string())
    );
    assert_eq!(
        stored_return_to(&st, &pool, &format!("{TEST_UI_ORIGIN}/meeting/abc")).await,
        Some(format!("{TEST_UI_ORIGIN}/meeting/abc"))
    );
    // With device auth off the API origin is not a target at all.
    st.device.audience = None;
    assert_eq!(
        stored_return_to(&st, &pool, &format!("{AUD}/devices")).await,
        None
    );
}

#[tokio::test]
#[serial]
async fn consent_post_csrf_and_replay_rules() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let id = authorize(&st, &Cli::loopback()).await;
    let nonce = consent_nonce(&st, &id, &user).await;
    let fields = [
        ("request", id.as_str()),
        ("nonce", nonce.as_str()),
        ("decision", "approve"),
    ];

    // Origin, Sec-Fetch-Site and content type.
    let base = || request_with_cookie("POST", "/cli/consent", &user);
    let bad_requests = [
        base().header("Content-Type", "application/x-www-form-urlencoded"),
        base()
            .header("Origin", "https://evil.example")
            .header("Content-Type", "application/x-www-form-urlencoded"),
        base()
            .header("Origin", "null")
            .header("Content-Type", "application/x-www-form-urlencoded"),
        base()
            .header("Origin", AUD)
            .header("Sec-Fetch-Site", "cross-site")
            .header("Content-Type", "application/x-www-form-urlencoded"),
        base()
            .header("Origin", AUD)
            .header("Content-Type", "application/json"),
        base().header("Origin", AUD),
    ];
    for req in bad_requests {
        let resp = send(&st, req.body(form(&fields)).unwrap()).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    // JSON bodies are refused even with a correct Origin.
    let json = serde_json::json!({"request": id, "nonce": nonce, "decision": "approve"});
    let resp = send(
        &st,
        base()
            .header("Origin", AUD)
            .header("Content-Type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Wrong nonce, and the right nonce from another user.
    let resp = decide(&st, &id, "wrong-nonce", &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let other = unique_user();
    let resp = decide(&st, &id, &nonce, &other, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Still open: the right user approves, then a replay is refused.
    let resp = decide(&st, &id, &nonce, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let resp = decide(&st, &id, &nonce, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = decide(&st, &id, &nonce, &user, "deny", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // A decided request no longer renders.
    let resp = send(
        &st,
        cookie_get(&format!("/cli/consent?request={id}"), &user),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn expired_requests_and_nonces_cannot_be_decided() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();

    let id = authorize(&st, &Cli::loopback()).await;
    let nonce = consent_nonce(&st, &id, &user).await;
    sqlx::query("UPDATE cli_auth_requests SET expires_at = $2 WHERE id = $1")
        .bind(&id)
        .bind(Utc::now() - Duration::seconds(1))
        .execute(&pool)
        .await
        .unwrap();
    let resp = decide(&st, &id, &nonce, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = send(
        &st,
        cookie_get(&format!("/cli/consent?request={id}"), &user),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let id = authorize(&st, &Cli::loopback()).await;
    let nonce = consent_nonce(&st, &id, &user).await;
    sqlx::query("UPDATE cli_auth_requests SET approval_nonce_expires_at = $2 WHERE id = $1")
        .bind(&id)
        .bind(Utc::now() - Duration::seconds(1))
        .execute(&pool)
        .await
        .unwrap();
    let resp = decide(&st, &id, &nonce, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // A re-render mints a new nonce and the old one stops working.
    let fresh = consent_nonce(&st, &id, &user).await;
    assert_ne!(fresh, nonce);
    let resp = decide(&st, &id, &nonce, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = decide(&st, &id, &fresh, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn deny_redirects_loopback_and_renders_denied_for_remote() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();

    let cli = Cli::loopback();
    let id = authorize(&st, &cli).await;
    let nonce = consent_nonce(&st, &id, &user).await;
    let resp = decide(&st, &id, &nonce, &user, "deny", false).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let pairs = query_pairs(&location(&resp));
    assert_eq!(pair(&pairs, "error"), Some("access_denied"));
    assert_eq!(pair(&pairs, "state"), Some(cli.state.as_str()));
    assert_eq!(pair(&pairs, "code"), None);

    let id = authorize(&st, &Cli::remote()).await;
    let nonce = consent_nonce(&st, &id, &user).await;
    let resp = decide(&st, &id, &nonce, &user, "deny", false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(header(&resp, "location").is_none());
    assert!(body_text(resp).await.contains("Request denied"));
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn remote_approval_needs_the_checkbox_and_shows_a_no_store_code() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let cli = Cli::remote();
    let id = authorize(&st, &cli).await;
    let nonce = consent_nonce(&st, &id, &user).await;

    let resp = decide(&st, &id, &nonce, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(body_text(resp).await.contains("Confirmation needed"));

    let resp = decide(&st, &id, &nonce, &user, "approve", true).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(header(&resp, "location").is_none());
    assert_eq!(header(&resp, "cache-control").as_deref(), Some("no-store"));
    assert_eq!(
        header(&resp, "referrer-policy").as_deref(),
        Some("no-referrer")
    );
    let html = body_text(resp).await;
    let code = extract_after(&html, "readonly value=\"").to_string();
    assert!(code.len() >= 43);

    // GET /cli/code never shows a code.
    let resp = send(
        &st,
        get(&format!("/cli/code?code={code}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!body_text(resp).await.contains(&code));

    let resp = exchange_json(&st, &cli.exchange_body(&code)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    delete_user_devices(&pool, &user).await;
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn loopback_consent_page_allows_only_its_callback_as_form_target() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let id = authorize(&st, &Cli::loopback()).await;
    let resp = send(
        &st,
        cookie_get(&format!("/cli/consent?request={id}"), &user),
    )
    .await;
    let csp = header(&resp, "content-security-policy").unwrap();
    assert!(csp.contains(&format!(
        "form-action 'self' http://127.0.0.1:{PORT}/callback;"
    )));
    assert!(csp.contains("default-src 'none'"));

    let id = authorize(&st, &Cli::remote()).await;
    let resp = send(
        &st,
        cookie_get(&format!("/cli/consent?request={id}"), &user),
    )
    .await;
    let csp = header(&resp, "content-security-policy").unwrap();
    assert!(csp.contains("form-action 'self';"));
    let html = body_text(resp).await;
    assert!(html.contains("name=\"confirm\""));
    reset(&pool).await;
}

// ---------------------------------------------------------------------------
// POST /api/v1/devices
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn code_exchange_registers_a_device_once() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let cli = Cli::loopback();
    let code = approve_loopback(&st, &cli, &user).await;

    let resp = exchange_json(&st, &cli.exchange_body(&code)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(header(&resp, "cache-control").as_deref(), Some("no-store"));
    let device: RegisterDeviceResponse = response_json(resp).await;
    assert_eq!(device.user_id, user);
    assert_eq!(device.name, "laptop");
    assert_eq!(
        device.fingerprint,
        videocall_meeting_types::device::fingerprint(&cli.key.verifying_key().to_bytes())
    );

    // decided_at is the consent decision, not the registration time.
    let (approved_at, registered_decided_at): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT r.decided_at, d.decided_at FROM cli_auth_requests r \
         JOIN devices d ON d.public_key = r.public_key WHERE d.id = $1",
    )
    .bind(&device.device_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(approved_at, registered_decided_at);

    // The device can call /devices/me.
    let token = cli.device_token(&device.device_id, AUD);
    let resp = send(
        &st,
        get("/api/v1/devices/me")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let me: Value = response_json(resp).await;
    assert_eq!(me["device_id"], device.device_id);
    assert_eq!(me["status"], "approved");

    // Code reuse.
    let resp = exchange_json(&st, &cli.exchange_body(&code)).await;
    assert_grant_error(resp, "invalid_grant").await;

    delete_user_devices(&pool, &user).await;
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn a_wrong_verifier_burns_the_code() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let cli = Cli::loopback();
    let code = approve_loopback(&st, &cli, &user).await;
    let mut body = cli.exchange_body(&code);
    body["code_verifier"] = Value::String(random_base64url(32));
    assert_grant_error(exchange_json(&st, &body).await, "invalid_grant").await;
    // The right verifier is now too late.
    assert_grant_error(
        exchange_json(&st, &cli.exchange_body(&code)).await,
        "invalid_grant",
    )
    .await;
    let devices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE user_id = $1")
        .bind(&user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(devices, 0);
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn exchange_rejects_mismatched_redirect_key_name_and_audience() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let other_key = Cli::loopback();

    type Tamper = fn(&Cli, &Cli, &mut Value);
    let cases: [(&str, Tamper); 5] = [
        ("redirect_uri mismatch", |_, _, body| {
            body["redirect_uri"] = Value::String("http://127.0.0.1:50000/callback".into());
        }),
        ("JWS for a different key", |_, other, body| {
            body["registration"] =
                Value::String(other.registration(AUD, &other.public_key(), "laptop"));
        }),
        ("JWS with a different name", |cli, _, body| {
            body["registration"] =
                Value::String(cli.registration(AUD, &cli.public_key(), "other name"));
        }),
        ("JWS with a different aud", |cli, _, body| {
            body["registration"] = Value::String(cli.registration(
                "https://elsewhere.example",
                &cli.public_key(),
                "laptop",
            ));
        }),
        (
            "JWS claiming the key but signed by another",
            |cli, other, body| {
                body["registration"] =
                    Value::String(other.registration(AUD, &cli.public_key(), "laptop"));
            },
        ),
    ];
    for (label, tamper) in cases {
        let cli = Cli::loopback();
        let code = approve_loopback(&st, &cli, &user).await;
        let mut body = cli.exchange_body(&code);
        tamper(&cli, &other_key, &mut body);
        let resp = exchange_json(&st, &body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{label}");
        assert_grant_error(resp, "invalid_grant").await;
        // Burned either way.
        assert_grant_error(
            exchange_json(&st, &cli.exchange_body(&code)).await,
            "invalid_grant",
        )
        .await;
    }
    // Shape errors are invalid_request / 415.
    let resp = send(
        &st,
        Request::builder()
            .method("POST")
            .uri("/api/v1/devices")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::from("code=x"))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let resp = exchange_json(&st, &serde_json::json!({"code": "x"})).await;
    assert_grant_error(resp, "invalid_request").await;
    reset(&pool).await;
}

async fn code_ttl(pool: &DbPool, code: &str) -> i64 {
    let (decided, expires): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT decided_at, code_expires_at FROM cli_auth_requests WHERE code_hash = $1",
    )
    .bind(meeting_api::secrets::sha256_hex(code))
    .fetch_one(pool)
    .await
    .unwrap();
    (expires - decided).num_seconds()
}

#[tokio::test]
#[serial]
async fn codes_expire_after_60s_loopback_and_300s_remote() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();

    let loopback = Cli::loopback();
    let code = approve_loopback(&st, &loopback, &user).await;
    assert_eq!(code_ttl(&pool, &code).await, 60);
    let remote = Cli::remote();
    let remote_code = approve_remote(&st, &remote, &user).await;
    assert_eq!(code_ttl(&pool, &remote_code).await, 300);

    sqlx::query("UPDATE cli_auth_requests SET code_expires_at = $1 WHERE code_hash IS NOT NULL")
        .bind(Utc::now() - Duration::seconds(1))
        .execute(&pool)
        .await
        .unwrap();
    assert_grant_error(
        exchange_json(&st, &loopback.exchange_body(&code)).await,
        "invalid_grant",
    )
    .await;
    assert_grant_error(
        exchange_json(&st, &remote.exchange_body(&remote_code)).await,
        "invalid_grant",
    )
    .await;
    reset(&pool).await;
}

// ---------------------------------------------------------------------------
// Max devices
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn max_devices_is_enforced_at_consent_and_exchange() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let mut st = state(&pool);
    st.device.max_per_user = 1;
    let user = unique_user();

    // Two approvals while the user has none; the first exchange fills the cap.
    let first = Cli::loopback();
    let first_code = approve_loopback(&st, &first, &user).await;
    let second = Cli::loopback();
    let second_code = approve_loopback(&st, &second, &user).await;
    assert_eq!(
        exchange_json(&st, &first.exchange_body(&first_code))
            .await
            .status(),
        StatusCode::CREATED
    );
    let resp = exchange_json(&st, &second.exchange_body(&second_code)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert_grant_error(resp, "too_many_devices").await;

    // At the cap, the consent page refuses to approve.
    let third = Cli::loopback();
    let id = authorize(&st, &third).await;
    let nonce = consent_nonce(&st, &id, &user).await;
    let resp = decide(&st, &id, &nonce, &user, "approve", false).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    assert!(body_text(resp).await.contains("Too many devices"));

    delete_user_devices(&pool, &user).await;
    reset(&pool).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn concurrent_exchanges_cannot_exceed_max_devices() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let mut st = state(&pool);
    st.device.max_per_user = 1;
    let user = unique_user();
    let mut bodies = Vec::new();
    for _ in 0..4 {
        let cli = Cli::loopback();
        let code = approve_loopback(&st, &cli, &user).await;
        bodies.push(cli.exchange_body(&code));
    }
    let tasks: Vec<_> = bodies
        .into_iter()
        .map(|body| {
            let st = st.clone();
            tokio::spawn(async move { exchange_json(&st, &body).await.status() })
        })
        .collect();
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap());
    }
    let created = statuses
        .iter()
        .filter(|s| **s == StatusCode::CREATED)
        .count();
    let conflicts = statuses
        .iter()
        .filter(|s| **s == StatusCode::CONFLICT)
        .count();
    assert_eq!((created, conflicts), (1, 3), "{statuses:?}");
    let live = db::devices::count_live_devices(&pool, &user, Utc::now())
        .await
        .unwrap();
    assert_eq!(live, 1);
    delete_user_devices(&pool, &user).await;
    reset(&pool).await;
}

// ---------------------------------------------------------------------------
// Secrets stay out of URLs and logs
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
#[serial]
async fn codes_nonces_and_tokens_never_reach_the_log() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let logs = LogBuffer::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut secrets = Vec::new();
    for cli in [Cli::remote(), Cli::loopback()] {
        let id = authorize(&st, &cli).await;
        let nonce = consent_nonce(&st, &id, &user).await;
        let resp = decide(&st, &id, &nonce, &user, "approve", true).await;
        let code = if resp.status() == StatusCode::SEE_OTHER {
            let pairs = query_pairs(&location(&resp));
            pair(&pairs, "code").unwrap().to_string()
        } else {
            let html = body_text(resp).await;
            extract_after(&html, "readonly value=\"").to_string()
        };
        let body = cli.exchange_body(&code);
        // A failing exchange with the real code logs too.
        let mut bad = body.clone();
        bad["code_verifier"] = Value::String(random_base64url(32));
        let _ = exchange_json(&st, &bad).await;
        secrets.extend([
            code,
            nonce,
            cli.verifier.clone(),
            body["registration"].as_str().unwrap().to_string(),
        ]);
    }
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("cli login decided"), "logs were captured");
    for secret in &secrets {
        assert!(!text.contains(secret.as_str()), "secret leaked into logs");
    }
    reset(&pool).await;
}

// ---------------------------------------------------------------------------
// /devices and /api/v1/devices/me
// ---------------------------------------------------------------------------

async fn register(st: &AppState, user: &str) -> (Cli, RegisterDeviceResponse) {
    let cli = Cli::loopback();
    let code = approve_loopback(st, &cli, user).await;
    let resp = exchange_json(st, &cli.exchange_body(&code)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    (cli, response_json(resp).await)
}

#[tokio::test]
#[serial]
async fn devices_page_lists_and_revokes_own_devices_only() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let other = unique_user();
    let (cli, device) = register(&st, &user).await;

    // Signed out: redirect to /login with the closed return target.
    let resp = send(&st, get("/devices").body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert!(location(&resp).starts_with("/login?returnTo="));

    let resp = send(&st, cookie_get("/devices", &user)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let html = body_text(resp).await;
    assert!(html.contains(&device.fingerprint));
    let resp = send(&st, cookie_get("/devices", &other)).await;
    assert!(!body_text(resp).await.contains(&device.fingerprint));

    let revoke_uri = format!("/devices/{}/revoke", device.device_id);
    let post = |who: &str| {
        request_with_cookie("POST", &revoke_uri, who)
            .header("Origin", AUD)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap()
    };
    // Cross-site and JSON posts are refused; another user gets 404.
    let resp = send(
        &st,
        request_with_cookie("POST", &revoke_uri, &user)
            .header("Origin", "https://evil.example")
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(
        &st,
        request_with_cookie("POST", &revoke_uri, &user)
            .header("Origin", AUD)
            .header("Content-Type", "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        send(&st, post(&other)).await.status(),
        StatusCode::NOT_FOUND
    );

    let resp = send(&st, post(&user)).await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&resp), "/devices");

    // The device is rejected right away.
    let token = cli.device_token(&device.device_id, AUD);
    let resp = send(
        &st,
        get("/api/v1/devices/me")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    delete_user_devices(&pool, &user).await;
    reset(&pool).await;
}

#[tokio::test]
#[serial]
async fn device_revokes_itself_and_other_audiences_are_rejected() {
    let pool = get_test_pool().await;
    reset(&pool).await;
    let st = state(&pool);
    let user = unique_user();
    let (cli, device) = register(&st, &user).await;
    let me = |token: String, method: &str| {
        Request::builder()
            .method(method)
            .uri("/api/v1/devices/me")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let wrong_aud = cli.device_token(&device.device_id, "https://elsewhere.example");
    let resp = send(&st, me(wrong_aud, "GET")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(header(&resp, "www-authenticate").is_some());

    let resp = send(&st, me(cli.device_token(&device.device_id, AUD), "DELETE")).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(&st, me(cli.device_token(&device.device_id, AUD), "GET")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let reason: Option<String> =
        sqlx::query_scalar("SELECT revoked_reason FROM devices WHERE id = $1")
            .bind(&device.device_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("device"));
    delete_user_devices(&pool, &user).await;
    reset(&pool).await;
}

// ---------------------------------------------------------------------------
// Headers, CORS, configuration
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn pages_and_css_carry_security_headers_and_json_routes_do_not() {
    let pool = get_test_pool().await;
    let st = state(&pool);
    let resp = send(&st, get("/static/auth.css").body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        header(&resp, "content-type").as_deref(),
        Some("text/css; charset=utf-8")
    );
    for name in [
        "content-security-policy",
        "x-frame-options",
        "x-content-type-options",
        "referrer-policy",
        "cross-origin-opener-policy",
    ] {
        assert!(header(&resp, name).is_some(), "{name} on CSS");
    }
    let resp = send(&st, get("/cli/code").body(Body::empty()).unwrap()).await;
    assert_eq!(header(&resp, "x-frame-options").as_deref(), Some("DENY"));
    assert_eq!(header(&resp, "cache-control").as_deref(), Some("no-store"));

    let resp = send(&st, get("/version").body(Body::empty()).unwrap()).await;
    assert!(header(&resp, "content-security-policy").is_none());
    assert!(header(&resp, "x-frame-options").is_none());
}

#[tokio::test]
#[serial]
async fn pages_are_outside_cors() {
    use tower_http::cors::{AllowOrigin, CorsLayer};
    let pool = get_test_pool().await;
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::mirror_request())
        .allow_credentials(true);
    let app = || meeting_api::routes::app(cors.clone()).with_state(state(&pool));
    let evil = "https://evil.example";
    for uri in ["/devices", "/cli/code", "/static/auth.css"] {
        let resp = app()
            .oneshot(get(uri).header("Origin", evil).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(
            header(&resp, "access-control-allow-origin").is_none(),
            "{uri} must not be CORS-readable"
        );
    }
    // JSON routes keep their CORS behaviour.
    let resp = app()
        .oneshot(
            get("/version")
                .header("Origin", evil)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        header(&resp, "access-control-allow-origin").as_deref(),
        Some(evil)
    );
}

#[tokio::test]
#[serial]
async fn device_auth_off_without_an_audience() {
    let pool = get_test_pool().await;
    let mut st = state(&pool);
    st.device.audience = None;
    let user = unique_user();
    let cli = Cli::loopback();
    let resp = send(&st, get(&cli.authorize_uri()).body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(body_text(resp).await.contains("not configured"));
    let resp = send(&st, cookie_get("/devices", &user)).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let resp = exchange_json(&st, &cli.exchange_body("x")).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let resp = send(
        &st,
        get("/api/v1/devices/me")
            .header(
                "Authorization",
                format!("Bearer {}", cli.device_token(&new_id(), AUD)),
            )
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
