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
use std::sync::Mutex;

use secrecy::ExposeSecret;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
    move |key| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| OsString::from(v))
    }
}

#[test]
fn headless_detection() {
    assert!(!browser_expected(env(&[]), "linux"));
    assert!(browser_expected(env(&[("DISPLAY", ":0")]), "linux"));
    assert!(browser_expected(
        env(&[("WAYLAND_DISPLAY", "wayland-0")]),
        "linux"
    ));
    assert!(!browser_expected(env(&[("DISPLAY", "")]), "linux"));
    assert!(!browser_expected(
        env(&[
            ("DISPLAY", ":0"),
            ("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22")
        ]),
        "linux"
    ));
    assert!(browser_expected(env(&[]), "macos"));
    assert!(browser_expected(env(&[]), "windows"));
    assert!(!browser_expected(
        env(&[("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22")]),
        "macos"
    ));
}

fn origin(s: &str) -> Result<ApiOrigin, AuthError> {
    ApiOrigin::from_url(&Url::parse(s).unwrap())
}

#[test]
fn api_origin_validation() {
    assert_eq!(
        origin("https://api.videocall.rs").unwrap().as_str(),
        "https://api.videocall.rs"
    );
    assert_eq!(
        origin("https://auth.videocall.rs/").unwrap().as_str(),
        "https://auth.videocall.rs"
    );
    assert_eq!(
        origin("https://API.videocall.rs:443").unwrap().as_str(),
        "https://api.videocall.rs"
    );
    assert_eq!(
        origin("http://localhost:8082").unwrap().as_str(),
        "http://localhost:8082"
    );
    assert!(origin("http://192.168.1.10:8081").is_ok());
    assert!(origin("http://auth.videocall.rs").is_err());
    assert!(origin("https://api.videocall.rs/api").is_err());
    assert!(origin("https://u:p@api.videocall.rs").is_err());
    assert!(origin("https://api.videocall.rs?x=1").is_err());
    assert!(origin("ftp://api.videocall.rs").is_err());
}

#[test]
fn name_defaulting_and_filtering() {
    assert_eq!(default_device_name("pi4\n"), "videocall-cli on pi4");
    assert_eq!(
        default_device_name("my host;rm -rf/\u{202e}x"),
        "videocall-cli on myhostrm-rfx"
    );
    assert_eq!(default_device_name("   "), "videocall-cli");
    let long = default_device_name(&"a".repeat(200));
    assert_eq!(long.len(), DEVICE_NAME_MAX_LEN);
    assert!(long.starts_with("videocall-cli on aaa"));
    // Defaults always pass the server's rule.
    for host in ["pi4", "", "-leading-dash", "x".repeat(100).as_str()] {
        let name = device_name(None, host).unwrap();
        assert_eq!(normalize_device_name(&name).as_deref(), Some(name.as_str()));
    }

    assert_eq!(
        device_name(Some("  Kitchen Pi (2)  "), "h").unwrap(),
        "Kitchen Pi (2)"
    );
    assert!(device_name(Some("-dash first"), "h").is_err());
    assert!(device_name(Some("tab\there"), "h").is_err());
    assert!(device_name(Some("caf\u{e9}"), "h").is_err());
    assert!(device_name(Some(&"n".repeat(65)), "h").is_err());
    assert!(device_name(Some(""), "h").is_err());
}

#[test]
fn clock_tls_failure_detection() {
    #[derive(Debug)]
    struct E(&'static str, Option<Box<E>>);
    impl std::fmt::Display for E {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for E {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|e| e as _)
        }
    }
    let expired = E(
        "error sending request",
        Some(Box::new(E("invalid peer certificate: Expired", None))),
    );
    assert!(clock_tls_failure(&expired)
        .unwrap()
        .contains("rejected as expired"));
    let not_yet = E("invalid peer certificate: NotValidYet", None);
    assert!(clock_tls_failure(&not_yet)
        .unwrap()
        .contains("not yet valid"));
    assert!(clock_tls_failure(&E("invalid peer certificate: UnknownIssuer", None)).is_none());
    assert!(clock_tls_failure(&E("connection refused", None)).is_none());
}

// ---------------------------------------------------------------------
// A minimal meeting-api: answers from a handler, stamps a configurable Date.

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    /// `iat` of the bearer token, if any.
    iat: Option<i64>,
    /// `kid` of the bearer token, if any.
    kid: Option<String>,
    body: String,
}

type Handler = dyn Fn(&Seen, i64) -> (u16, String) + Send + Sync;

struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Mock {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn signed(&self) -> usize {
        self.seen().iter().filter(|s| s.iat.is_some()).count()
    }

    fn origin(&self) -> ApiOrigin {
        origin(&self.url).unwrap()
    }
}

fn bearer_part(head: &str, part: usize) -> Option<serde_json::Value> {
    let token = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("authorization")
            .then(|| value.trim().strip_prefix("Bearer ").map(str::to_string))?
    })?;
    let bytes = URL_SAFE_NO_PAD.decode(token.split('.').nth(part)?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// `skew`: seconds the server's clock is ahead of ours.
async fn mock(skew: i64, handler: Box<Handler>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let handler: Arc<Handler> = Arc::from(handler);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (log, handler) = (log.clone(), handler.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break (
                                text[..end].to_string(),
                                text[end + 4..end + 4 + len].to_string(),
                            );
                        }
                    }
                };
                let mut words = head.split_whitespace();
                let seen = Seen {
                    method: words.next().unwrap_or_default().to_string(),
                    path: words.next().unwrap_or_default().to_string(),
                    iat: bearer_part(&head, 1).and_then(|c| c["iat"].as_i64()),
                    kid: bearer_part(&head, 0).and_then(|h| h["kid"].as_str().map(str::to_string)),
                    body,
                };
                log.lock().unwrap().push(seen.clone());
                let server_now = clock::local_now() + skew;
                let (status, body) = handler(&seen, server_now);
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
    Mock { url, seen }
}

/// Accept a token only if its `iat` is within 60 s of the server's clock,
/// like the real verifier; 401 otherwise.
fn time_checked(ok: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) -> Box<Handler> {
    Box::new(move |seen, server_now| match seen.iat {
        Some(iat) if (iat - server_now).abs() <= 60 => ok(seen),
        Some(_) => (
            401,
            r#"{"error":"invalid_token","error_description":"x"}"#.into(),
        ),
        None => (404, "{}".into()),
    })
}

fn me_json(device_id: &str) -> String {
    serde_json::json!({
        "device_id": device_id,
        "name": "videocall-cli on test",
        "fingerprint": "9F3C-21AB-77D0-4E18",
        "user_id": "alice@example.com",
        "user_name": "Alice",
        "created_at": 1_764_000_000,
        "decided_at": 1_764_000_000,
        "last_used_at": 1_764_000_000,
        "idle_expires_at": 1_771_776_000,
        "abs_expires_at": 1_795_536_000,
        "status": "approved",
        "interval": 5,
    })
    .to_string()
}

fn test_clock() -> &'static Clock {
    Box::leak(Box::new(Clock::new()))
}

/// A context whose browser "opens" by calling `open`, and whose terminal
/// "types" `pasted`.
fn ctx_with(
    tmp: &tempfile::TempDir,
    open: impl Fn(&str, bool) -> bool + Send + Sync + 'static,
    pasted: &'static str,
) -> Ctx {
    Ctx {
        store: Store::with_dir(tmp.path().join("videocall"), false),
        http: reqwest::Client::new(),
        clock: test_clock(),
        browser_expected: false,
        show_url: Arc::new(open),
        read_code: Arc::new(move || Ok(format!("{pasted}\n"))),
    }
}

fn ctx(tmp: &tempfile::TempDir) -> Ctx {
    ctx_with(tmp, |_, _| false, "")
}

const OLD_ID: &str = "oLdDeViCe0123456789abc";
const NEW_ID: &str = "nEwDeViCe0123456789abc";

async fn me(ctx: &Ctx, api: &ApiOrigin) -> Result<String, AuthError> {
    let creds = ctx.credentials(DeviceKey::generate().unwrap(), OLD_ID.into(), api);
    creds
        .call(|client| async move { client.device_me().await })
        .await
        .map(|me| me.device_id)
}

#[tokio::test]
async fn clock_offset_applied_after_skewed_401_and_retried_once() {
    let server = mock(3600, time_checked(|_| (200, me_json(OLD_ID)))).await;
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx(&tmp);

    assert_eq!(me(&ctx, &server.origin()).await.unwrap(), OLD_ID);
    assert_eq!(server.signed(), 2, "{:?}", server.seen());
    assert!((ctx.clock.offset() - 3600).abs() <= 2);
    // The time probe is the unauthenticated GET /version between the two.
    assert!(server
        .seen()
        .iter()
        .any(|s| s.iat.is_none() && s.path == "/version"));
}

#[tokio::test]
async fn every_request_gets_a_fresh_token() {
    let server = mock(0, time_checked(|_| (200, me_json(OLD_ID)))).await;
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx(&tmp);
    let creds = ctx.credentials(
        DeviceKey::generate().unwrap(),
        OLD_ID.into(),
        &server.origin(),
    );
    for _ in 0..2 {
        creds
            .call(|client| async move { client.device_me().await })
            .await
            .unwrap();
    }
    assert_eq!(server.signed(), 2);
    let seen = server.seen();
    assert!(seen.iter().all(|s| s.kid.as_deref() == Some(OLD_ID)));
}

#[tokio::test]
async fn second_401_after_clock_retry_means_revoked() {
    let server = mock(
        3600,
        Box::new(|seen, _| match seen.iat {
            Some(_) => (401, "{}".into()),
            None => (404, "{}".into()),
        }),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let err = me(&ctx(&tmp), &server.origin()).await.unwrap_err();
    assert!(matches!(err, AuthError::DeviceRevoked));
    assert!(err.to_string().contains("no longer authorized"));
    assert!(err.to_string().contains("videocall-cli auth login"));
    assert_eq!(server.signed(), 2);
}

#[tokio::test]
async fn unskewed_401_is_not_retried() {
    let server = mock(0, Box::new(|_, _| (401, "{}".into()))).await;
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx(&tmp);
    let err = me(&ctx, &server.origin()).await.unwrap_err();
    assert!(matches!(err, AuthError::DeviceRevoked));
    assert_eq!(server.signed(), 1);
    assert_eq!(ctx.clock.offset(), 0);
}

async fn save_device(ctx: &Ctx, api: &ApiOrigin, device_id: &str) -> DeviceKey {
    let key = DeviceKey::generate().unwrap();
    let lock = ctx.store.lock().await.unwrap();
    ctx.store
        .save(
            api.as_str(),
            HostEntry {
                device_id: device_id.into(),
                fingerprint: key.fingerprint(),
                name: "videocall-cli on old".into(),
                user_id: "alice@example.com".into(),
                user_name: "Alice".into(),
                store: StoreKind::File,
            },
            &Secret {
                private_key: key.seed_b64(),
            },
            &lock,
        )
        .unwrap();
    key
}

#[tokio::test]
async fn logout_deletes_locally_even_when_server_says_401() {
    let server = mock(0, Box::new(|_, _| (401, "{}".into()))).await;
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx(&tmp);
    save_device(&ctx, &server.origin(), OLD_ID).await;

    logout_with(&ctx, &server.origin()).await.unwrap();
    assert!(ctx.store.host(server.origin().as_str()).unwrap().is_none());
    let deletes = server
        .seen()
        .iter()
        .filter(|s| s.method == "DELETE" && s.path == "/api/v1/devices/me")
        .count();
    assert_eq!(deletes, 1);
}

#[tokio::test]
async fn logout_deletes_locally_when_server_is_unreachable() {
    // Bind and drop to get a port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let api = origin(&format!("http://127.0.0.1:{port}")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx(&tmp);
    save_device(&ctx, &api, OLD_ID).await;

    logout_with(&ctx, &api).await.unwrap();
    assert!(ctx.store.host(api.as_str()).unwrap().is_none());
}

#[test]
fn authorize_url_must_be_on_the_api_origin() {
    let api = origin("https://api.example.com").unwrap();
    assert!(check_authorize_url("https://api.example.com/cli/authorize?x=1", &api).is_ok());
    assert!(check_authorize_url("https://evil.example.com/cli/authorize", &api).is_err());
    assert!(check_authorize_url("http://api.example.com/cli/authorize", &api).is_err());
    assert!(check_authorize_url("https://api.example.com:8443/cli/authorize", &api).is_err());
    assert!(check_authorize_url("javascript:alert(1)", &api).is_err());
    assert!(check_authorize_url("not a url", &api).is_err());
}

#[test]
fn pasted_code_parsing() {
    assert_eq!(
        parse_pasted_code("  abc_DEF-123 \r\n")
            .unwrap()
            .expose_secret(),
        "abc_DEF-123"
    );
    assert!(parse_pasted_code("\n").is_err());
    assert!(parse_pasted_code("has space").is_err());
    assert!(parse_pasted_code(&"x".repeat(MAX_PASTED_CODE + 1)).is_err());
    assert!(parse_pasted_code("caf\u{e9}").is_err());
}

/// Claims of the registration JWS inside a register request body.
fn registration_claims(body: &serde_json::Value) -> serde_json::Value {
    let jws = body["registration"].as_str().unwrap();
    let claims = URL_SAFE_NO_PAD
        .decode(jws.split('.').nth(1).unwrap())
        .unwrap();
    serde_json::from_slice(&claims).unwrap()
}

/// How the fake meeting-api answers the code exchange.
#[derive(Clone, Copy, PartialEq)]
enum Exchange {
    /// 201 with the real fingerprint of the registered key.
    Ok,
    /// 201 with a wrong fingerprint.
    WrongFingerprint,
    /// 400 invalid_grant.
    InvalidGrant,
}

/// A fake meeting-api for login: answers the exchange per `mode` (only
/// for `expected_code`), accepts signed DELETE /me, and stamps `Date`.
async fn login_server(mode: Exchange, expected_code: &'static str) -> Mock {
    mock(
        0,
        Box::new(
            move |seen, _| match (seen.method.as_str(), seen.path.as_str()) {
                ("POST", "/api/v1/devices") => {
                    let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
                    if mode == Exchange::InvalidGrant || body["code"] != expected_code {
                        return (
                            400,
                            r#"{"error":"invalid_grant","error_description":"x"}"#.into(),
                        );
                    }
                    let claims = registration_claims(&body);
                    let pk = URL_SAFE_NO_PAD
                        .decode(claims["public_key"].as_str().unwrap())
                        .unwrap();
                    let fingerprint = if mode == Exchange::WrongFingerprint {
                        "0000-0000-0000-0000".to_string()
                    } else {
                        videocall_meeting_types::device::fingerprint(&pk)
                    };
                    (
                        201,
                        serde_json::json!({
                            "device_id": NEW_ID,
                            "fingerprint": fingerprint,
                            "name": claims["name"],
                            "user_id": "alice@example.com",
                            "user_name": "Alice",
                            "idle_expires_at": 1_771_776_000,
                            "abs_expires_at": 1_795_536_000,
                        })
                        .to_string(),
                    )
                }
                ("DELETE", "/api/v1/devices/me") if seen.iat.is_some() => (204, String::new()),
                _ => (404, "{}".into()),
            },
        ),
    )
    .await
}

fn query(url: &str, key: &str) -> String {
    Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .unwrap()
}

/// Checks a successful exchange body against the authorize URL.
fn check_exchange(server: &Mock, authorize_url: &str, expected_redirect: &str) {
    let post = server
        .seen()
        .into_iter()
        .find(|s| s.method == "POST")
        .expect("exchange sent");
    let body: serde_json::Value = serde_json::from_str(&post.body).unwrap();
    assert_eq!(body["redirect_uri"], expected_redirect);
    assert_eq!(query(authorize_url, "redirect_uri"), expected_redirect);
    // PKCE: the verifier matches the challenge in the authorize URL.
    assert_eq!(
        loopback::s256_challenge(body["code_verifier"].as_str().unwrap()),
        query(authorize_url, "code_challenge")
    );
    // The registration JWS is for this server and the key in the URL.
    let claims = registration_claims(&body);
    assert_eq!(claims["aud"], server.origin().as_str());
    assert_eq!(claims["public_key"], query(authorize_url, "public_key"));
    assert_eq!(claims["name"], query(authorize_url, "device_name"));
    assert!(post.iat.is_none(), "exchange is unauthenticated");
}

/// The last URL handed to the "browser".
type UrlSlot = Arc<Mutex<Option<String>>>;

fn captured() -> (UrlSlot, impl Fn(&str, bool) -> bool + Send + Sync) {
    let slot = Arc::new(Mutex::new(None));
    let sink = slot.clone();
    (slot, move |url: &str, _remote: bool| {
        *sink.lock().unwrap() = Some(url.to_string());
        false
    })
}

#[tokio::test]
async fn remote_login_pastes_code_and_saves_after_exchange() {
    let server = login_server(Exchange::Ok, "pasted-code").await;
    let tmp = tempfile::tempdir().unwrap();
    let (url, open) = captured();
    let ctx = ctx_with(&tmp, open, "  pasted-code  ");
    let api = server.origin();

    login_with(&ctx, &api, "Garage Pi", true).await.unwrap();

    let url = url.lock().unwrap().clone().unwrap();
    assert!(url.starts_with(&format!("{}/cli/authorize?", api.as_str())));
    check_exchange(&server, &url, &format!("{}/cli/code", api.as_str()));

    let host = ctx.store.host(api.as_str()).unwrap().unwrap();
    assert_eq!(host.device_id, NEW_ID);
    assert_eq!(host.name, "Garage Pi");
    assert_eq!(host.user_id, "alice@example.com");
    assert_eq!(host.user_name, "Alice");
    let stored = ctx
        .store
        .load_secret(api.as_str(), StoreKind::File)
        .unwrap()
        .unwrap();
    let key = DeviceKey::from_seed_b64(&stored.private_key).unwrap();
    assert_eq!(key.fingerprint(), host.fingerprint);
    assert_eq!(key.public_key_b64(), query(&url, "public_key"));
    let hosts = std::fs::read_to_string(ctx.store.dir().join("hosts.toml")).unwrap();
    assert!(!hosts.contains(stored.private_key.expose_secret()));
}

#[tokio::test]
async fn loopback_login_receives_code_on_callback() {
    let server = login_server(Exchange::Ok, "loopback-code").await;
    let tmp = tempfile::tempdir().unwrap();
    let (url, record) = captured();
    // The "browser": follow the consent redirect to the loopback callback
    // with the stored state, as meeting-api would.
    let open = move |authorize: &str, remote: bool| {
        assert!(!remote);
        record(authorize, remote);
        let redirect = Url::parse(&query(authorize, "redirect_uri")).unwrap();
        let state = query(authorize, "state");
        let port = redirect.port().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(
                stream,
                "GET /callback?code=loopback-code&state={state} HTTP/1.1\r\nHost: x\r\n\r\n"
            )
            .unwrap();
            let mut reply = String::new();
            let _ = stream.read_to_string(&mut reply);
        });
        true
    };
    let ctx = ctx_with(&tmp, open, "");
    let api = server.origin();

    login_with(&ctx, &api, "Laptop", false).await.unwrap();

    let url = url.lock().unwrap().clone().unwrap();
    let redirect = query(&url, "redirect_uri");
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"));
    check_exchange(&server, &url, &redirect);
    assert_eq!(
        ctx.store.host(api.as_str()).unwrap().unwrap().device_id,
        NEW_ID
    );
}

#[tokio::test]
async fn failed_exchange_writes_nothing_and_keeps_old_device() {
    for mode in [Exchange::InvalidGrant, Exchange::WrongFingerprint] {
        let server = login_server(mode, "code").await;
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(&tmp, |_, _| false, "code");
        let api = server.origin();

        // No previous device: the store stays empty.
        let err = login_with(&ctx, &api, "n", true).await.unwrap_err();
        match mode {
            Exchange::InvalidGrant => assert!(matches!(err, AuthError::Login(_)), "{err}"),
            _ => assert!(
                matches!(err, AuthError::FingerprintMismatch { .. }),
                "{err}"
            ),
        }
        assert!(ctx.store.host(api.as_str()).unwrap().is_none());
        assert!(!ctx.store.credentials_path().exists());

        // A previous device survives untouched and is not revoked.
        let old = save_device(&ctx, &api, OLD_ID).await;
        login_with(&ctx, &api, "n", true).await.unwrap_err();
        let host = ctx.store.host(api.as_str()).unwrap().unwrap();
        assert_eq!(host.device_id, OLD_ID);
        let stored = ctx
            .store
            .load_secret(api.as_str(), StoreKind::File)
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.private_key.expose_secret(),
            old.seed_b64().expose_secret()
        );
        assert!(!server.seen().iter().any(|s| s.method == "DELETE"));
    }
}

#[tokio::test]
async fn old_device_revoked_only_after_new_login_succeeds() {
    let server = login_server(Exchange::Ok, "code").await;
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx_with(&tmp, |_, _| false, "code");
    let api = server.origin();
    let old = save_device(&ctx, &api, OLD_ID).await;

    login_with(&ctx, &api, "n", true).await.unwrap();

    let seen = server.seen();
    let post = seen.iter().position(|s| s.method == "POST").unwrap();
    let delete = seen
        .iter()
        .position(|s| s.method == "DELETE")
        .expect("old device revoked");
    assert!(post < delete, "{seen:?}");
    // Signed by the old key's device id, not the new one.
    assert_eq!(seen[delete].kid.as_deref(), Some(OLD_ID));
    let host = ctx.store.host(api.as_str()).unwrap().unwrap();
    assert_eq!(host.device_id, NEW_ID);
    assert_ne!(host.fingerprint, old.fingerprint());
}

#[tokio::test]
async fn login_corrects_the_clock_before_signing() {
    let server = mock(
        3600,
        Box::new(
            |seen, server_now| match (seen.method.as_str(), seen.path.as_str()) {
                ("POST", "/api/v1/devices") => {
                    let body: serde_json::Value = serde_json::from_str(&seen.body).unwrap();
                    let claims = registration_claims(&body);
                    if (claims["iat"].as_i64().unwrap() - server_now).abs() > 60 {
                        return (
                            400,
                            r#"{"error":"invalid_request","error_description":"x"}"#.into(),
                        );
                    }
                    let pk = URL_SAFE_NO_PAD
                        .decode(claims["public_key"].as_str().unwrap())
                        .unwrap();
                    (
                        201,
                        serde_json::json!({
                            "device_id": NEW_ID,
                            "fingerprint": videocall_meeting_types::device::fingerprint(&pk),
                            "name": "n",
                            "user_id": "u",
                            "user_name": "U",
                            "idle_expires_at": 1,
                            "abs_expires_at": 2,
                        })
                        .to_string(),
                    )
                }
                _ => (404, "{}".into()),
            },
        ),
    )
    .await;
    let tmp = tempfile::tempdir().unwrap();
    let ctx = ctx_with(&tmp, |_, _| false, "code");
    login_with(&ctx, &server.origin(), "n", true).await.unwrap();
    let seen = server.seen();
    assert_eq!(seen[0].path, "/version");
    assert_eq!(seen.iter().filter(|s| s.method == "POST").count(), 1);
}

#[test]
fn exchange_errors_are_explained() {
    let device = |status, code: &str| {
        exchange_error(AuthError::Api(ApiError::Device {
            status,
            error: code.into(),
            error_description: String::new(),
        }))
        .to_string()
    };
    assert!(device(503, "not_configured").contains("device login disabled"));
    assert!(device(400, "invalid_grant").contains("Check the browser page"));
    assert!(device(409, "too_many_devices").contains("maximum number of devices"));
    assert!(device(415, "invalid_request").contains("update videocall-cli"));
    assert!(device(413, "invalid_request").contains("update videocall-cli"));
    assert!(matches!(
        exchange_error(AuthError::Timeout),
        AuthError::Timeout
    ));
    assert!(parse_pasted_code("")
        .unwrap_err()
        .to_string()
        .contains("Check the browser page"));
}

#[tokio::test]
async fn failed_local_save_revokes_the_new_device() {
    let server = login_server(Exchange::Ok, "code").await;
    let tmp = tempfile::tempdir().unwrap();
    let mut ctx = ctx_with(&tmp, |_, _| false, "code");
    // The config directory can't be created: its parent is a regular file.
    let blocker = tmp.path().join("not-a-dir");
    std::fs::write(&blocker, b"").unwrap();
    ctx.store = Store::with_dir(blocker.join("videocall"), false);
    let api = server.origin();

    let err = login_with(&ctx, &api, "n", true).await.unwrap_err();
    assert!(matches!(err, AuthError::Store(_)), "{err}");

    let seen = server.seen();
    let post = seen.iter().position(|s| s.method == "POST").unwrap();
    let delete = seen
        .iter()
        .position(|s| s.method == "DELETE")
        .expect("new device revoked");
    assert!(post < delete);
    assert_eq!(seen[delete].kid.as_deref(), Some(NEW_ID));
}
