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

//! A device's side of meeting-api: the login that registers a key, and a signing session.
//! The seed leaves only through [`DeviceLogin::seed_b64`], for the caller's key store.

use std::future::Future;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use secrecy::{ExposeSecret, SecretString};
use videocall_meeting_types::device::crypto::{random_b64url, Pkce};
use videocall_meeting_types::device::{
    RegisterDeviceRequest, RegisterDeviceResponse, CLI_AUTHORIZE_PATH, CLI_CLIENT_ID,
    CODE_CHALLENGE_METHOD_S256,
};

use crate::{ApiError, AuthMode, MeetingApiClient, TokenSource};

pub use videocall_meeting_types::device::crypto::{ct_eq, DeviceKey, KeyError};

/// Local time plus a learned offset. A Raspberry Pi has no RTC, so its clock can be minutes
/// off after boot; beyond 30 s the device signs with the server's time.
#[derive(Debug)]
pub struct Clock {
    offset: AtomicI64,
    /// Told the new offset in seconds each time it changes.
    on_change: fn(i64),
}

impl Clock {
    pub const fn new(on_change: fn(i64)) -> Self {
        Self {
            offset: AtomicI64::new(0),
            on_change,
        }
    }

    pub fn now(&self) -> i64 {
        local_now() + self.offset.load(Ordering::Relaxed)
    }

    /// Returns true if the offset changed, which makes one retry worthwhile.
    pub fn adopt_server_time(&self, server_now: i64) -> bool {
        let wanted = server_now - local_now();
        if (wanted - self.offset.load(Ordering::Relaxed)).abs() <= 30 {
            return false;
        }
        self.offset.store(wanted, Ordering::Relaxed);
        (self.on_change)(wanted);
        true
    }
}

pub fn local_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

pub struct DeviceLogin {
    key: DeviceKey,
    pkce: Pkce,
    state: String,
    redirect_uri: String,
    name: String,
}

impl DeviceLogin {
    pub fn new(redirect_uri: String, name: &str) -> Result<Self, KeyError> {
        Ok(Self {
            key: DeviceKey::generate()?,
            pkce: Pkce::generate()?,
            state: random_b64url(32)?,
            redirect_uri,
            name: name.to_string(),
        })
    }

    /// The `state` the loopback callback must carry.
    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn fingerprint(&self) -> String {
        self.key.fingerprint()
    }

    pub fn seed_b64(&self) -> SecretString {
        self.key.seed_b64()
    }

    pub fn authorize_url(&self, client: &MeetingApiClient) -> Result<String, ApiError> {
        let mut url = reqwest::Url::parse(&client.url(CLI_AUTHORIZE_PATH))
            .map_err(|e| ApiError::Config(format!("bad base URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLI_CLIENT_ID)
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("code_challenge", &self.pkce.challenge)
            .append_pair("code_challenge_method", CODE_CHALLENGE_METHOD_S256)
            .append_pair("state", &self.state)
            .append_pair("public_key", &self.key.public_key_b64())
            .append_pair("device_name", &self.name);
        Ok(url.into())
    }

    pub async fn exchange(
        &self,
        client: &MeetingApiClient,
        code: &SecretString,
        now: i64,
    ) -> Result<RegisterDeviceResponse, ApiError> {
        let request = RegisterDeviceRequest {
            code: code.expose_secret().to_string(),
            code_verifier: self.pkce.verifier.expose_secret().to_string(),
            redirect_uri: self.redirect_uri.clone(),
            registration: self
                .key
                .registration_jws(client.base_url(), &self.name, now)?,
        };
        client.register_device(&request).await
    }
}

struct DeviceTokens {
    key: DeviceKey,
    device_id: String,
    aud: String,
    clock: &'static Clock,
    /// A signing failure, reported instead of the 401 it causes.
    error: Mutex<Option<KeyError>>,
}

impl TokenSource for DeviceTokens {
    fn token(&self) -> Option<String> {
        let token = self
            .key
            .device_token(&self.device_id, &self.aud, self.clock.now());
        token
            .map_err(|e| *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e))
            .ok()
    }
}

#[derive(Clone)]
pub struct DeviceSession {
    tokens: Arc<DeviceTokens>,
    http: reqwest::Client,
}

impl DeviceSession {
    pub fn from_seed(
        http: reqwest::Client,
        api_origin: &str,
        seed: &SecretString,
        device_id: String,
        clock: &'static Clock,
    ) -> Result<Self, KeyError> {
        let tokens = DeviceTokens {
            key: DeviceKey::from_seed_b64(seed)?,
            device_id,
            aud: api_origin.to_string(),
            clock,
            error: Mutex::new(None),
        };
        Ok(Self {
            tokens: Arc::new(tokens),
            http,
        })
    }

    fn client(&self) -> MeetingApiClient {
        let auth = AuthMode::Signed(self.tokens.clone());
        MeetingApiClient::with_http(self.http.clone(), &self.tokens.aud, auth)
    }

    /// On a 401, adopt the server's clock if ours is off and retry once; any other 401 is
    /// [`ApiError::NotAuthenticated`] (revoked or expired).
    pub async fn call<T, F, Fut>(&self, f: F) -> Result<T, ApiError>
    where
        F: Fn(MeetingApiClient) -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        for attempt in 0..2 {
            match f(self.client()).await {
                Err(ApiError::NotAuthenticated) => {}
                other => return other,
            }
            if let Some(e) = self.tokens.error.lock().ok().and_then(|mut e| e.take()) {
                return Err(e.into());
            }
            let server_now = if attempt == 0 {
                self.client().server_time().await
            } else {
                None
            };
            if !server_now.is_some_and(|now| self.tokens.clock.adopt_server_time(now)) {
                break;
            }
        }
        Err(ApiError::NotAuthenticated)
    }

    /// Revoke this device; already revoked counts as done.
    pub async fn revoke(&self) -> Result<(), ApiError> {
        match self
            .call(|c| async move { c.delete_device_me().await })
            .await
        {
            Ok(()) | Err(ApiError::NotAuthenticated) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_shape() {
        let client = MeetingApiClient::new("https://api.example.com", AuthMode::Cookie);
        let mut login = DeviceLogin::new("http://127.0.0.1:5000/callback".into(), "pi 4").unwrap();
        login.state = "st&ate=x".into();
        let url = reqwest::Url::parse(&login.authorize_url(&client).unwrap()).unwrap();
        assert_eq!(
            url.origin().ascii_serialization(),
            "https://api.example.com"
        );
        assert_eq!(url.path(), "/cli/authorize");
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let get = |k: &str| {
            pairs
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("response_type"), Some("code"));
        assert_eq!(get("client_id"), Some("videocall-cli"));
        assert_eq!(get("redirect_uri"), Some("http://127.0.0.1:5000/callback"));
        assert_eq!(get("code_challenge_method"), Some("S256"));
        assert_eq!(get("state"), Some("st&ate=x"));
        assert_eq!(get("code_challenge"), Some(login.pkce.challenge.as_str()));
        assert_eq!(get("public_key"), Some(login.key.public_key_b64().as_str()));
        assert_eq!(get("device_name"), Some("pi 4"));
    }

    #[test]
    fn clock_adopts_a_large_skew_once() {
        let clock = Clock::new(|_| {});
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
