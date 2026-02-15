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
    /// Base URL for the meeting REST API (meeting-api service).
    /// If not set, defaults to api_base_url for backwards compatibility.
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
    /// OAuth provider hint for branding (e.g. "google", "okta").
    /// When set, the login screen shows provider-specific logo and text.
    #[serde(rename = "oauthProvider")]
    #[serde(default)]
    pub oauth_provider: Option<String>,
    #[serde(rename = "serverElectionPeriodMs")]
    pub server_election_period_ms: u64,
    #[serde(rename = "audioBitrateKbps")]
    pub audio_bitrate_kbps: u32,
    #[serde(rename = "videoBitrateKbps")]
    pub video_bitrate_kbps: u32,
    #[serde(rename = "screenBitrateKbps")]
    pub screen_bitrate_kbps: u32,
    // ui_url intentionally omitted; unused by the UI
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

// No Default implementation on purpose: config must be present at runtime.
pub const CANVAS_LIMIT: usize = 20;

pub fn audio_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.audio_bitrate_kbps)
}
pub fn video_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.video_bitrate_kbps)
}
pub fn screen_bitrate_kbps() -> Result<u32, String> {
    app_config().map(|c| c.screen_bitrate_kbps)
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
// Removed lazy statics for runtime config. Use function accessors below.

pub fn login_url() -> Result<String, String> {
    meeting_api_base_url().map(|url| format!("{}/login", url))
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

pub fn users_allowed_to_stream() -> Result<Vec<String>, String> {
    app_config().map(|c| split_users(Some(&c.users_allowed_to_stream)))
}
pub fn server_election_period_ms() -> Result<u64, String> {
    app_config().map(|c| c.server_election_period_ms)
}

/// Returns the configured OAuth provider hint (e.g. "google", "okta").
/// Returns `None` when unset — the login screen shows a generic "Sign in" button.
pub fn oauth_provider() -> Option<String> {
    app_config()
        .ok()
        .and_then(|c| c.oauth_provider)
        .filter(|s| !s.is_empty())
}

/// Returns the meeting API base URL.
/// Falls back to api_base_url if meeting_api_base_url is not set.
pub fn meeting_api_base_url() -> Result<String, String> {
    app_config().map(|c| {
        c.meeting_api_base_url
            .clone()
            .unwrap_or_else(|| c.api_base_url.clone())
    })
}

/// Create a [`MeetingApiClient`] configured from the runtime config.
///
/// Uses cookie-based auth (browser mode) since `yew-ui` runs in the browser.
pub fn meeting_api_client() -> Result<videocall_meeting_client::MeetingApiClient, String> {
    let base_url = meeting_api_base_url()?;
    Ok(videocall_meeting_client::MeetingApiClient::new(
        &base_url,
        videocall_meeting_client::AuthMode::Cookie,
    ))
}
