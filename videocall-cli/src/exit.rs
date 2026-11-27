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

//! 0 stopped or succeeded; 1 restart me (lost the relay or camera, transient failures);
//! 2 needs a human (logged out, revoked, declined, meeting gone, relay refused us, bad flag).

use videocall_meeting_client::ApiError;

use crate::auth::store::StoreError;
use crate::auth::AuthError;

pub const RESTART: u8 = 1;
pub const NEEDS_HUMAN: u8 = 2;

/// Exit [`NEEDS_HUMAN`] with this message.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct NeedsHuman(pub String);

pub fn needs_human(message: impl Into<String>) -> anyhow::Error {
    NeedsHuman(message.into()).into()
}

pub fn code(error: &anyhow::Error) -> u8 {
    for cause in error.chain() {
        if cause.is::<NeedsHuman>() {
            return NEEDS_HUMAN;
        }
        if let Some(auth) = cause.downcast_ref::<AuthError>() {
            return auth_code(auth);
        }
        if let Some(store) = cause.downcast_ref::<StoreError>() {
            return store_code(store);
        }
        if let Some(api) = cause.downcast_ref::<ApiError>() {
            return api_code(api);
        }
    }
    RESTART
}

pub fn auth_code(error: &AuthError) -> u8 {
    match error {
        AuthError::Api(api) => api_code(api),
        AuthError::Store(store) => store_code(store),
        AuthError::Io(_) => RESTART,
        AuthError::NotLoggedIn(_)
        | AuthError::DeviceRevoked
        | AuthError::Login(_)
        | AuthError::Config(_)
        | AuthError::Key(_) => NEEDS_HUMAN,
    }
}

fn store_code(error: &StoreError) -> u8 {
    match error {
        StoreError::Keychain(_) | StoreError::Io(..) => RESTART,
        StoreError::NoConfigDir | StoreError::Insecure(..) | StoreError::Corrupt(_) => NEEDS_HUMAN,
    }
}

fn api_code(error: &ApiError) -> u8 {
    match error {
        ApiError::Network(e) if is_transient_network(e) => RESTART,
        ApiError::ServerError { status, .. } if *status >= 500 || *status == 429 => RESTART,
        _ => NEEDS_HUMAN,
    }
}

/// Timeout, failed connect or failed send; not an undecodable body or an unbuildable request.
pub fn is_transient_network(error: &videocall_meeting_client::reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let server = |status| {
            AuthError::Api(ApiError::ServerError {
                status,
                body: String::new(),
            })
        };
        let restart: [anyhow::Error; 3] = [
            anyhow::anyhow!("lost the relay connection"),
            server(503).into(),
            server(429).into(),
        ];
        for e in restart {
            assert_eq!(code(&e), RESTART, "{e}");
        }
        let human: [anyhow::Error; 8] = [
            needs_human("the host declined"),
            needs_human("x").context("joining"),
            AuthError::DeviceRevoked.into(),
            AuthError::NotLoggedIn("a".into()).into(),
            AuthError::Api(ApiError::NotFound(String::new())).into(),
            AuthError::Api(ApiError::MeetingNotActive).into(),
            server(400).into(),
            StoreError::Corrupt("f".into()).into(),
        ];
        for e in human {
            assert_eq!(code(&e), NEEDS_HUMAN, "{e}");
        }
    }
}
