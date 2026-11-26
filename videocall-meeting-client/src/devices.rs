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

//! `videocall-cli` device login and device self-management.
//!
//! - [`cli_authorize_url`](MeetingApiClient::cli_authorize_url): the
//!   browser URL that starts an authorization-code + PKCE login for a new
//!   device key. Builds a URL only; no network.
//! - [`register_device`](MeetingApiClient::register_device): exchange the
//!   code for an approved device (`POST /api/v1/devices`). Always sent
//!   without credentials, whatever the client's [`AuthMode`](crate::AuthMode).
//! - [`device_me`](MeetingApiClient::device_me) and
//!   [`delete_device_me`](MeetingApiClient::delete_device_me): status and
//!   self-revoke for the calling device, authenticated with a device JWT
//!   (normally [`AuthMode::Signed`](crate::AuthMode::Signed)).
//!
//! A 401 is [`ApiError::NotAuthenticated`]; errors in the device endpoints'
//! `{"error","error_description"}` shape are [`ApiError::Device`].

use videocall_meeting_types::device::{
    DeviceErrorResponse, DeviceStatusResponse, RegisterDeviceRequest, RegisterDeviceResponse,
    CLI_AUTHORIZE_PATH, CLI_CLIENT_ID, CLI_REMOTE_CODE_PATH, DEVICES_ME_PATH, DEVICES_PATH,
};

use crate::error::ApiError;
use crate::MeetingApiClient;

/// Inputs of [`MeetingApiClient::cli_authorize_url`].
#[derive(Debug, Clone, Copy)]
pub struct CliAuthorizeParams<'a> {
    /// `http://127.0.0.1:{port}/callback`, or `{api origin}/cli/code`.
    pub redirect_uri: &'a str,
    /// `BASE64URL(SHA256(code_verifier))`.
    pub code_challenge: &'a str,
    /// Random `state`, checked on the loopback callback.
    pub state: &'a str,
    /// The new device's Ed25519 public key, base64url (43 chars).
    pub public_key: &'a str,
    /// Device name shown on the consent page.
    pub device_name: &'a str,
}

impl MeetingApiClient {
    /// `{base}/cli/authorize?response_type=code&client_id=videocall-cli&…`
    /// with `code_challenge_method=S256`. No network.
    pub fn cli_authorize_url(&self, params: &CliAuthorizeParams<'_>) -> Result<String, ApiError> {
        let mut url = reqwest::Url::parse(&self.url(CLI_AUTHORIZE_PATH))
            .map_err(|e| ApiError::Config(format!("bad base URL: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLI_CLIENT_ID)
            .append_pair("redirect_uri", params.redirect_uri)
            .append_pair("code_challenge", params.code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", params.state)
            .append_pair("public_key", params.public_key)
            .append_pair("device_name", params.device_name);
        Ok(url.into())
    }

    /// `{base}/cli/code`, the `redirect_uri` of a `--remote` login.
    pub fn cli_code_redirect_uri(&self) -> String {
        self.url(CLI_REMOTE_CODE_PATH)
    }

    fn register_request(&self, request: &RegisterDeviceRequest) -> reqwest::RequestBuilder {
        // Unauthenticated by design: the code, verifier and registration JWS
        // are the proof.
        self.http.post(self.url(DEVICES_PATH)).json(request)
    }

    /// Exchange an authorization code for an approved device (`201`).
    pub async fn register_device(
        &self,
        request: &RegisterDeviceRequest,
    ) -> Result<RegisterDeviceResponse, ApiError> {
        let response = self.register_request(request).send().await?;
        parse_device_json(response).await
    }

    /// The calling device's record (`GET /api/v1/devices/me`).
    pub async fn device_me(&self) -> Result<DeviceStatusResponse, ApiError> {
        let response = self.get(DEVICES_ME_PATH).send().await?;
        parse_device_json(response).await
    }

    /// Revoke the calling device (`DELETE /api/v1/devices/me`, `204`).
    pub async fn delete_device_me(&self) -> Result<(), ApiError> {
        let response = self.delete(DEVICES_ME_PATH).send().await?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(());
        }
        Err(device_error(status, response).await)
    }
}

async fn parse_device_json<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, ApiError> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(device_error(status, response).await);
    }
    Ok(response.json().await?)
}

async fn device_error(status: u16, response: reqwest::Response) -> ApiError {
    if status == 401 {
        return ApiError::NotAuthenticated;
    }
    let text = response.text().await.unwrap_or_default();
    device_error_from_body(status, &text)
}

fn device_error_from_body(status: u16, text: &str) -> ApiError {
    match serde_json::from_str::<DeviceErrorResponse>(text) {
        Ok(body) => ApiError::Device {
            status,
            error: body.error,
            error_description: body.error_description,
        },
        Err(_) if status == 403 => ApiError::Forbidden(text.to_string()),
        Err(_) if status == 404 => ApiError::NotFound(text.to_string()),
        Err(_) => ApiError::ServerError {
            status,
            body: text.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthMode, TokenSource};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Counter(AtomicUsize);

    impl TokenSource for Counter {
        fn token(&self) -> Option<String> {
            Some(format!("tok{}", self.0.fetch_add(1, Ordering::SeqCst)))
        }
    }

    fn signed() -> (Arc<Counter>, MeetingApiClient) {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        let client = MeetingApiClient::new(
            "https://api.example.com/",
            AuthMode::Signed(counter.clone()),
        );
        (counter, client)
    }

    #[test]
    fn authorize_url_shape() {
        let (_, client) = signed();
        let url = client
            .cli_authorize_url(&CliAuthorizeParams {
                redirect_uri: "http://127.0.0.1:5000/callback",
                code_challenge: "challenge",
                state: "st&ate=x",
                public_key: "pk",
                device_name: "videocall-cli on pi4",
            })
            .unwrap();
        let url = reqwest::Url::parse(&url).unwrap();
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
        assert_eq!(get("device_name"), Some("videocall-cli on pi4"));
        assert_eq!(
            client.cli_code_redirect_uri(),
            "https://api.example.com/cli/code"
        );
    }

    #[test]
    fn register_is_unauthenticated() {
        let (counter, client) = signed();
        let request = client
            .register_request(&RegisterDeviceRequest {
                code: "c".into(),
                code_verifier: "v".into(),
                redirect_uri: "r".into(),
                registration: "hdr.claims.sig".into(),
            })
            .build()
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.url().as_str(),
            "https://api.example.com/api/v1/devices"
        );
        assert!(request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .is_none());
        assert_eq!(counter.0.load(Ordering::SeqCst), 0, "nothing minted");
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["registration"], "hdr.claims.sig");
        assert_eq!(body["code_verifier"], "v");
    }

    #[test]
    fn signed_mode_mints_per_request() {
        let (counter, client) = signed();
        let first = client.get(DEVICES_ME_PATH).build().unwrap();
        let second = client.delete(DEVICES_ME_PATH).build().unwrap();
        assert_eq!(
            first.headers()[reqwest::header::AUTHORIZATION],
            "Bearer tok0"
        );
        assert_eq!(
            second.headers()[reqwest::header::AUTHORIZATION],
            "Bearer tok1"
        );
        assert_eq!(second.method(), reqwest::Method::DELETE);
        assert_eq!(
            second.url().as_str(),
            "https://api.example.com/api/v1/devices/me"
        );
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
        assert!(format!("{client:?}").contains("Signed([REDACTED])"));
    }

    #[test]
    fn error_bodies() {
        assert!(matches!(
            device_error_from_body(400, r#"{"error":"invalid_grant","error_description":"x"}"#),
            ApiError::Device { status: 400, ref error, .. } if error == "invalid_grant"
        ));
        assert!(matches!(
            device_error_from_body(502, "<html>"),
            ApiError::ServerError { status: 502, .. }
        ));
    }
}
