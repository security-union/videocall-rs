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

//! Shared test helpers for meeting-api integration tests.

#![allow(dead_code)]

use axum::body::Body;
use axum::http;
use axum::response::Response;
use axum::Router;
use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use http_body_util::BodyExt;
use meeting_api::config::DeviceAuthConfig;
use meeting_api::db::{self, DbPool};
use meeting_api::secrets::random_base64url;
use meeting_api::{routes, state::AppState, token::generate_session_token};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tower::ServiceExt;
use videocall_meeting_types::device::{
    base64url_encode, crypto, fingerprint, DeviceTokenClaims, CLI_ISSUER,
};

pub const TEST_JWT_SECRET: &str = "test-secret-for-integration-tests";
const TEST_TOKEN_TTL: i64 = 600;
const TEST_SESSION_TTL: i64 = 3600;

/// Connect to the test database using `DATABASE_URL`.
pub async fn get_test_pool() -> DbPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    DbPool::connect(&url)
        .await
        .expect("Failed to connect to test database")
}

/// Delete all test data for a given `room_id` (participants first due to FK).
pub async fn cleanup_test_data(pool: &DbPool, room_id: &str) {
    // `$1` is parsed natively by both the PostgreSQL and SQLite sqlx drivers.
    let delete_participants = "DELETE FROM meeting_participants WHERE meeting_id IN \
         (SELECT id FROM meetings WHERE room_id = $1)";
    let _ = sqlx::query(delete_participants)
        .bind(room_id)
        .execute(pool)
        .await;

    let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .execute(pool)
        .await;
}

pub const TEST_UI_ORIGIN: &str = "http://localhost:3001";
pub const TEST_DEVICE_AUDIENCE: &str = "http://localhost:8081";

/// Build the test `AppState` backed by the given pool.
pub fn build_state(pool: DbPool) -> AppState {
    AppState {
        db: pool,
        jwt_secret: TEST_JWT_SECRET.to_string(),
        token_ttl_secs: TEST_TOKEN_TTL,
        session_ttl_secs: TEST_SESSION_TTL,
        oauth: None,
        jwks_cache: None,
        cookie_domain: None,
        cookie_name: "session".to_string(),
        cookie_secure: false,
        nats: None,
        service_version_urls: Vec::new(),
        http_client: reqwest::Client::new(),
        device: DeviceAuthConfig {
            audience: Some(TEST_DEVICE_AUDIENCE.to_string()),
            ui_origin: Some(TEST_UI_ORIGIN.to_string()),
            browser_origins: vec![TEST_UI_ORIGIN.to_string()],
        },
    }
}

/// Build the Axum router backed by the given pool, ready for `tower::ServiceExt::oneshot`.
pub fn build_app(pool: DbPool) -> Router {
    routes::router().with_state(build_state(pool))
}

/// Build the router from an explicit state (e.g. with OAuth configured).
pub fn build_app_with_state(state: AppState) -> Router {
    routes::router().with_state(state)
}

/// Build an HTTP request with a signed session JWT in the `Cookie: session=<jwt>` header.
///
/// This replaces the old `Cookie: email=<email>` pattern. The JWT is signed
/// with [`TEST_JWT_SECRET`] and contains the email in the `sub` claim.
pub fn request_with_cookie(method: &str, uri: &str, email: &str) -> http::request::Builder {
    let session_jwt = generate_session_token(TEST_JWT_SECRET, email, email, TEST_SESSION_TTL)
        .expect("signing session JWT for test should not fail");
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Cookie", format!("session={session_jwt}"))
}

/// Consume a response body and deserialize JSON into `T`.
pub async fn response_json<T: DeserializeOwned>(resp: Response) -> T {
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    serde_json::from_slice(&bytes).expect("deserialize response body")
}

/// Send one request through a fresh router.
pub async fn send(state: &AppState, req: http::Request<Body>) -> Response {
    build_app_with_state(state.clone())
        .oneshot(req)
        .await
        .expect("oneshot")
}

pub fn bearer(method: &str, uri: &str, token: &str) -> http::request::Builder {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"))
}

// ---------------------------------------------------------------------------
// CLI devices
// ---------------------------------------------------------------------------

pub fn unique_user() -> String {
    format!("dev-{}@test.local", random_base64url(16).to_lowercase())
}

pub fn new_key() -> SigningKey {
    SigningKey::from_bytes(&rand::random())
}

pub fn public_key_of(key: &SigningKey) -> String {
    base64url_encode(&key.verifying_key().to_bytes())
}

/// Sign `claims` as an EdDSA JWT, the way the CLI does.
pub fn sign_eddsa<T: Serialize>(key: &SigningKey, kid: Option<&str>, claims: &T) -> String {
    let key = crypto::encoding_key(key).expect("encoding key");
    crypto::sign(&key, kid, claims).expect("sign")
}

pub fn device_claims(sub: &str, aud: &str, iat: i64, exp: i64) -> DeviceTokenClaims {
    DeviceTokenClaims {
        iss: CLI_ISSUER.to_string(),
        sub: sub.to_string(),
        aud: aud.to_string(),
        iat,
        exp,
        jti: random_base64url(16),
    }
}

/// A valid 120 s device JWT for `device_id`.
pub fn device_jwt(key: &SigningKey, device_id: &str) -> String {
    let now = Utc::now().timestamp();
    let claims = device_claims(device_id, TEST_DEVICE_AUDIENCE, now, now + 120);
    sign_eddsa(key, Some(device_id), &claims)
}

pub struct TestDevice {
    pub id: String,
    pub key: SigningKey,
    pub user_id: String,
}

impl TestDevice {
    pub fn token(&self) -> String {
        device_jwt(&self.key, &self.id)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Fixture {
    Live,
    /// Live, last used two hours ago (due for a touch).
    Stale,
    Revoked,
    IdleExpired,
    /// `idle <= abs` is a schema CHECK, so this is idle-expired too.
    AbsExpired,
}

/// Insert a device directly, skipping the sign-in flow. Its user name is
/// `Name of {user_id}`.
pub async fn insert_device(pool: &DbPool, user_id: &str, fixture: Fixture) -> TestDevice {
    let key = new_key();
    let raw = key.verifying_key().to_bytes();
    let id = random_base64url(16);
    let now = Utc::now();
    let day = Duration::days(1);
    let (last_used, idle, abs) = match fixture {
        Fixture::Live | Fixture::Revoked => (now, now + day * 90, now + day * 365),
        Fixture::Stale => {
            let used = now - Duration::hours(2);
            (used, used + day * 90, now + day * 365)
        }
        Fixture::IdleExpired => (now - day * 91, now - day, now + day * 200),
        Fixture::AbsExpired => (
            now - day,
            now - Duration::minutes(1),
            now - Duration::minutes(1),
        ),
    };
    let device = db::devices::NewDevice {
        id: &id,
        public_key: &base64url_encode(&raw),
        fingerprint: &fingerprint(&raw),
        name: "fixture",
        user_id,
        user_name: &format!("Name of {user_id}"),
        now: last_used,
        idle_expires_at: idle,
        abs_expires_at: abs,
    };
    let inserted = db::devices::insert_device_capped(pool, &device, i64::MAX)
        .await
        .expect("insert device");
    assert_eq!(inserted, db::devices::CappedInsert::Inserted);
    if matches!(fixture, Fixture::Revoked) {
        assert!(db::devices::revoke(pool, &id, user_id, "user", now)
            .await
            .unwrap());
    }
    TestDevice {
        id,
        key,
        user_id: user_id.to_string(),
    }
}

/// Delete the devices and codes of `users`.
pub async fn delete_device_data(pool: &DbPool, users: &[&str]) {
    for user in users {
        for table in ["devices", "device_auth_codes"] {
            let _ = sqlx::query(&format!("DELETE FROM {table} WHERE user_id = $1"))
                .bind(user)
                .execute(pool)
                .await;
        }
    }
}
