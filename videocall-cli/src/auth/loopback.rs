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

//! `state` is checked before anything else, so a request from any other
//! page on this machine neither uses up the one callback nor ends the login.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use videocall_meeting_client::device_session::ct_eq;
use videocall_meeting_types::device::RedirectUri;

pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, thiserror::Error)]
pub enum LoopbackError {
    #[error("login cancelled in the browser. Nothing was saved.")]
    Cancelled,
    #[error("the browser returned a failed or incomplete login; try again")]
    Malformed,
    #[error("timed out waiting for the browser")]
    Timeout,
    #[error("local callback server failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
enum Outcome {
    /// Not ours (wrong path, or no matching `state`): answer, keep waiting.
    Ignored,
    Done(Result<SecretString, LoopbackError>),
}

fn judge(request_line: &str, expected_state: &str) -> Outcome {
    let Some(query) = request_line
        .strip_prefix("GET /callback?")
        .and_then(|rest| {
            rest.strip_suffix(" HTTP/1.1")
                .or_else(|| rest.strip_suffix(" HTTP/1.0"))
        })
    else {
        return Outcome::Ignored;
    };
    let (mut codes, mut states, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "code" => codes.push(value.into_owned()),
            "state" => states.push(value.into_owned()),
            "error" => errors.push(value.into_owned()),
            _ => {}
        }
    }
    match states.as_slice() {
        [state] if ct_eq(state, expected_state) => {}
        _ => return Outcome::Ignored,
    }
    Outcome::Done(match (errors.as_slice(), codes.as_slice()) {
        ([error], _) if error == "access_denied" => Err(LoopbackError::Cancelled),
        ([], [code]) if !code.is_empty() => Ok(SecretString::from(code.clone())),
        _ => Err(LoopbackError::Malformed),
    })
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>videocall-cli</title>\
         <p style=\"font-family:system-ui;margin:4rem auto;max-width:32rem\">{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The request line, or `None` after 10 s, 8 KiB, or non-UTF-8 input.
async fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let read = async {
        loop {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.ok().filter(|&n| n > 0)?;
            buf.extend_from_slice(&chunk[..n]);
            if let Some(end) = buf.windows(2).position(|w| w == b"\r\n") {
                return String::from_utf8(buf[..end].to_vec()).ok();
            }
            if buf.len() > 8 * 1024 {
                return None;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .ok()
        .flatten()
}

pub struct LoopbackServer {
    listener: TcpListener,
    port: u16,
}

impl LoopbackServer {
    pub async fn bind() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    /// `http://127.0.0.1:{port}/callback`, the spelling the server requires.
    pub fn redirect_uri(&self) -> String {
        RedirectUri::Loopback { port: self.port }.as_uri()
    }

    pub async fn wait_for_code(
        self,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<SecretString, LoopbackError> {
        let (tx, mut rx) = mpsc::channel(1);
        let claimed = Arc::new(AtomicBool::new(false));
        let state: Arc<str> = Arc::from(expected_state);
        let mut connections = JoinSet::new();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let result = loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let mut stream = match accepted {
                        Ok((stream, _)) => stream,
                        Err(e) => break Err(LoopbackError::Io(e)),
                    };
                    let (tx, claimed, state) = (tx.clone(), claimed.clone(), state.clone());
                    connections.spawn(async move {
                        let Some(line) = read_request_line(&mut stream).await else { return };
                        match judge(&line, &state) {
                            Outcome::Ignored => {
                                respond(&mut stream, "404 Not Found", "This request is not part of a login in progress and was ignored.").await;
                            }
                            Outcome::Done(_) if claimed.swap(true, Ordering::SeqCst) => {
                                respond(&mut stream, "400 Bad Request", "This login has already finished.").await;
                            }
                            Outcome::Done(outcome) => {
                                let message = match outcome {
                                    Ok(_) => "Return to your terminal to finish logging in. You can close this tab.",
                                    Err(_) => "Login did not finish. Go back to your terminal for details.",
                                };
                                respond(&mut stream, if outcome.is_ok() { "200 OK" } else { "400 Bad Request" }, message).await;
                                let _ = tx.send(outcome).await;
                            }
                        }
                    });
                }
                Some(outcome) = rx.recv() => break outcome,
                () = &mut deadline => break Err(LoopbackError::Timeout),
            }
        };
        connections.abort_all();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    const STATE: &str = "state-0123456789abcdef";

    #[test]
    fn judging_requests() {
        let s = STATE;
        let ignored = [
            "GET /favicon.ico HTTP/1.1".to_string(),
            format!("POST /callback?code=a&state={s} HTTP/1.1"),
            format!("GET /callback/x?code=a&state={s} HTTP/1.1"),
            "GET /callback?code=a&state=state-XXXXXXXXXXXXXXXX HTTP/1.1".into(),
            "GET /callback?code=a HTTP/1.1".into(),
            format!("GET /callback?code=a&state={s}&state={s} HTTP/1.1"),
            "GET /callback?error=access_denied HTTP/1.1".into(),
        ];
        for line in ignored {
            assert!(matches!(judge(&line, s), Outcome::Ignored), "{line}");
        }
        let done = |q: &str| match judge(&format!("GET /callback?{q} HTTP/1.1"), s) {
            Outcome::Done(result) => result,
            Outcome::Ignored => panic!("{q} ignored"),
        };
        assert_eq!(
            done(&format!("code=abc&state={s}"))
                .unwrap()
                .expose_secret(),
            "abc"
        );
        assert!(matches!(
            done(&format!("error=access_denied&state={s}")),
            Err(LoopbackError::Cancelled)
        ));
        for q in [
            format!("error=server_error&state={s}"),
            format!("state={s}"),
            format!("code=&state={s}"),
            format!("code=a&code=b&state={s}"),
        ] {
            assert!(matches!(done(&q), Err(LoopbackError::Malformed)), "{q}");
        }
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
    async fn serves_exactly_one_callback_and_ignores_foreign_requests() {
        let server = LoopbackServer::bind().await.unwrap();
        let port = server.port;
        assert_eq!(
            server.redirect_uri(),
            format!("http://127.0.0.1:{port}/callback")
        );
        let wait =
            tokio::spawn(async move { server.wait_for_code(STATE, Duration::from_secs(10)).await });

        let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let page = get(port, "/callback?code=c&state=state-attacker-000000").await;
        let page = page.unwrap();
        assert!(page.starts_with("HTTP/1.1 404") && !page.contains("attacker"));
        assert!(!wait.is_finished());

        let ok = get(port, &format!("/callback?code=the-code&state={STATE}"))
            .await
            .unwrap();
        assert!(ok.starts_with("HTTP/1.1 200") && !ok.contains("the-code"));
        assert_eq!(wait.await.unwrap().unwrap().expose_secret(), "the-code");
        assert!(get(port, "/callback").await.is_err(), "listener closed");
    }

    #[tokio::test]
    async fn cancel_and_timeout_end_the_login() {
        let server = LoopbackServer::bind().await.unwrap();
        let port = server.port;
        let wait =
            tokio::spawn(async move { server.wait_for_code(STATE, Duration::from_secs(10)).await });
        get(
            port,
            &format!("/callback?error=access_denied&state={STATE}"),
        )
        .await
        .unwrap();
        assert!(matches!(wait.await.unwrap(), Err(LoopbackError::Cancelled)));

        let server = LoopbackServer::bind().await.unwrap();
        let result = server.wait_for_code(STATE, Duration::from_millis(50)).await;
        assert!(matches!(result, Err(LoopbackError::Timeout)));
    }
}
