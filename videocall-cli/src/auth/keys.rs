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

//! The device's Ed25519 key and the JWTs it signs: the self-signed
//! registration JWS (proof of possession, no `kid`) and a fresh device JWT
//! for every request (`kid` = `sub` = device id, `aud` = the exact origin
//! called, 120 s lifetime, random `jti`).
//!
//! The 32-byte seed is generated from the OS CSPRNG, stored only in the
//! credential store, and held in memory inside [`SigningKey`] (zeroized on
//! drop). It never goes on argv, in the environment, or into logs; `Debug`
//! shows only the public key.
//!
//! Copies of the private key in memory, all zeroized on drop:
//! - the [`SigningKey`] (ed25519-dalek, `zeroize` feature);
//! - one PKCS#8 encoding, built once per [`DeviceKey`]. `to_pkcs8_der`
//!   returns a `SecretDocument` (zeroized on drop, and its scratch buffers
//!   are wiped with the `ed25519/zeroize` feature enabled in Cargo.toml);
//!   [`EncodingKey::from_ed_der`] copies it into its own buffer, which
//!   jsonwebtoken 10 also zeroizes on drop (`ZeroizeOnDrop`);
//! - inside each `jsonwebtoken::encode` call, jsonwebtoken re-parses that
//!   DER into a temporary `SigningKey` (zeroized on drop). Its API takes
//!   no pre-parsed key, so this per-signature copy can't be avoided.
//!
//! Like any Rust value, these may be moved (memcpy'd) on the stack before
//! they are dropped, and zeroize can't reach those stale stack slots.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};
use videocall_meeting_types::device::{
    fingerprint, DeviceTokenClaims, RegistrationClaims, CLI_ISSUER,
};

use super::random_b64url;

/// Lifetime of every token the CLI mints (the server allows up to 300 s).
pub const TOKEN_LIFETIME_SECS: i64 = 120;

/// Errors from key handling and signing.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The OS random number generator failed.
    #[error("system random number generator failed: {0}")]
    Random(#[from] getrandom::Error),
    /// The stored key isn't a base64url 32-byte seed.
    #[error("the stored device key is corrupt; run `videocall-cli auth login` again")]
    Corrupt,
    /// PKCS#8 encoding or JWT signing failed.
    #[error("signing failed: {0}")]
    Sign(String),
}

/// An Ed25519 device key.
pub struct DeviceKey {
    signing: SigningKey,
    /// PKCS#8 form for jsonwebtoken, built once (zeroized on drop).
    encoding: EncodingKey,
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceKey")
            .field("public_key", &self.public_key_b64())
            .finish_non_exhaustive()
    }
}

impl DeviceKey {
    /// A fresh key from 32 OS CSPRNG bytes.
    pub fn generate() -> Result<Self, KeyError> {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(seed.as_mut())?;
        Self::from_signing(SigningKey::from_bytes(&seed))
    }

    /// Load from the stored base64url seed.
    pub fn from_seed_b64(seed: &SecretString) -> Result<Self, KeyError> {
        let bytes = Zeroizing::new(
            URL_SAFE_NO_PAD
                .decode(seed.expose_secret())
                .map_err(|_| KeyError::Corrupt)?,
        );
        let seed: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| KeyError::Corrupt)?;
        Self::from_signing(SigningKey::from_bytes(seed))
    }

    fn from_signing(signing: SigningKey) -> Result<Self, KeyError> {
        // jsonwebtoken's rust_crypto signer takes PKCS#8 DER, not the seed.
        // `der` is a zeroize-on-drop SecretDocument; EncodingKey copies it
        // into its own zeroize-on-drop buffer.
        let der = signing
            .to_pkcs8_der()
            .map_err(|e| KeyError::Sign(e.to_string()))?;
        let encoding = EncodingKey::from_ed_der(der.as_bytes());
        Ok(Self { signing, encoding })
    }

    /// The seed, base64url, for the credential store.
    pub fn seed_b64(&self) -> SecretString {
        let seed = Zeroizing::new(self.signing.to_bytes());
        SecretString::from(URL_SAFE_NO_PAD.encode(seed.as_slice()))
    }

    /// Raw 32-byte public key.
    pub fn public_key_raw(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Public key as base64url without padding (43 characters).
    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public_key_raw())
    }

    /// `XXXX-XXXX-XXXX-XXXX` fingerprint of the public key.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public_key_raw())
    }

    fn sign<T: serde::Serialize>(&self, header: &Header, claims: &T) -> Result<String, KeyError> {
        jsonwebtoken::encode(header, claims, &self.encoding)
            .map_err(|e| KeyError::Sign(e.to_string()))
    }

    /// A per-request device JWT: header `{"alg":"EdDSA","typ":"JWT","kid"}`,
    /// claims `iss`, `sub = kid`, `aud`, `iat = now`, `exp = now + 120`,
    /// fresh `jti`. `aud` must be the exact origin being called.
    pub fn device_token(&self, device_id: &str, aud: &str, now: i64) -> Result<String, KeyError> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(device_id.to_string());
        let claims = DeviceTokenClaims {
            iss: CLI_ISSUER.to_string(),
            sub: device_id.to_string(),
            aud: aud.to_string(),
            iat: now,
            exp: now + TOKEN_LIFETIME_SECS,
            jti: random_b64url(16)?,
        };
        self.sign(&header, &claims)
    }

    /// The self-signed registration JWS (proof of possession): header with
    /// no `kid`; claims `iss`, `aud` = auth origin, `iat`, `exp`,
    /// `public_key`, `name`.
    pub fn registration_jws(&self, aud: &str, name: &str, now: i64) -> Result<String, KeyError> {
        let header = Header::new(Algorithm::EdDSA);
        let claims = RegistrationClaims {
            iss: CLI_ISSUER.to_string(),
            aud: aud.to_string(),
            iat: now,
            exp: now + TOKEN_LIFETIME_SECS,
            public_key: self.public_key_b64(),
            name: name.to_string(),
        };
        self.sign(&header, &claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{DecodingKey, Validation};

    const DEVICE_ID: &str = "aB3dE5fG7hI9jK1lM3nO5p";

    fn decode_parts(token: &str) -> (serde_json::Value, serde_json::Value) {
        let mut parts = token.split('.');
        let mut part = || {
            let bytes = URL_SAFE_NO_PAD.decode(parts.next().unwrap()).unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        (part(), part())
    }

    #[test]
    fn keygen_shapes() {
        let key = DeviceKey::generate().unwrap();
        let pk = key.public_key_b64();
        assert_eq!(pk.len(), 43);
        assert!(pk
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(key.fingerprint(), fingerprint(&key.public_key_raw()));
        assert_eq!(key.fingerprint().len(), 19);
        assert_ne!(
            key.public_key_b64(),
            DeviceKey::generate().unwrap().public_key_b64()
        );
    }

    #[test]
    fn seed_round_trip_and_redaction() {
        let key = DeviceKey::generate().unwrap();
        let seed = key.seed_b64();
        let again = DeviceKey::from_seed_b64(&seed).unwrap();
        assert_eq!(again.public_key_raw(), key.public_key_raw());
        // The cached PKCS#8 signing key belongs to the same seed: Ed25519 is
        // deterministic, so the same claims give the same signature.
        assert_eq!(
            again
                .registration_jws("https://api.example.com", "n", 1)
                .unwrap(),
            key.registration_jws("https://api.example.com", "n", 1)
                .unwrap()
        );
        let printed = format!("{key:?} {seed:?}");
        assert!(!printed.contains(seed.expose_secret()));
        assert!(printed.contains(&key.public_key_b64()));
        assert!(DeviceKey::from_seed_b64(&SecretString::from("short".to_string())).is_err());
    }

    #[test]
    fn device_token_header_and_claims() {
        let key = DeviceKey::generate().unwrap();
        let token = key
            .device_token(DEVICE_ID, "https://api.example.com", 1_764_000_000)
            .unwrap();
        let (header, claims) = decode_parts(&token);
        assert_eq!(
            header,
            serde_json::json!({"alg": "EdDSA", "typ": "JWT", "kid": DEVICE_ID})
        );
        assert_eq!(claims["iss"], "videocall-cli");
        assert_eq!(claims["sub"], DEVICE_ID);
        assert_eq!(claims["aud"], "https://api.example.com");
        assert_eq!(claims["iat"], 1_764_000_000);
        assert_eq!(claims["exp"], 1_764_000_120);
        assert_eq!(claims["jti"].as_str().unwrap().len(), 22);

        let other = key
            .device_token(DEVICE_ID, "https://api.example.com", 1_764_000_000)
            .unwrap();
        assert_ne!(decode_parts(&other).1["jti"], claims["jti"]);

        // Verifies with the public key, and only for its audience.
        let decoding = DecodingKey::from_ed_der(&key.public_key_raw());
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.validate_exp = false;
        validation.set_audience(&["https://api.example.com"]);
        assert!(jsonwebtoken::decode::<DeviceTokenClaims>(&token, &decoding, &validation).is_ok());
        validation.set_audience(&["https://auth.example.com"]);
        assert!(jsonwebtoken::decode::<DeviceTokenClaims>(&token, &decoding, &validation).is_err());
    }

    #[test]
    fn registration_jws_shape() {
        let key = DeviceKey::generate().unwrap();
        let jws = key
            .registration_jws(
                "https://auth.example.com",
                "videocall-cli on pi4",
                1_764_000_000,
            )
            .unwrap();
        let (header, claims) = decode_parts(&jws);
        assert_eq!(header, serde_json::json!({"alg": "EdDSA", "typ": "JWT"}));
        assert!(header.get("kid").is_none());
        assert_eq!(claims["iss"], "videocall-cli");
        assert_eq!(claims["aud"], "https://auth.example.com");
        assert_eq!(claims["iat"], 1_764_000_000);
        assert_eq!(claims["exp"], 1_764_000_120);
        assert_eq!(claims["public_key"], key.public_key_b64());
        assert_eq!(claims["name"], "videocall-cli on pi4");
        assert!(claims.get("sub").is_none());
    }
}
