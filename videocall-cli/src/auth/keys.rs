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

//! The device's Ed25519 key, the clock it signs with, and the two tokens:
//! the registration JWS (proof of possession, no `kid`) and a fresh device
//! JWT per request (`kid` = `sub` = device id, `aud` = the API origin,
//! 120 s lifetime, random `jti`).
//!
//! The seed comes from the OS CSPRNG, lives only in the store and in
//! zeroize-on-drop types, and never goes on argv, in the environment or in
//! logs; `Debug` shows only the public key. In memory there are three
//! copies, all zeroized on drop: the [`SigningKey`]; one PKCS#8 encoding
//! inside the [`EncodingKey`] (jsonwebtoken zeroizes it; the `ed25519/zeroize`
//! feature wipes the encoder's scratch buffers); and a temporary
//! `SigningKey` jsonwebtoken parses on every `encode`, which its API can't
//! avoid. Stack slots left by moves are out of zeroize's reach.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};
use videocall_meeting_types::device::{
    fingerprint, DeviceTokenClaims, RegistrationClaims, CLI_ISSUER, DEVICE_TOKEN_TTL_SECS,
};

use super::random_b64url;

/// Key generation, loading or signing failed.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("system random number generator failed: {0}")]
    Random(#[from] getrandom::Error),
    #[error("the stored device key is corrupt; run `videocall-cli auth login` again")]
    Corrupt,
    #[error("signing failed: {0}")]
    Sign(String),
}

/// An Ed25519 device key.
pub struct DeviceKey {
    signing: SigningKey,
    /// PKCS#8 form for jsonwebtoken, built once.
    encoding: EncodingKey,
}

impl std::fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DeviceKey({})", self.public_key_b64())
    }
}

impl DeviceKey {
    /// A new key from 32 OS CSPRNG bytes.
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
        // jsonwebtoken takes PKCS#8 DER; `der` is a zeroize-on-drop SecretDocument.
        let der = signing
            .to_pkcs8_der()
            .map_err(|e| KeyError::Sign(e.to_string()))?;
        let encoding = EncodingKey::from_ed_der(der.as_bytes());
        Ok(Self { signing, encoding })
    }

    /// The seed, base64url, for the store.
    pub fn seed_b64(&self) -> SecretString {
        let seed = Zeroizing::new(self.signing.to_bytes());
        SecretString::from(URL_SAFE_NO_PAD.encode(seed.as_slice()))
    }

    /// Public key, base64url (43 characters).
    pub fn public_key_b64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing.verifying_key().to_bytes())
    }

    /// `XXXX-XXXX-XXXX-XXXX` fingerprint of the public key.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.signing.verifying_key().to_bytes())
    }

    fn sign(&self, header: &Header, claims: &impl serde::Serialize) -> Result<String, KeyError> {
        jsonwebtoken::encode(header, claims, &self.encoding)
            .map_err(|e| KeyError::Sign(e.to_string()))
    }

    /// A device JWT for one request to `aud`.
    pub fn device_token(&self, device_id: &str, aud: &str, now: i64) -> Result<String, KeyError> {
        self.device_token_with_jti(device_id, aud, now, random_b64url(16)?)
    }

    /// [`Self::device_token`] with a given `jti`, so tests can compare
    /// against the shared vectors.
    fn device_token_with_jti(
        &self,
        device_id: &str,
        aud: &str,
        now: i64,
        jti: String,
    ) -> Result<String, KeyError> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(device_id.to_string());
        let claims = DeviceTokenClaims {
            iss: CLI_ISSUER.to_string(),
            sub: device_id.to_string(),
            aud: aud.to_string(),
            iat: now,
            exp: now + DEVICE_TOKEN_TTL_SECS,
            jti,
        };
        self.sign(&header, &claims)
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
        self.sign(&Header::new(Algorithm::EdDSA), &claims)
    }
}

/// The clock tokens are signed with: local time plus a learned offset.
///
/// A Raspberry Pi has no RTC, so after boot its clock can be minutes off
/// until NTP catches up, and the server (60 s leeway) would reject every
/// token. When the server's time differs by more than 30 s, the CLI signs
/// with the server's time for the rest of the process. Nothing is persisted.
#[derive(Debug, Default)]
pub struct Clock {
    offset: AtomicI64,
}

static GLOBAL_CLOCK: Clock = Clock {
    offset: AtomicI64::new(0),
};

impl Clock {
    /// The process-wide clock.
    pub fn global() -> &'static Clock {
        &GLOBAL_CLOCK
    }

    /// Unix time to sign with.
    pub fn now(&self) -> i64 {
        local_now() + self.offset.load(Ordering::Relaxed)
    }

    /// Adopt the server's time if we are more than 30 s off it. Returns true
    /// if the offset changed, which makes one retry worthwhile.
    pub fn adopt_server_time(&self, server_now: i64) -> bool {
        let wanted = server_now - local_now();
        if (wanted - self.offset.load(Ordering::Relaxed)).abs() <= 30 {
            return false;
        }
        self.offset.store(wanted, Ordering::Relaxed);
        eprintln!("warning: this machine's clock is {wanted:+} s off the server's; using the server's time. Fix NTP.");
        true
    }
}

/// Local Unix time in seconds, uncorrected.
pub fn local_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared with meeting-api's `device_token_vectors` test; read by
    /// relative path so neither crate depends on the other.
    #[test]
    fn signing_matches_the_shared_vectors() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../videocall-meeting-types/tests/vectors/device_tokens.json"
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let s = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap().to_string();
        let key = DeviceKey::from_seed_b64(&SecretString::from(s(&v, "seed_b64url"))).unwrap();
        assert_eq!(key.public_key_b64(), s(&v, "public_key"));
        assert_eq!(key.fingerprint(), s(&v, "fingerprint"));

        let (d, r) = (&v["device_token"], &v["registration"]);
        assert_eq!(
            d["exp"].as_i64().unwrap(),
            d["iat"].as_i64().unwrap() + DEVICE_TOKEN_TTL_SECS
        );
        let token = key.device_token_with_jti(
            &s(d, "device_id"),
            &s(d, "aud"),
            d["iat"].as_i64().unwrap(),
            s(d, "jti"),
        );
        assert_eq!(token.unwrap(), s(d, "token"), "device JWT");
        let jws = key.registration_jws(&s(r, "aud"), &s(r, "name"), r["iat"].as_i64().unwrap());
        assert_eq!(jws.unwrap(), s(r, "token"), "registration JWS");
    }

    #[test]
    fn seed_round_trip_redaction_and_fresh_jti() {
        let key = DeviceKey::generate().unwrap();
        let seed = key.seed_b64();
        let again = DeviceKey::from_seed_b64(&seed).unwrap();
        assert_eq!(again.public_key_b64(), key.public_key_b64());
        assert!(!format!("{key:?} {seed:?}").contains(seed.expose_secret()));
        assert!(DeviceKey::from_seed_b64(&SecretString::from("short".to_string())).is_err());
        let a = key
            .device_token("aB3dE5fG7hI9jK1lM3nO5p", "https://a.example", 1)
            .unwrap();
        let b = key
            .device_token("aB3dE5fG7hI9jK1lM3nO5p", "https://a.example", 1)
            .unwrap();
        assert_ne!(a, b, "every token has its own jti");
    }

    #[test]
    fn clock_adopts_a_large_skew_once() {
        let clock = Clock::default();
        assert!(
            !clock.adopt_server_time(local_now() + 5),
            "small skew ignored"
        );
        assert!(clock.adopt_server_time(local_now() + 3600));
        assert!((clock.now() - (local_now() + 3600)).abs() <= 1);
        assert!(
            !clock.adopt_server_time(local_now() + 3600),
            "already applied"
        );
    }
}
