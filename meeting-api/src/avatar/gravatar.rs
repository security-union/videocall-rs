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

//! Gravatar input parsing and the one-time fetch made when the user saves.

use std::borrow::Cow;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use reqwest::{header, redirect, StatusCode};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use url::{Host, Url};
use videocall_meeting_types::avatar::{AVATAR_OUTPUT_DIMENSION, MAX_AVATAR_UPLOAD_BYTES};

use super::AvatarError;

/// Public Gravatar origin, used for fetches and for the owner-facing link.
pub const DEFAULT_GRAVATAR_BASE: &str = "https://gravatar.com";

const MAX_INPUT_BYTES: usize = 2048;
const MAX_CONCURRENT_FETCHES: usize = 4;

static AVATAR_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/avatar/([0-9a-fA-F]{32}|[0-9a-fA-F]{64})(\.(jpg|jpeg|png))?/?$")
        .expect("valid Gravatar path pattern")
});

/// A lowercase MD5 or SHA-256 hex Gravatar hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GravatarHash(String);

impl GravatarHash {
    /// The lowercase hex hash.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a Gravatar input was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GravatarInputError {
    Empty,
    TooLong,
    InvalidEmail,
    InvalidUrl,
    Scheme,
    Credentials,
    Port,
    Host,
    Path,
}

impl GravatarInputError {
    /// Stable reason code for logs.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::TooLong => "too_long",
            Self::InvalidEmail => "invalid_email",
            Self::InvalidUrl => "invalid_url",
            Self::Scheme => "scheme",
            Self::Credentials => "credentials",
            Self::Port => "port",
            Self::Host => "host",
            Self::Path => "path",
        }
    }
}

/// The default Gravatar origin as a URL.
pub fn default_base() -> Url {
    Url::parse(DEFAULT_GRAVATAR_BASE).expect("valid default Gravatar base")
}

/// The owner-facing link for a stored hash.
pub fn public_url(hash: &str) -> String {
    format!("{DEFAULT_GRAVATAR_BASE}/avatar/{hash}")
}

/// Accepts a Gravatar e-mail or a `gravatar.com/avatar/<hash>` link and returns its hash.
pub fn parse_gravatar_input(input: &str) -> Result<GravatarHash, GravatarInputError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(GravatarInputError::Empty);
    }
    if input.len() > MAX_INPUT_BYTES {
        return Err(GravatarInputError::TooLong);
    }
    if input.contains('@') && !input.contains("://") {
        hash_email(input)
    } else {
        hash_from_url(input)
    }
}

fn hash_email(email: &str) -> Result<GravatarHash, GravatarInputError> {
    let well_formed = email
        .rsplit_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
        && !email
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '/');
    if !well_formed {
        return Err(GravatarInputError::InvalidEmail);
    }
    Ok(GravatarHash(super::hex(&Sha256::digest(
        email.to_lowercase().as_bytes(),
    ))))
}

fn hash_from_url(input: &str) -> Result<GravatarHash, GravatarInputError> {
    let candidate = if input.contains("://") {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(format!("https://{input}"))
    };
    let url = Url::parse(&candidate).map_err(|_| GravatarInputError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(GravatarInputError::Scheme);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(GravatarInputError::Credentials);
    }
    if url.port().is_some() {
        return Err(GravatarInputError::Port);
    }
    match url.host() {
        Some(Host::Domain(domain))
            if domain == "gravatar.com" || domain.ends_with(".gravatar.com") => {}
        _ => return Err(GravatarInputError::Host),
    }
    let captures = AVATAR_PATH
        .captures(url.path())
        .ok_or(GravatarInputError::Path)?;
    Ok(GravatarHash(captures[1].to_ascii_lowercase()))
}

/// The only URL ever fetched for `hash`.
pub fn fetch_url(base: &Url, hash: &GravatarHash) -> String {
    format!(
        "{}/avatar/{}?s={AVATAR_OUTPUT_DIMENSION}&d=404&r=g",
        base.as_str().trim_end_matches('/'),
        hash.0
    )
}

/// Outbound Gravatar client: no redirects, short timeouts, bounded concurrency and body size.
pub struct GravatarClient {
    base: Url,
    http: reqwest::Client,
    permits: Semaphore,
}

impl GravatarClient {
    /// A client that fetches from `base`.
    pub fn new(base: Url) -> Self {
        let http = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .https_only(base.scheme() == "https")
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .user_agent(concat!("videocall-meeting-api/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("failed to build the Gravatar HTTP client");
        Self {
            base,
            http,
            permits: Semaphore::new(MAX_CONCURRENT_FETCHES),
        }
    }

    /// Downloads the image for `hash`, at most [`MAX_AVATAR_UPLOAD_BYTES`].
    pub async fn fetch(&self, hash: &GravatarHash) -> Result<Vec<u8>, AvatarError> {
        let _permit = self.permits.try_acquire().map_err(|_| AvatarError::Busy)?;
        let response = self
            .http
            .get(fetch_url(&self.base, hash))
            .header(header::ACCEPT, "image/png,image/jpeg,image/webp")
            .send()
            .await
            .map_err(transport_error)?;
        match response.status() {
            StatusCode::OK => read_capped(response).await,
            StatusCode::NOT_FOUND => Err(AvatarError::GravatarNotFound),
            _ => Err(AvatarError::GravatarUnavailable),
        }
    }
}

fn transport_error(error: reqwest::Error) -> AvatarError {
    if error.is_timeout() {
        AvatarError::GravatarTimeout
    } else {
        AvatarError::GravatarUnavailable
    }
}

async fn read_capped(mut response: reqwest::Response) -> Result<Vec<u8>, AvatarError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_AVATAR_UPLOAD_BYTES as u64)
    {
        return Err(AvatarError::GravatarTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if body.len() + chunk.len() > MAX_AVATAR_UPLOAD_BYTES {
            return Err(AvatarError::GravatarTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{headers, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const H32: &str = "0123456789abcdef0123456789abcdef";
    const H64: &str = "16c18d336f0b250f0e2d907452ceb9658a74ecdae8bc94864c23122a72cc27a5";
    const PNG: &[u8] = include_bytes!("../../tests/fixtures/avatars/ok-64.png");

    fn hash(input: &str) -> String {
        parse_gravatar_input(input)
            .unwrap_or_else(|e| panic!("{input}: {e:?}"))
            .0
    }

    #[test]
    fn accepts_avatar_links_on_any_gravatar_host() {
        for input in [
            format!("https://gravatar.com/avatar/{H64}"),
            format!("https://www.gravatar.com/avatar/{H64}"),
            format!("https://secure.gravatar.com/avatar/{H32}"),
            format!("https://0.gravatar.com/avatar/{H32}.jpg"),
            format!("http://1.gravatar.com/avatar/{H32}.png/"),
            format!("gravatar.com/avatar/{H64}.jpeg?s=80&d=mp#top"),
            format!("  https://GRAVATAR.com/avatar/{}  ", H64.to_uppercase()),
            format!("https://gravatar.com:443/avatar/{H32}"),
        ] {
            let expected = if input.contains(H32) { H32 } else { H64 };
            assert_eq!(hash(&input), expected, "{input}");
        }
        assert_eq!(
            hash(&format!(
                "https://gravatar.com/avatar/{}",
                H32.to_uppercase()
            )),
            H32
        );
    }

    #[test]
    fn hashes_a_trimmed_lowercased_email() {
        assert_eq!(hash(" MyEmail@Example.com "), H64);
    }

    #[test]
    fn refuses_everything_else() {
        let cases = [
            (String::new(), GravatarInputError::Empty),
            ("   ".to_string(), GravatarInputError::Empty),
            (
                format!("https://gravatar.com/avatar/{}", "a".repeat(2100)),
                GravatarInputError::TooLong,
            ),
            (
                format!("ftp://gravatar.com/avatar/{H32}"),
                GravatarInputError::Scheme,
            ),
            (
                format!("javascript://gravatar.com/avatar/{H32}"),
                GravatarInputError::Scheme,
            ),
            (
                format!("https://gravatar.com@evil.com/avatar/{H32}"),
                GravatarInputError::Credentials,
            ),
            (
                format!("https://u:p@gravatar.com/avatar/{H32}"),
                GravatarInputError::Credentials,
            ),
            (
                format!("https://gravatar.com:8443/avatar/{H32}"),
                GravatarInputError::Port,
            ),
            (
                format!("https://1.2.3.4/avatar/{H32}"),
                GravatarInputError::Host,
            ),
            (
                format!("https://[::1]/avatar/{H32}"),
                GravatarInputError::Host,
            ),
            (
                format!("https://gravatar.com./avatar/{H32}"),
                GravatarInputError::Host,
            ),
            (
                format!("https://gravatar.com.evil.io/avatar/{H32}"),
                GravatarInputError::Host,
            ),
            (
                format!("https://evilgravatar.com/avatar/{H32}"),
                GravatarInputError::Host,
            ),
            (
                format!("https://gravatar.com/avatar/{H32}/x"),
                GravatarInputError::Path,
            ),
            (
                "https://gravatar.com/someusername".to_string(),
                GravatarInputError::Path,
            ),
            (
                format!("https://gravatar.com/avatar/{}", &H32[..31]),
                GravatarInputError::Path,
            ),
            (
                format!("https://gravatar.com/avatar/{H32}0"),
                GravatarInputError::Path,
            ),
            (
                format!("https://gravatar.com/avatar/{}", &H64[..63]),
                GravatarInputError::Path,
            ),
            (
                format!("https://gravatar.com/avatar/{H64}0"),
                GravatarInputError::Path,
            ),
            (
                format!("https://gravatar.com/avatar/{}g", &H32[..31]),
                GravatarInputError::Path,
            ),
            (
                format!("https://gravatar.com/avatar/{H32}.gif"),
                GravatarInputError::Path,
            ),
            (
                "javascript:alert(1)".to_string(),
                GravatarInputError::InvalidUrl,
            ),
            (
                "a b@example.com".to_string(),
                GravatarInputError::InvalidEmail,
            ),
            ("@example.com".to_string(), GravatarInputError::InvalidEmail),
            (
                format!("u:p@gravatar.com/avatar/{H32}"),
                GravatarInputError::InvalidEmail,
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_gravatar_input(&input), Err(expected), "{input}");
        }
    }

    #[test]
    fn the_fetch_url_ignores_everything_the_user_pasted_but_the_hash() {
        let parsed = parse_gravatar_input(&format!(
            "https://gravatar.com/avatar/{}?d=https://evil&s=9999",
            H64.to_uppercase()
        ))
        .unwrap();
        assert_eq!(
            fetch_url(&default_base(), &parsed),
            format!("https://gravatar.com/avatar/{H64}?s=256&d=404&r=g")
        );
        assert_eq!(
            public_url(parsed.as_str()),
            format!("https://gravatar.com/avatar/{H64}")
        );
    }

    fn client(server: &str) -> GravatarClient {
        GravatarClient::new(Url::parse(server).unwrap())
    }

    fn h64() -> GravatarHash {
        GravatarHash(H64.to_string())
    }

    #[tokio::test]
    async fn fetches_exactly_the_rebuilt_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/avatar/{H64}")))
            .and(query_param("s", "256"))
            .and(query_param("d", "404"))
            .and(query_param("r", "g"))
            .and(headers(
                "accept",
                vec!["image/png", "image/jpeg", "image/webp"],
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(client(&server.uri()).fetch(&h64()).await.unwrap(), PNG);
    }

    #[tokio::test]
    async fn maps_upstream_statuses() {
        for (status, expected) in [
            (404, AvatarError::GravatarNotFound),
            (500, AvatarError::GravatarUnavailable),
            (204, AvatarError::GravatarUnavailable),
            (304, AvatarError::GravatarUnavailable),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            assert_eq!(
                client(&server.uri()).fetch(&h64()).await,
                Err(expected),
                "{status}"
            );
        }
    }

    #[tokio::test]
    async fn never_follows_a_redirect() {
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
            .expect(0)
            .mount(&target)
            .await;
        let origin = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/elsewhere", target.uri())),
            )
            .mount(&origin)
            .await;
        assert_eq!(
            client(&origin.uri()).fetch(&h64()).await,
            Err(AvatarError::GravatarUnavailable)
        );
    }

    #[tokio::test]
    async fn refuses_a_declared_oversize_body_without_reading_it() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\npartial",
                MAX_AVATAR_UPLOAD_BYTES + 1
            );
            let _ = socket.write_all(head.as_bytes()).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let started = std::time::Instant::now();
        assert_eq!(
            client(&format!("http://{addr}")).fetch(&h64()).await,
            Err(AvatarError::GravatarTooLarge)
        );
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn stops_reading_an_undeclared_oversize_body() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await;
            let chunk = vec![0u8; 16 * 1024];
            for _ in 0..(MAX_AVATAR_UPLOAD_BYTES / chunk.len() + 2) {
                let framed =
                    [format!("{:x}\r\n", chunk.len()).as_bytes(), &chunk, b"\r\n"].concat();
                if socket.write_all(&framed).await.is_err() {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        assert_eq!(
            client(&format!("http://{addr}")).fetch(&h64()).await,
            Err(AvatarError::GravatarTooLarge)
        );
    }

    #[tokio::test]
    async fn a_slow_drip_body_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (headers_sent, headers_received) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                .await;
            let _ = headers_sent.send(());
            for _ in 0..24 {
                if socket.write_all(b"1\r\nx\r\n").await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        let started = std::time::Instant::now();
        assert_eq!(
            client(&format!("http://{addr}")).fetch(&h64()).await,
            Err(AvatarError::GravatarTimeout)
        );
        assert!(headers_received.await.is_ok());
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[tokio::test]
    async fn sheds_fetches_beyond_the_concurrency_bound() {
        let gravatar = client("http://127.0.0.1:1");
        let _held = gravatar
            .permits
            .try_acquire_many(MAX_CONCURRENT_FETCHES as u32)
            .unwrap();
        assert_eq!(gravatar.fetch(&h64()).await, Err(AvatarError::Busy));
    }
}
