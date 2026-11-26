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

//! `videocall-cli auth ...`: give this machine an Ed25519 key, have a human
//! approve it in a browser, and sign every meeting-api request with it.
//!
//! Login works like `aws login`: authorization code + PKCE against
//! `{api}/cli/authorize`, with the code delivered either to a loopback
//! listener ([`loopback`]) or, with `--remote`, pasted by the human from the
//! browser. The code, PKCE verifier and a registration JWS signed by the new
//! key are exchanged at `POST /api/v1/devices` for an approved device.
//!
//! - [`keys`]: keygen, the registration JWS, and per-request device JWTs.
//! - [`clock`]: the clock-offset correction for machines with a wrong clock.
//! - [`store`]: the private key (keychain or a 0600 file) and the non-secret
//!   `hosts.toml`, keyed by API origin.
//!
//! There are no bearer secrets: each request carries a fresh JWT that lives
//! 120 s and is bound (`aud`) to the meeting-api origin. The private key
//! never leaves the store except to sign.

pub mod clock;
pub mod keys;
pub mod loopback;
pub mod store;

use std::ffi::OsString;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use secrecy::{ExposeSecret, SecretString};
use url::Url;
use videocall_meeting_client::devices::CliAuthorizeParams;
use videocall_meeting_client::{reqwest, ApiError, AuthMode, MeetingApiClient, TokenSource};
use videocall_meeting_types::device::{
    is_valid_device_id, normalize_device_name, RegisterDeviceRequest, DEVICE_NAME_MAX_LEN,
};

use crate::cli_args::{ApiArgs, LoginArgs};
use crate::net_policy::is_local_host;
use clock::Clock;
use keys::{DeviceKey, KeyError};
use loopback::{generate_state, LoopbackError, LoopbackServer, Pkce, LOGIN_TIMEOUT};
use store::{run_blocking, HostEntry, Secret, Store, StoreError, StoreKind};

/// Production meeting-api origin (`helm/global/us-east/meeting-api`).
pub const DEFAULT_API_URL: &str = "https://api.videocall.rs";

/// Upper bound on any single API request.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest pasted code accepted from the terminal.
const MAX_PASTED_CODE: usize = 256;

/// What to run after a revoked or expired device is detected.
const RELOGIN_HINT: &str = "Run `videocall-cli auth login`.";

/// Errors from login, logout, status and request signing.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// No device key for this API origin.
    #[error("this machine isn't logged in to {0}. Run `videocall-cli auth login` first.")]
    NotLoggedIn(String),
    /// A signed request was rejected even after correcting the clock.
    #[error("this device is no longer authorized (revoked or expired). {RELOGIN_HINT}")]
    DeviceRevoked,
    /// The human chose Deny in the browser.
    #[error("login cancelled in the browser. Nothing was saved.")]
    Cancelled,
    /// Login did not complete; nothing was saved.
    #[error("login failed: {0}")]
    Login(String),
    /// The server's fingerprint for our key differs from ours.
    #[error(
        "the server reports fingerprint {server} for this machine's key, but the key's \
         fingerprint is {local}. Something between you and {host} is altering requests; \
         nothing was saved."
    )]
    FingerprintMismatch {
        /// API host, for the message.
        host: String,
        /// Locally computed fingerprint.
        local: String,
        /// Fingerprint the server returned.
        server: String,
    },
    /// TLS certificate validation failed in a way a wrong clock explains.
    #[error("system clock looks wrong ({0}); fix NTP")]
    ClockWrong(String),
    /// Bad `--api-url`, `--name` or similar.
    #[error("{0}")]
    Config(String),
    /// A request took longer than [`HTTP_TIMEOUT`].
    #[error("the server did not respond within {} seconds", HTTP_TIMEOUT.as_secs())]
    Timeout,
    /// The API returned an error.
    #[error(transparent)]
    Api(#[from] ApiError),
    /// Local credential storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Key loading or signing failed.
    #[error(transparent)]
    Key(#[from] KeyError),
    /// Local I/O (loopback listener, terminal) failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<getrandom::Error> for AuthError {
    fn from(e: getrandom::Error) -> Self {
        AuthError::Key(KeyError::Random(e))
    }
}

/// Run an API call with [`HTTP_TIMEOUT`], turning a TLS failure caused by
/// a wrong clock into [`AuthError::ClockWrong`].
pub(crate) async fn with_timeout<T>(
    call: impl Future<Output = Result<T, ApiError>>,
) -> Result<T, AuthError> {
    match tokio::time::timeout(HTTP_TIMEOUT, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(ApiError::Network(e))) => Err(match clock_tls_failure(&e) {
            Some(detail) => AuthError::ClockWrong(detail),
            None => AuthError::Api(ApiError::Network(e)),
        }),
        Ok(Err(e)) => Err(AuthError::Api(e)),
        Err(_) => Err(AuthError::Timeout),
    }
}

/// If `error` is a certificate rejected as expired or not yet valid, a
/// description including the local time. Verification is never skipped.
fn clock_tls_failure(error: &(dyn std::error::Error + 'static)) -> Option<String> {
    let mut chain = Vec::new();
    let mut next = Some(error);
    while let Some(e) = next {
        chain.push(e.to_string());
        next = e.source();
    }
    let lower = chain.join(": ").to_ascii_lowercase();
    let expired = lower.contains("expired");
    let not_yet = lower.contains("notvalidyet") || lower.contains("not valid yet");
    (lower.contains("certificate") && (expired || not_yet)).then(|| {
        format!(
            "TLS certificate rejected as {}; local time is {}",
            if expired { "expired" } else { "not yet valid" },
            httpdate::fmt_http_date(SystemTime::now())
        )
    })
}

/// `n` bytes from the OS CSPRNG, base64url without padding.
pub(crate) fn random_b64url(n: usize) -> Result<String, getrandom::Error> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// A validated meeting-api origin, e.g. `https://api.videocall.rs`. It is
/// the `aud` of every token the CLI signs and the store key, so two servers
/// never share a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiOrigin {
    origin: String,
    host: String,
}

impl ApiOrigin {
    /// Validate `--api-url`.
    ///
    /// Must be `https://host[:port]` with no path, query, or credentials.
    /// Plain `http://` is accepted only for localhost and private-range
    /// addresses (a local dev stack).
    pub fn from_url(url: &Url) -> Result<Self, AuthError> {
        let host = url.host_str().unwrap_or_default().to_string();
        match url.scheme() {
            "https" => {}
            "http" if is_local_host(url) => {}
            "http" => {
                return Err(AuthError::Config(format!(
                    "refusing plain http:// for {host}: logins and signed requests would \
                     travel unencrypted. Use https://, or a localhost/private address for a \
                     dev stack."
                )))
            }
            other => {
                return Err(AuthError::Config(format!(
                    "--api-url must use https:// (got {other}://)"
                )))
            }
        }
        if host.is_empty()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(AuthError::Config(
                "--api-url must be just an origin, like https://api.videocall.rs".to_string(),
            ));
        }
        Ok(Self {
            origin: url.origin().ascii_serialization(),
            host,
        })
    }

    /// `scheme://host[:port]`, the exact `aud` for this server.
    pub fn as_str(&self) -> &str {
        &self.origin
    }

    /// Host name, for messages.
    pub fn host(&self) -> &str {
        &self.host
    }
}

/// The process-wide HTTP client. `reqwest::Client` is a handle onto one
/// connection pool, so every call reuses connections instead of paying a
/// TCP + TLS handshake each time.
pub fn http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!("couldn't configure the HTTP client ({e}); using defaults");
                    reqwest::Client::new()
                })
        })
        .clone()
}

/// Mints one device JWT per request for [`AuthMode::Signed`].
struct DeviceTokens {
    key: DeviceKey,
    device_id: String,
    aud: String,
    clock: &'static Clock,
    /// A minting failure, reported instead of the resulting 401.
    error: Mutex<Option<KeyError>>,
}

impl TokenSource for DeviceTokens {
    fn token(&self) -> Option<String> {
        match self
            .key
            .device_token(&self.device_id, &self.aud, self.clock.now())
        {
            Ok(token) => Some(token),
            Err(e) => {
                if let Ok(mut slot) = self.error.lock() {
                    *slot = Some(e);
                }
                None
            }
        }
    }
}

impl DeviceTokens {
    fn take_error(&self) -> Option<KeyError> {
        self.error.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// This machine's device, ready to call meeting-api.
#[derive(Clone)]
pub struct Credentials {
    tokens: Arc<DeviceTokens>,
    api: ApiOrigin,
    http: reqwest::Client,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api", &self.api)
            .field("device_id", &self.tokens.device_id)
            .field("key", &self.tokens.key)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Credentials for `device_id` signing with `key` against `api`.
    pub(crate) fn new(
        key: DeviceKey,
        device_id: String,
        api: ApiOrigin,
        http: reqwest::Client,
        clock: &'static Clock,
    ) -> Self {
        Self {
            tokens: Arc::new(DeviceTokens {
                key,
                device_id,
                aud: api.as_str().to_string(),
                clock,
                error: Mutex::new(None),
            }),
            api,
            http,
        }
    }

    /// Load this machine's device for `api` from `store`.
    pub async fn load(store: Store, api: ApiOrigin) -> Result<Self, AuthError> {
        let Some((host, key)) = load_device(&store, &api).await? else {
            return Err(AuthError::NotLoggedIn(api.host().to_string()));
        };
        Ok(Self::new(
            key,
            host.device_id,
            api,
            http_client(),
            Clock::global(),
        ))
    }

    /// The meeting-api origin.
    pub fn origin(&self) -> &ApiOrigin {
        &self.api
    }

    /// The device id (`kid`, `sub`).
    pub fn device_id(&self) -> &str {
        &self.tokens.device_id
    }

    /// A client that signs each request with a fresh device JWT.
    fn client(&self) -> MeetingApiClient {
        MeetingApiClient::with_http(
            self.http.clone(),
            self.api.as_str(),
            AuthMode::Signed(self.tokens.clone()),
        )
    }

    /// Run a meeting-api call. Every request inside it carries a freshly
    /// minted device JWT.
    ///
    /// On a 401, compare the server's `Date` with the local clock; if they
    /// differ by more than 30 s, adopt the offset and run `f` once more. Any
    /// remaining 401 means the device is revoked or expired.
    pub async fn call<T, F, Fut>(&self, f: F) -> Result<T, AuthError>
    where
        F: Fn(MeetingApiClient) -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        match with_timeout(f(self.client())).await {
            Err(AuthError::Api(ApiError::NotAuthenticated)) => {}
            other => return other,
        }
        if let Some(e) = self.tokens.take_error() {
            return Err(e.into());
        }
        let skewed = clock::server_time(&self.http, self.api.as_str())
            .await
            .is_some_and(|server_now| self.tokens.clock.adopt_server_time(server_now));
        if !skewed {
            return Err(AuthError::DeviceRevoked);
        }
        match with_timeout(f(self.client())).await {
            Err(AuthError::Api(ApiError::NotAuthenticated)) => match self.tokens.take_error() {
                Some(e) => Err(e.into()),
                None => Err(AuthError::DeviceRevoked),
            },
            other => other,
        }
    }
}

/// The stored metadata and key for `api`, if any.
async fn load_device(
    store: &Store,
    api: &ApiOrigin,
) -> Result<Option<(HostEntry, DeviceKey)>, AuthError> {
    let (store, origin) = (store.clone(), api.as_str().to_string());
    let found = run_blocking(move || {
        let Some(host) = store.host(&origin)? else {
            return Ok(None);
        };
        Ok(store.load_secret(&origin, host.store)?.map(|s| (host, s)))
    })
    .await?;
    match found {
        Some((host, secret)) => Ok(Some((host, DeviceKey::from_seed_b64(&secret.private_key)?))),
        None => Ok(None),
    }
}

async fn remove_device(store: &Store, api: &ApiOrigin) -> Result<(), AuthError> {
    let lock = store.lock().await?;
    let (store, origin) = (store.clone(), api.as_str().to_string());
    run_blocking(move || store.remove(&origin, &lock)).await?;
    Ok(())
}

/// Whether a browser can be expected to open on this machine.
///
/// False over SSH, and on Linux/BSD without `DISPLAY` or `WAYLAND_DISPLAY`.
pub(crate) fn browser_expected(env: impl Fn(&str) -> Option<OsString>, os: &str) -> bool {
    let set = |key: &str| env(key).is_some_and(|v| !v.is_empty());
    if set("SSH_CONNECTION") {
        return false;
    }
    match os {
        "macos" | "windows" => true,
        _ => set("DISPLAY") || set("WAYLAND_DISPLAY"),
    }
}

/// `videocall-cli on <hostname>` with the hostname filtered to
/// `[A-Za-z0-9._-]`, truncated to 64 characters (contract 6.1).
pub(crate) fn default_device_name(hostname: &str) -> String {
    let host: String = hostname
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .collect();
    let name = if host.is_empty() {
        "videocall-cli".to_string()
    } else {
        format!("videocall-cli on {host}")
    };
    // ASCII only after filtering, so byte truncation is char truncation.
    name[..name.len().min(DEVICE_NAME_MAX_LEN)].to_string()
}

fn hostname() -> String {
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
        })
        .unwrap_or_default()
}

/// The device name to register: `--name` validated per contract 6.1, or
/// the default.
pub(crate) fn device_name(requested: Option<&str>, hostname: &str) -> Result<String, AuthError> {
    match requested {
        Some(name) => normalize_device_name(name).ok_or_else(|| {
            AuthError::Config(format!(
                "--name must start with a letter or digit and use only letters, digits, \
                 spaces and . _ ( ) @ : + - (at most {DEVICE_NAME_MAX_LEN} characters)"
            ))
        }),
        None => Ok(normalize_device_name(&default_device_name(hostname))
            .unwrap_or_else(|| "videocall-cli".to_string())),
    }
}

/// Refuse an authorize URL that isn't http(s) on the API origin itself.
pub(crate) fn check_authorize_url(url: &str, api: &ApiOrigin) -> Result<(), AuthError> {
    let parsed =
        Url::parse(url).map_err(|_| AuthError::Login("malformed login URL".to_string()))?;
    let same_origin = matches!(parsed.scheme(), "http" | "https")
        && parsed.origin().ascii_serialization() == api.as_str();
    if same_origin {
        Ok(())
    } else {
        Err(AuthError::Login(format!(
            "the login URL is not on {}; refusing to open it",
            api.as_str()
        )))
    }
}

/// A pasted authorization code: trimmed, non-empty, visible ASCII, bounded.
pub(crate) fn parse_pasted_code(line: &str) -> Result<SecretString, AuthError> {
    let code = line.trim();
    if code.is_empty()
        || code.len() > MAX_PASTED_CODE
        || !code.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(AuthError::Login(format!(
            "that doesn't look like the code from the browser page. {BROWSER_HINT}"
        )));
    }
    Ok(SecretString::from(code.to_string()))
}

fn store_label(store: &Store, kind: StoreKind) -> String {
    match kind {
        StoreKind::Keychain if cfg!(target_os = "macos") => {
            format!("macOS Keychain (service \"{}\")", store::KEYCHAIN_SERVICE)
        }
        StoreKind::Keychain => format!(
            "Windows Credential Manager (service \"{}\")",
            store::KEYCHAIN_SERVICE
        ),
        StoreKind::File => format!("{} (plain text)", store.credentials_path().display()),
    }
}

fn fmt_unix(secs: i64) -> String {
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
}

/// Said whenever the CLI can't see what happened in the browser: the
/// authorize step reports its errors (key already registered, too many
/// pending logins) as a page there, never to the CLI.
const BROWSER_HINT: &str = "Check the browser page: if it shows an error, fix that first. Then \
                            run `videocall-cli auth login` again.";

/// A plain-English error for a failed code exchange.
fn exchange_error(error: AuthError) -> AuthError {
    let AuthError::Api(api) = error else {
        return error;
    };
    if let Some(message) = disabled_message(&api) {
        return AuthError::Login(message.to_string());
    }
    let message = match &api {
        ApiError::Device {
            status: 413 | 415, ..
        }
        | ApiError::ServerError {
            status: 413 | 415, ..
        } => "the server refused the login request's format; update videocall-cli.".to_string(),
        ApiError::Device { error, .. } => match error.as_str() {
            "invalid_grant" => format!(
                "the server rejected the login code (expired, already used, or from another \
                 login). {BROWSER_HINT}"
            ),
            "too_many_devices" => "your account has the maximum number of devices. Revoke one \
                                   on the devices page, then try again."
                .to_string(),
            "key_already_registered" => {
                "this key is already registered. Run `videocall-cli auth login` again for a \
                 new key."
                    .to_string()
            }
            _ => return AuthError::Api(api),
        },
        _ => return AuthError::Api(api),
    };
    AuthError::Login(message)
}

/// Message for a server that has device login turned off (503
/// `not_configured`), on any device endpoint.
fn disabled_message(error: &ApiError) -> Option<&'static str> {
    match error {
        ApiError::Device { error, .. } if error == "not_configured" => {
            Some("this server has device login disabled.")
        }
        _ => None,
    }
}

/// Shows the authorize URL; returns whether a browser was opened.
type ShowUrl = dyn Fn(&str, bool) -> bool + Send + Sync;
/// Reads the code pasted in `--remote` mode.
type ReadCode = dyn Fn() -> std::io::Result<String> + Send + Sync;

/// Dependencies of the auth commands, injectable for tests.
pub(crate) struct Ctx {
    pub store: Store,
    pub http: reqwest::Client,
    pub clock: &'static Clock,
    /// Whether a browser can be expected here (else login goes remote).
    pub browser_expected: bool,
    /// Given the authorize URL and whether this is a remote login, open it
    /// if appropriate. Returns true if a browser was opened.
    pub show_url: Arc<ShowUrl>,
    /// Read one line from the terminal (the pasted code).
    pub read_code: Arc<ReadCode>,
}

impl Ctx {
    fn from_env() -> Result<Self, AuthError> {
        Ok(Self {
            store: Store::open_default()?,
            http: http_client(),
            clock: Clock::global(),
            browser_expected: browser_expected(|key| std::env::var_os(key), std::env::consts::OS),
            show_url: Arc::new(|url, remote| !remote && webbrowser::open(url).is_ok()),
            read_code: Arc::new(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                Ok(line)
            }),
        })
    }

    fn credentials(&self, key: DeviceKey, device_id: String, api: &ApiOrigin) -> Credentials {
        Credentials::new(key, device_id, api.clone(), self.http.clone(), self.clock)
    }

    fn client(&self, api: &ApiOrigin) -> MeetingApiClient {
        MeetingApiClient::with_http(self.http.clone(), api.as_str(), AuthMode::Cookie)
    }
}

/// `videocall-cli auth login`.
pub async fn login(args: &LoginArgs) -> Result<(), AuthError> {
    let api = ApiOrigin::from_url(&args.api.api_url)?;
    let name = device_name(args.name.as_deref(), &hostname())?;
    let ctx = Ctx::from_env()?;
    let remote = args.remote || !ctx.browser_expected;
    if remote && !args.remote {
        println!("No browser here, so logging in with --remote.");
    }
    login_with(&ctx, &api, &name, remote).await
}

pub(crate) async fn login_with(
    ctx: &Ctx,
    api: &ApiOrigin,
    name: &str,
    remote: bool,
) -> Result<(), AuthError> {
    let client = ctx.client(api);

    // Correct a wrong clock before signing anything: the code is single-use,
    // so a registration rejected for its timestamps can't be retried.
    if let Some(server_now) = clock::server_time(&ctx.http, api.as_str()).await {
        ctx.clock.adopt_server_time(server_now);
    }

    // New key and PKCE. Nothing is written to the store until the exchange
    // succeeds.
    let key = DeviceKey::generate()?;
    let local_fingerprint = key.fingerprint();
    let pkce = Pkce::generate()?;
    let state = generate_state()?;
    let listener = if remote {
        None
    } else {
        Some(LoopbackServer::bind().await?)
    };
    let redirect_uri = match &listener {
        Some(server) => server.redirect_uri(),
        None => client.cli_code_redirect_uri(),
    };
    let url = client.cli_authorize_url(&CliAuthorizeParams {
        redirect_uri: &redirect_uri,
        code_challenge: &pkce.challenge,
        state: &state,
        public_key: &key.public_key_b64(),
        device_name: name,
    })?;
    check_authorize_url(&url, api)?;

    println!("Logging in to {} as device \"{name}\".", api.host());
    println!("Key fingerprint (check it matches the page): {local_fingerprint}");
    println!();
    let code = match listener {
        Some(server) => {
            let opened = (ctx.show_url)(&url, false);
            if opened {
                println!("Opened your browser. If it didn't open, visit this URL on this machine:");
            } else {
                println!("Open this URL in a browser on this machine:");
            }
            println!();
            println!("    {url}");
            println!();
            println!(
                "(No browser on this machine? Press Ctrl-C and run `videocall-cli auth login \
                 --remote`.)"
            );
            println!("Waiting for the browser login to finish (Ctrl-C to cancel)...");
            server
                .wait_for_code(&state, LOGIN_TIMEOUT)
                .await
                .map_err(|e| match e {
                    LoopbackError::Cancelled => AuthError::Cancelled,
                    LoopbackError::Timeout => AuthError::Login(format!(
                        "timed out waiting for the browser. {BROWSER_HINT}"
                    )),
                    other => AuthError::Login(other.to_string()),
                })?
        }
        None => {
            (ctx.show_url)(&url, true);
            println!("Open this URL in a browser on any device where you can sign in:");
            println!();
            println!("    {url}");
            println!();
            print!("Paste the code shown in your browser: ");
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let read = ctx.read_code.clone();
            let line = tokio::task::spawn_blocking(move || read())
                .await
                .map_err(|e| AuthError::Io(std::io::Error::other(e.to_string())))??;
            parse_pasted_code(&line)?
        }
    };

    // Exchange: the registration JWS proves possession of the key.
    let request = RegisterDeviceRequest {
        code: code.expose_secret().to_string(),
        code_verifier: pkce.verifier.expose_secret().to_string(),
        redirect_uri,
        registration: key.registration_jws(api.as_str(), name, ctx.clock.now())?,
    };
    let registered = with_timeout(client.register_device(&request))
        .await
        .map_err(exchange_error)?;

    if registered.fingerprint != local_fingerprint {
        return Err(AuthError::FingerprintMismatch {
            host: api.host().to_string(),
            local: local_fingerprint,
            server: registered.fingerprint,
        });
    }
    if !is_valid_device_id(&registered.device_id) {
        return Err(AuthError::Login(
            "the server returned a malformed device id; nothing was saved".to_string(),
        ));
    }

    // Save the new device, then revoke the one it replaces.
    let previous = load_device(&ctx.store, api).await.unwrap_or_else(|e| {
        eprintln!("warning: couldn't read this machine's previous key ({e}); replacing it.");
        None
    });
    let entry = HostEntry {
        device_id: registered.device_id.clone(),
        fingerprint: local_fingerprint,
        name: registered.name.clone(),
        user_id: registered.user_id.clone(),
        user_name: registered.user_name.clone(),
        store: StoreKind::File,
    };
    let secret = Secret {
        private_key: key.seed_b64(),
    };
    let lock = ctx.store.lock().await?;
    let (store, origin, saved) = (ctx.store.clone(), api.as_str().to_string(), entry.clone());
    let kind = run_blocking(move || store.save(&origin, saved, &secret, &lock)).await?;

    if let Some((old, old_key)) = previous {
        if old.device_id != entry.device_id {
            let revoked = ctx
                .credentials(old_key, old.device_id, api)
                .call(|client| async move { client.delete_device_me().await })
                .await;
            match revoked {
                Ok(()) | Err(AuthError::DeviceRevoked) => {}
                Err(e) => eprintln!(
                    "warning: couldn't revoke this machine's previous key {} [{}] ({e}); \
                     revoke it on the devices page.",
                    old.name, old.fingerprint
                ),
            }
        }
    }

    println!();
    println!(
        "Logged in as {} ({}). Device {} [{}]",
        entry.user_name, entry.user_id, entry.name, entry.fingerprint
    );
    println!("Key stored in the {}.", store_label(&ctx.store, kind));
    Ok(())
}

/// `videocall-cli auth logout`.
pub async fn logout(args: &ApiArgs) -> Result<(), AuthError> {
    let api = ApiOrigin::from_url(&args.api_url)?;
    logout_with(&Ctx::from_env()?, &api).await
}

pub(crate) async fn logout_with(ctx: &Ctx, api: &ApiOrigin) -> Result<(), AuthError> {
    let (store, origin) = (ctx.store.clone(), api.as_str().to_string());
    let Some(host) = run_blocking(move || store.host(&origin)).await? else {
        println!("No key for {} on this machine.", api.host());
        return Ok(());
    };
    // Best effort: revoke on the server, then always delete locally.
    match load_device(&ctx.store, api).await {
        Ok(Some((_, key))) => {
            let revoked = ctx
                .credentials(key, host.device_id.clone(), api)
                .call(|client| async move { client.delete_device_me().await })
                .await;
            match revoked {
                // 401: already revoked or expired on the server.
                Ok(()) | Err(AuthError::DeviceRevoked) => {}
                Err(e) => eprintln!(
                    "warning: couldn't revoke this machine on {} ({e}). The local key is \
                     deleted anyway; remove device {} [{}] on the devices page.",
                    api.host(),
                    host.name,
                    host.fingerprint
                ),
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("warning: couldn't read the stored key ({e}); deleting it."),
    }
    remove_device(&ctx.store, api).await?;
    println!(
        "Logged out of {}. Deleted device {} [{}] from this machine.",
        api.host(),
        host.name,
        host.fingerprint
    );
    Ok(())
}

/// `videocall-cli auth status`. Returns whether the device is valid.
pub async fn status(args: &ApiArgs) -> Result<bool, AuthError> {
    let api = ApiOrigin::from_url(&args.api_url)?;
    status_with(&Ctx::from_env()?, &api).await
}

pub(crate) async fn status_with(ctx: &Ctx, api: &ApiOrigin) -> Result<bool, AuthError> {
    println!("{}", api.as_str());
    let Some((host, key)) = load_device(&ctx.store, api).await? else {
        println!("  Not logged in. Run `videocall-cli auth login`.");
        println!("  Config directory: {}", ctx.store.dir().display());
        return Ok(false);
    };
    println!("  Device:       {} ({})", host.name, host.device_id);
    println!("  Fingerprint:  {}", host.fingerprint);
    println!("  Key:          {}", store_label(&ctx.store, host.store));

    let creds = ctx.credentials(key, host.device_id.clone(), api);
    match creds
        .call(|client| async move { client.device_me().await })
        .await
    {
        Ok(me) => {
            println!("  Status:       valid");
            println!("  User:         {} ({})", me.user_name, me.user_id);
            println!("  Expires:      {} if unused", fmt_unix(me.idle_expires_at));
            println!("  Expires by:   {}", fmt_unix(me.abs_expires_at));
            Ok(true)
        }
        Err(AuthError::DeviceRevoked) => {
            println!("  Status:       revoked or expired. {RELOGIN_HINT}");
            Ok(false)
        }
        Err(AuthError::Api(e)) if disabled_message(&e).is_some() => {
            println!(
                "  Status:       {}",
                disabled_message(&e).unwrap_or_default()
            );
            Ok(false)
        }
        Err(e) => {
            println!("  Status:       unknown, couldn't check ({e})");
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests;
