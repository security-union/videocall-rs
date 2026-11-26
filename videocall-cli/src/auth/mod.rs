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
//! Login is authorization code + PKCE against `{api}/cli/authorize`. The code
//! arrives on a loopback listener or, with `--remote`, is pasted by the
//! human, and is exchanged with a registration JWS signed by the new key at
//! `POST /api/v1/devices`. Nothing is saved before that exchange succeeds and
//! the server's fingerprint matches ours.

pub mod loopback;
pub mod store;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::io::Write as _;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use secrecy::SecretString;
use url::{Host, Url};
use videocall_meeting_client::device_session::{Clock, DeviceLogin, DeviceSession, KeyError};
use videocall_meeting_client::{reqwest, ApiError, AuthMode, MeetingApiClient};
use videocall_meeting_types::device::{
    normalize_device_name, CLI_REMOTE_REDIRECT_URI, DEVICE_NAME_MAX_LEN,
};

use crate::cli_args::{AuthCommand, LoginArgs};
use loopback::{LoopbackError, LoopbackServer, LOGIN_TIMEOUT};
use store::{Store, StoreError, StoredDevice};

/// Production meeting-api origin.
pub const DEFAULT_API_URL: &str = "https://api.videocall.rs";

/// Said whenever the CLI can't see what happened in the browser.
const BROWSER_HINT: &str =
    "Check the browser page for an error, then run `videocall-cli auth login` again.";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("this machine isn't logged in to {0}. Run `videocall-cli auth login` first.")]
    NotLoggedIn(String),
    /// A 401 the clock doesn't explain.
    #[error(
        "this device is no longer authorized (revoked or expired). Run `videocall-cli auth login`."
    )]
    DeviceRevoked,
    /// Login did not complete; nothing was saved.
    #[error("login failed: {0}")]
    Login(String),
    /// Bad `--api-url` or `--name`.
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}{}", if matches!(.0, KeyError::Corrupt) { "; run `videocall-cli auth login` again" } else { "" })]
    Key(#[from] KeyError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A meeting-api origin: the `aud` of every token and the store key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiOrigin(String);

impl ApiOrigin {
    /// `https://host[:port]` with no path, query or credentials; `http://`
    /// only for a loopback host (a dev stack).
    pub fn from_url(url: &Url) -> Result<Self, AuthError> {
        let loopback = match url.host() {
            Some(Host::Domain(domain)) => domain == "localhost",
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
        let bare = url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && matches!(url.path(), "" | "/")
            && url.query().is_none()
            && url.fragment().is_none();
        if !scheme_ok || !bare {
            return Err(AuthError::Config(format!(
                "--api-url must be an https:// origin like {DEFAULT_API_URL} (http:// only for localhost), got {url}"
            )));
        }
        Ok(Self(url.origin().ascii_serialization()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// This machine's device, ready to call meeting-api.
#[derive(Clone)]
pub struct Credentials(DeviceSession);

impl Credentials {
    /// A session for a stored device (its key, id and API origin).
    fn new(device: &StoredDevice, ctx: &Ctx) -> Result<Self, KeyError> {
        let (seed, id) = (&device.private_key, device.device_id.clone());
        let session =
            DeviceSession::from_seed(ctx.http.clone(), &device.origin, seed, id, ctx.clock);
        Ok(Self(session?))
    }

    /// This machine's device for `api`.
    pub async fn load(store: Store, api: ApiOrigin) -> Result<Self, AuthError> {
        let ctx = Ctx::new(store);
        let Some((device, _)) = ctx.store.load(api.as_str())? else {
            return Err(AuthError::NotLoggedIn(api.as_str().to_string()));
        };
        Ok(Self::new(&device, &ctx)?)
    }

    /// Run a signed meeting-api call (see [`DeviceSession::call`]); a 401
    /// the clock doesn't explain is [`AuthError::DeviceRevoked`].
    pub async fn call<T, F, Fut>(&self, f: F) -> Result<T, AuthError>
    where
        F: Fn(MeetingApiClient) -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        self.0.call(f).await.map_err(|e| match e {
            ApiError::NotAuthenticated => AuthError::DeviceRevoked,
            ApiError::Key(e) => AuthError::Key(e),
            e => AuthError::Api(e),
        })
    }
}

/// A browser can be expected: not over SSH, and on Linux/BSD only with a
/// desktop session.
pub(crate) fn browser_expected(env: impl Fn(&str) -> Option<String>, os: &str) -> bool {
    let set = |key: &str| env(key).is_some_and(|v| !v.is_empty());
    !set("SSH_CONNECTION")
        && (matches!(os, "macos" | "windows") || set("DISPLAY") || set("WAYLAND_DISPLAY"))
}

/// `--name`, or `videocall-cli on <hostname>` reduced to what the server accepts.
pub(crate) fn device_name(requested: Option<&str>, hostname: &str) -> Result<String, AuthError> {
    if let Some(name) = requested {
        return normalize_device_name(name).ok_or_else(|| {
            AuthError::Config(format!(
                "--name must start with a letter or digit and use only letters, digits, spaces and \
                 . _ ( ) @ : + - (at most {DEVICE_NAME_MAX_LEN} characters)"
            ))
        });
    }
    let host: String = hostname
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .collect();
    let mut name = format!("videocall-cli on {host}");
    name.truncate(DEVICE_NAME_MAX_LEN);
    Ok(normalize_device_name(&name).unwrap_or_else(|| "videocall-cli".to_string()))
}

fn hostname() -> String {
    let from_env = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok();
    from_env
        .or_else(|| {
            String::from_utf8(std::process::Command::new("hostname").output().ok()?.stdout).ok()
        })
        .unwrap_or_default()
}

/// Refuse an authorize URL that isn't on the API origin itself.
pub(crate) fn check_authorize_url(url: &str, api: &ApiOrigin) -> Result<(), AuthError> {
    match Url::parse(url) {
        Ok(parsed) if parsed.origin().ascii_serialization() == api.as_str() => Ok(()),
        _ => Err(AuthError::Login(format!(
            "the login URL is not on {}; refusing to open it",
            api.as_str()
        ))),
    }
}

/// A pasted code: trimmed, 1..=256 visible ASCII characters.
pub(crate) fn parse_pasted_code(line: &str) -> Result<SecretString, AuthError> {
    let code = line.trim();
    if code.is_empty() || code.len() > 256 || !code.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AuthError::Login(format!(
            "that doesn't look like the code from the browser. {BROWSER_HINT}"
        )));
    }
    Ok(SecretString::from(code.to_string()))
}

/// A plain-English message for a failed code exchange.
fn exchange_error(error: ApiError) -> AuthError {
    let message = match &error {
        ApiError::Device { error: code, .. } => match code.as_str() {
            "invalid_grant" => format!("the code was rejected (expired, already used, or from another login). {BROWSER_HINT}"),
            "too_many_devices" => "your account has the maximum number of devices; revoke one, then try again.".into(),
            "not_configured" => "this server has device login disabled.".into(),
            _ => return AuthError::Api(error),
        },
        _ => return AuthError::Api(error),
    };
    AuthError::Login(message)
}

/// The process-wide clock; it says so when it starts using the server's time.
static CLOCK: Clock = Clock::new(|offset| {
    eprintln!("warning: this machine's clock is {offset:+} s off the server's; using the server's time. Fix NTP.");
});

type ShowUrl = dyn Fn(&str, bool) -> bool + Send + Sync;

/// What the auth commands touch, replaceable in tests.
pub(crate) struct Ctx {
    pub store: Store,
    pub http: reqwest::Client,
    pub clock: &'static Clock,
    /// Show the authorize URL (`remote` = don't open a browser); returns
    /// whether a browser was opened.
    pub show_url: Arc<ShowUrl>,
    /// Read the line pasted in `--remote` mode.
    pub read_code: Arc<dyn Fn() -> std::io::Result<String> + Send + Sync>,
}

impl Ctx {
    fn new(store: Store) -> Self {
        static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
        let http = HTTP.get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default()
        });
        Self {
            store,
            http: http.clone(),
            clock: &CLOCK,
            show_url: Arc::new(|url, remote| !remote && webbrowser::open(url).is_ok()),
            read_code: Arc::new(|| {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).map(|_| line)
            }),
        }
    }

    /// Revoke a device with its own key; already revoked counts as done.
    async fn revoke(&self, device: &StoredDevice) -> Result<(), AuthError> {
        let session = Credentials::new(device, self)?.0;
        Ok(session.revoke().await?)
    }
}

/// Run `videocall-cli auth <command>`.
pub async fn run(command: &AuthCommand) -> anyhow::Result<()> {
    let ctx = Ctx::new(Store::open_default()?);
    match command {
        AuthCommand::Login(args) => login(&ctx, args).await?,
        AuthCommand::Logout(args) => logout(&ctx, &ApiOrigin::from_url(&args.api_url)?).await?,
        AuthCommand::Status(args) => {
            if !status(&ctx, &ApiOrigin::from_url(&args.api_url)?).await? {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}

async fn login(ctx: &Ctx, args: &LoginArgs) -> Result<(), AuthError> {
    let api = ApiOrigin::from_url(&args.api.api_url)?;
    let name = device_name(args.name.as_deref(), hostname().trim())?;
    let remote =
        args.remote || !browser_expected(|key| std::env::var(key).ok(), std::env::consts::OS);
    if remote && !args.remote {
        println!("No browser here, so logging in with --remote.");
    }
    login_with(ctx, &api, &name, remote).await
}

pub(crate) async fn login_with(
    ctx: &Ctx,
    api: &ApiOrigin,
    name: &str,
    remote: bool,
) -> Result<(), AuthError> {
    let client = MeetingApiClient::with_http(ctx.http.clone(), api.as_str(), AuthMode::Cookie);
    // The code is single-use, so fix a wrong clock before signing with it.
    if let Some(server_now) = client.server_time().await {
        ctx.clock.adopt_server_time(server_now);
    }

    let listener = if remote {
        None
    } else {
        Some(LoopbackServer::bind().await?)
    };
    let redirect_uri = listener
        .as_ref()
        .map_or(CLI_REMOTE_REDIRECT_URI.to_string(), |l| l.redirect_uri());
    let login = DeviceLogin::new(redirect_uri, name)?;
    let fingerprint = login.fingerprint();
    let url = login.authorize_url(&client)?;
    check_authorize_url(&url, api)?;

    println!("Logging in to {} as device \"{name}\".", api.as_str());
    println!("Key fingerprint (check it matches the page): {fingerprint}\n");
    let code = match listener {
        Some(server) => {
            let opened = (ctx.show_url)(&url, false);
            let verb = if opened {
                "Opened your browser. If it didn't open, visit"
            } else {
                "Open"
            };
            println!("{verb} this URL on this machine:\n\n    {url}\n");
            println!("No browser here? Press Ctrl-C and run `videocall-cli auth login --remote`.");
            server
                .wait_for_code(login.state(), LOGIN_TIMEOUT)
                .await
                .map_err(|e| match e {
                    LoopbackError::Timeout => AuthError::Login(format!(
                        "timed out waiting for the browser. {BROWSER_HINT}"
                    )),
                    other => AuthError::Login(other.to_string()),
                })?
        }
        None => {
            (ctx.show_url)(&url, true);
            println!(
                "Open this URL in a browser on any device where you can sign in:\n\n    {url}\n"
            );
            print!("Paste the code shown in your browser: ");
            let _ = std::io::stdout().flush();
            let read = ctx.read_code.clone();
            let line = tokio::task::spawn_blocking(move || read())
                .await
                .map_err(std::io::Error::other)??;
            parse_pasted_code(&line)?
        }
    };

    let registered = login
        .exchange(&client, &code, ctx.clock.now())
        .await
        .map_err(exchange_error)?;
    if registered.fingerprint != fingerprint {
        return Err(AuthError::Login(format!(
            "the server registered fingerprint {} but this key's is {fingerprint}; something between you and {} \
             is altering requests. Nothing was saved.",
            registered.fingerprint,
            api.as_str()
        )));
    }

    // Save the new device, then revoke the one it replaces.
    let previous = ctx.store.load(api.as_str()).unwrap_or_else(|e| {
        eprintln!("warning: couldn't read this machine's previous key ({e}); replacing it.");
        None
    });
    let device = StoredDevice {
        origin: api.as_str().into(),
        device_id: registered.device_id,
        fingerprint,
        name: registered.name,
        user_id: registered.user_id,
        user_name: registered.user_name,
        private_key: login.seed_b64(),
    };
    let location = match ctx.store.save(&device) {
        Ok(location) => location,
        Err(e) => {
            // Don't leave an approved device whose key is lost.
            if let Err(revoke) = ctx.revoke(&device).await {
                eprintln!("warning: couldn't revoke the new device either ({revoke}); revoke {} on the devices page.", device.name);
            }
            return Err(e.into());
        }
    };
    if let Some((old, _)) = previous.filter(|(old, _)| old.device_id != device.device_id) {
        if let Err(e) = ctx.revoke(&old).await {
            eprintln!("warning: couldn't revoke this machine's previous key ({e}); revoke {} on the devices page.", old.name);
        }
    }
    println!(
        "Logged in as {} ({}). Device {} [{}], key stored in {location}.",
        device.user_name, device.user_id, device.name, device.fingerprint
    );
    Ok(())
}

/// `auth logout`: revoke on the server (best effort), always delete locally.
pub(crate) async fn logout(ctx: &Ctx, api: &ApiOrigin) -> Result<(), AuthError> {
    match ctx.store.load(api.as_str()) {
        Ok(None) => {
            println!("No key for {} on this machine.", api.as_str());
            return Ok(());
        }
        Ok(Some((device, _))) => {
            if let Err(e) = ctx.revoke(&device).await {
                eprintln!(
                    "warning: couldn't revoke this machine on the server ({e}); deleting the key anyway. Remove {} on \
                     the devices page.",
                    device.name
                );
            }
        }
        Err(e) => eprintln!("warning: couldn't read the stored key ({e}); deleting it."),
    }
    ctx.store.remove(api.as_str())?;
    println!(
        "Logged out of {}; deleted this machine's key.",
        api.as_str()
    );
    Ok(())
}

/// `auth status`. Returns whether the device is valid.
pub(crate) async fn status(ctx: &Ctx, api: &ApiOrigin) -> Result<bool, AuthError> {
    println!("{}", api.as_str());
    let Some((device, location)) = ctx.store.load(api.as_str())? else {
        println!("  Not logged in. Run `videocall-cli auth login`.");
        return Ok(false);
    };
    println!("  Device:       {} ({})", device.name, device.device_id);
    println!("  Fingerprint:  {}", device.fingerprint);
    println!("  Key:          {location}");
    let creds = Credentials::new(&device, ctx)?;
    match creds
        .call(|client| async move { client.device_me().await })
        .await
    {
        Ok(me) => {
            let days = |at: i64| (at - ctx.clock.now()).max(0) / 86_400;
            println!("  Status:       valid");
            println!("  User:         {} ({})", me.user_name, me.user_id);
            println!(
                "  Expires:      in {} days if unused, {} days at most",
                days(me.idle_expires_at),
                days(me.abs_expires_at)
            );
            Ok(true)
        }
        Err(e) => {
            println!("  Status:       not valid ({e})");
            Ok(false)
        }
    }
}
