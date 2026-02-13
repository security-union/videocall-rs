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

//! Auth URL construction and authorization code → token exchange.

use std::collections::HashMap;

use serde::Deserialize;
use url::Url;

use crate::error::AppError;

use super::claims::IdTokenClaims;
use super::jwks::JwksCache;
use super::verify::{decode_id_token_claims_unverified, verify_and_decode_id_token};

/// Raw response from the OAuth token endpoint.
#[derive(Debug, Deserialize, Clone)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// Build an OAuth2 authorization URL with PKCE and optional nonce.
///
/// Parameters are properly URL-encoded. `prompt` and `extra_auth_params` are
/// appended only when provided.
#[allow(clippy::too_many_arguments)]
pub fn build_auth_url(
    auth_url: &str,
    client_id: &str,
    redirect_url: &str,
    scopes: &str,
    code_challenge: &str,
    csrf_state: &str,
    nonce: Option<&str>,
    prompt: Option<&str>,
    extra_auth_params: &HashMap<String, String>,
) -> String {
    let mut url = Url::parse(auth_url).expect("OAUTH_AUTH_URL must be a valid URL");

    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("client_id", client_id);
        pairs.append_pair("redirect_uri", redirect_url);
        pairs.append_pair("response_type", "code");
        pairs.append_pair("scope", scopes);
        pairs.append_pair("code_challenge", code_challenge);
        pairs.append_pair("code_challenge_method", "S256");
        pairs.append_pair("state", csrf_state);

        if let Some(n) = nonce {
            pairs.append_pair("nonce", n);
        }
        if let Some(p) = prompt {
            pairs.append_pair("prompt", p);
        }
        for (k, v) in extra_auth_params {
            pairs.append_pair(k, v);
        }
    }

    url.to_string()
}

/// Exchange an authorization code for tokens. When a `JwksCache` is provided,
/// the ID token is cryptographically verified; otherwise falls back to
/// unverified base64 decode (for backward compatibility when JWKS is not configured).
#[allow(clippy::too_many_arguments)]
pub async fn exchange_code_for_claims(
    redirect_url: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code_verifier: &str,
    token_url: &str,
    authorization_code: &str,
    jwks: Option<&JwksCache>,
    issuer: Option<&str>,
    expected_nonce: Option<&str>,
) -> Result<(OAuthTokenResponse, IdTokenClaims), AppError> {
    let client = reqwest::Client::new();

    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("redirect_uri", redirect_url),
        ("client_id", client_id),
        ("code", authorization_code),
        ("code_verifier", code_verifier),
    ];

    // Only include client_secret when configured (confidential clients).
    let secret_owned;
    if let Some(secret) = client_secret {
        secret_owned = secret.to_string();
        params.push(("client_secret", &secret_owned));
    }

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

    let claims = if let Some(jwks) = jwks {
        verify_and_decode_id_token(jwks, id_token, client_id, issuer, expected_nonce).await?
    } else {
        tracing::warn!(
            "JWKS not configured — ID token signature, issuer, audience, and nonce are NOT verified. \
             Set OAUTH_JWKS_URL or OAUTH_ISSUER to enable verification."
        );
        decode_id_token_claims_unverified(id_token)?
    };

    Ok((token_response, claims))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_auth_url_encodes_spaces_in_scopes() {
        let url = build_auth_url(
            "https://accounts.google.com/o/oauth2/v2/auth",
            "client123",
            "https://example.com/callback",
            "openid email profile",
            "challenge_abc",
            "state_xyz",
            Some("nonce_123"),
            None,
            &HashMap::new(),
        );

        // Scopes should be percent-encoded (spaces → +  or %20).
        assert!(!url.contains(' '), "URL must not contain literal spaces");
        assert!(url.contains("openid"));
        assert!(url.contains("client_id=client123"));
        assert!(url.contains("nonce=nonce_123"));
    }

    #[test]
    fn build_auth_url_includes_prompt_when_set() {
        let url = build_auth_url(
            "https://accounts.google.com/o/oauth2/v2/auth",
            "client123",
            "https://example.com/callback",
            "openid",
            "challenge",
            "state",
            None,
            Some("select_account"),
            &HashMap::new(),
        );

        assert!(url.contains("prompt=select_account"));
    }

    #[test]
    fn build_auth_url_omits_prompt_when_none() {
        let url = build_auth_url(
            "https://accounts.google.com/o/oauth2/v2/auth",
            "client123",
            "https://example.com/callback",
            "openid",
            "challenge",
            "state",
            None,
            None,
            &HashMap::new(),
        );

        assert!(!url.contains("prompt="));
    }

    #[test]
    fn build_auth_url_includes_extra_params() {
        let mut extra = HashMap::new();
        extra.insert("access_type".to_string(), "offline".to_string());
        extra.insert("hd".to_string(), "example.com".to_string());

        let url = build_auth_url(
            "https://accounts.google.com/o/oauth2/v2/auth",
            "client123",
            "https://example.com/callback",
            "openid",
            "challenge",
            "state",
            None,
            None,
            &extra,
        );

        assert!(url.contains("access_type=offline"));
        assert!(url.contains("hd=example.com"));
    }
}
