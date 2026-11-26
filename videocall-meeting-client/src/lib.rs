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

//! Cross-platform REST client for the videocall.rs meeting API.
//!
//! Works on WASM (browser), desktop, and mobile targets via [`reqwest`].
//!
//! # Example
//!
//! ```no_run
//! use videocall_meeting_client::{MeetingApiClient, AuthMode};
//!
//! # async fn example() -> Result<(), videocall_meeting_client::ApiError> {
//! // Browser: cookies are sent automatically
//! let client = MeetingApiClient::new("http://localhost:8081", AuthMode::Cookie);
//!
//! // Native / mobile: use a bearer token
//! let client = MeetingApiClient::new(
//!     "http://localhost:8081",
//!     AuthMode::Bearer("eyJ...".to_string()),
//! );
//!
//! let profile = client.get_profile().await?;
//! println!("Logged in as: {}", profile.user_id);
//! # Ok(())
//! # }
//! ```

pub mod auth;
pub mod device_browser;
pub mod devices;
pub mod error;
pub mod meetings;
pub mod participants;
pub mod waiting_room;

pub use error::ApiError;
/// Re-exported so callers of [`MeetingApiClient::with_http`] build the
/// `reqwest::Client` from the same version this crate uses.
pub use reqwest;
pub use videocall_meeting_types;

use std::sync::Arc;

use reqwest::Client;

/// Mints a fresh `Authorization: Bearer` token for each request.
///
/// Used for credentials that must not be reused across requests, such as
/// `videocall-cli`'s device JWTs (one signed token per request, bound to
/// the meeting-api origin). Implementations must never log the token.
pub trait TokenSource: Send + Sync {
    /// A new token for the next request, or `None` if one couldn't be made
    /// (the request is then sent without credentials and fails with 401).
    fn token(&self) -> Option<String>;
}

/// How the client authenticates with the meeting API.
///
/// `Debug` never prints the bearer token.
#[derive(Clone)]
pub enum AuthMode {
    /// Browser mode: send credentials (cookies) automatically via `fetch`.
    /// This is the mode used by `dioxus-ui` and other WASM frontends.
    Cookie,
    /// Bearer token mode: attach `Authorization: Bearer <token>` to every
    /// request. Used by mobile apps and integration tests.
    Bearer(String),
    /// Per-request token mode: ask the [`TokenSource`] for a new token for
    /// every request. Used by `videocall-cli` device keys.
    Signed(Arc<dyn TokenSource>),
}

impl std::fmt::Debug for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthMode::Cookie => f.write_str("Cookie"),
            AuthMode::Bearer(_) => f.write_str("Bearer([REDACTED])"),
            AuthMode::Signed(_) => f.write_str("Signed([REDACTED])"),
        }
    }
}

/// A typed REST client for the videocall.rs meeting API.
///
/// All methods return strongly-typed responses from
/// [`videocall_meeting_types`] and map HTTP errors to [`ApiError`].
#[derive(Debug, Clone)]
pub struct MeetingApiClient {
    base_url: String,
    auth: AuthMode,
    http: Client,
}

impl MeetingApiClient {
    /// Create a new client pointing at the given meeting-api base URL.
    ///
    /// # Arguments
    ///
    /// * `base_url` - e.g. `"http://localhost:8081"`
    /// * `auth` - how to authenticate requests
    pub fn new(base_url: &str, auth: AuthMode) -> Self {
        Self::with_http(Client::new(), base_url, auth)
    }

    /// Create a client that reuses an existing [`reqwest::Client`].
    ///
    /// `reqwest::Client` is a cheap handle onto a shared connection pool, so
    /// a long-running native program (e.g. `videocall-cli`) should build one
    /// and pass it to every `MeetingApiClient` it creates, instead of paying
    /// a new TCP + TLS handshake per client.
    pub fn with_http(http: Client, base_url: &str, auth: AuthMode) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            auth,
            http,
        }
    }

    /// Update the bearer token (e.g. after a token refresh).
    /// No-op if the client is in cookie mode.
    pub fn set_bearer_token(&mut self, token: String) {
        self.auth = AuthMode::Bearer(token);
    }

    /// Build a GET request with auth applied.
    pub(crate) fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.apply_auth(self.http.get(self.url(path)))
    }

    /// Build a POST request with auth applied.
    pub(crate) fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.apply_auth(self.http.post(self.url(path)))
    }

    /// Build a DELETE request with auth applied.
    pub(crate) fn delete(&self, path: &str) -> reqwest::RequestBuilder {
        self.apply_auth(self.http.delete(self.url(path)))
    }

    /// Build a PATCH request with auth applied.
    pub(crate) fn patch(&self, path: &str) -> reqwest::RequestBuilder {
        self.apply_auth(self.http.patch(self.url(path)))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            AuthMode::Cookie => {
                #[cfg(target_arch = "wasm32")]
                {
                    builder.fetch_credentials_include()
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    builder
                }
            }
            AuthMode::Bearer(token) => {
                builder.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
            }
            AuthMode::Signed(source) => match source.token() {
                Some(token) => {
                    builder.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
                }
                None => builder,
            },
        }
    }
}

/// Parse a standard `APIResponse<T>` body, returning `T` on success or
/// mapping the error to [`ApiError`].
pub(crate) async fn parse_api_response<T: serde::de::DeserializeOwned + serde::Serialize>(
    response: reqwest::Response,
) -> Result<T, ApiError> {
    let status = response.status().as_u16();
    match status {
        200 | 201 => {
            let wrapper: videocall_meeting_types::responses::APIResponse<T> =
                response.json().await?;
            Ok(wrapper.result)
        }
        401 => Err(ApiError::NotAuthenticated),
        403 => {
            let text = response.text().await.unwrap_or_default();
            Err(ApiError::Forbidden(text))
        }
        404 => {
            let text = response.text().await.unwrap_or_default();
            Err(ApiError::NotFound(text))
        }
        400 => {
            let text = response.text().await.unwrap_or_default();
            if text.contains("MEETING_NOT_ACTIVE") {
                Err(ApiError::MeetingNotActive)
            } else {
                Err(ApiError::ServerError {
                    status: 400,
                    body: text,
                })
            }
        }
        _ => {
            let text = response.text().await.unwrap_or_default();
            Err(ApiError::ServerError { status, body: text })
        }
    }
}

/// Parse a response where we only care about the status code, not the body.
pub(crate) async fn parse_status_only(response: reqwest::Response) -> Result<(), ApiError> {
    let status = response.status().as_u16();
    match status {
        200..=299 => Ok(()),
        401 => Err(ApiError::NotAuthenticated),
        403 => {
            let text = response.text().await.unwrap_or_default();
            Err(ApiError::Forbidden(text))
        }
        404 => {
            let text = response.text().await.unwrap_or_default();
            Err(ApiError::NotFound(text))
        }
        _ => {
            let text = response.text().await.unwrap_or_default();
            Err(ApiError::ServerError { status, body: text })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_bearer_token() {
        let client = MeetingApiClient::new(
            "http://localhost:8081",
            AuthMode::Bearer("secret-jwt-value".to_string()),
        );
        let printed = format!("{client:?}");
        assert!(!printed.contains("secret-jwt-value"), "{printed}");
        assert!(printed.contains("Bearer([REDACTED])"));
    }
}
