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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use super::*;
use std::time::UNIX_EPOCH;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const OLD_ID: &str = "oLdDeViCe0123456789abc";
const NEW_ID: &str = "nEwDeViCe0123456789abc";

fn origin(s: &str) -> Result<ApiOrigin, AuthError> {
    ApiOrigin::from_url(&Url::parse(s).unwrap())
}

#[test]
fn input_checks() {
    assert_eq!(
        origin("https://API.example.com:443/").unwrap().as_str(),
        "https://api.example.com"
    );
    assert!(origin("http://localhost:8081").is_ok() && origin("http://127.0.0.1:8081").is_ok());
    for bad in [
        "http://api.example.com",
        "https://a.example/api",
        "https://u:p@a.example",
        "https://a.example?x=1",
        "ftp://a.example",
    ] {
        assert!(origin(bad).is_err(), "{bad}");
    }

    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.to_string())
        }
    };
    assert!(browser_expected(env(&[]), "macos"));
    assert!(!browser_expected(env(&[]), "linux"));
    assert!(browser_expected(env(&[("DISPLAY", ":0")]), "linux"));
    assert!(!browser_expected(
        env(&[("DISPLAY", ":0"), ("SSH_CONNECTION", "x")]),
        "linux"
    ));

    assert_eq!(
        device_name(None, "my host;\u{202e}x").unwrap(),
        "videocall-cli on myhostx"
    );
    assert_eq!(
        device_name(None, &"a".repeat(200)).unwrap().len(),
        DEVICE_NAME_MAX_LEN
    );
    assert_eq!(
        device_name(Some("  Kitchen Pi (2) "), "h").unwrap(),
        "Kitchen Pi (2)"
    );
    assert!(device_name(Some("caf\u{e9}"), "h").is_err());

    let api = origin("https://api.example.com").unwrap();
    assert!(check_authorize_url("https://api.example.com/cli/authorize?x=1", &api).is_ok());
    for bad in [
        "https://evil.example.com/x",
        "http://api.example.com/x",
        "https://api.example.com:8443/x",
        "javascript:alert(1)",
    ] {
        assert!(check_authorize_url(bad, &api).is_err(), "{bad}");
    }

    assert_eq!(
        parse_pasted_code("  abc_DEF-1 \r\n")
            .unwrap()
            .expose_secret(),
        "abc_DEF-1"
    );
    for bad in [
        "\n".to_string(),
        "has space".into(),
        "caf\u{e9}".into(),
        "x".repeat(257),
    ] {
        assert!(parse_pasted_code(&bad).is_err(), "{bad:?}");
    }

    let exchange = |code: &str| {
        exchange_error(ApiError::Device {
            status: 400,
            error: code.into(),
            error_description: String::new(),
        })
        .to_string()
    };
    for (code, says) in [
        ("not_configured", "device login disabled"),
        ("invalid_grant", "Check the browser page"),
        ("too_many_devices", "maximum number of devices"),
        ("other", "other"),
    ] {
        assert!(exchange(code).contains(says), "{code}");
    }
}

// A minimal meeting-api: answers from a handler and stamps a `Date` that is
// `skew` seconds ahead of ours.

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    /// `iat` and `kid` of the bearer device JWT, if any.
    iat: Option<i64>,
    kid: Option<String>,
    body: Value,
}

struct Mock {
    api: ApiOrigin,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn find(&self, method: &str) -> Option<(usize, Seen)> {
        self.seen()
            .into_iter()
            .enumerate()
            .find(|(_, s)| s.method == method)
    }
}

fn jwt_part(head: &str, part: usize) -> Option<Value> {
    let token = head
        .lines()
        .find_map(|l| l.strip_prefix("authorization: Bearer "))?;
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(part)?).ok()?).ok()
}

async fn mock(
    skew: i64,
    handler: impl Fn(&Seen, i64) -> (u16, Value) + Send + Sync + 'static,
) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = origin(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, handler) = (seen.clone(), Arc::new(handler));
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (log, handler) = (log.clone(), handler.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let (head, body) = loop {
                    let mut chunk = [0u8; 4096];
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                    let text = String::from_utf8_lossy(&buf).to_string();
                    let Some(end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let len = text[..end]
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: ")?.parse().ok())
                        .unwrap_or(0usize);
                    if buf.len() >= end + 4 + len {
                        break (
                            text[..end].to_string(),
                            text[end + 4..end + 4 + len].to_string(),
                        );
                    }
                };
                let mut words = head.split_whitespace();
                let seen = Seen {
                    method: words.next().unwrap_or_default().into(),
                    path: words.next().unwrap_or_default().into(),
                    iat: jwt_part(&head, 1).and_then(|c| c["iat"].as_i64()),
                    kid: jwt_part(&head, 0).and_then(|h| h["kid"].as_str().map(String::from)),
                    body: serde_json::from_str(&body).unwrap_or(Value::Null),
                };
                log.lock().unwrap().push(seen.clone());
                let server_now = keys::local_now() + skew;
                let (status, body) = handler(&seen, server_now);
                let body = if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                };
                let date =
                    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(server_now as u64));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nDate: {date}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    Mock { api, seen }
}

/// Like the real server: a device JWT or registration more than 60 s off
/// the server's clock is rejected.
fn fresh(iat: Option<i64>, server_now: i64) -> bool {
    iat.is_some_and(|iat| (iat - server_now).abs() <= 60)
}

fn ctx_with(
    tmp: &tempfile::TempDir,
    open: impl Fn(&str, bool) -> bool + Send + Sync + 'static,
    pasted: &'static str,
) -> Ctx {
    Ctx {
        store: Store::with_dir(tmp.path().join("videocall"), false),
        http: reqwest::Client::new(),
        clock: Box::leak(Box::default()),
        show_url: Arc::new(open),
        read_code: Arc::new(move || Ok(format!("{pasted}\n"))),
    }
}

fn save_device(ctx: &Ctx, api: &ApiOrigin, device_id: &str) -> DeviceKey {
    let key = DeviceKey::generate().unwrap();
    let device = StoredDevice {
        device_id: device_id.into(),
        fingerprint: key.fingerprint(),
        name: "old".into(),
        user_id: "u".into(),
        user_name: "U".into(),
        private_key: key.seed_b64(),
    };
    ctx.store.save(api.as_str(), &device).unwrap();
    key
}

#[tokio::test]
async fn a_401_is_retried_once_only_if_the_clock_explains_it() {
    let me = json!({
        "device_id": OLD_ID, "status": "approved", "name": "n", "fingerprint": "f", "user_id": "u",
        "user_name": "U", "created_at": 0, "decided_at": 0, "last_used_at": 0, "idle_expires_at": 0,
        "abs_expires_at": 0,
    });
    // (server skew, server accepts fresh tokens?, expected signed requests, ok?)
    for (skew, accepts, signed, ok) in [
        (3600, true, 2, true),
        (3600, false, 2, false),
        (0, false, 1, false),
    ] {
        let me = me.clone();
        let server = mock(skew, move |seen, now| {
            match accepts && fresh(seen.iat, now) {
                true => (200, me.clone()),
                false => (401, Value::Null),
            }
        })
        .await;
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(&tmp, |_, _| false, "");
        let creds = Credentials::new(
            DeviceKey::generate().unwrap(),
            OLD_ID.into(),
            &server.api,
            &ctx,
        );
        let result = creds
            .call(|client| async move { client.device_me().await })
            .await;
        assert_eq!(result.is_ok(), ok, "skew {skew}: {result:?}");
        if !ok {
            assert!(matches!(result, Err(AuthError::DeviceRevoked)));
        }
        assert_eq!(
            server.seen().iter().filter(|s| s.iat.is_some()).count(),
            signed,
            "skew {skew}"
        );
    }
}

#[tokio::test]
async fn logout_deletes_locally_even_if_the_server_refuses_or_is_gone() {
    let server = mock(0, |_, _| (401, Value::Null)).await;
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx_with(&tmp, |_, _| false, "");
    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let gone = origin(&format!("http://127.0.0.1:{port}")).unwrap();
    for api in [&server.api, &gone] {
        save_device(&ctx, api, OLD_ID);
        logout(&ctx, api).await.unwrap();
        assert!(ctx.store.load(api.as_str()).unwrap().is_none());
    }
    assert_eq!(
        server.find("DELETE").unwrap().1.kid.as_deref(),
        Some(OLD_ID)
    );
}

// Login.

fn registration_claims(body: &Value) -> Value {
    let jws = body["registration"].as_str().unwrap();
    serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(jws.split('.').nth(1).unwrap())
            .unwrap(),
    )
    .unwrap()
}

/// A meeting-api for login whose clock is an hour ahead of ours. The
/// exchange succeeds for `code` with a fresh registration, returning the
/// key's fingerprint unless `wrong_fingerprint`; a fresh signed DELETE /me
/// gets 204.
async fn login_server(code: &'static str, wrong_fingerprint: bool) -> Mock {
    mock(3600, move |seen, now| match (seen.method.as_str(), seen.path.as_str()) {
        ("POST", "/api/v1/devices") => {
            let claims = registration_claims(&seen.body);
            if seen.body["code"] != code || !fresh(claims["iat"].as_i64(), now) {
                return (400, json!({"error": "invalid_grant", "error_description": "x"}));
            }
            let pk = URL_SAFE_NO_PAD.decode(claims["public_key"].as_str().unwrap()).unwrap();
            let fingerprint = match wrong_fingerprint {
                true => "0000-0000-0000-0000".to_string(),
                false => videocall_meeting_types::device::fingerprint(&pk),
            };
            let body = json!({
                "device_id": NEW_ID, "fingerprint": fingerprint, "name": claims["name"], "user_id": "u",
                "user_name": "U", "idle_expires_at": 0, "abs_expires_at": 0,
            });
            (201, body)
        }
        ("DELETE", "/api/v1/devices/me") if fresh(seen.iat, now) => (204, Value::Null),
        _ => (404, json!({})),
    })
    .await
}

fn query(url: &str, key: &str) -> String {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == key)
        .unwrap()
        .1
        .into_owned()
}

/// A "browser" that records the URL and runs `then` on it.
fn browser(
    then: impl Fn(&str) + Send + Sync + 'static,
) -> (
    Arc<Mutex<String>>,
    impl Fn(&str, bool) -> bool + Send + Sync,
) {
    let slot = Arc::new(Mutex::new(String::new()));
    let sink = slot.clone();
    (slot, move |url: &str, _| {
        *sink.lock().unwrap() = url.to_string();
        then(url);
        true
    })
}

/// The exchange matches the authorize URL, and the saved key is the one
/// the URL carried.
fn check_login(server: &Mock, ctx: &Ctx, url: &str, redirect_uri: &str) {
    assert!(url.starts_with(&format!("{}/cli/authorize?", server.api.as_str())));
    let (_, post) = server.find("POST").expect("exchange sent");
    assert!(post.iat.is_none(), "the exchange is unauthenticated");
    assert_eq!(post.body["redirect_uri"], redirect_uri);
    assert_eq!(query(url, "redirect_uri"), redirect_uri);
    let verifier = post.body["code_verifier"].as_str().unwrap();
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)),
        query(url, "code_challenge")
    );
    let claims = registration_claims(&post.body);
    assert_eq!(claims["aud"], server.api.as_str());
    assert_eq!(claims["name"], query(url, "device_name"));

    let (device, _) = ctx.store.load(server.api.as_str()).unwrap().unwrap();
    assert_eq!(device.device_id, NEW_ID);
    let key = DeviceKey::from_seed_b64(&device.private_key).unwrap();
    assert_eq!(key.public_key_b64(), query(url, "public_key"));
    assert_eq!(claims["public_key"], query(url, "public_key"));
}

#[tokio::test]
async fn remote_login_fixes_the_clock_saves_then_revokes_the_old_device() {
    let server = login_server("pasted-code", false).await;
    let tmp = tempfile::tempdir().unwrap();
    let (url, open) = browser(|_| {});
    let ctx = ctx_with(&tmp, open, "  pasted-code  ");
    save_device(&ctx, &server.api, OLD_ID);

    login_with(&ctx, &server.api, "Garage Pi", true)
        .await
        .unwrap();

    check_login(&server, &ctx, &url.lock().unwrap(), CLI_REMOTE_REDIRECT_URI);
    assert_eq!(
        server.seen()[0].path,
        "/version",
        "clock checked before signing"
    );
    let (post, _) = server.find("POST").unwrap();
    let (delete, revoke) = server.find("DELETE").expect("old device revoked");
    assert!(post < delete && revoke.kid.as_deref() == Some(OLD_ID));
}

#[tokio::test]
async fn loopback_login_receives_the_code_on_the_callback() {
    let server = login_server("loopback-code", false).await;
    let tmp = tempfile::tempdir().unwrap();
    // Do what the app does after approval: navigate to the callback.
    let (url, open) = browser(|authorize| {
        let port = Url::parse(&query(authorize, "redirect_uri"))
            .unwrap()
            .port()
            .unwrap();
        let state = query(authorize, "state");
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                stream,
                "GET /callback?code=loopback-code&state={state} HTTP/1.1\r\n\r\n"
            )
            .unwrap();
            let _ = stream.read_to_string(&mut String::new());
        });
    });
    let ctx = ctx_with(&tmp, open, "");

    login_with(&ctx, &server.api, "Laptop", false)
        .await
        .unwrap();

    let url = url.lock().unwrap().clone();
    let redirect = query(&url, "redirect_uri");
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"));
    check_login(&server, &ctx, &url, &redirect);
}

#[tokio::test]
async fn a_failed_login_saves_and_revokes_nothing() {
    // Wrong code, wrong fingerprint, and a store that can't be written (its
    // parent is a file): the last one revokes the new device, nothing else.
    for (code, wrong_fingerprint, broken_store) in [
        ("other", false, false),
        ("code", true, false),
        ("code", false, true),
    ] {
        let server = login_server(code, wrong_fingerprint).await;
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = ctx_with(&tmp, |_, _| false, "code");
        let old = save_device(&ctx, &server.api, OLD_ID);
        if broken_store {
            std::fs::write(tmp.path().join("file"), b"").unwrap();
            ctx.store = Store::with_dir(tmp.path().join("file").join("videocall"), false);
        }

        let err = login_with(&ctx, &server.api, "n", true).await.unwrap_err();

        let revoked = server.find("DELETE").map(|(_, s)| s.kid);
        if broken_store {
            assert!(matches!(err, AuthError::Store(_)), "{err}");
            assert_eq!(revoked, Some(Some(NEW_ID.to_string())));
        } else {
            assert!(matches!(err, AuthError::Login(_)), "{err}");
            assert_eq!(revoked, None);
            let (device, _) = ctx.store.load(server.api.as_str()).unwrap().unwrap();
            assert_eq!(
                device.private_key.expose_secret(),
                old.seed_b64().expose_secret()
            );
        }
    }
}
