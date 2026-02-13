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

//! JWT signature verification and unverified decode fallback.

use jsonwebtoken::{decode, decode_header, Validation};

use crate::error::AppError;

use super::claims::IdTokenClaims;
use super::jwks::JwksCache;

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

/// Decode the claims from an ID token JWT **without** signature verification.
/// Used as fallback when JWKS is not configured.
pub(crate) fn decode_id_token_claims_unverified(id_token: &str) -> Result<IdTokenClaims, AppError> {
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};

    use super::*;
    use crate::oauth::claims::IdTokenClaims;
    use crate::oauth::jwks::JwksCache;

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

    fn test_jwks(kid: &str, alg: Algorithm, key: DecodingKey) -> Arc<JwksCache> {
        let mut keys = HashMap::new();
        keys.insert(kid.to_string(), (alg, key));
        JwksCache::with_keys(keys)
    }

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
        let token = sign_token(&enc, &kid, &claims);

        // Tamper with the signature by flipping a character (safe version).
        let len = token.len();
        let last = token.as_bytes()[len - 1];
        let replacement = if last == b'A' { b'B' } else { b'A' };
        let mut bytes = token.into_bytes();
        bytes[len - 1] = replacement;
        let token = String::from_utf8(bytes).unwrap();

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
