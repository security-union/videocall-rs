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

//! Generic OAuth2/OIDC helpers: OIDC discovery, JWKS caching, JWT verification,
//! PKCE generation, token exchange, and ID token claims extraction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::AppError;

/// Minimum interval between JWKS refreshes (5 minutes).
const JWKS_REFRESH_INTERVAL_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// ID token claims
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// OIDC discovery
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// JWKS cache
// ---------------------------------------------------------------------------

/// A JWK entry from the JWKS endpoint.
#[derive(Debug, Deserialize)]
struct JwkEntry {
    kid: Option<String>,
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    // RSA fields
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    // EC fields
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwksDocument {
    keys: Vec<JwkEntry>,
}

/// Caches JWKS keys fetched from the provider, with rate-limited refresh.
pub struct JwksCache {
    keys: RwLock<HashMap<String, (Algorithm, DecodingKey)>>,
    jwks_url: String,
    last_refresh: RwLock<Instant>,
}

impl JwksCache {
    /// Create a test-only JwksCache with pre-loaded keys (no HTTP fetching).
    #[cfg(test)]
    pub fn with_keys(keys: HashMap<String, (Algorithm, DecodingKey)>) -> Arc<Self> {
        Arc::new(Self {
            keys: RwLock::new(keys),
            jwks_url: String::new(),
            last_refresh: RwLock::new(Instant::now()),
        })
    }

    pub fn new(jwks_url: String) -> Arc<Self> {
        Arc::new(Self {
            keys: RwLock::new(HashMap::new()),
            jwks_url,
            // Set to epoch-ish so the first request triggers a refresh.
            last_refresh: RwLock::new(
                Instant::now() - std::time::Duration::from_secs(JWKS_REFRESH_INTERVAL_SECS + 1),
            ),
        })
    }

    /// Get the decoding key for a given `kid`. Refreshes the cache if the key
    /// is not found (rate-limited to once per 5 minutes).
    pub async fn get_key(&self, kid: &str) -> Result<(Algorithm, DecodingKey), AppError> {
        // Try read lock first.
        {
            let keys = self.keys.read().await;
            if let Some((alg, key)) = keys.get(kid) {
                return Ok((*alg, key.clone()));
            }
        }

        // Key not found — try refreshing (rate-limited).
        self.refresh().await?;

        let keys = self.keys.read().await;
        keys.get(kid)
            .map(|(alg, key)| (*alg, key.clone()))
            .ok_or_else(|| AppError::internal(&format!("JWKS key not found for kid: {kid}")))
    }

    /// Fetch the JWKS document and update the cache. Rate-limited.
    async fn refresh(&self) -> Result<(), AppError> {
        {
            let last = self.last_refresh.read().await;
            if last.elapsed().as_secs() < JWKS_REFRESH_INTERVAL_SECS {
                return Ok(());
            }
        }

        let resp = reqwest::get(&self.jwks_url)
            .await
            .map_err(|e| AppError::internal(&format!("JWKS fetch failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(AppError::internal(&format!(
                "JWKS fetch returned HTTP {status}"
            )));
        }

        let doc: JwksDocument = resp
            .json()
            .await
            .map_err(|e| AppError::internal(&format!("Failed to parse JWKS: {e}")))?;

        let mut new_keys = HashMap::new();
        for jwk in &doc.keys {
            let kid = match &jwk.kid {
                Some(k) => k.clone(),
                None => continue,
            };

            let alg = jwk_algorithm(jwk);

            let decoding_key = match jwk.kty.as_str() {
                "RSA" => {
                    let n = jwk.n.as_deref().unwrap_or_default();
                    let e = jwk.e.as_deref().unwrap_or_default();
                    if n.is_empty() || e.is_empty() {
                        continue;
                    }
                    DecodingKey::from_rsa_components(n, e)
                        .map_err(|e| AppError::internal(&format!("Invalid RSA JWK: {e}")))?
                }
                "EC" => {
                    let x = jwk.x.as_deref().unwrap_or_default();
                    let y = jwk.y.as_deref().unwrap_or_default();
                    if x.is_empty() || y.is_empty() {
                        continue;
                    }
                    DecodingKey::from_ec_components(x, y)
                        .map_err(|e| AppError::internal(&format!("Invalid EC JWK: {e}")))?
                }
                _ => continue,
            };

            new_keys.insert(kid, (alg, decoding_key));
        }

        *self.keys.write().await = new_keys;
        *self.last_refresh.write().await = Instant::now();
        Ok(())
    }
}

/// Determine the JWT algorithm for a JWK entry.
fn jwk_algorithm(jwk: &JwkEntry) -> Algorithm {
    if let Some(alg) = &jwk.alg {
        match alg.as_str() {
            "RS384" => return Algorithm::RS384,
            "RS512" => return Algorithm::RS512,
            "ES256" => return Algorithm::ES256,
            "ES384" => return Algorithm::ES384,
            "RS256" => return Algorithm::RS256,
            _ if jwk.kty == "RSA" => return Algorithm::RS256,
            _ => {}
        }
    }
    // Default based on key type.
    match jwk.kty.as_str() {
        "EC" => match jwk.crv.as_deref() {
            Some("P-384") => Algorithm::ES384,
            _ => Algorithm::ES256,
        },
        _ => Algorithm::RS256,
    }
}

// ---------------------------------------------------------------------------
// JWT verification
// ---------------------------------------------------------------------------

/// Verify an ID token's signature and standard claims, returning the decoded claims.
///
/// Validates: signature (via JWKS), `exp`, `aud` == `client_id`,
/// optionally `iss` == `issuer`, optionally `nonce` == `expected_nonce`.
pub async fn verify_and_decode_id_token(
    jwks: &JwksCache,
    id_token: &str,
    client_id: &str,
    issuer: Option<&str>,
    expected_nonce: Option<&str>,
) -> Result<IdTokenClaims, AppError> {
    let header = decode_header(id_token)
        .map_err(|e| AppError::internal(&format!("Invalid JWT header: {e}")))?;

    let kid = header
        .kid
        .as_deref()
        .ok_or_else(|| AppError::internal("JWT header missing kid"))?;

    let (alg, key) = jwks.get_key(kid).await?;

    let mut validation = Validation::new(alg);
    validation.set_audience(&[client_id]);
    if let Some(iss) = issuer {
        validation.set_issuer(&[iss]);
    }
    // When issuer is None, validation.iss stays None — issuer check is skipped.
    validation.validate_exp = true;

    let token_data = decode::<IdTokenClaims>(id_token, &key, &validation)
        .map_err(|e| AppError::internal(&format!("JWT validation failed: {e}")))?;

    let claims = token_data.claims;

    // Validate nonce if expected.
    if let Some(expected) = expected_nonce {
        match &claims.nonce {
            Some(n) if n == expected => {}
            Some(_) => return Err(AppError::internal("JWT nonce mismatch")),
            None => return Err(AppError::internal("JWT missing expected nonce")),
        }
    }

    Ok(claims)
}

// ---------------------------------------------------------------------------
// OAuth token response
// ---------------------------------------------------------------------------

/// Raw response from the OAuth token endpoint.
#[derive(Debug, Deserialize, Clone)]
pub struct OAuthTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

// ---------------------------------------------------------------------------
// Auth URL builder
// ---------------------------------------------------------------------------

/// Build an OAuth2 authorization URL with PKCE and optional nonce.
pub fn build_auth_url(
    auth_url: &str,
    client_id: &str,
    redirect_url: &str,
    scopes: &str,
    code_challenge: &str,
    csrf_state: &str,
    nonce: Option<&str>,
) -> String {
    let mut url = format!(
        "{auth_url}?client_id={client_id}\
         &redirect_uri={redirect_url}\
         &response_type=code\
         &scope={scopes}\
         &prompt=select_account\
         &code_challenge={code_challenge}\
         &code_challenge_method=S256\
         &state={csrf_state}"
    );
    if let Some(n) = nonce {
        url.push_str(&format!("&nonce={n}"));
    }
    url
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

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
        decode_id_token_claims_unverified(id_token)?
    };

    Ok((token_response, claims))
}

/// Decode the claims from an ID token JWT **without** signature verification.
/// Used as fallback when JWKS is not configured.
fn decode_id_token_claims_unverified(id_token: &str) -> Result<IdTokenClaims, AppError> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

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

// ---------------------------------------------------------------------------
// UserInfo endpoint fallback
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    /// Helper: generate an RSA keypair and return (encoding_key, decoding_key, kid).
    fn test_rsa_keypair() -> (EncodingKey, DecodingKey, String) {
        use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
        use rsa::RsaPrivateKey;

        let mut rng = rand::thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let priv_pem = private_key
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let encoding = EncodingKey::from_rsa_pem(priv_pem.as_bytes()).unwrap();

        let public_key = private_key.to_public_key();
        let pub_pem = public_key
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let decoding = DecodingKey::from_rsa_pem(pub_pem.as_bytes()).unwrap();

        (encoding, decoding, "test-kid-1".to_string())
    }

    /// Helper: build a JwksCache pre-loaded with a test key.
    fn test_jwks(kid: &str, alg: Algorithm, key: DecodingKey) -> Arc<JwksCache> {
        let mut keys = HashMap::new();
        keys.insert(kid.to_string(), (alg, key));
        JwksCache::with_keys(keys)
    }

    /// Helper: sign a JWT with the given claims.
    fn sign_token(encoding_key: &EncodingKey, kid: &str, claims: &IdTokenClaims) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        encode(&header, claims, encoding_key).unwrap()
    }

    fn base_claims() -> IdTokenClaims {
        IdTokenClaims {
            email: Some("user@example.com".to_string()),
            name: "Test User".to_string(),
            email_verified: Some(true),
            given_name: Some("Test".to_string()),
            family_name: Some("User".to_string()),
            nonce: Some("test-nonce-123".to_string()),
            iss: Some("https://accounts.google.com".to_string()),
            aud: Some(serde_json::Value::String("my-client-id".to_string())),
            exp: Some(
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs())
                    + 3600,
            ),
        }
    }

    #[tokio::test]
    async fn valid_id_token_verifies_successfully() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let claims = base_claims();
        let token = sign_token(&enc, &kid, &claims);

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://accounts.google.com"),
            Some("test-nonce-123"),
        )
        .await;

        let decoded = result.expect("should verify successfully");
        assert_eq!(decoded.email.as_deref(), Some("user@example.com"));
        assert_eq!(decoded.name, "Test User");
    }

    #[tokio::test]
    async fn tampered_token_rejected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let claims = base_claims();
        let mut token = sign_token(&enc, &kid, &claims);

        // Tamper with the signature by flipping a character.
        let len = token.len();
        let last = token.as_bytes()[len - 1];
        let replacement = if last == b'A' { b'B' } else { b'A' };
        unsafe { token.as_bytes_mut()[len - 1] = replacement };

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://accounts.google.com"),
            Some("test-nonce-123"),
        )
        .await;

        assert!(result.is_err(), "tampered token should be rejected");
    }

    #[tokio::test]
    async fn wrong_issuer_rejected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let claims = base_claims();
        let token = sign_token(&enc, &kid, &claims);

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://wrong-issuer.com"),
            Some("test-nonce-123"),
        )
        .await;

        assert!(result.is_err(), "wrong issuer should be rejected");
    }

    #[tokio::test]
    async fn expired_token_rejected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let mut claims = base_claims();
        claims.exp = Some(1_000_000); // long expired

        let token = sign_token(&enc, &kid, &claims);

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://accounts.google.com"),
            Some("test-nonce-123"),
        )
        .await;

        assert!(result.is_err(), "expired token should be rejected");
    }

    #[tokio::test]
    async fn wrong_nonce_rejected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let claims = base_claims();
        let token = sign_token(&enc, &kid, &claims);

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://accounts.google.com"),
            Some("wrong-nonce"),
        )
        .await;

        assert!(result.is_err(), "wrong nonce should be rejected");
    }

    #[tokio::test]
    async fn missing_nonce_rejected_when_expected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let mut claims = base_claims();
        claims.nonce = None;
        let token = sign_token(&enc, &kid, &claims);

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://accounts.google.com"),
            Some("test-nonce-123"),
        )
        .await;

        assert!(
            result.is_err(),
            "missing nonce should be rejected when expected"
        );
    }

    #[tokio::test]
    async fn wrong_audience_rejected() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let claims = base_claims();
        let token = sign_token(&enc, &kid, &claims);

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "wrong-client-id",
            Some("https://accounts.google.com"),
            Some("test-nonce-123"),
        )
        .await;

        assert!(result.is_err(), "wrong audience should be rejected");
    }

    #[tokio::test]
    async fn issuer_validation_skipped_when_none() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let claims = base_claims();
        let token = sign_token(&enc, &kid, &claims);

        // Pass None for issuer — should skip issuer check.
        let result =
            verify_and_decode_id_token(&jwks, &token, "my-client-id", None, Some("test-nonce-123"))
                .await;

        assert!(
            result.is_ok(),
            "should succeed when issuer validation is skipped"
        );
    }

    #[tokio::test]
    async fn nonce_validation_skipped_when_none() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let claims = base_claims();
        let token = sign_token(&enc, &kid, &claims);

        // Pass None for expected_nonce — should skip nonce check.
        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://accounts.google.com"),
            None,
        )
        .await;

        assert!(
            result.is_ok(),
            "should succeed when nonce validation is skipped"
        );
    }

    #[tokio::test]
    async fn token_without_email_deserializes() {
        let (enc, dec, kid) = test_rsa_keypair();
        let jwks = test_jwks(&kid, Algorithm::RS256, dec);
        let mut claims = base_claims();
        claims.email = None;
        let token = sign_token(&enc, &kid, &claims);

        let result = verify_and_decode_id_token(
            &jwks,
            &token,
            "my-client-id",
            Some("https://accounts.google.com"),
            Some("test-nonce-123"),
        )
        .await;

        let decoded = result.expect("should verify even without email");
        assert!(decoded.email.is_none());
    }

    #[test]
    fn display_name_uses_name_field() {
        let c = IdTokenClaims {
            email: Some("a@b.com".to_string()),
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
            email: Some("a@b.com".to_string()),
            name: String::new(),
            given_name: Some("Al".to_string()),
            family_name: Some("Ice".to_string()),
            ..base_claims()
        };
        assert_eq!(c.display_name(), "Al Ice");
    }

    #[test]
    fn display_name_falls_back_to_email() {
        let c = IdTokenClaims {
            email: Some("a@b.com".to_string()),
            name: String::new(),
            given_name: None,
            family_name: None,
            ..base_claims()
        };
        assert_eq!(c.display_name(), "a@b.com");
    }

    #[test]
    fn display_name_empty_when_no_email() {
        let c = IdTokenClaims {
            email: None,
            name: String::new(),
            given_name: None,
            family_name: None,
            ..base_claims()
        };
        assert_eq!(c.display_name(), "");
    }

    #[test]
    fn unverified_decode_extracts_claims() {
        use base64::Engine;
        // Build a fake JWT with valid base64 payload (no real signature).
        let payload = serde_json::json!({
            "email": "test@example.com",
            "name": "Test",
        });
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let fake_jwt = format!("eyJhbGciOiJSUzI1NiJ9.{payload_b64}.fakesig");

        let claims = decode_id_token_claims_unverified(&fake_jwt).unwrap();
        assert_eq!(claims.email.as_deref(), Some("test@example.com"));
        assert_eq!(claims.name, "Test");
    }
}
