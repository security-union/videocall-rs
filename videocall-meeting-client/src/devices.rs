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

//! `videocall-cli` device login and self-management.

use videocall_meeting_types::device::{
    DeviceErrorResponse, DeviceStatusResponse, RegisterDeviceRequest, RegisterDeviceResponse,
    DEVICES_ME_PATH, DEVICES_PATH,
};

use crate::error::ApiError;
use crate::MeetingApiClient;
use videocall_meeting_types::{APIError, APIResponse};

impl MeetingApiClient {
    fn register_request(&self, request: &RegisterDeviceRequest) -> reqwest::RequestBuilder {
        // Unauthenticated by design: the code, verifier and registration JWS are the proof.
        self.http.post(self.url(DEVICES_PATH)).json(request)
    }

    pub async fn register_device(
        &self,
        request: &RegisterDeviceRequest,
    ) -> Result<RegisterDeviceResponse, ApiError> {
        let response = self.register_request(request).send().await?;
        Ok(success(response).await?.json().await?)
    }

    pub async fn device_me(&self) -> Result<DeviceStatusResponse, ApiError> {
        let response = self.get(DEVICES_ME_PATH).send().await?;
        Ok(success(response).await?.json().await?)
    }

    /// The server's clock, from the `Date` header of an unauthenticated `GET /version`.
    #[cfg(feature = "device-session")]
    pub async fn server_time(&self) -> Option<i64> {
        let response = self.http.get(self.url("/version")).send().await.ok()?;
        let date = response
            .headers()
            .get(reqwest::header::DATE)?
            .to_str()
            .ok()?;
        let time = httpdate::parse_http_date(date).ok()?;
        let secs = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
        i64::try_from(secs).ok()
    }

    pub async fn delete_device_me(&self) -> Result<(), ApiError> {
        let response = self.delete(DEVICES_ME_PATH).send().await?;
        success(response).await.map(drop)
    }
}

pub(crate) async fn success(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(response);
    }
    if status == 401 {
        return Err(ApiError::NotAuthenticated);
    }
    let text = response.text().await.unwrap_or_default();
    Err(device_error_from_body(status, &text))
}

/// Any 404 is [`ApiError::NotFound`] (callers treat it as already gone); otherwise the code
/// comes from either the `APIResponse` or the OAuth error shape.
pub(crate) fn device_error_from_body(status: u16, text: &str) -> ApiError {
    if status == 404 {
        return ApiError::NotFound(text.to_string());
    }
    let parsed = serde_json::from_str::<APIResponse<APIError>>(text)
        .map(|body| (body.result.code, body.result.message))
        .or_else(|_| {
            serde_json::from_str::<DeviceErrorResponse>(text)
                .map(|body| (body.error, body.error_description))
        });
    match parsed {
        Ok((error, error_description)) => ApiError::Device {
            status,
            error,
            error_description,
        },
        Err(_) if status == 403 => ApiError::Forbidden(text.to_string()),
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
