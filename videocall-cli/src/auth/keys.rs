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
//! 120 s lifetime, random `jti`). The token format is shared with
//! meeting-api's verifier in `videocall_meeting_types::device::crypto`.
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

use ed25519_dalek::SigningKey;
use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretString};
use videocall_meeting_types::device::crypto::{self, EncodingKey};
use videocall_meeting_types::device::{base64url_decode, base64url_encode, fingerprint};

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
        let bytes =
            Zeroizing::new(base64url_decode(seed.expose_secret()).ok_or(KeyError::Corrupt)?);
        let seed: &[u8; 32] = bytes.as_slice().try_into().map_err(|_| KeyError::Corrupt)?;
        Self::from_signing(SigningKey::from_bytes(seed))
    }

    fn from_signing(signing: SigningKey) -> Result<Self, KeyError> {
        let encoding = crypto::encoding_key(&signing).map_err(KeyError::Sign)?;
        Ok(Self { signing, encoding })
    }

    /// The seed, base64url, for the store.
    pub fn seed_b64(&self) -> SecretString {
        let seed = Zeroizing::new(self.signing.to_bytes());
        SecretString::from(base64url_encode(seed.as_slice()))
    }

    /// Public key, base64url (43 characters).
    pub fn public_key_b64(&self) -> String {
        base64url_encode(&self.signing.verifying_key().to_bytes())
    }

    /// `XXXX-XXXX-XXXX-XXXX` fingerprint of the public key.
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.signing.verifying_key().to_bytes())
    }

    /// A device JWT for one request to `aud`.
    pub fn device_token(&self, device_id: &str, aud: &str, now: i64) -> Result<String, KeyError> {
        crypto::sign_device_token(&self.encoding, device_id, aud, now, random_b64url(16)?)
            .map_err(|e| KeyError::Sign(e.to_string()))
    }

    /// The registration JWS for `aud`, naming the device `name`.
    pub fn registration_jws(&self, aud: &str, name: &str, now: i64) -> Result<String, KeyError> {
        crypto::sign_registration(&self.encoding, &self.public_key_b64(), aud, name, now)
            .map_err(|e| KeyError::Sign(e.to_string()))
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
