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
//! tokens can never pass as device tokens. Also the device key itself, PKCE
//! and the random values, hashes and comparisons both sides need. Behind the
//! `device-crypto` feature so the browser build stays free of signing code.
//!
//! The seed never goes on argv, in the environment or in logs, and every
//! in-memory copy is a zeroize-on-drop type; `Debug` shows only the public key.

use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::{SigningKey, VerifyingKey};
use jsonwebtoken::{
    decode, decode_header, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::{
    base64url_decode, base64url_encode, code_challenge_s256, decode_public_key, fingerprint,
    is_valid_device_id, normalize_device_name, DeviceTokenClaims, RegistrationClaims, CLI_ISSUER,
    DEVICE_TOKEN_LEEWAY_SECS, DEVICE_TOKEN_MAX_LIFETIME_SECS, DEVICE_TOKEN_TTL_SECS,
};

/// Key generation, loading or signing failed.
#[derive(Debug)]
pub enum KeyError {
    Random(getrandom::Error),
    Corrupt,
    Sign(String),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Random(e) => write!(f, "system random number generator failed: {e}"),
            KeyError::Corrupt => f.write_str("the stored device key is corrupt"),
            KeyError::Sign(e) => write!(f, "signing failed: {e}"),
        }
    }
}

impl std::error::Error for KeyError {}

/// `n` bytes from the OS CSPRNG, base64url without padding.
pub fn random_b64url(n: usize) -> Result<String, KeyError> {
    let mut bytes = Zeroizing::new(vec![0u8; n]);
    getrandom::fill(&mut bytes).map_err(KeyError::Random)?;
    Ok(base64url_encode(&bytes))
}

/// Lowercase hex SHA-256, the only form in which secrets are stored.
pub fn sha256_hex(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Constant-time byte equality. Lengths are not secret.
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// A PKCE S256 verifier (sent only in the code exchange) and its challenge.
pub struct Pkce {
    pub verifier: SecretString,
    pub challenge: String,
}

impl Pkce {
    /// A verifier from 32 CSPRNG bytes (43 characters).
    pub fn generate() -> Result<Self, KeyError> {
        let verifier = random_b64url(32)?;
        let challenge = code_challenge_s256(&verifier);
        let verifier = SecretString::from(verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

/// An Ed25519 device key.
pub struct DeviceKey {
    signing: SigningKey,
    encoding: EncodingKey,
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceKey({})", self.public_key_b64())
    }
}

impl DeviceKey {
    /// A new key from the OS CSPRNG.
    pub fn generate() -> Result<Self, KeyError> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut()).map_err(KeyError::Random)?;
        Self::from_signing(SigningKey::from_bytes(&seed))
    }

    /// Load from the stored base64url seed.
    pub fn from_seed_b64(seed: &SecretString) -> Result<Self, KeyError> {
        let bytes =
            Zeroizing::new(base64url_decode(seed.expose_secret()).ok_or(KeyError::Corrupt)?);
        let seed: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| KeyError::Corrupt)?;
        Self::from_signing(SigningKey::from_bytes(seed))
    }

    fn from_signing(signing: SigningKey) -> Result<Self, KeyError> {
        let encoding = encoding_key(&signing).map_err(KeyError::Sign)?;
        Ok(Self { signing, encoding })
    }

    pub fn seed_b64(&self) -> SecretString {
        let seed = Zeroizing::new(self.signing.to_bytes());
        SecretString::from(base64url_encode(seed.as_slice()))
    }

    pub fn public_key_b64(&self) -> String {
        base64url_encode(&self.signing.verifying_key().to_bytes())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.signing.verifying_key().to_bytes())
    }

    /// A device JWT for one request to `aud`, with a fresh random `jti`.
    pub fn device_token(&self, device_id: &str, aud: &str, now: i64) -> Result<String, KeyError> {
        sign_device_token(&self.encoding, device_id, aud, now, random_b64url(16)?)
            .map_err(|e| KeyError::Sign(e.to_string()))
    }

    /// The registration JWS for `aud`, naming the device `name`.
    pub fn registration_jws(&self, aud: &str, name: &str, now: i64) -> Result<String, KeyError> {
        let claims = RegistrationClaims {
            iss: CLI_ISSUER.to_string(),
            aud: aud.to_string(),
            iat: now,
            exp: now + DEVICE_TOKEN_TTL_SECS,
            public_key: self.public_key_b64(),
            name: name.to_string(),
        };
        self.sign_claims(None, &claims)
    }

    /// Sign arbitrary `claims` with the device header shape.
    #[doc(hidden)] // Public only so meeting-api's tests can forge tokens.
    pub fn sign_claims(
        &self,
        kid: Option<&str>,
        claims: &impl Serialize,
    ) -> Result<String, KeyError> {
        sign(&self.encoding, kid, claims).map_err(|e| KeyError::Sign(e.to_string()))
    }
}

/// Longest device token or registration JWS read at all.
pub const MAX_DEVICE_TOKEN_LEN: usize = 2048;

/// The jsonwebtoken key for `signing`. jsonwebtoken takes PKCS#8 DER; the
/// intermediate document zeroizes itself on drop.
fn encoding_key(signing: &SigningKey) -> Result<EncodingKey, String> {
    let der = signing.to_pkcs8_der().map_err(|e| e.to_string())?;
    Ok(EncodingKey::from_ed_der(der.as_bytes()))
}

/// Sign `claims` with the one device header shape: EdDSA, `typ: JWT`, and
/// `kid` when given.
fn sign(
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
fn sign_device_token(
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
    if token.len() > MAX_DEVICE_TOKEN_LEN {
        return Err("token too long");
    }
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

    #[test]
    fn golden_vectors_sign_and_verify() {
        let key = DeviceKey::from_seed_b64(&SecretString::from(SEED.to_string())).unwrap();
        let raw = key.signing.verifying_key().to_bytes();
        assert_eq!(key.public_key_b64(), PUBLIC_KEY);
        assert_eq!(key.fingerprint(), "21FE-31DF-A154-A261");

        let jti = "AAECAwQFBgcICQoLDA0ODw".to_string();
        let token = sign_device_token(&key.encoding, DEVICE_ID, AUD, IAT, jti.clone()).unwrap();
        assert_eq!(token, DEVICE_TOKEN);
        // Through `DeviceKey`, so a swapped argument changes the bytes.
        let jws = key.registration_jws(AUD, "videocall-cli on test-vector", IAT);
        assert_eq!(jws.unwrap(), REGISTRATION);
        let fresh = key.device_token(DEVICE_ID, AUD, IAT).unwrap();
        assert_eq!(
            verify_device_token(&fresh, DEVICE_ID, PUBLIC_KEY, AUD, IAT)
                .unwrap()
                .sub,
            DEVICE_ID
        );
        assert_ne!(
            fresh,
            key.device_token(DEVICE_ID, AUD, IAT).unwrap(),
            "fresh jti"
        );

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
    fn secrets_pkce_and_seed() {
        let sha_abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(sha256_hex("abc"), sha_abc);
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "abcd"));
        assert_eq!(
            base64url_decode(&random_b64url(16).unwrap()).unwrap().len(),
            16
        );
        let pkce = Pkce::generate().unwrap();
        assert_eq!(pkce.verifier.expose_secret().len(), 43);
        assert_eq!(
            code_challenge_s256(pkce.verifier.expose_secret()),
            pkce.challenge
        );

        let key = DeviceKey::generate().unwrap();
        let seed = key.seed_b64();
        let again = DeviceKey::from_seed_b64(&seed).unwrap();
        assert_eq!(again.public_key_b64(), key.public_key_b64());
        assert!(!format!("{key:?} {seed:?}").contains(seed.expose_secret()));
        assert!(DeviceKey::from_seed_b64(&SecretString::from("short".to_string())).is_err());
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
        let key = DeviceKey::from_seed_b64(&SecretString::from(SEED.to_string())).unwrap();
        let encoding = &key.encoding;
        let now = IAT;
        let claims = |edit: &dyn Fn(&mut Value)| {
            let mut claims = json!({"iss": CLI_ISSUER, "sub": DEVICE_ID, "aud": AUD, "iat": now, "exp": now + 120, "jti": "j"});
            edit(&mut claims);
            claims
        };
        let signed = |kid: Option<&str>, claims: Value| sign(encoding, kid, &claims).unwrap();
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
        let raw = key.signing.verifying_key().to_bytes();
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
