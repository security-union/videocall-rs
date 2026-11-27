// SPDX-License-Identifier: MIT OR Apache-2.0

//! Read-only registry access over the OCI distribution API.
//!
//! The registry is the source of truth: `tags/list` says which builds exist,
//! and a manifest HEAD gives the `Docker-Content-Digest` the kubelet resolves.
//! Nothing is pinned unless the registry itself confirmed it.
//!
//! Authentication follows the distribution spec: a request goes out without
//! credentials, and a `401` names the scheme in `WWW-Authenticate`. For
//! `Bearer` the client asks the named realm for a pull token (anonymous
//! unless credentials are configured); for `Basic` it needs credentials.
//! Credentials come only from the environment, never from the config file:
//!
//! - `DEPLOY_PINS_REGISTRY_HOST`: the registry they belong to (`ghcr.io`).
//! - `DEPLOY_PINS_REGISTRY_USER`, `DEPLOY_PINS_REGISTRY_PASSWORD`.
//!
//! They are never printed, and only sent over https: to that host, or to the
//! https token realm that host names in its own challenge. The client
//! refuses plain http and follows at most a few redirects, without the
//! Authorization header.
//!
//! Repository names are normalized the way the container runtime does it
//! (`github.com/distribution/reference`): a first path component with a `.`
//! or `:` in it, or `localhost`, is the registry host (`ghcr.io/owner/app`).
//! Anything else is on Docker Hub, and a one-component name is `library/x`.

use crate::error::{Error, Result};
use crate::pin::Digest;
use base64::Engine;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

/// Docker Hub's registry API host.
pub const DOCKER_HUB: &str = "registry-1.docker.io";

/// A repository as written, plus where its registry API lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    /// Exactly as configured and as values.yaml must say it.
    pub written: String,
    /// Registry API host (`registry-1.docker.io` for Docker Hub).
    pub host: String,
    /// Repository path on that registry (`library/nginx`, `owner/app`).
    pub path: String,
}

impl ImageRef {
    /// Parse a repository name with no tag or digest.
    pub fn parse(written: &str) -> std::result::Result<Self, String> {
        if written.contains('@') {
            return Err(format!(
                "{written:?} holds a digest; give the repository only"
            ));
        }
        let looks_like_host = |c: &str| c.contains(['.', ':']) || c == "localhost";
        let (host, path) = match written.split_once('/') {
            Some((first, rest)) if looks_like_host(first) => {
                (normalize_host(first).to_string(), rest.to_string())
            }
            None if looks_like_host(written) => {
                return Err(format!(
                    "{written:?} is a registry host without a repository path"
                ));
            }
            _ => (DOCKER_HUB.to_string(), written.to_string()),
        };
        let host_ok = !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'));
        if !host_ok {
            return Err(format!("{written:?} has an invalid registry host {host:?}"));
        }
        let path = if host == DOCKER_HUB && !path.contains('/') {
            format!("library/{path}")
        } else {
            path
        };
        let component_ok = |c: &str| {
            !c.is_empty()
                && c.bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
                && c.starts_with(|ch: char| ch.is_ascii_alphanumeric())
                && c.ends_with(|ch: char| ch.is_ascii_alphanumeric())
        };
        if !path.split('/').all(component_ok) {
            return Err(format!(
                "{written:?} is not a repository name (lowercase letters, digits, '.', '_', '-', \
                 no tag)"
            ));
        }
        Ok(ImageRef {
            written: written.to_string(),
            host,
            path,
        })
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.written)
    }
}

/// What the pin logic needs from a registry.
pub trait Registry {
    /// Every tag of the repository.
    fn tags(&self, image: &ImageRef) -> Result<Vec<String>>;
    /// The digest `image:tag` resolves to right now.
    fn digest(&self, image: &ImageRef, tag: &str) -> Result<Digest>;
}

/// RFC 3986 unreserved characters stay as they are in token query values.
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');
const PAGE_SIZE: usize = 1000;
/// More pages than this means something is off; fail instead of guessing.
const MAX_PAGES: usize = 50;
/// Largest response body read, tags/list pages included.
const BODY_LIMIT: u64 = 8 << 20;
/// Every manifest kind a multi-arch or single-arch image can be stored as.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json";

/// Retry schedule for 429, 5xx and network errors.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts, including the first.
    pub attempts: u32,
    /// Wait before the second attempt; doubles after that.
    pub base: Duration,
    /// Longest wait, also caps a server's `Retry-After`.
    pub max_wait: Duration,
    /// Timeout of one HTTP request, connect to last body byte.
    pub request_timeout: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: 5,
            base: Duration::from_secs(1),
            max_wait: Duration::from_secs(30),
            request_timeout: Duration::from_secs(20),
        }
    }
}

impl RetryPolicy {
    /// The longest one attempt can take: the request, plus a token fetch.
    pub fn attempt_cost(&self) -> Duration {
        self.request_timeout * 2
    }
}

/// One HTTP attempt's outcome, as far as retrying is concerned.
#[derive(Debug)]
pub enum Attempt<T> {
    /// Done.
    Ok(T),
    /// Worth another try (429, 5xx, network). Optional server-requested wait.
    Retry(String, Option<Duration>),
    /// Not worth another try.
    Fail(String),
    /// 404: the repository or tag does not exist.
    Missing,
}

/// Run `attempt` under `policy`, sleeping with `sleep` between tries.
/// `remaining` is the time left in the caller's budget, if it has one; no
/// attempt starts that could not finish inside it.
pub fn with_retries<T>(
    policy: &RetryPolicy,
    what: &str,
    remaining: &dyn Fn() -> Option<Duration>,
    sleep: &dyn Fn(Duration),
    mut attempt: impl FnMut() -> Attempt<T>,
) -> Result<T> {
    let spent = |why: &str| {
        Error::Registry(format!(
            "{what}: {why}; registry time budget spent, giving up"
        ))
    };
    if remaining().is_some_and(|left| left < policy.attempt_cost()) {
        return Err(spent("not started"));
    }
    let mut wait = policy.base;
    for n in 1..=policy.attempts {
        match attempt() {
            Attempt::Ok(value) => return Ok(value),
            Attempt::Fail(why) => return Err(Error::Registry(format!("{what}: {why}"))),
            Attempt::Missing => {
                return Err(Error::NotFound(format!("{what}: not found (HTTP 404)")));
            }
            Attempt::Retry(why, server_wait) => {
                if n == policy.attempts {
                    return Err(Error::Registry(format!(
                        "{what}: {why} (gave up after {n} attempts)"
                    )));
                }
                let pause = server_wait.unwrap_or(wait).min(policy.max_wait);
                if remaining().is_some_and(|left| left < pause + policy.attempt_cost()) {
                    return Err(spent(&why));
                }
                if !pause.is_zero() {
                    eprintln!("{what}: {why}; retrying in {}s", pause.as_secs());
                    sleep(pause);
                }
                wait = (wait * 2).min(policy.max_wait);
            }
        }
    }
    unreachable!("the loop returns on the last attempt")
}

/// How a status code should be treated.
pub fn classify_status(status: u16) -> StatusClass {
    match status {
        200..=299 => StatusClass::Ok,
        401 => StatusClass::Unauthorized,
        404 => StatusClass::NotFound,
        429 | 500..=599 => StatusClass::Retry,
        _ => StatusClass::Fail,
    }
}

/// See [`classify_status`].
#[derive(Debug, PartialEq, Eq)]
pub enum StatusClass {
    /// 2xx.
    Ok,
    /// 401: authenticate, or refresh an expired token, once.
    Unauthorized,
    /// 404.
    NotFound,
    /// 429 or 5xx.
    Retry,
    /// Anything else.
    Fail,
}

/// A parsed `WWW-Authenticate` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    /// `bearer` or `basic`, lowercased.
    pub scheme: String,
    /// Parameters, names lowercased.
    pub params: HashMap<String, String>,
}

/// Parse every challenge in a `WWW-Authenticate` value (RFC 7235: a scheme,
/// then auth-params that are tokens or quoted strings with backslash
/// escapes; `Basic realm="r", Bearer realm="..."` holds two). `None` if it
/// does not parse.
pub fn parse_challenges(header: &str) -> Option<Vec<Challenge>> {
    let mut challenges: Vec<Challenge> = Vec::new();
    let mut chars = header.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let token: String =
            std::iter::from_fn(|| chars.next_if(|c| !matches!(c, '=' | ',') && !c.is_whitespace()))
                .collect();
        if token.is_empty() {
            return match chars.peek() {
                None => Some(challenges),
                Some(_) => None,
            };
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next_if_eq(&'=').is_none() {
            // A token not followed by '=' starts the next challenge.
            challenges.push(Challenge {
                scheme: token.to_ascii_lowercase(),
                params: HashMap::new(),
            });
            continue;
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let value = if chars.next_if_eq(&'"').is_some() {
            let mut value = String::new();
            loop {
                match chars.next()? {
                    '\\' => value.push(chars.next()?),
                    '"' => break,
                    c => value.push(c),
                }
            }
            value
        } else {
            std::iter::from_fn(|| chars.next_if(|c| *c != ','))
                .collect::<String>()
                .trim()
                .to_string()
        };
        challenges
            .last_mut()?
            .params
            .insert(token.to_ascii_lowercase(), value);
    }
}

/// The challenge to answer: Bearer when offered, else the first one.
pub fn pick_challenge(header: &str) -> Option<Challenge> {
    let mut all = parse_challenges(header)?;
    let at = all.iter().position(|c| c.scheme == "bearer").unwrap_or(0);
    (!all.is_empty()).then(|| all.swap_remove(at))
}

/// Parse a `Link: <...>; rel="next"` header into the next page's path. Only
/// a path under this repository's `tags/list` is accepted, and a Link header
/// without a next page is an error: a truncated tag list could hide the
/// newest build.
pub fn next_page(link: &str, repo_path: &str) -> Result<String> {
    for part in link.split(',') {
        let mut pieces = part.split(';').map(str::trim);
        let target = pieces.next().unwrap_or_default();
        let is_next = pieces.any(|p| {
            p.split_once('=').is_some_and(|(name, value)| {
                name.trim().eq_ignore_ascii_case("rel")
                    && value
                        .trim()
                        .trim_matches('"')
                        .split_whitespace()
                        .any(|rel| rel.eq_ignore_ascii_case("next"))
            })
        });
        if !is_next {
            continue;
        }
        let path = target
            .strip_prefix('<')
            .and_then(|t| t.strip_suffix('>'))
            .ok_or_else(|| Error::Registry(format!("malformed Link header {link:?}")))?;
        let expected = format!("/v2/{repo_path}/tags/list?");
        if !path.starts_with(&expected) {
            return Err(Error::Registry(format!(
                "Link header points outside {expected}: {path:?}"
            )));
        }
        return Ok(path.to_string());
    }
    Err(Error::Registry(format!(
        "Link header {link:?} has no rel=next page; refusing a truncated tag list"
    )))
}

/// Tags from one `tags/list` page body.
pub fn parse_tags_page(body: &str) -> Result<Vec<String>> {
    let bad = || Error::Registry("unexpected tags/list body".into());
    let page: serde_json::Value = serde_json::from_str(body).map_err(|_| bad())?;
    match &page["tags"] {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::Array(tags) => tags
            .iter()
            .map(|t| t.as_str().map(String::from).ok_or_else(bad))
            .collect(),
        _ => Err(bad()),
    }
}

/// HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// GET.
    Get,
    /// HEAD.
    Head,
}

/// One request.
#[derive(Debug, Clone)]
pub struct Request {
    /// Method.
    pub method: Method,
    /// Full URL.
    pub url: String,
    /// Extra headers.
    pub headers: Vec<(&'static str, String)>,
}

/// One response. Header names are lowercase.
#[derive(Debug, Clone)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Headers.
    pub headers: Vec<(String, String)>,
    /// The body, or why it could not be read (a reset mid-body, a timeout).
    pub body: std::result::Result<String, String>,
}

impl Response {
    /// First header with this (lowercase) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Sends requests. `Err` means no response at all (DNS, connect, TLS).
pub trait Transport {
    /// Send one request.
    fn send(&self, request: &Request) -> std::result::Result<Response, String>;
}

/// [`Transport`] over `ureq` with rustls.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    /// A client whose every request must finish within `timeout`. It
    /// follows at most 3 redirects and never sends Authorization on one.
    pub fn new(timeout: Duration, https_only: bool) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .https_only(https_only)
            .max_redirects(3)
            .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
            .http_status_as_error(false)
            .user_agent(concat!("deploy-pins/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        UreqTransport { agent }
    }
}

impl Transport for UreqTransport {
    fn send(&self, request: &Request) -> std::result::Result<Response, String> {
        let result = match request.method {
            Method::Get => {
                let mut builder = self.agent.get(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(*name, value);
                }
                builder.call()
            }
            Method::Head => {
                let mut builder = self.agent.head(&request.url);
                for (name, value) in &request.headers {
                    builder = builder.header(*name, value);
                }
                builder.call()
            }
        };
        let mut resp = result.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().ok()?.to_string(),
                ))
            })
            .collect();
        let body = match request.method {
            Method::Head => Ok(String::new()),
            Method::Get => resp
                .body_mut()
                .with_config()
                .limit(BODY_LIMIT)
                .read_to_string()
                .map_err(|e| e.to_string()),
        };
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

/// Registry credentials from the environment. Debug output hides them.
#[derive(Clone)]
pub struct Credentials {
    host: String,
    user: String,
    password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials {{ host: {:?}, .. }}", self.host)
    }
}

fn normalize_host(host: &str) -> &str {
    match host {
        "docker.io" | "index.docker.io" => DOCKER_HUB,
        other => other,
    }
}

impl Credentials {
    /// Read `DEPLOY_PINS_REGISTRY_{HOST,USER,PASSWORD}`. All three or none.
    pub fn from_env() -> Result<Option<Self>> {
        let get = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        match (
            get("DEPLOY_PINS_REGISTRY_HOST"),
            get("DEPLOY_PINS_REGISTRY_USER"),
            get("DEPLOY_PINS_REGISTRY_PASSWORD"),
        ) {
            (None, None, None) => Ok(None),
            (Some(host), Some(user), Some(password)) => Ok(Some(Credentials {
                host: normalize_host(&host).to_string(),
                user,
                password,
            })),
            _ => Err(Error::Refused(
                "set all of DEPLOY_PINS_REGISTRY_HOST, _USER and _PASSWORD, or none".into(),
            )),
        }
    }

    fn basic(&self) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", self.user, self.password))
        )
    }
}

/// [`Registry`] over the OCI distribution API.
pub struct OciRegistry<T: Transport> {
    transport: T,
    policy: RetryPolicy,
    credentials: Option<Credentials>,
    /// Authorization header per `host/path`, once a challenge was answered.
    auth: RefCell<HashMap<String, String>>,
    deadline: Option<Instant>,
    sleep: Box<dyn Fn(Duration)>,
}

impl OciRegistry<UreqTransport> {
    /// A real client: default retries, environment credentials, and an
    /// optional cap on the total time spent talking to registries.
    pub fn from_env(budget: Option<Duration>) -> Result<Self> {
        let policy = RetryPolicy::default();
        Ok(OciRegistry::new(
            UreqTransport::new(policy.request_timeout, true),
            policy,
            Credentials::from_env()?,
            budget,
        ))
    }
}

impl<T: Transport> OciRegistry<T> {
    /// A client over any transport.
    pub fn new(
        transport: T,
        policy: RetryPolicy,
        credentials: Option<Credentials>,
        budget: Option<Duration>,
    ) -> Self {
        OciRegistry {
            transport,
            policy,
            credentials,
            auth: RefCell::new(HashMap::new()),
            // A budget too large for the clock means no deadline.
            deadline: budget.and_then(|b| Instant::now().checked_add(b)),
            sleep: Box::new(std::thread::sleep),
        }
    }

    fn credentials_for(&self, host: &str) -> Option<&Credentials> {
        self.credentials.as_ref().filter(|c| c.host == host)
    }

    /// Answer a 401 challenge: returns the Authorization header to use.
    fn authenticate(&self, image: &ImageRef, resp: &Response) -> Attempt<String> {
        let Some(challenge) = resp.header("www-authenticate").and_then(pick_challenge) else {
            return Attempt::Fail("HTTP 401 without a usable WWW-Authenticate challenge".into());
        };
        let creds = self.credentials_for(&image.host);
        match challenge.scheme.as_str() {
            "basic" => match creds {
                Some(c) => Attempt::Ok(c.basic()),
                None => Attempt::Fail(format!(
                    "{} asks for credentials; set DEPLOY_PINS_REGISTRY_HOST, _USER and _PASSWORD",
                    image.host
                )),
            },
            "bearer" => {
                let Some(realm) = challenge.params.get("realm") else {
                    return Attempt::Fail("Bearer challenge without a realm".into());
                };
                if !realm.starts_with("https://") {
                    return Attempt::Fail(format!("token realm {realm:?} is not https"));
                }
                let scope = challenge
                    .params
                    .get("scope")
                    .cloned()
                    .unwrap_or_else(|| format!("repository:{}:pull", image.path));
                let sep = if realm.contains('?') { '&' } else { '?' };
                let mut url = format!("{realm}{sep}scope={}", utf8_percent_encode(&scope, QUERY));
                if let Some(service) = challenge.params.get("service") {
                    url.push_str(&format!("&service={}", utf8_percent_encode(service, QUERY)));
                }
                let mut headers = Vec::new();
                if let Some(c) = creds {
                    headers.push(("Authorization", c.basic()));
                }
                let request = Request {
                    method: Method::Get,
                    url,
                    headers,
                };
                let token_resp = match self.transport.send(&request) {
                    Ok(r) => r,
                    Err(e) => return Attempt::Retry(format!("token request: {e}"), None),
                };
                match classify_status(token_resp.status) {
                    StatusClass::Ok => {}
                    StatusClass::Retry => {
                        return Attempt::Retry(
                            format!("token request: HTTP {}", token_resp.status),
                            None,
                        );
                    }
                    _ => {
                        return Attempt::Fail(format!(
                            "token request refused: HTTP {}",
                            token_resp.status
                        ));
                    }
                }
                let body = match token_resp.body {
                    Ok(body) => body,
                    Err(e) => return Attempt::Retry(format!("token body: {e}"), None),
                };
                let json: serde_json::Value = match serde_json::from_str(&body) {
                    Ok(v) => v,
                    Err(_) => return Attempt::Fail("token response is not JSON".into()),
                };
                match json["token"]
                    .as_str()
                    .or_else(|| json["access_token"].as_str())
                {
                    Some(token) if !token.is_empty() => Attempt::Ok(format!("Bearer {token}")),
                    _ => Attempt::Fail("token response has no token".into()),
                }
            }
            other => Attempt::Fail(format!("unsupported auth scheme {other:?}")),
        }
    }

    /// One request with retries, authentication on the first 401, and one
    /// refresh if a cached token was rejected.
    fn request(&self, image: &ImageRef, what: &str, method: Method, url: &str) -> Result<Response> {
        let key = format!("{}/{}", image.host, image.path);
        let mut authenticated_here = false;
        let remaining = || {
            self.deadline
                .map(|d| d.saturating_duration_since(Instant::now()))
        };
        with_retries(&self.policy, what, &remaining, &*self.sleep, || {
            let mut headers = vec![("Accept", MANIFEST_ACCEPT.to_string())];
            if let Some(auth) = self.auth.borrow().get(&key) {
                headers.push(("Authorization", auth.clone()));
            }
            let request = Request {
                method,
                url: url.to_string(),
                headers,
            };
            let resp = match self.transport.send(&request) {
                Ok(resp) => resp,
                Err(e) => return Attempt::Retry(e, None),
            };
            match classify_status(resp.status) {
                StatusClass::Ok => match &resp.body {
                    Ok(_) => Attempt::Ok(resp),
                    Err(e) => Attempt::Retry(format!("reading the body: {e}"), None),
                },
                StatusClass::Unauthorized if authenticated_here => {
                    Attempt::Fail("HTTP 401 with fresh credentials".into())
                }
                StatusClass::Unauthorized => match self.authenticate(image, &resp) {
                    Attempt::Ok(header) => {
                        authenticated_here = true;
                        self.auth.borrow_mut().insert(key.clone(), header);
                        Attempt::Retry("HTTP 401, authenticating".into(), Some(Duration::ZERO))
                    }
                    Attempt::Retry(why, wait) => Attempt::Retry(why, wait),
                    Attempt::Fail(why) => Attempt::Fail(why),
                    Attempt::Missing => Attempt::Missing,
                },
                StatusClass::NotFound => Attempt::Missing,
                StatusClass::Retry => {
                    let after = resp
                        .header("retry-after")
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    Attempt::Retry(format!("HTTP {}", resp.status), after)
                }
                StatusClass::Fail => Attempt::Fail(format!("HTTP {}", resp.status)),
            }
        })
    }

    fn base(&self, image: &ImageRef) -> String {
        format!("https://{}", image.host)
    }
}

impl<T: Transport> Registry for OciRegistry<T> {
    fn tags(&self, image: &ImageRef) -> Result<Vec<String>> {
        let mut path = Some(format!("/v2/{}/tags/list?n={PAGE_SIZE}", image.path));
        let mut tags = Vec::new();
        let mut pages = 0;
        while let Some(current) = path.take() {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(Error::Registry(format!(
                    "{image} has more than {MAX_PAGES} pages of tags; refusing a truncated list"
                )));
            }
            let url = format!("{}{current}", self.base(image));
            let resp = self.request(image, &format!("tags/list for {image}"), Method::Get, &url)?;
            tags.extend(parse_tags_page(resp.body.as_deref().unwrap_or_default())?);
            path = resp
                .header("link")
                .map(|link| next_page(link, &image.path))
                .transpose()?;
        }
        Ok(tags)
    }

    fn digest(&self, image: &ImageRef, tag: &str) -> Result<Digest> {
        let url = format!("{}/v2/{}/manifests/{tag}", self.base(image), image.path);
        let resp = self.request(
            image,
            &format!("manifest {image}:{tag}"),
            Method::Head,
            &url,
        )?;
        let header = resp
            .header("docker-content-digest")
            .ok_or_else(|| Error::Registry(format!("{image}:{tag}: no Docker-Content-Digest")))?;
        Digest::parse(header).ok_or_else(|| {
            Error::Registry(format!(
                "{image}:{tag} digest {header:?} is not sha256:<64 hex>"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted [`Transport`] for tests: replies in order, records requests.
    #[derive(Default)]
    pub struct ScriptedTransport {
        /// Replies, consumed front to back.
        pub replies: RefCell<VecDeque<std::result::Result<Response, String>>>,
        /// Every request sent.
        pub sent: RefCell<Vec<Request>>,
    }

    impl ScriptedTransport {
        /// Queue a reply.
        pub fn reply(
            self,
            status: u16,
            headers: &[(&str, &str)],
            body: std::result::Result<&str, &str>,
        ) -> Self {
            self.replies.borrow_mut().push_back(Ok(Response {
                status,
                headers: headers
                    .iter()
                    .map(|(n, v)| (n.to_string(), v.to_string()))
                    .collect(),
                body: body.map(str::to_string).map_err(str::to_string),
            }));
            self
        }

        /// Queue a connection failure.
        pub fn no_response(self, why: &str) -> Self {
            self.replies.borrow_mut().push_back(Err(why.to_string()));
            self
        }
    }

    impl Transport for &ScriptedTransport {
        fn send(&self, request: &Request) -> std::result::Result<Response, String> {
            self.sent.borrow_mut().push(request.clone());
            self.replies
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("script ran out".into()))
        }
    }
    use std::cell::Cell;

    const D: &str = "sha256:e1af8a1063203b62fbcc418c2cfede548cce0e1952d1d62dade9a6b51301626b";
    const HUB_CHALLENGE: &str = r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:acme/app:pull""#;

    fn fast() -> RetryPolicy {
        RetryPolicy {
            attempts: 4,
            base: Duration::from_secs(1),
            max_wait: Duration::from_secs(5),
            request_timeout: Duration::from_secs(1),
        }
    }

    fn no_budget() -> Option<Duration> {
        None
    }

    fn image() -> ImageRef {
        ImageRef::parse("acme/app").unwrap()
    }

    fn client(
        t: &ScriptedTransport,
        creds: Option<Credentials>,
    ) -> OciRegistry<&ScriptedTransport> {
        let mut registry = OciRegistry::new(t, fast(), creds, None);
        registry.sleep = Box::new(|_| {});
        registry
    }

    #[test]
    fn retries_with_backoff_then_succeeds() {
        let waits = RefCell::new(Vec::new());
        let mut calls = 0;
        let got = with_retries(
            &fast(),
            "x",
            &no_budget,
            &|d| waits.borrow_mut().push(d),
            || {
                calls += 1;
                if calls < 3 {
                    Attempt::Retry("HTTP 503".into(), None)
                } else {
                    Attempt::Ok(7)
                }
            },
        )
        .unwrap();
        assert_eq!(got, 7);
        assert_eq!(
            *waits.borrow(),
            [Duration::from_secs(1), Duration::from_secs(2)]
        );
    }

    #[test]
    fn gives_up_loudly_and_caps_retry_after() {
        let waits = RefCell::new(Vec::new());
        let err = with_retries(
            &fast(),
            "x",
            &no_budget,
            &|d| waits.borrow_mut().push(d),
            || Attempt::<()>::Retry("HTTP 429".into(), Some(Duration::from_secs(600))),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("gave up after 4 attempts"),
            "{err}"
        );
        assert_eq!(*waits.borrow(), vec![Duration::from_secs(5); 3]);
        assert_eq!(err.exit_code(), crate::error::EXIT_ERROR);
    }

    #[test]
    fn stops_when_the_budget_cannot_cover_another_attempt() {
        let left = Cell::new(Duration::from_secs(8));
        let calls = Cell::new(0);
        let err = with_retries(
            &fast(),
            "x",
            &|| Some(left.get()),
            &|d| left.set(left.get().saturating_sub(d)),
            || {
                calls.set(calls.get() + 1);
                Attempt::<()>::Retry("HTTP 503".into(), Some(Duration::from_secs(5)))
            },
        )
        .unwrap_err();
        // 8 s left: one wait of 5 s plus a 2 s attempt fits, the next does not.
        assert_eq!(calls.get(), 2);
        assert!(err.to_string().contains("budget spent"), "{err}");
        let err = with_retries(&fast(), "x", &|| Some(Duration::ZERO), &|_| {}, || {
            Attempt::Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("not started"), "{err}");
    }

    #[test]
    fn fails_fast_on_non_retryable_and_404_is_not_found() {
        let mut calls = 0;
        let err = with_retries(&fast(), "x", &no_budget, &|_| {}, || {
            calls += 1;
            Attempt::<()>::Fail("HTTP 403".into())
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(err.to_string().contains("403"));
        let err =
            with_retries(&fast(), "x", &no_budget, &|_| {}, || Attempt::<()>::Missing).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
        assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
    }

    #[test]
    fn status_classes() {
        assert_eq!(classify_status(200), StatusClass::Ok);
        assert_eq!(classify_status(401), StatusClass::Unauthorized);
        assert_eq!(classify_status(404), StatusClass::NotFound);
        assert_eq!(classify_status(429), StatusClass::Retry);
        assert_eq!(classify_status(502), StatusClass::Retry);
        assert_eq!(classify_status(403), StatusClass::Fail);
    }

    #[test]
    fn challenge_parsing() {
        let parse_challenge = pick_challenge;
        let c = parse_challenge(HUB_CHALLENGE).unwrap();
        assert_eq!(c.scheme, "bearer");
        assert_eq!(c.params["realm"], "https://auth.docker.io/token");
        assert_eq!(c.params["service"], "registry.docker.io");
        assert_eq!(c.params["scope"], "repository:acme/app:pull");

        // ghcr.io's shape, a comma inside a quoted value, an escape, odd spacing.
        let c = parse_challenge(
            r#"Bearer realm="https://ghcr.io/token" , service="ghcr.io",scope="repository:o/a:pull,push", note="a \"q\"""#,
        )
        .unwrap();
        assert_eq!(c.params["scope"], "repository:o/a:pull,push");
        assert_eq!(c.params["note"], "a \"q\"");
        assert_eq!(c.params.len(), 4);

        let c = parse_challenge(r#"Basic realm="Registry""#).unwrap();
        assert_eq!(c.scheme, "basic");
        assert_eq!(parse_challenge("Bearer").unwrap().params.len(), 0);
        assert!(parse_challenge(r#"Bearer realm="unterminated"#).is_none());
        assert!(parse_challenge(r#"realm="no scheme""#).is_none());
        assert!(parse_challenge("").is_none());

        // Two challenges in one header: Bearer wins wherever it is.
        let both = format!(r#"Basic realm="Registry", {HUB_CHALLENGE}"#);
        assert_eq!(parse_challenges(&both).unwrap().len(), 2);
        let c = parse_challenge(&both).unwrap();
        assert_eq!(c.scheme, "bearer");
        assert_eq!(c.params["service"], "registry.docker.io");
        assert_eq!(c.params.len(), 3);
    }

    #[test]
    fn discovers_the_token_endpoint_from_the_challenge() {
        let t = ScriptedTransport::default()
            .reply(401, &[("www-authenticate", HUB_CHALLENGE)], Ok(""))
            .reply(200, &[], Ok(r#"{"token":"t1"}"#))
            .reply(200, &[("docker-content-digest", D)], Ok(""));
        let registry = client(&t, None);
        assert_eq!(
            registry.digest(&image(), "main-8011b395").unwrap().as_str(),
            D
        );
        let sent = t.sent.borrow();
        assert_eq!(sent.len(), 3);
        assert_eq!(
            sent[0].url,
            "https://registry-1.docker.io/v2/acme/app/manifests/main-8011b395"
        );
        assert!(sent[0].headers.iter().all(|(n, _)| *n != "Authorization"));
        assert_eq!(
            sent[1].url,
            "https://auth.docker.io/token?scope=repository%3Aacme%2Fapp%3Apull&service=registry.docker.io"
        );
        assert!(sent[1].headers.is_empty(), "anonymous token request");
        assert!(
            sent[2]
                .headers
                .contains(&("Authorization", "Bearer t1".to_string()))
        );
    }

    #[test]
    fn refreshes_an_expired_token_once_then_fails() {
        let t = ScriptedTransport::default()
            .reply(401, &[("www-authenticate", HUB_CHALLENGE)], Ok(""))
            .reply(200, &[], Ok(r#"{"token":"t1"}"#))
            .reply(200, &[("docker-content-digest", D)], Ok(""))
            // Later: the cached token expired.
            .reply(401, &[("www-authenticate", HUB_CHALLENGE)], Ok(""))
            .reply(200, &[], Ok(r#"{"access_token":"t2"}"#))
            .reply(200, &[("docker-content-digest", D)], Ok(""))
            // And a registry that keeps saying no.
            .reply(401, &[("www-authenticate", HUB_CHALLENGE)], Ok(""))
            .reply(200, &[], Ok(r#"{"token":"t3"}"#))
            .reply(401, &[("www-authenticate", HUB_CHALLENGE)], Ok(""));
        let registry = client(&t, None);
        registry.digest(&image(), "a").unwrap();
        registry.digest(&image(), "b").unwrap();
        assert!(
            t.sent.borrow()[5]
                .headers
                .contains(&("Authorization", "Bearer t2".to_string()))
        );
        let err = registry.digest(&image(), "c").unwrap_err();
        assert!(
            err.to_string().contains("401 with fresh credentials"),
            "{err}"
        );
        assert_eq!(err.exit_code(), crate::error::EXIT_ERROR);
        assert_eq!(t.sent.borrow().len(), 9);
    }

    #[test]
    fn honours_retry_after_and_retries_a_body_read_error() {
        let waits = std::rc::Rc::new(RefCell::new(Vec::new()));
        let t = ScriptedTransport::default()
            .reply(429, &[("retry-after", "3")], Ok(""))
            .no_response("connection reset")
            .reply(200, &[], Err("unexpected end of body"))
            .reply(
                200,
                &[],
                Ok(r#"{"name":"acme/app","tags":["main-8011b395","latest"]}"#),
            );
        let mut registry = client(&t, None);
        let record = waits.clone();
        registry.sleep = Box::new(move |d| record.borrow_mut().push(d));
        let tags = registry.tags(&image()).unwrap();
        assert_eq!(tags, ["main-8011b395", "latest"]);
        assert_eq!(
            *waits.borrow(),
            [
                Duration::from_secs(3),
                Duration::from_secs(2),
                Duration::from_secs(4)
            ]
        );
    }

    #[test]
    fn a_missing_tag_is_not_found_not_a_network_error() {
        let t = ScriptedTransport::default().reply(404, &[], Ok("{}"));
        let err = client(&t, None)
            .digest(&image(), "main-deadbeef")
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
        assert_eq!(err.exit_code(), crate::error::EXIT_REFUSED);
    }

    #[test]
    fn credentials_only_go_to_their_host_and_never_into_debug_output() {
        let creds = Credentials {
            host: "ghcr.io".into(),
            user: "bot".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{creds:?}").contains("hunter2"));
        assert!(!format!("{creds:?}").contains("bot"));

        // Docker Hub image: the ghcr credentials stay home.
        let t = ScriptedTransport::default()
            .reply(401, &[("www-authenticate", HUB_CHALLENGE)], Ok(""))
            .reply(200, &[], Ok(r#"{"token":"t"}"#))
            .reply(200, &[("docker-content-digest", D)], Ok(""));
        client(&t, Some(creds.clone()))
            .digest(&image(), "x")
            .unwrap();
        assert!(t.sent.borrow()[1].headers.is_empty());

        // ghcr image: the token request carries them.
        let ghcr = ImageRef::parse("ghcr.io/o/a").unwrap();
        let t = ScriptedTransport::default()
            .reply(
                401,
                &[("www-authenticate", r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:o/a:pull""#)],
                Ok(""),
            )
            .reply(200, &[], Ok(r#"{"token":"t"}"#))
            .reply(200, &[("docker-content-digest", D)], Ok(""));
        client(&t, Some(creds)).digest(&ghcr, "x").unwrap();
        let sent = t.sent.borrow();
        assert_eq!(sent[0].url, "https://ghcr.io/v2/o/a/manifests/x");
        assert_eq!(
            sent[1].headers,
            [(
                "Authorization",
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode("bot:hunter2")
                )
            )]
        );
    }

    #[test]
    fn a_link_header_without_a_next_page_fails_instead_of_truncating() {
        let t = ScriptedTransport::default().reply(
            200,
            &[("link", "</v2/acme/app/tags/list?last=b>; rel=nope")],
            Ok(r#"{"tags":["a","b"]}"#),
        );
        let err = client(&t, None).tags(&image()).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }

    #[test]
    fn a_realm_with_a_query_string_keeps_it() {
        let t = ScriptedTransport::default()
            .reply(
                401,
                &[(
                    "www-authenticate",
                    r#"Bearer realm="https://auth.example/token?account=ci",service="reg""#,
                )],
                Ok(""),
            )
            .reply(200, &[], Ok(r#"{"token":"t"}"#))
            .reply(200, &[("docker-content-digest", D)], Ok(""));
        client(&t, None).digest(&image(), "x").unwrap();
        assert_eq!(
            t.sent.borrow()[1].url,
            "https://auth.example/token?account=ci&scope=repository%3Aacme%2Fapp%3Apull&service=reg"
        );
    }

    #[test]
    fn a_huge_budget_means_no_deadline_instead_of_a_panic() {
        let t = ScriptedTransport::default();
        let registry = OciRegistry::new(&t, fast(), None, Some(Duration::MAX));
        assert!(registry.deadline.is_none());
    }

    #[test]
    fn refuses_a_plain_http_realm_and_basic_without_credentials() {
        let t = ScriptedTransport::default().reply(
            401,
            &[(
                "www-authenticate",
                r#"Bearer realm="http://evil.example/token""#,
            )],
            Ok(""),
        );
        let err = client(&t, None).digest(&image(), "x").unwrap_err();
        assert!(err.to_string().contains("not https"), "{err}");
        let t = ScriptedTransport::default().reply(
            401,
            &[("www-authenticate", r#"Basic realm="r""#)],
            Ok(""),
        );
        let err = client(&t, None).digest(&image(), "x").unwrap_err();
        assert!(err.to_string().contains("asks for credentials"), "{err}");
    }

    #[test]
    fn paginates_and_stays_inside_the_repository() {
        let t = ScriptedTransport::default()
            .reply(
                200,
                &[(
                    "link",
                    r#"</v2/acme/app/tags/list?last=b&n=1000>; rel="next""#,
                )],
                Ok(r#"{"tags":["a","b"]}"#),
            )
            .reply(200, &[], Ok(r#"{"tags":["c"]}"#));
        assert_eq!(client(&t, None).tags(&image()).unwrap(), ["a", "b", "c"]);
        assert_eq!(
            t.sent.borrow()[1].url,
            "https://registry-1.docker.io/v2/acme/app/tags/list?last=b&n=1000"
        );

        let repo = "acme/app";
        let next = "/v2/acme/app/tags/list?last=b";
        assert_eq!(
            next_page(&format!("<{next}>; rel=next"), repo).unwrap(),
            next
        );
        assert_eq!(
            next_page(&format!(r#"<{next}>; REL="next""#), repo).unwrap(),
            next
        );
        assert!(next_page(r#"</x>; rel="prev""#, repo).is_err());
        assert!(next_page(r#"</v2/acme/app/tags/list?last=b>; rel="nxt""#, repo).is_err());
        assert!(next_page(r#"<https://evil.example/v2/x>; rel="next""#, repo).is_err());
        assert!(next_page(r#"/v2/no-brackets; rel="next""#, repo).is_err());
        assert!(
            parse_tags_page(r#"{"name":"x","tags":null}"#)
                .unwrap()
                .is_empty()
        );
        assert!(parse_tags_page("<html>").is_err());
    }

    /// Opt-in: `cargo test -p deploy-pins -- --ignored live_docker_hub`
    #[test]
    #[ignore = "talks to the real registry"]
    fn live_docker_hub() {
        let registry = OciRegistry::from_env(Some(Duration::from_secs(120))).unwrap();
        let media = ImageRef::parse("securityunion/videocall-media-server").unwrap();
        let tags = registry.tags(&media).unwrap();
        assert!(tags.iter().any(|t| t == "main-8011b395"));
        assert_eq!(
            registry.digest(&media, "main-8011b395").unwrap().as_str(),
            D
        );
        let err = registry.digest(&media, "main-00000000").unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    /// Opt-in: `cargo test -p deploy-pins -- --ignored live_ghcr_anonymous`
    #[test]
    #[ignore = "talks to the real registry"]
    fn live_ghcr_anonymous() {
        let registry = OciRegistry::from_env(Some(Duration::from_secs(120))).unwrap();
        let runner = ImageRef::parse("ghcr.io/actions/actions-runner").unwrap();
        let digest = registry.digest(&runner, "latest").unwrap();
        assert!(digest.as_str().starts_with("sha256:"));
        assert!(!registry.tags(&runner).unwrap().is_empty());
    }

    #[test]
    fn image_refs_normalize_like_the_runtime() {
        fn parts(s: &str) -> (String, String) {
            let r = ImageRef::parse(s).unwrap();
            assert_eq!(r.written, s);
            (r.host, r.path)
        }
        let hub = DOCKER_HUB.to_string();
        assert_eq!(parts("owner/app"), (hub.clone(), "owner/app".into()));
        assert_eq!(parts("nginx"), (hub.clone(), "library/nginx".into()));
        assert_eq!(
            parts("docker.io/owner/app"),
            (hub.clone(), "owner/app".into())
        );
        assert_eq!(parts("docker.io/nginx"), (hub, "library/nginx".into()));
        assert_eq!(
            parts("ghcr.io/owner/app"),
            ("ghcr.io".into(), "owner/app".into())
        );
        assert_eq!(
            parts("localhost:5000/a/b/c"),
            ("localhost:5000".into(), "a/b/c".into())
        );
        for bad in [
            "owner/app:latest",
            "owner/app@sha256:00",
            "Owner/app",
            "owner//app",
            "owner/app/",
            "owner/../app",
            "-owner/app",
            "",
            "ghcr.io/",
            "ghcr.io",
            "localhost:5000",
            "a?b/c",
        ] {
            assert!(ImageRef::parse(bad).is_err(), "{bad}");
        }
    }
}
