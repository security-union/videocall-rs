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

//! OIDC discovery: fetching `.well-known/openid-configuration`.

use serde::Deserialize;

use crate::error::AppError;

/// Endpoints discovered from an OIDC provider's `.well-known/openid-configuration`.
#[derive(Debug, Clone, Deserialize)]
pub struct OidcEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub jwks_uri: Option<String>,
    #[serde(default)]
    pub userinfo_endpoint: Option<String>,
}

/// Fetch OIDC discovery document from `{issuer}/.well-known/openid-configuration`.
pub async fn discover_oidc_endpoints(issuer: &str) -> Result<OidcEndpoints, AppError> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| AppError::internal(&format!("OIDC discovery request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::internal(&format!(
            "OIDC discovery failed (HTTP {status}): {body}"
        )));
    }

    resp.json::<OidcEndpoints>()
        .await
        .map_err(|e| AppError::internal(&format!("Failed to parse OIDC discovery document: {e}")))
}
