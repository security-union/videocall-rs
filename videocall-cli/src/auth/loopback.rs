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

//! Loopback browser login (RFC 8252 §7.3) with PKCE (RFC 7636).
//!
//! The CLI listens on `127.0.0.1:{ephemeral}`, opens
//! `{api}/cli/authorize?...` in the browser, and waits for the one redirect
//! to `http://127.0.0.1:{port}/callback?code=…&state=…`. The listener
//! accepts exactly one `/callback` request and is closed afterwards.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::random_b64url;

/// How long the user has to finish the browser login.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Largest request head we read from the browser.
const MAX_REQUEST_HEAD: usize = 8 * 1024;

/// How long one connection may take to send its request line.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A PKCE S256 verifier and its challenge.
pub struct Pkce {
    /// The secret verifier, sent only to the token endpoint.
    pub verifier: SecretString,
    /// `BASE64URL(SHA256(verifier))`, sent to the authorize endpoint.
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh verifier from 32 CSPRNG bytes (43 characters).
    pub fn generate() -> Result<Self, getrandom::Error> {
        Ok(Self::from_verifier(random_b64url(32)?))
    }

    fn from_verifier(verifier: String) -> Self {
        let challenge = s256_challenge(&verifier);
        Self {
            verifier: SecretString::from(verifier),
            challenge,
        }
    }
}

/// `BASE64URL-ENCODE(SHA256(ASCII(verifier)))` (RFC 7636 §4.2).
pub fn s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Generate an OAuth `state` value: 32 CSPRNG bytes, base64url (43 chars).
pub fn generate_state() -> Result<String, getrandom::Error> {
    random_b64url(32)
}

/// Why the loopback login did not produce a code.
#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    /// The `state` on the callback was missing, duplicated, or not ours.
    /// Such a request is answered with an error page and ignored; it never
    /// ends the login (any local web page can send one).
    #[error("callback does not belong to this login attempt (state mismatch)")]
    StateMismatch,
    /// Our `state`, but no `code` (or a duplicated one).
    #[error("the browser returned an incomplete login response; try again")]
    Malformed,
    /// The user cancelled at the identity provider (`error=access_denied`).
    #[error("login cancelled in the browser")]
    Cancelled,
    /// The server redirected with another OAuth `error` parameter.
    #[error("login was refused by the server ({0})")]
    Denied(String),
    /// The user did not finish within [`LOGIN_TIMEOUT`].
    #[error("timed out waiting for the browser login to finish")]
    Timeout,
    /// The local listener failed.
    #[error("local callback server failed: {0}")]
    Io(#[from] std::io::Error),
}

/// What a request to the loopback listener asked for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route<'a> {
    /// `GET /callback?{query}`.
    Callback(&'a str),
    /// Anything else (favicon, scanners, other methods).
    NotFound,
}

/// Parse an HTTP request line (`GET /callback?x HTTP/1.1`) into a [`Route`].
pub(crate) fn route(request_line: &str) -> Route<'_> {
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Route::NotFound;
    };
    if method != "GET" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Route::NotFound;
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path == "/callback" {
        Route::Callback(query)
    } else {
        Route::NotFound
    }
}

/// Validate the callback query and return the authorization code.
///
/// `state` is checked first, in constant time, before anything else in the
/// request is honoured (including `error=`): a missing, duplicated, or wrong
/// `state` gives [`LoopbackError::StateMismatch`]. With our `state`,
/// `error=access_denied` is [`LoopbackError::Cancelled`], any other `error`
/// is [`LoopbackError::Denied`] (only a short, well-formed code is echoed,
/// to the terminal), and a missing or duplicated `code` is
/// [`LoopbackError::Malformed`].
pub(crate) fn parse_callback_query(
    query: &str,
    expected_state: &str,
) -> Result<SecretString, LoopbackError> {
    let mut codes = Vec::new();
    let mut states = Vec::new();
    let mut errors = Vec::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => codes.push(value.into_owned()),
            "state" => states.push(value.into_owned()),
            "error" => errors.push(value.into_owned()),
            _ => {}
        }
    }
    let [state] = states.as_slice() else {
        return Err(LoopbackError::StateMismatch);
    };
    if !bool::from(state.as_bytes().ct_eq(expected_state.as_bytes())) {
        return Err(LoopbackError::StateMismatch);
    }
    if let Some(error) = errors.first() {
        if error == "access_denied" {
            return Err(LoopbackError::Cancelled);
        }
        let printable = error.len() <= 64
            && !error.is_empty()
            && error.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
        return Err(LoopbackError::Denied(if printable {
            error.clone()
        } else {
            "unknown error".to_string()
        }));
    }
    match codes.as_slice() {
        [code] if !code.is_empty() => Ok(SecretString::from(code.clone())),
        _ => Err(LoopbackError::Malformed),
    }
}

const PAGE_HEAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>videocall-cli</title><style>body{font-family:system-ui,sans-serif;\
max-width:32rem;margin:4rem auto;padding:0 1rem;line-height:1.5}</style></head><body>";

fn foreign_page() -> String {
    format!(
        "{PAGE_HEAD}<h1>Not part of a login</h1><p>This request doesn't match the \
         login in progress and was ignored.</p></body></html>"
    )
}

fn success_page() -> String {
    format!(
        "{PAGE_HEAD}<h1>Almost done</h1><p>Return to your terminal to finish logging \
         in. You can close this tab.</p></body></html>"
    )
}

fn error_page() -> String {
    format!(
        "{PAGE_HEAD}<h1>Login failed</h1><p>Go back to the terminal for details, \
         then run <code>videocall-cli auth login</code> again.</p></body></html>"
    )
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Read up to the end of the request line. Returns `None` on timeout,
/// oversize, or a non-UTF-8 line.
async fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let read = async {
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(2).position(|w| w == b"\r\n") {
                return String::from_utf8(buf[..end].to_vec()).ok();
            }
            if buf.len() > MAX_REQUEST_HEAD {
                return None;
            }
        }
    };
    tokio::time::timeout(REQUEST_READ_TIMEOUT, read)
        .await
        .ok()
        .flatten()
}

/// A bound loopback listener waiting for one `/callback` request.
pub struct LoopbackServer {
    listener: TcpListener,
    port: u16,
}

impl LoopbackServer {
    /// Bind `127.0.0.1` on an ephemeral port.
    pub async fn bind() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    /// `http://127.0.0.1:{port}/callback`, the exact spelling the server
    /// requires.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Serve until the first `/callback` request (or `timeout`), reply with a
    /// small page, and close the listener.
    ///
    /// Other paths get a 404 and do not end the wait, so a browser's
    /// speculative connection or a favicon request can't block or abort the
    /// login. Exactly one `/callback` is processed; later ones get the error
    /// page.
    pub async fn wait_for_code(
        self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<SecretString, LoopbackError> {
        let (tx, mut rx) = mpsc::channel::<Result<SecretString, LoopbackError>>(1);
        let claimed = Arc::new(AtomicBool::new(false));
        let state: Arc<str> = Arc::from(expected_state);
        let mut connections = JoinSet::new();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        let result = loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (mut stream, _) = match accepted {
                        Ok(pair) => pair,
                        Err(e) => break Err(LoopbackError::Io(e)),
                    };
                    let tx = tx.clone();
                    let claimed = claimed.clone();
                    let state = state.clone();
                    connections.spawn(async move {
                        let Some(line) = read_request_line(&mut stream).await else {
                            return;
                        };
                        let Route::Callback(query) = route(&line) else {
                            respond(&mut stream, "404 Not Found", "Not found").await;
                            return;
                        };
                        let outcome = parse_callback_query(query, &state);
                        // Without our `state` the request proves nothing:
                        // answer it, but don't let it use up the one
                        // callback or end the login.
                        if matches!(outcome, Err(LoopbackError::StateMismatch)) {
                            respond(&mut stream, "400 Bad Request", &foreign_page()).await;
                            return;
                        }
                        if claimed.swap(true, Ordering::SeqCst) {
                            respond(&mut stream, "400 Bad Request", &error_page()).await;
                            return;
                        }
                        let (status, page) = match outcome {
                            Ok(_) => ("200 OK", success_page()),
                            Err(_) => ("400 Bad Request", error_page()),
                        };
                        respond(&mut stream, status, &page).await;
                        let _ = tx.send(outcome).await;
                    });
                }
                Some(outcome) = rx.recv() => break outcome,
                () = &mut deadline => break Err(LoopbackError::Timeout),
            }
        };
        connections.abort_all();
        drop(self.listener);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    const STATE: &str = "state-0123456789abcdef";

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_values_fit_server_limits() {
        let pkce = Pkce::generate().unwrap();
        let v = pkce.verifier.expose_secret();
        assert_eq!(v.len(), 43);
        assert!(v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(pkce.challenge.len(), 43);
        let state = generate_state().unwrap();
        assert!((16..=128).contains(&state.len()));
        assert!(state.bytes().all(|b| b.is_ascii_graphic()));
        assert_ne!(state, generate_state().unwrap());
    }

    #[test]
    fn routes() {
        assert_eq!(
            route("GET /callback?code=a&state=b HTTP/1.1"),
            Route::Callback("code=a&state=b")
        );
        assert_eq!(route("GET /callback HTTP/1.1"), Route::Callback(""));
        assert_eq!(route("GET /favicon.ico HTTP/1.1"), Route::NotFound);
        assert_eq!(route("GET /callback/x?code=a HTTP/1.1"), Route::NotFound);
        assert_eq!(route("POST /callback?code=a HTTP/1.1"), Route::NotFound);
        assert_eq!(route("GET /callback"), Route::NotFound);
        assert_eq!(route("garbage"), Route::NotFound);
    }

    #[test]
    fn callback_accepts_matching_state() {
        let code = parse_callback_query(&format!("code=abc123&state={STATE}"), STATE).unwrap();
        assert_eq!(code.expose_secret(), "abc123");
    }

    #[test]
    fn callback_rejects_wrong_or_missing_state() {
        for q in [
            "code=abc&state=state-XXXXXXXXXXXXXXXX".to_string(),
            "code=abc&state=short".to_string(),
            "code=abc".to_string(),
            format!("code=a&state={STATE}&state={STATE}"),
            // `error` is not honoured without our state.
            "error=access_denied".to_string(),
            "error=access_denied&state=wrong-state-0000000".to_string(),
        ] {
            assert!(
                matches!(
                    parse_callback_query(&q, STATE),
                    Err(LoopbackError::StateMismatch)
                ),
                "{q}"
            );
        }
    }

    #[test]
    fn callback_rejects_missing_or_duplicate_code() {
        for q in [
            format!("state={STATE}"),
            format!("code=&state={STATE}"),
            format!("code=a&code=b&state={STATE}"),
        ] {
            assert!(
                matches!(
                    parse_callback_query(&q, STATE),
                    Err(LoopbackError::Malformed)
                ),
                "{q}"
            );
        }
    }

    #[test]
    fn callback_error_param() {
        let err =
            parse_callback_query(&format!("error=access_denied&state={STATE}"), STATE).unwrap_err();
        assert!(matches!(err, LoopbackError::Cancelled));
        let err =
            parse_callback_query(&format!("error=server_error&state={STATE}"), STATE).unwrap_err();
        assert!(matches!(err, LoopbackError::Denied(ref e) if e == "server_error"));
        let err =
            parse_callback_query(&format!("error=%3Cscript%3E&state={STATE}"), STATE).unwrap_err();
        assert!(matches!(err, LoopbackError::Denied(ref e) if e == "unknown error"));
    }

    #[test]
    fn pages_do_not_reflect_input_or_overclaim() {
        assert!(!error_page().contains("script"));
        assert!(!foreign_page().contains("script"));
        assert!(!success_page().contains("code="));
        assert!(!success_page().contains("logged in</h1>"));
        assert!(success_page().contains("Return to your terminal"));
    }

    async fn get(port: u16, target: &str) -> std::io::Result<String> {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await?;
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await?;
        let mut out = String::new();
        s.read_to_string(&mut out).await?;
        Ok(out)
    }

    #[tokio::test]
    async fn serves_exactly_one_callback() {
        let server = LoopbackServer::bind().await.unwrap();
        let port = server.port;
        assert_eq!(
            server.redirect_uri(),
            format!("http://127.0.0.1:{port}/callback")
        );
        let wait =
            tokio::spawn(async move { server.wait_for_code(STATE, Duration::from_secs(10)).await });

        // An idle speculative connection and a wrong path don't end the wait.
        let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let not_found = get(port, "/favicon.ico").await.unwrap();
        assert!(not_found.starts_with("HTTP/1.1 404"));

        let ok = get(port, &format!("/callback?code=the-code&state={STATE}"))
            .await
            .unwrap();
        assert!(ok.starts_with("HTTP/1.1 200"));
        assert!(!ok.contains("the-code"));
        let code = wait.await.unwrap().unwrap();
        assert_eq!(code.expose_secret(), "the-code");

        // The listener is gone: a second request can't be served.
        assert!(get(port, &format!("/callback?code=x&state={STATE}"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn foreign_requests_do_not_end_the_login() {
        let server = LoopbackServer::bind().await.unwrap();
        let port = server.port;
        let wait =
            tokio::spawn(async move { server.wait_for_code(STATE, Duration::from_secs(10)).await });
        for target in [
            "/callback?code=c&state=state-attacker-000000".to_string(),
            "/callback?error=access_denied".to_string(),
            "/callback?error=access_denied&state=state-attacker-000000".to_string(),
        ] {
            let page = get(port, &target).await.unwrap();
            assert!(page.starts_with("HTTP/1.1 400"), "{target}");
            assert!(!page.contains("attacker"));
        }
        assert!(!wait.is_finished());

        let ok = get(port, &format!("/callback?code=real&state={STATE}"))
            .await
            .unwrap();
        assert!(ok.starts_with("HTTP/1.1 200"));
        assert_eq!(wait.await.unwrap().unwrap().expose_secret(), "real");
    }

    #[tokio::test]
    async fn cancel_with_our_state_ends_the_login() {
        let server = LoopbackServer::bind().await.unwrap();
        let port = server.port;
        let wait =
            tokio::spawn(async move { server.wait_for_code(STATE, Duration::from_secs(10)).await });
        let page = get(
            port,
            &format!("/callback?error=access_denied&state={STATE}"),
        )
        .await
        .unwrap();
        assert!(page.starts_with("HTTP/1.1 400"));
        assert!(matches!(wait.await.unwrap(), Err(LoopbackError::Cancelled)));
    }

    #[tokio::test]
    async fn times_out() {
        let server = LoopbackServer::bind().await.unwrap();
        let result = server.wait_for_code(STATE, Duration::from_millis(50)).await;
        assert!(matches!(result, Err(LoopbackError::Timeout)));
    }
}
