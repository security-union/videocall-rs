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

//! The two device tokens, signed by videocall-cli and verified by
//! meeting-api with the same code: the per-request device JWT (`kid` = `sub`
//! = device id) and the self-signed registration JWS (no `kid`). Both are
//! `{"typ":"JWT","alg":"EdDSA"}` over JSON claims. The verifiers pin EdDSA
//! and never read the algorithm from the header, so HS256 session and room
//! tokens can never pass as device tokens. Behind the `device-crypto`
//! feature so the browser build stays free of signing code.

use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::{SigningKey, VerifyingKey};
pub use jsonwebtoken::EncodingKey;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use super::{
    decode_public_key, is_valid_device_id, normalize_device_name, DeviceTokenClaims,
    RegistrationClaims, CLI_ISSUER, DEVICE_TOKEN_LEEWAY_SECS, DEVICE_TOKEN_MAX_LIFETIME_SECS,
    DEVICE_TOKEN_TTL_SECS,
};

/// Longest device token or registration JWS read at all.
pub const MAX_DEVICE_TOKEN_LEN: usize = 2048;

/// The jsonwebtoken key for `signing`. jsonwebtoken takes PKCS#8 DER; the
/// intermediate document zeroizes itself on drop.
pub fn encoding_key(signing: &SigningKey) -> Result<EncodingKey, String> {
    let der = signing.to_pkcs8_der().map_err(|e| e.to_string())?;
    Ok(EncodingKey::from_ed_der(der.as_bytes()))
}

/// Sign `claims` with the one device header shape: EdDSA, `typ: JWT`, and
/// `kid` when given.
pub fn sign(
    key: &EncodingKey,
    kid: Option<&str>,
    claims: &impl Serialize,
) -> Result<String, jsonwebtoken::errors::Error> {
    let header = Header {
        kid: kid.map(str::to_string),
        ..Header::new(Algorithm::EdDSA)
    };
    jsonwebtoken::encode(&header, claims, key)
}

/// A device JWT for one request to `aud`, valid [`DEVICE_TOKEN_TTL_SECS`].
pub fn sign_device_token(
    key: &EncodingKey,
    device_id: &str,
    aud: &str,
    now: i64,
    jti: String,
) -> Result<String, jsonwebtoken::errors::Error> {
    let claims = DeviceTokenClaims {
        iss: CLI_ISSUER.to_string(),
        sub: device_id.to_string(),
        aud: aud.to_string(),
        iat: now,
        exp: now + DEVICE_TOKEN_TTL_SECS,
        jti,
    };
    sign(key, Some(device_id), &claims)
}

/// The registration JWS for `aud`, naming the device `name`.
pub fn sign_registration(
    key: &EncodingKey,
    public_key_b64: &str,
    aud: &str,
    name: &str,
    now: i64,
) -> Result<String, jsonwebtoken::errors::Error> {
    let claims = RegistrationClaims {
        iss: CLI_ISSUER.to_string(),
        aud: aud.to_string(),
        iat: now,
        exp: now + DEVICE_TOKEN_TTL_SECS,
        public_key: public_key_b64.to_string(),
        name: name.to_string(),
    };
    sign(key, None, &claims)
}

/// 43 base64url chars, a valid Edwards point, not a small-order (weak) key.
pub fn validate_public_key(encoded: &str) -> Option<[u8; 32]> {
    let raw = decode_public_key(encoded)?;
    let key = VerifyingKey::from_bytes(&raw).ok()?;
    (!key.is_weak()).then_some(raw)
}

/// EdDSA only, issuer `videocall-cli`, the exact audience. `exp` is checked
/// by [`check_token_times`] against the caller's `now` instead (jsonwebtoken
/// would read the system clock), so always pair the two.
fn validation(expected_aud: &str, require_sub: bool) -> Validation {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[CLI_ISSUER]);
    validation.set_audience(&[expected_aud]);
    if require_sub {
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    } else {
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    }
    validation.leeway = DEVICE_TOKEN_LEEWAY_SECS as u64;
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation
}

/// `exp` and `iat` within the leeway of `now` (Unix seconds), and
/// `exp - iat` in `1..=DEVICE_TOKEN_MAX_LIFETIME_SECS`. Checked arithmetic,
/// so extreme claim values cannot wrap around the bounds.
pub fn check_token_times(iat: i64, exp: i64, now: i64) -> Result<(), &'static str> {
    let earliest_exp = now
        .checked_sub(DEVICE_TOKEN_LEEWAY_SECS)
        .ok_or("clock overflow")?;
    if exp < earliest_exp {
        return Err("expired");
    }
    let latest_iat = now
        .checked_add(DEVICE_TOKEN_LEEWAY_SECS)
        .ok_or("clock overflow")?;
    if iat > latest_iat {
        return Err("iat in the future");
    }
    match exp.checked_sub(iat) {
        Some(lifetime) if (1..=DEVICE_TOKEN_MAX_LIFETIME_SECS).contains(&lifetime) => Ok(()),
        Some(_) => Err("lifetime out of range"),
        None => Err("lifetime overflow"),
    }
}

/// The well-formed `kid` of an unverified EdDSA header. Proves nothing; it
/// only selects the key [`verify_device_token`] uses. The reason is for logs.
pub fn device_token_kid(token: &str) -> Result<String, &'static str> {
    if token.len() > MAX_DEVICE_TOKEN_LEN {
        return Err("token too long");
    }
    let header = decode_header(token).map_err(|_| "header")?;
    if header.alg != Algorithm::EdDSA {
        return Err("not EdDSA");
    }
    header
        .kid
        .filter(|kid| is_valid_device_id(kid))
        .ok_or("missing or malformed kid")
}

/// Verify a device JWT against the stored key of device `kid`: pinned EdDSA
/// signature and claims, `sub == kid`, time bounds. The reason is for logs.
pub fn verify_device_token(
    token: &str,
    kid: &str,
    public_key_b64: &str,
    expected_aud: &str,
    now: i64,
) -> Result<DeviceTokenClaims, &'static str> {
    // Only ever build a DecodingKey from exactly 32 bytes: the EdDSA verifier
    // slices [..32] and would panic on a shorter key.
    let raw = decode_public_key(public_key_b64).ok_or("stored key")?;
    let claims = decode::<DeviceTokenClaims>(
        token,
        &DecodingKey::from_ed_der(&raw),
        &validation(expected_aud, true),
    )
    .map_err(|_| "signature or claims")?
    .claims;
    if claims.sub != kid {
        return Err("sub does not match kid");
    }
    check_token_times(claims.iat, claims.exp, now)?;
    Ok(claims)
}

#[derive(Deserialize)]
struct PublicKeyOnly {
    public_key: String,
}

/// Verify the self-signed registration JWS: read `public_key` unverified,
/// validate it, then verify the token with that key. Returns the claims
/// (name normalized) and the raw key; `None` for any rejection.
pub fn verify_registration(
    token: &str,
    audience: &str,
    now: i64,
) -> Option<(RegistrationClaims, [u8; 32])> {
    if token.len() > MAX_DEVICE_TOKEN_LEN || decode_header(token).ok()?.alg != Algorithm::EdDSA {
        return None;
    }
    let unverified = jsonwebtoken::dangerous::insecure_decode::<PublicKeyOnly>(token).ok()?;
    let raw = validate_public_key(&unverified.claims.public_key)?;
    let mut claims = decode::<RegistrationClaims>(
        token,
        &DecodingKey::from_ed_der(&raw),
        &validation(audience, false),
    )
    .ok()?
    .claims;
    if claims.public_key != unverified.claims.public_key {
        return None;
    }
    check_token_times(claims.iat, claims.exp, now).ok()?;
    claims.name = normalize_device_name(&claims.name)?;
    Some((claims, raw))
}

#[cfg(test)]
mod tests {
    use super::super::{base64url_decode, base64url_encode, fingerprint};
    use super::*;
    use serde_json::{json, Value};

    // Golden vectors. TEST ONLY: the seed is the public RFC 8032 section 7.1
    // TEST 1 secret key. Ed25519 is deterministic, so a changed token means
    // the wire format changed.
    const SEED: &str = "nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A";
    const PUBLIC_KEY: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
    const DEVICE_ID: &str = "aB3dE5fG7hI9jK1lM3nO5p";
    const AUD: &str = "https://api.example.com";
    const IAT: i64 = 1764000000;
    const DEVICE_TOKEN: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFZERTQSIsImtpZCI6ImFCM2RFNWZHN2hJOWpLMWxNM25PNXAifQ.eyJpc3MiOiJ2aWRlb2NhbGwtY2xpIiwic3ViIjoiYUIzZEU1Zkc3aEk5aksxbE0zbk81cCIsImF1ZCI6Imh0dHBzOi8vYXBpLmV4YW1wbGUuY29tIiwiaWF0IjoxNzY0MDAwMDAwLCJleHAiOjE3NjQwMDAxMjAsImp0aSI6IkFBRUNBd1FGQmdjSUNRb0xEQTBPRHcifQ.3oPfSaPL-XrcGwbQraQC76e8mgkIfOGJ5Sd8DkTXc73p8D6A_FTKydl6kiJdUE-EIhT_kcp-WCAf0gsbPsWMAg";
    const REGISTRATION: &str = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFZERTQSJ9.eyJpc3MiOiJ2aWRlb2NhbGwtY2xpIiwiYXVkIjoiaHR0cHM6Ly9hcGkuZXhhbXBsZS5jb20iLCJpYXQiOjE3NjQwMDAwMDAsImV4cCI6MTc2NDAwMDEyMCwicHVibGljX2tleSI6IjExcVlBWUt4Q3JmVlNfN1R5V1FIT2c3aGN2UGFwaU1scndJYWFQY0hVUm8iLCJuYW1lIjoidmlkZW9jYWxsLWNsaSBvbiB0ZXN0LXZlY3RvciJ9.qmF7w0f_v9jqjBVCjF2FhN4eH4hA5zRXZnJCzz_TNky7QKJjPbj-kxxW20o6yw47vH_i825KZ6ARvNv3JeUODg";

    fn vector_key() -> SigningKey {
        SigningKey::from_bytes(&base64url_decode(SEED).unwrap().try_into().unwrap())
    }

    #[test]
    fn golden_vectors_sign_and_verify() {
        let key = vector_key();
        let encoding = encoding_key(&key).unwrap();
        let raw = key.verifying_key().to_bytes();
        assert_eq!(base64url_encode(&raw), PUBLIC_KEY);
        assert_eq!(fingerprint(&raw), "21FE-31DF-A154-A261");

        let jti = "AAECAwQFBgcICQoLDA0ODw".to_string();
        let token = sign_device_token(&encoding, DEVICE_ID, AUD, IAT, jti.clone()).unwrap();
        assert_eq!(token, DEVICE_TOKEN);
        let jws = sign_registration(
            &encoding,
            PUBLIC_KEY,
            AUD,
            "videocall-cli on test-vector",
            IAT,
        );
        assert_eq!(jws.unwrap(), REGISTRATION);

        let now = IAT + 30;
        assert_eq!(device_token_kid(DEVICE_TOKEN).unwrap(), DEVICE_ID);
        // Only the header is read, so the bound is what rejects this.
        let padded = format!("{DEVICE_TOKEN}{}", "A".repeat(MAX_DEVICE_TOKEN_LEN));
        assert_eq!(device_token_kid(&padded), Err("token too long"));
        let claims = verify_device_token(DEVICE_TOKEN, DEVICE_ID, PUBLIC_KEY, AUD, now).unwrap();
        assert_eq!((claims.exp, claims.jti), (IAT + 120, jti));
        let (claims, raw_back) = verify_registration(REGISTRATION, AUD, now).unwrap();
        assert_eq!((claims.public_key.as_str(), raw_back), (PUBLIC_KEY, raw));
        assert!(verify_registration(REGISTRATION, "https://other.example.com", now).is_none());
    }

    #[test]
    fn token_time_bounds() {
        let t = 1_764_000_000;
        assert!(check_token_times(t, t + 120, t).is_ok());
        assert!(check_token_times(t, t + 300, t).is_ok());
        assert!(check_token_times(t, t + 301, t).is_err());
        assert!(check_token_times(t + 60, t + 120, t).is_ok());
        assert!(check_token_times(t + 120, t + 240, t).is_err());
        // Overflow and non-positive lifetimes.
        assert!(check_token_times(i64::MIN, t + 120, t).is_err());
        assert!(check_token_times(i64::MIN, i64::MAX, t).is_err());
        assert!(check_token_times(t, t - 1, t).is_err());
        assert!(check_token_times(t, t, t).is_err());
        assert!(check_token_times(t, i64::MIN, t).is_err());
        assert!(check_token_times(t, t + 1, t).is_ok());
        // `exp` is checked against the given `now`, with the leeway.
        assert!(check_token_times(t - 150, t - 50, t).is_ok());
        assert!(check_token_times(t - 200, t - 61, t).is_err());
        assert!(check_token_times(t - 200, t - 100, t).is_err());
    }

    #[test]
    fn public_key_validation() {
        assert!(validate_public_key(&base64url_encode(&[9u8; 31])).is_none());
        assert!(validate_public_key(&base64url_encode(&[9u8; 33])).is_none());
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(validate_public_key(&base64url_encode(&identity)).is_none());
        assert!(validate_public_key(&base64url_encode(&[0u8; 32])).is_none());
        assert!(validate_public_key(PUBLIC_KEY).is_some());
    }

    /// Everything the device-token verifier checks, short of the stored row.
    #[test]
    fn device_token_rejections() {
        let key = vector_key();
        let encoding = encoding_key(&key).unwrap();
        let now = IAT;
        let claims = |edit: &dyn Fn(&mut Value)| {
            let mut claims = json!({"iss": CLI_ISSUER, "sub": DEVICE_ID, "aud": AUD, "iat": now, "exp": now + 120, "jti": "j"});
            edit(&mut claims);
            claims
        };
        let signed = |kid: Option<&str>, claims: Value| sign(&encoding, kid, &claims).unwrap();
        let verify = |token: &str| {
            device_token_kid(token)
                .and_then(|kid| verify_device_token(token, &kid, PUBLIC_KEY, AUD, now))
        };
        assert!(verify(&signed(Some(DEVICE_ID), claims(&|_| {}))).is_ok());

        let other = encoding_key(&SigningKey::from_bytes(&[8u8; 32])).unwrap();
        let hs256 = Header {
            kid: Some(DEVICE_ID.to_string()),
            ..Header::new(Algorithm::HS256)
        };
        let raw = key.verifying_key().to_bytes();
        let cases = [
            (
                "signed by another key",
                sign(&other, Some(DEVICE_ID), &claims(&|_| {})).unwrap(),
            ),
            (
                "aud mismatch",
                signed(
                    Some(DEVICE_ID),
                    claims(&|c| c["aud"] = json!("https://other.example")),
                ),
            ),
            (
                "kid != sub",
                signed(
                    Some(DEVICE_ID),
                    claims(&|c| c["sub"] = json!("x".repeat(22))),
                ),
            ),
            (
                "wrong issuer",
                signed(
                    Some(DEVICE_ID),
                    claims(&|c| c["iss"] = json!("someone-else")),
                ),
            ),
            (
                "expired",
                signed(
                    Some(DEVICE_ID),
                    claims(&|c| {
                        c["iat"] = json!(now - 400);
                        c["exp"] = json!(now - 280);
                    }),
                ),
            ),
            (
                "future iat",
                signed(
                    Some(DEVICE_ID),
                    claims(&|c| {
                        c["iat"] = json!(now + 120);
                        c["exp"] = json!(now + 240);
                    }),
                ),
            ),
            (
                "overlong lifetime",
                signed(Some(DEVICE_ID), claims(&|c| c["exp"] = json!(now + 301))),
            ),
            ("malformed kid", signed(Some("short"), claims(&|_| {}))),
            ("no kid", signed(None, claims(&|_| {}))),
            (
                "HS256 with the public key",
                jsonwebtoken::encode(&hs256, &claims(&|_| {}), &EncodingKey::from_secret(&raw))
                    .unwrap(),
            ),
            (
                "alg none",
                format!(
                    "{}.{}.",
                    base64url_encode(format!(r#"{{"alg":"none","kid":"{DEVICE_ID}"}}"#).as_bytes()),
                    base64url_encode(claims(&|_| {}).to_string().as_bytes())
                ),
            ),
        ];
        for (name, token) in cases {
            assert!(verify(&token).is_err(), "{name}");
        }
        // The stored key must be the signer's, and decode to exactly 32 bytes.
        let token = signed(Some(DEVICE_ID), claims(&|_| {}));
        let wrong_key = "PUAXw-hDiVqStwqnTRt-vJyYLM8uxJaMwM1V8Sr0Zgw";
        assert!(verify_device_token(&token, DEVICE_ID, wrong_key, AUD, now).is_err());
        assert!(verify_device_token(&token, DEVICE_ID, "short", AUD, now).is_err());
    }
}
