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

//! OAuth helper functions: PKCE generation, token exchange, JWT claims extraction.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::error::AppError;

const SCOPE: &str = "openid email profile";

/// Claims extracted from the Google ID token JWT.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GoogleClaims {
    pub email: String,
    #[serde(default)]
    pub name: String,
}

/// Raw response from the OAuth token endpoint.
#[derive(Debug, Deserialize, Clone)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// Build the Google OAuth authorization URL.
pub fn build_auth_url(
    auth_url: &str,
    client_id: &str,
    redirect_url: &str,
    pkce_challenge: &str,
    csrf_state: &str,
) -> String {
    format!(
        "{auth_url}?client_id={client_id}\
         &redirect_uri={redirect_url}\
         &response_type=code\
         &scope={SCOPE}\
         &prompt=select_account\
         &pkce_challenge={pkce_challenge}\
         &state={csrf_state}\
         &access_type=offline"
    )
}

/// Exchange an authorization code for tokens and extract user claims from the id_token.
pub async fn exchange_code_for_claims(
    redirect_url: &str,
    client_id: &str,
    client_secret: &str,
    pkce_verifier: &str,
    token_url: &str,
    authorization_code: &str,
) -> Result<(OAuthTokenResponse, GoogleClaims), AppError> {
    let client = reqwest::Client::new();
    let params = [
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_url),
        ("client_id", client_id),
        ("code", authorization_code),
        ("client_secret", client_secret),
        ("pkce_verifier", pkce_verifier),
    ];

    let response = client
        .post(token_url)
        .form(&params)
        .send()
        .await
        .map_err(|e| AppError::internal(&format!("OAuth token request failed: {e}")))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::error!("OAuth token request failed. Status: {status}, Body: {body}");
        return Err(AppError::internal("OAuth token exchange failed"));
    }

    let body_text = response
        .text()
        .await
        .map_err(|e| AppError::internal(&format!("Failed to read OAuth response: {e}")))?;

    let token_response: OAuthTokenResponse = serde_json::from_str(&body_text)
        .map_err(|e| AppError::internal(&format!("Failed to parse OAuth response: {e}")))?;

    let id_token = token_response
        .id_token
        .as_deref()
        .ok_or_else(|| AppError::internal("OAuth response missing id_token"))?;

    let claims = decode_id_token_claims(id_token)?;
    Ok((token_response, claims))
}

/// Decode the claims from a Google id_token JWT (without signature verification,
/// since we just received it directly from Google over HTTPS).
fn decode_id_token_claims(id_token: &str) -> Result<GoogleClaims, AppError> {
    let parts: Vec<&str> = id_token.split('.').collect();
    let claims_b64 = parts
        .get(1)
        .ok_or_else(|| AppError::internal("Invalid id_token format"))?;

    let claims_bytes = URL_SAFE_NO_PAD
        .decode(claims_b64)
        .map_err(|e| AppError::internal(&format!("Failed to base64-decode id_token: {e}")))?;

    serde_json::from_slice(&claims_bytes)
        .map_err(|e| AppError::internal(&format!("Failed to parse id_token claims: {e}")))
}
