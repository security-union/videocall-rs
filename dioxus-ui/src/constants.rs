// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::Deserialize;
use serde_wasm_bindgen::from_value as from_js_value;
use videocall_types::truthy;
use wasm_bindgen::JsValue;
use web_sys::window;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RuntimeConfig {
    #[serde(rename = "apiBaseUrl")]
    pub api_base_url: String,
    #[serde(rename = "meetingApiBaseUrl")]
    #[serde(default)]
    pub meeting_api_base_url: Option<String>,
    #[serde(rename = "wsUrl")]
    pub ws_url: String,
    #[serde(rename = "webTransportHost")]
    pub web_transport_host: String,
    #[serde(rename = "oauthEnabled")]
    pub oauth_enabled: String,
    #[serde(rename = "e2eeEnabled")]
    pub e2ee_enabled: String,
    #[serde(rename = "webTransportEnabled")]
    pub web_transport_enabled: String,
    #[serde(rename = "firefoxEnabled")]
    #[serde(default)]
    pub firefox_enabled: String,
    #[serde(rename = "usersAllowedToStream")]
    pub users_allowed_to_stream: String,
    #[serde(rename = "oauthProvider")]
    #[serde(default)]
    pub oauth_provider: Option<String>,
    /// Authorization endpoint URL for the OAuth provider.  Written into
    /// `config.js` from the `OAUTH_AUTH_URL` environment variable.
    ///
    /// When empty the UI falls back to fetching `GET /api/v1/oauth/provider-config`
    /// from the meeting-api (useful when only `OAUTH_ISSUER` is set and the
    /// auth URL was resolved via OIDC discovery).
    #[serde(rename = "oauthAuthUrl")]
    #[serde(default)]
    pub oauth_auth_url: Option<String>,
    /// OAuth client ID (public — safe to expose in the browser).
    /// Written from the `OAUTH_CLIENT_ID` environment variable.
    #[serde(rename = "oauthClientId")]
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    /// Absolute URL the identity provider should redirect to after
    /// authentication.  Must be registered with the provider and must point
    /// to the dioxus-ui `/auth/callback` route.
    /// Written from the `OAUTH_REDIRECT_URL` environment variable.
    #[serde(rename = "oauthRedirectUrl")]
    #[serde(default)]
    pub oauth_redirect_url: Option<String>,
    /// Space-separated OAuth scopes (default: `"openid email profile"`).
    /// Written from the `OAUTH_SCOPES` environment variable.
    #[serde(rename = "oauthScopes")]
    #[serde(default)]
    pub oauth_scopes: Option<String>,
    /// Token endpoint URL for the identity provider.  Written from the
    /// `OAUTH_TOKEN_URL` environment variable.
    ///
    /// When empty the UI falls back to OIDC discovery via `oauthIssuer` or,
    /// as a last resort, to `GET /api/v1/oauth/provider-config`.
    #[serde(rename = "oauthTokenUrl")]
    #[serde(default)]
    pub oauth_token_url: Option<String>,
    /// OIDC issuer URL (e.g. `https://accounts.google.com`).  Written from
    /// the `OAUTH_ISSUER` environment variable.
    ///
    /// When `oauthTokenUrl` is not set, the UI fetches
    /// `{oauthIssuer}/.well-known/openid-configuration` to discover the
    /// token endpoint.
    #[serde(rename = "oauthIssuer")]
    #[serde(default)]
    pub oauth_issuer: Option<String>,
    /// Optional `prompt` parameter appended to the authorization URL.
    /// Written from the `OAUTH_PROMPT` environment variable.
    ///
    /// Common values: `"login"` (force re-authentication), `"consent"`,
    /// `"select_account"` (Google/Okta/Entra).  Leave empty (the default)
    /// for maximum provider compatibility — the parameter is omitted
    /// entirely when blank so it does not cause errors on providers that
    /// do not recognise it.
    #[serde(rename = "oauthPrompt")]
    #[serde(default)]
    pub oauth_prompt: Option<String>,
    /// OAuth flow mode: `"pkce"` for client-side PKCE, any other value
    /// (including absent/empty) for server-side OAuth where the backend
    /// exchanges the authorization code using the client secret.
    #[serde(rename = "oauthFlow")]
    #[serde(default)]
    pub oauth_flow: Option<String>,
    #[serde(rename = "serverElectionPeriodMs")]
    pub server_election_period_ms: u64,
    #[serde(rename = "audioBitrateKbps")]
    pub audio_bitrate_kbps: u32,
    #[serde(rename = "videoBitrateKbps")]
    pub video_bitrate_kbps: u32,
    #[serde(rename = "screenBitrateKbps")]
    pub screen_bitrate_kbps: u32,
    #[serde(rename = "vadThreshold", default = "default_vad_threshold")]
    pub vad_threshold: f32,
    #[serde(rename = "mockPeersEnabled")]
    #[serde(default)]
    pub mock_peers_enabled: String,
}

fn default_vad_threshold() -> f32 {
    0.02
}

pub fn app_config() -> Result<RuntimeConfig, String> {
    let win = window().expect("window");
    let config = js_sys::Reflect::get(&win, &JsValue::from_str("__APP_CONFIG"))
        .unwrap_or(JsValue::UNDEFINED);
    if config.is_undefined() || config.is_null() {
        return Err("Runtime configuration not found (window.__APP_CONFIG missing)".to_string());
    }
    from_js_value::<RuntimeConfig>(config)
        .map_err(|e| format!("Failed to parse __APP_CONFIG: {e:?}"))
}

/// Maximum number of **real** peer tiles rendered with full PeerTile treatment
/// (canvas, diagnostics subscription, signal history). Mock peers are
/// layout-only and bypass this limit.
pub const CANVAS_LIMIT: usize = 30;

pub fn audio_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.audio_bitrate_kbps)
}
pub fn video_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.video_bitrate_kbps)
}
pub fn screen_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.screen_bitrate_kbps)
}
pub fn vad_threshold() -> Result<f32, String> {
    app_config().map(|c| c.vad_threshold)
}

pub fn split_users(s: Option<&str>) -> Vec<String> {
    if let Some(s) = s {
        s.split(',')
            .filter_map(|s| {
                let s = s.trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            })
            .collect::<Vec<String>>()
    } else {
        Vec::new()
    }
}

pub fn login_url() -> Result<String, String> {
    meeting_api_base_url().map(|url| format!("{}/login", url))
}
pub fn logout_url() -> Result<String, String> {
    meeting_api_base_url().map(|url| format!("{}/logout", url))
}
pub fn actix_websocket_base() -> Result<String, String> {
    app_config().map(|c| c.ws_url)
}
pub fn webtransport_host_base() -> Result<String, String> {
    app_config().map(|c| c.web_transport_host)
}

pub fn webtransport_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.web_transport_enabled.as_str())))
}
pub fn oauth_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.oauth_enabled.as_str())))
}
pub fn e2ee_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.e2ee_enabled.as_str())))
}
pub fn firefox_enabled() -> Result<bool, String> {
    app_config().map(|c| truthy(Some(c.firefox_enabled.as_str())))
}

pub fn mock_peers_enabled() -> bool {
    app_config()
        .map(|c| truthy(Some(c.mock_peers_enabled.as_str())))
        .unwrap_or(false)
}

pub fn users_allowed_to_stream() -> Result<Vec<String>, String> {
    app_config().map(|c| split_users(Some(&c.users_allowed_to_stream)))
}
pub fn server_election_period_ms() -> Result<u64, String> {
    app_config().map(|c| c.server_election_period_ms)
}

pub fn oauth_provider() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_provider)
        .filter(|s| !s.is_empty())
}

/// Authorization endpoint URL of the identity provider, read from
/// `window.__APP_CONFIG.oauthAuthUrl` (written from `OAUTH_AUTH_URL`).
///
/// Returns `None` when the variable was not set; the caller is expected to
/// fall back to `GET /api/v1/oauth/provider-config`.
pub fn oauth_auth_url() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_auth_url)
        .filter(|s| !s.is_empty())
}

/// OAuth client ID, read from `window.__APP_CONFIG.oauthClientId`.
pub fn oauth_client_id() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_client_id)
        .filter(|s| !s.is_empty())
}

/// Redirect URI registered with the provider, read from
/// `window.__APP_CONFIG.oauthRedirectUrl`.
pub fn oauth_redirect_url() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_redirect_url)
        .filter(|s| !s.is_empty())
}

/// Space-separated OAuth scopes.  Defaults to `"openid email profile"` when
/// not set.
pub fn oauth_scopes() -> String {
    app_config()
        .ok()
        .and_then(|c| c.oauth_scopes)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openid email profile".to_string())
}

/// Token endpoint URL of the identity provider, read from
/// `window.__APP_CONFIG.oauthTokenUrl` (written from `OAUTH_TOKEN_URL`).
///
/// Returns `None` when the variable is not set; callers should then attempt
/// OIDC discovery via [`oauth_issuer`] or fall back to the backend
/// `GET /api/v1/oauth/provider-config` endpoint.
pub fn oauth_token_url() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_token_url)
        .filter(|s| !s.is_empty())
}

/// OIDC issuer URL, read from `window.__APP_CONFIG.oauthIssuer` (written from
/// `OAUTH_ISSUER`).
///
/// When [`oauth_token_url`] is not set, the callback page uses this to
/// construct the OIDC well-known discovery URL:
/// `{issuer}/.well-known/openid-configuration`.
pub fn oauth_issuer() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_issuer)
        .filter(|s| !s.is_empty())
}

/// Optional `prompt` value appended to the OIDC authorization URL, read from
/// `window.__APP_CONFIG.oauthPrompt` (written from `OAUTH_PROMPT`).
///
/// Returns `None` when the variable is empty or unset; the parameter is then
/// omitted from the authorization URL so it does not cause errors on providers
/// that do not recognise it.  Set to e.g. `"select_account"` or `"login"` to
/// force the provider to show the account-chooser or re-authentication screen.
pub fn oauth_prompt() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_prompt)
        .filter(|s| !s.is_empty())
}

/// OAuth flow mode, read from `window.__APP_CONFIG.oauthFlow`.
/// Returns `Some("pkce")` for client-side PKCE flow.
/// Any other value (including `None` / empty) means server-side flow.
pub fn oauth_flow() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_flow)
        .filter(|s| !s.is_empty())
}

/// Returns `true` when OAuth is enabled AND the flow is explicitly set to
/// `"pkce"`.  All code paths that decide between client-side PKCE and
/// server-side OAuth should use this single predicate.
pub fn is_pkce_flow() -> bool {
    oauth_enabled().unwrap_or(false) && oauth_flow().as_deref() == Some("pkce")
}

pub fn meeting_api_base_url() -> Result<String, String> {
    app_config().map(|c| {
        c.meeting_api_base_url
            .clone()
            .unwrap_or_else(|| c.api_base_url.clone())
    })
}

pub fn meeting_api_client() -> Result<videocall_meeting_client::MeetingApiClient, String> {
    let base_url = meeting_api_base_url()?;
    // PKCE flow: check sessionStorage for Bearer tokens (access_token preferred,
    //   id_token as fallback for older sessions). The meeting-api validates
    //   these via JWKS.
    // All other flows (server-side OAuth with HttpOnly session cookie, or
    //   non-OAuth deployments with HMAC session JWT cookie): use Cookie mode
    //   so that fetch includes `credentials: 'include'`.
    let auth_mode = if is_pkce_flow() {
        crate::auth::get_stored_access_token()
            .or_else(crate::auth::get_stored_id_token)
            .map(videocall_meeting_client::AuthMode::Bearer)
            .unwrap_or(videocall_meeting_client::AuthMode::Cookie)
    } else {
        videocall_meeting_client::AuthMode::Cookie
    };
    Ok(videocall_meeting_client::MeetingApiClient::new(
        &base_url, auth_mode,
    ))
}
