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

//! ID token claims and UserInfo endpoint helpers.

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// Claims extracted from an OIDC ID token JWT.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct IdTokenClaims {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub given_name: Option<String>,
    #[serde(default)]
    pub family_name: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
    /// Standard OIDC `iss` claim.
    #[serde(default)]
    pub iss: Option<String>,
    /// Standard OIDC `aud` — can be a single string or array.
    /// We handle both forms during validation; this captures the raw value.
    #[serde(default)]
    pub aud: Option<serde_json::Value>,
    /// Expiration time (Unix timestamp).
    #[serde(default)]
    pub exp: Option<u64>,
}

impl IdTokenClaims {
    /// Return a display name, coalescing `name`, `given_name + family_name`, or email.
    pub fn display_name(&self) -> String {
        if !self.name.is_empty() {
            return self.name.clone();
        }
        match (&self.given_name, &self.family_name) {
            (Some(g), Some(f)) if !g.is_empty() => format!("{g} {f}"),
            (Some(g), _) if !g.is_empty() => g.clone(),
            _ => self.email.clone().unwrap_or_default(),
        }
    }
}

/// Response from the OIDC UserInfo endpoint.
#[derive(Debug, Deserialize)]
pub struct UserInfoResponse {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub given_name: Option<String>,
    #[serde(default)]
    pub family_name: Option<String>,
}

/// Fetch user info from the provider's UserInfo endpoint using the access token.
/// Used as a fallback when the ID token lacks the `email` claim.
pub async fn fetch_userinfo(
    userinfo_url: &str,
    access_token: &str,
) -> Result<UserInfoResponse, AppError> {
    let client = reqwest::Client::new();
    let resp = client
        .get(userinfo_url)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| AppError::internal(&format!("UserInfo request failed: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::internal(&format!(
            "UserInfo endpoint returned HTTP {status}: {body}"
        )));
    }

    resp.json::<UserInfoResponse>()
        .await
        .map_err(|e| AppError::internal(&format!("Failed to parse UserInfo response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_claims() -> IdTokenClaims {
        IdTokenClaims {
            email: Some("a@b.com".to_string()),
            name: String::new(),
            email_verified: None,
            given_name: None,
            family_name: None,
            nonce: None,
            iss: None,
            aud: None,
            exp: None,
        }
    }

    #[test]
    fn display_name_uses_name_field() {
        let c = IdTokenClaims {
            name: "Alice".to_string(),
            given_name: Some("Al".to_string()),
            family_name: Some("Ice".to_string()),
            ..base_claims()
        };
        assert_eq!(c.display_name(), "Alice");
    }

    #[test]
    fn display_name_falls_back_to_given_family() {
        let c = IdTokenClaims {
            given_name: Some("Al".to_string()),
            family_name: Some("Ice".to_string()),
            ..base_claims()
        };
        assert_eq!(c.display_name(), "Al Ice");
    }

    #[test]
    fn display_name_falls_back_to_email() {
        let c = base_claims();
        assert_eq!(c.display_name(), "a@b.com");
    }

    #[test]
    fn display_name_empty_when_no_email() {
        let c = IdTokenClaims {
            email: None,
            ..base_claims()
        };
        assert_eq!(c.display_name(), "");
    }
}
