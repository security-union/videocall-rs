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

//! The device sign-in JSON API: `GET /cli/authorize`, approve, the CLI's
//! code exchange, and the user's device list and revoke. The CLI side is
//! played with real Ed25519 keys and PKCE.

mod test_helpers;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use chrono::{Duration, Utc};
use meeting_api::db::{self, DbPool};
use meeting_api::routes::devices_api::MAX_DEVICES_PER_USER;
use meeting_api::state::AppState;
use serde_json::{json, Value};
use serial_test::serial;
use test_helpers::*;
use url::Url;
use videocall_meeting_types::device::{
    self, base64url_decode, base64url_encode, crypto, crypto::DeviceKey, decode_authorize_request,
    RegistrationClaims, CLI_ISSUER, CLI_REMOTE_REDIRECT_URI,
};

const AUD: &str = TEST_DEVICE_AUDIENCE;
const APPROVE: &str = "/api/v1/devices/authorizations";

/// The CLI side of one login.
struct Cli {
    key: DeviceKey,
    verifier: String,
    state: String,
    redirect_uri: String,
}

impl Cli {
    fn new(redirect_uri: &str) -> Self {
        Self {
            key: new_key(),
            verifier: crypto::random_b64url(32).unwrap(),
            state: crypto::random_b64url(32).unwrap(),
            redirect_uri: redirect_uri.to_string(),
        }
    }

    fn loopback() -> Self {
        Self::new("http://127.0.0.1:53682/callback")
    }

    fn remote() -> Self {
        Self::new(CLI_REMOTE_REDIRECT_URI)
    }

    /// The approve body the app sends, built from the CLI's query.
    fn approval(&self, confirm: bool) -> Value {
        json!({
            "redirect_uri": self.redirect_uri,
            "code_challenge": device::code_challenge_s256(&self.verifier),
            "code_challenge_method": "S256",
            "state": self.state,
            "public_key": self.key.public_key_b64(),
            "device_name": "laptop",
            "confirm": confirm,
        })
    }

    fn registration(&self, signer: &DeviceKey, aud: &str, public_key: &str, name: &str) -> String {
        let now = Utc::now().timestamp();
        let claims = RegistrationClaims {
            iss: CLI_ISSUER.into(),
            aud: aud.into(),
            iat: now,
            exp: now + 120,
            public_key: public_key.into(),
            name: name.into(),
        };
        sign_eddsa(signer, None, &claims)
    }

    fn exchange_body(&self, code: &str) -> Value {
        json!({
            "code": code,
            "code_verifier": self.verifier,
            "redirect_uri": self.redirect_uri,
            "registration": self.registration(&self.key, AUD, &self.key.public_key_b64(), "laptop"),
        })
    }
}

fn header(resp: &Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn approve(state: &AppState, user: &str, body: &Value) -> Response {
    let req = request_with_cookie("POST", APPROVE, user)
        .header("Origin", TEST_UI_ORIGIN)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, req).await
}

fn query_value(url: &str, key: &str) -> Option<String> {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Approve and return the code (loopback: read from `redirect_to`).
async fn approved_code(state: &AppState, user: &str, cli: &Cli) -> String {
    let resp = approve(state, user, &cli.approval(true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let result = response_json::<Value>(resp).await["result"].clone();
    match result["code"].as_str() {
        Some(code) => code.to_string(),
        None => query_value(result["redirect_to"].as_str().unwrap(), "code").unwrap(),
    }
}

async fn exchange(state: &AppState, body: &Value) -> Response {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/devices")
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, req).await
}

async fn me(state: &AppState, cli: &Cli, device_id: &str) -> StatusCode {
    let token = device_jwt(&cli.key, device_id);
    let req = bearer("GET", "/api/v1/devices/me", &token);
    send(state, req.body(Body::empty()).unwrap()).await.status()
}

/// Approve and exchange; returns the new device id.
async fn sign_in(state: &AppState, user: &str, cli: &Cli) -> String {
    let code = approved_code(state, user, cli).await;
    let resp = exchange(state, &cli.exchange_body(&code)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    response_json::<Value>(resp).await["device_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn expire_codes_of(pool: &DbPool, user: &str) {
    sqlx::query("UPDATE device_auth_codes SET expires_at = $2 WHERE user_id = $1")
        .bind(user)
        .bind(Utc::now() - Duration::seconds(1))
        .execute(pool)
        .await
        .unwrap();
}

async fn live_devices(pool: &DbPool, user: &str) -> i64 {
    db::devices::count_live_devices(pool, user, Utc::now())
        .await
        .unwrap()
}

async fn assert_error(resp: Response, status: StatusCode, code: &str) {
    assert_eq!(resp.status(), status);
    let body: Value = response_json(resp).await;
    assert_eq!(body["success"], false);
    assert_eq!(body["result"]["code"], code, "{body}");
}

async fn assert_cli_error(resp: Response, status: StatusCode, error: &str) {
    assert_eq!(resp.status(), status);
    assert_eq!(response_json::<Value>(resp).await["error"], error);
}

#[tokio::test]
#[serial]
async fn authorize_redirects_to_the_app_with_the_query_verbatim() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    // The raw query reaches the app byte for byte, `+` and `%` included.
    let query = "response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2Fcallback\
                 &device_name=C%2B%2B+rig&state=a%2Bb%25c&x=%3Cscript%3E";
    for query in [query, ""] {
        let resp = send(&state, get(&format!("/cli/authorize?{query}"))).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        assert_eq!(header(&resp, "cache-control").as_deref(), Some("no-store"));
        let location = header(&resp, "location").unwrap();
        let request = location
            .strip_prefix(&format!("{TEST_UI_ORIGIN}/devices/authorize?request="))
            .expect("redirect to the app's consent page");
        assert_eq!(decode_authorize_request(request).as_deref(), Some(query));
    }

    // Over 2048 bytes: 400, no redirect.
    let long = format!("/cli/authorize?state={}", "a".repeat(2043));
    let resp = send(&state, get(&long)).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(header(&resp, "location").is_none());

    // OAuth is not needed (`build_state` has none); a missing audience or
    // app origin is a 503.
    let mut no_audience = state.clone();
    no_audience.device.audience = None;
    let mut no_app = state.clone();
    no_app.device.ui_origin = None;
    for state in [no_audience, no_app] {
        let resp = send(&state, get("/cli/authorize?state=x")).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

#[tokio::test]
#[serial]
async fn loopback_approve_and_exchange() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let mut cli = Cli::loopback();
    // The state is form-encoded into redirect_to, never pasted raw.
    cli.state = "a&b=c#d+e%f/g?h~i!".to_string();

    let resp = approve(&state, &user, &cli.approval(false)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "cache-control").as_deref(), Some("no-store"));
    let body: Value = response_json(resp).await;
    assert!(body["result"].get("code").is_none());
    let redirect_to = Url::parse(body["result"]["redirect_to"].as_str().unwrap()).unwrap();
    assert_eq!(
        &redirect_to[..url::Position::AfterPath],
        "http://127.0.0.1:53682/callback"
    );
    let pairs: Vec<(String, String)> = redirect_to.query_pairs().into_owned().collect();
    assert_eq!(pairs.len(), 2);
    let (key, code) = &pairs[0];
    assert_eq!(key, "code");
    assert_eq!(base64url_decode(code).unwrap().len(), 32);
    assert_eq!(pairs[1], ("state".to_string(), cli.state.clone()));

    // Only the hash is stored.
    let stored: String =
        sqlx::query_scalar("SELECT code_hash FROM device_auth_codes WHERE user_id = $1")
            .bind(&user)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, crypto::sha256_hex(code));

    let resp = exchange(&state, &cli.exchange_body(code)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let device: Value = response_json(resp).await;
    assert_eq!(device["user_id"], user.as_str());
    assert_eq!(device["name"], "laptop");
    assert_eq!(device["fingerprint"], cli.key.fingerprint());
    let device_id = device["device_id"].as_str().unwrap();
    assert_eq!(me(&state, &cli, device_id).await, StatusCode::OK);
    assert_eq!(codes_of(&pool, &user).await, 0, "the code is consumed");

    delete_device_data(&pool, &[&user]).await;
}

#[tokio::test]
#[serial]
async fn remote_approve_needs_confirm() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let mut cli = Cli::remote();
    cli.state = String::new(); // ignored for a remote login

    let mut no_confirm = cli.approval(true);
    no_confirm.as_object_mut().unwrap().remove("confirm");
    for body in [cli.approval(false), no_confirm] {
        let resp = approve(&state, &user, &body).await;
        assert_error(resp, StatusCode::BAD_REQUEST, "CONFIRMATION_REQUIRED").await;
    }
    assert_eq!(codes_of(&pool, &user).await, 0);

    let resp = approve(&state, &user, &cli.approval(true)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "cache-control").as_deref(), Some("no-store"));
    let result = response_json::<Value>(resp).await["result"].clone();
    assert_eq!(result["expires_in"], 300);
    assert!(result.get("redirect_to").is_none());
    let resp = exchange(&state, &cli.exchange_body(result["code"].as_str().unwrap())).await;
    assert_eq!(resp.status(), StatusCode::CREATED);

    delete_device_data(&pool, &[&user]).await;
}

#[tokio::test]
#[serial]
async fn approve_rejects_invalid_input_and_writes_nothing() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let cli = Cli::loopback();

    let cases: Vec<(&str, Value)> = vec![
        ("code_challenge_method", json!("plain")),
        ("code_challenge", json!("short")),
        ("code_challenge", json!(format!("{}=", "A".repeat(43)))),
        ("redirect_uri", json!("https://evil.example/callback")),
        ("state", json!("short")),
        ("state", json!(format!("{} x", "a".repeat(20)))),
        ("public_key", json!("A".repeat(42))),
        ("public_key", json!(base64url_encode(&[0u8; 32]))), // weak
        ("device_name", json!("-leading")),
        ("device_name", json!("caf\u{e9}")),
        ("device_name", json!("")),
    ];
    for (field, value) in cases {
        let mut body = cli.approval(true);
        body[field] = value.clone();
        let resp = approve(&state, &user, &body).await;
        assert_error(resp, StatusCode::BAD_REQUEST, "INVALID_REQUEST").await;
    }
    let not_json = request_with_cookie("POST", APPROVE, &user)
        .header("Origin", TEST_UI_ORIGIN)
        .header("Content-Type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    assert_error(
        send(&state, not_json).await,
        StatusCode::BAD_REQUEST,
        "INVALID_REQUEST",
    )
    .await;
    assert_eq!(codes_of(&pool, &user).await, 0);

    // No session at all.
    let anonymous = Request::builder()
        .method("POST")
        .uri(APPROVE)
        .header("Origin", TEST_UI_ORIGIN)
        .header("Content-Type", "application/json")
        .body(Body::from(cli.approval(true).to_string()))
        .unwrap();
    assert_eq!(
        send(&state, anonymous).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // A key already registered to anyone: 409.
    let other = unique_user();
    let registered = Cli::loopback();
    sign_in(&state, &other, &registered).await;
    let resp = approve(&state, &user, &registered.approval(true)).await;
    assert_error(resp, StatusCode::CONFLICT, "KEY_ALREADY_REGISTERED").await;
    assert_eq!(codes_of(&pool, &user).await, 0);

    delete_device_data(&pool, &[&user, &other]).await;
}

#[tokio::test]
#[serial]
async fn origin_and_content_type_guard() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let body = Cli::loopback().approval(true).to_string();
    let device = insert_device(&pool, &user, Fixture::Live).await;

    for origin in [
        None,
        Some("null"),
        Some("https://evil.example"),
        Some("http://localhost:3001/"),
        Some("http://localhost:30011"),
        Some(AUD),
    ] {
        let approve =
            request_with_cookie("POST", APPROVE, &user).header("Content-Type", "application/json");
        let delete =
            request_with_cookie("DELETE", &format!("/api/v1/devices/{}", device.id), &user);
        for (req, body) in [(approve, body.clone()), (delete, String::new())] {
            let req = match origin {
                Some(origin) => req.header("Origin", origin),
                None => req,
            };
            let resp = send(&state, req.body(Body::from(body)).unwrap()).await;
            assert_error(resp, StatusCode::FORBIDDEN, "ORIGIN_NOT_ALLOWED").await;
        }
    }

    for content_type in [
        None,
        Some("application/x-www-form-urlencoded"),
        Some("text/plain"),
        Some("multipart/form-data; boundary=x"),
    ] {
        let mut req = request_with_cookie("POST", APPROVE, &user).header("Origin", TEST_UI_ORIGIN);
        if let Some(content_type) = content_type {
            req = req.header("Content-Type", content_type);
        }
        let resp = send(&state, req.body(Body::from(body.clone())).unwrap()).await;
        assert_error(
            resp,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "UNSUPPORTED_MEDIA_TYPE",
        )
        .await;
    }

    assert_eq!(codes_of(&pool, &user).await, 0);
    assert_eq!(live_devices(&pool, &user).await, 1, "nothing was revoked");
    delete_device_data(&pool, &[&user]).await;
}

#[tokio::test]
#[serial]
async fn one_outstanding_code_per_user() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let other = unique_user();
    let (first, second, bystander) = (Cli::loopback(), Cli::remote(), Cli::loopback());

    let first_code = approved_code(&state, &user, &first).await;
    let bystander_code = approved_code(&state, &other, &bystander).await;
    let second_code = approved_code(&state, &user, &second).await;
    assert_eq!(codes_of(&pool, &user).await, 1);

    // Approving again replaced the first code; another user's code is kept.
    let resp = exchange(&state, &first.exchange_body(&first_code)).await;
    assert_cli_error(resp, StatusCode::BAD_REQUEST, "invalid_grant").await;
    for (cli, code) in [(&second, &second_code), (&bystander, &bystander_code)] {
        let resp = exchange(&state, &cli.exchange_body(code)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Expired codes of anyone are deleted by the next approval.
    approved_code(&state, &other, &Cli::loopback()).await;
    expire_codes_of(&pool, &other).await;
    approved_code(&state, &user, &Cli::loopback()).await;
    assert_eq!(codes_of(&pool, &other).await, 0);

    delete_device_data(&pool, &[&user, &other]).await;
}

#[tokio::test]
#[serial]
async fn every_invalid_grant_burns_the_code() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let stranger = new_key();
    let stranger_pk = stranger.public_key_b64();

    type Tamper = fn(&Cli, &mut Value, &DeviceKey, &str);
    let tampers: Vec<(&str, Tamper)> = vec![
        ("wrong verifier", |_, body, _, _| {
            body["code_verifier"] = json!(crypto::random_b64url(32).unwrap());
        }),
        ("wrong redirect_uri", |_, body, _, _| {
            body["redirect_uri"] = json!("http://127.0.0.1:53683/callback");
        }),
        ("registration for another key", |cli, body, key, pk| {
            body["registration"] = json!(cli.registration(key, AUD, pk, "laptop"));
        }),
        ("registration with another name", |cli, body, _, _| {
            let pk = cli.key.public_key_b64();
            body["registration"] = json!(cli.registration(&cli.key, AUD, &pk, "desktop"));
        }),
        ("registration for another audience", |cli, body, _, _| {
            let pk = cli.key.public_key_b64();
            let other_aud = "http://localhost:9999";
            body["registration"] = json!(cli.registration(&cli.key, other_aud, &pk, "laptop"));
        }),
        ("registration signed by another key", |cli, body, key, _| {
            let pk = cli.key.public_key_b64();
            body["registration"] = json!(cli.registration(key, AUD, &pk, "laptop"));
        }),
        ("expired code", |_, _, _, _| {}),
    ];
    for (cause, tamper) in tampers {
        let cli = Cli::loopback();
        let code = approved_code(&state, &user, &cli).await;
        if cause == "expired code" {
            expire_codes_of(&pool, &user).await;
        }
        let mut body = cli.exchange_body(&code);
        tamper(&cli, &mut body, &stranger, &stranger_pk);
        let resp = exchange(&state, &body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{cause}");
        assert_eq!(
            response_json::<Value>(resp).await["error"],
            "invalid_grant",
            "{cause}"
        );
        // The honest retry with the same code fails too: it was burned.
        let resp = exchange(&state, &cli.exchange_body(&code)).await;
        assert_cli_error(resp, StatusCode::BAD_REQUEST, "invalid_grant").await;
    }

    // Unknown and reused codes.
    let cli = Cli::loopback();
    let resp = exchange(
        &state,
        &cli.exchange_body(&crypto::random_b64url(32).unwrap()),
    )
    .await;
    assert_cli_error(resp, StatusCode::BAD_REQUEST, "invalid_grant").await;
    let code = approved_code(&state, &user, &cli).await;
    let resp = exchange(&state, &cli.exchange_body(&code)).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = exchange(&state, &cli.exchange_body(&code)).await;
    assert_cli_error(resp, StatusCode::BAD_REQUEST, "invalid_grant").await;
    assert_eq!(
        live_devices(&pool, &user).await,
        1,
        "only the honest exchange"
    );

    delete_device_data(&pool, &[&user]).await;
}

#[tokio::test]
#[serial]
async fn too_many_devices_at_approve_and_at_exchange() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    for _ in 1..MAX_DEVICES_PER_USER {
        insert_device(&pool, &user, Fixture::Live).await;
    }
    // Revoked and expired devices do not count.
    insert_device(&pool, &user, Fixture::Revoked).await;
    insert_device(&pool, &user, Fixture::IdleExpired).await;

    // One slot left: approve succeeds, then the slot is taken before the
    // exchange.
    let cli = Cli::loopback();
    let code = approved_code(&state, &user, &cli).await;
    insert_device(&pool, &user, Fixture::Live).await;
    let resp = exchange(&state, &cli.exchange_body(&code)).await;
    assert_cli_error(resp, StatusCode::CONFLICT, "too_many_devices").await;

    // At the cap, approve refuses and stores nothing.
    let resp = approve(&state, &user, &Cli::loopback().approval(true)).await;
    assert_error(resp, StatusCode::CONFLICT, "TOO_MANY_DEVICES").await;
    assert_eq!(codes_of(&pool, &user).await, 0);

    delete_device_data(&pool, &[&user]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial]
async fn device_cap_holds_under_concurrent_inserts() {
    let pool = get_test_pool().await;
    let user = unique_user();
    const MAX: i64 = 3;
    let now = Utc::now();

    let tasks: Vec<_> = (0..12)
        .map(|_| {
            let (pool, user) = (pool.clone(), user.clone());
            tokio::spawn(async move {
                let key = new_key();
                let device = db::devices::NewDevice {
                    id: &crypto::random_b64url(16).unwrap(),
                    public_key: &key.public_key_b64(),
                    fingerprint: &key.fingerprint(),
                    name: "racer",
                    user_id: &user,
                    user_name: "Racer",
                    now,
                    idle_expires_at: now + Duration::days(90),
                    abs_expires_at: now + Duration::days(365),
                };
                db::devices::insert_device_capped(&pool, &device, MAX).await
            })
        })
        .collect();
    let mut inserted = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(db::devices::CappedInsert::Inserted) => inserted += 1,
            Ok(db::devices::CappedInsert::TooManyDevices) => {}
            // SQLite may report a busy database under contention; such an
            // insert simply did not happen.
            Err(e) if cfg!(feature = "postgres") => panic!("unexpected error: {e}"),
            Err(_) => {}
        }
    }
    let live = live_devices(&pool, &user).await;
    assert_eq!(live, inserted);
    assert!((1..=MAX).contains(&live), "{live} live devices, cap {MAX}");

    delete_device_data(&pool, &[&user]).await;
}

#[tokio::test]
#[serial]
async fn list_shows_only_own_live_devices() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let other = unique_user();
    let cli = Cli::loopback();
    let live = sign_in(&state, &user, &cli).await;
    insert_device(&pool, &user, Fixture::IdleExpired).await;
    insert_device(&pool, &user, Fixture::Revoked).await;
    insert_device(&pool, &other, Fixture::Live).await;

    let req = request_with_cookie("GET", "/api/v1/devices", &user);
    let resp = send(&state, req.body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "cache-control").as_deref(), Some("no-store"));
    let body: Value = response_json(resp).await;
    let items = body["result"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{body}");
    let item = &items[0];
    assert_eq!(item["id"], live.as_str());
    assert_eq!(item["name"], "laptop");
    assert_eq!(item["fingerprint"], cli.key.fingerprint());
    let now = Utc::now().timestamp();
    assert!((item["created_at"].as_i64().unwrap() - now).abs() < 60);
    assert!((item["last_used_at"].as_i64().unwrap() - now).abs() < 60);
    assert!(item["expires_at"].as_i64().unwrap() > now + 89 * 86_400);
    assert_eq!(item.as_object().unwrap().len(), 6, "no other fields");

    let resp = send(&state, get("/api/v1/devices")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    delete_device_data(&pool, &[&user, &other]).await;
}

#[tokio::test]
#[serial]
async fn revoke_own_device_is_immediate_and_others_are_404() {
    let pool = get_test_pool().await;
    let state = build_state(pool.clone());
    let user = unique_user();
    let other = unique_user();
    let (mine, theirs) = (Cli::loopback(), Cli::loopback());
    let my_device = sign_in(&state, &user, &mine).await;
    let their_device = sign_in(&state, &other, &theirs).await;
    let delete = |id: &str| {
        request_with_cookie("DELETE", &format!("/api/v1/devices/{id}"), &user)
            .header("Origin", TEST_UI_ORIGIN)
            .body(Body::empty())
            .unwrap()
    };

    // Someone else's device, an unknown id and a malformed id: the same 404.
    let mut bodies = Vec::new();
    for id in [
        their_device.as_str(),
        &crypto::random_b64url(16).unwrap(),
        "not-a-device-id",
    ] {
        let resp = send(&state, delete(id)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{id}");
        bodies.push(response_json::<Value>(resp).await);
    }
    assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(bodies[0]["result"]["code"], "DEVICE_NOT_FOUND");
    assert_eq!(me(&state, &theirs, &their_device).await, StatusCode::OK);

    // Own device: 204, and its very next request is refused.
    assert_eq!(me(&state, &mine, &my_device).await, StatusCode::OK);
    assert_eq!(
        send(&state, delete(&my_device)).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        me(&state, &mine, &my_device).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&state, delete(&my_device)).await.status(),
        StatusCode::NOT_FOUND
    );

    delete_device_data(&pool, &[&user, &other]).await;
}
