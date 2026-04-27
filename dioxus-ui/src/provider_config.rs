// SPDX-License-Identifier: MIT OR Apache-2.0

//! Fetches and caches the OAuth provider configuration from the meeting-api.
//!
//! Both [`crate::auth`] (needs `auth_url` + `client_id` to build the
//! authorization URL) and [`crate::pages::oauth_callback`] (needs `token_url`
//! + `issuer` for the token exchange) fall back to this module when the values
//! are not present in `window.__APP_CONFIG`.
//!
//! The full response is cached in `sessionStorage` as a JSON string on the
//! first successful fetch so that subsequent calls within the same tab are
//! free (no network round-trip).

use dioxus_sdk_storage::{SessionStorage, StorageBacking};
use serde::Deserialize;
use videocall_meeting_types::responses::OAuthProviderConfigResponse;

use crate::constants::meeting_api_base_url;

/// `sessionStorage` key under which the serialised provider config is cached.
const CACHE_KEY: &str = "vc_oauth_provider_config";

/// Thin envelope matching `{ "success": bool, "result": T }`.
#[derive(Deserialize)]
struct Envelope {
    result: OAuthProviderConfigResponse,
}

/// Fetch the OAuth provider configuration from the meeting-api backend,
/// using the per-session cache when available.
///
/// ## Resolution order
///
/// 1. `sessionStorage` — populated by a previous call in this tab.
/// 2. `GET /api/v1/oauth/provider-config` — the meeting-api returns all OAuth
///    endpoints it resolved at startup (via OIDC discovery or env vars). The
///    result is cached for the lifetime of the tab.
///
/// Callers are responsible for deciding which fields they need and whether the
/// returned config is "good enough" (e.g. `auth_url` non-empty for the login
/// redirect, `token_url` non-empty for the token exchange).
pub(crate) async fn fetch_provider_config() -> Result<OAuthProviderConfigResponse, String> {
    // 1. Session cache — no network needed.
    if let Some(json) = SessionStorage::get::<Option<String>>(&CACHE_KEY.to_string()).flatten() {
        if let Ok(cfg) = serde_json::from_str::<OAuthProviderConfigResponse>(&json) {
            return Ok(cfg);
        }
        // Cached bytes are corrupt (e.g. schema changed) — fall through and
        // re-fetch so the tab recovers automatically.
    }

    // 2. Fetch from backend.
    let base =
        meeting_api_base_url().map_err(|e| format!("Cannot build provider-config URL: {e}"))?;
    let url = format!("{base}/api/v1/oauth/provider-config");

    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Failed to fetch provider config: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Provider config endpoint returned HTTP {status}: {body}"
        ));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read provider config response: {e}"))?;

    let envelope: Envelope = serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse provider config: {e} — body: {text}"))?;

    let cfg = envelope.result;

    // 3. Populate the cache.
    if let Ok(json) = serde_json::to_string(&cfg) {
        SessionStorage::set(CACHE_KEY.to_string(), &Some(json));
    }

    Ok(cfg)
}
