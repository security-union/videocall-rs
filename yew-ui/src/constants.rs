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
    app_config().map(|c| format!("{}/login", c.api_base_url))
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
