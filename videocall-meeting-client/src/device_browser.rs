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

//! Device management from a signed-in browser session. These routes answer
//! in the standard `APIResponse` envelope; a failure with a machine-readable
//! code is [`ApiError::Device`] whichever error shape the server used, so
//! callers match on one variant.

use videocall_meeting_types::device::{
    is_valid_device_id, CreateDeviceAuthorizationRequest, CreateDeviceAuthorizationResponse,
    DeviceListItem, DEVICES_PATH, DEVICE_AUTHORIZATIONS_PATH,
};
use videocall_meeting_types::APIResponse;

use crate::devices::success;
use crate::error::ApiError;
use crate::MeetingApiClient;

impl MeetingApiClient {
    /// Approve a CLI login (`POST /api/v1/devices/authorizations`).
    ///
    /// Sent as JSON, so the browser adds `Content-Type: application/json`
    /// and its `Origin`, which the server requires on this route.
    pub async fn authorize_device(
        &self,
        request: &CreateDeviceAuthorizationRequest,
    ) -> Result<CreateDeviceAuthorizationResponse, ApiError> {
        let response = self
            .post(DEVICE_AUTHORIZATIONS_PATH)
            .json(request)
            .send()
            .await?;
        enveloped(response).await
    }

    /// The signed-in user's live devices.
    pub async fn list_devices(&self) -> Result<Vec<DeviceListItem>, ApiError> {
        let response = self.get(DEVICES_PATH).send().await?;
        enveloped(response).await
    }

    /// Revoke one of the signed-in user's devices.
    ///
    /// An id that is not device-id shaped never reaches the network, so it
    /// cannot change the request path.
    pub async fn revoke_device(&self, device_id: &str) -> Result<(), ApiError> {
        if !is_valid_device_id(device_id) {
            return Err(ApiError::NotFound(String::new()));
        }
        let response = self
            .delete(&format!("{DEVICES_PATH}/{device_id}"))
            .send()
            .await?;
        success(response).await.map(drop)
    }
}

async fn enveloped<T: serde::de::DeserializeOwned + serde::Serialize>(
    response: reqwest::Response,
) -> Result<T, ApiError> {
    let wrapper: APIResponse<T> = success(response).await?.json().await?;
    Ok(wrapper.result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::device_error_from_body;
    use crate::AuthMode;
    use videocall_meeting_types::device::{DEVICE_NOT_FOUND, TOO_MANY_DEVICES};

    fn client() -> MeetingApiClient {
        MeetingApiClient::new("https://api.example.com/", AuthMode::Cookie)
    }

    #[test]
    fn authorize_is_a_json_post() {
        let request = client()
            .post(DEVICE_AUTHORIZATIONS_PATH)
            .json(&CreateDeviceAuthorizationRequest {
                redirect_uri: "urn:ietf:wg:oauth:2.0:oob".into(),
                code_challenge: "c".repeat(43),
                code_challenge_method: "S256".into(),
                state: String::new(),
                public_key: "A".repeat(43),
                device_name: "pi4".into(),
                confirm: true,
            })
            .build()
            .unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(
            request.url().as_str(),
            "https://api.example.com/api/v1/devices/authorizations"
        );
        assert_eq!(
            request.headers()[reqwest::header::CONTENT_TYPE],
            "application/json"
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
        assert_eq!(body["confirm"], true);
    }

    #[test]
    fn error_shapes_become_device_errors() {
        let enveloped = &format!(
            r#"{{"success":false,"result":{{"code":"{TOO_MANY_DEVICES}","message":"m"}}}}"#
        );
        assert!(matches!(
            device_error_from_body(409, enveloped),
            ApiError::Device { status: 409, ref error, .. } if error == TOO_MANY_DEVICES
        ));
        let flat = r#"{"error":"not_configured","error_description":"d"}"#;
        assert!(matches!(
            device_error_from_body(503, flat),
            ApiError::Device { status: 503, ref error, .. } if error == "not_configured"
        ));
        let not_found = &format!(
            r#"{{"success":false,"result":{{"code":"{DEVICE_NOT_FOUND}","message":"m"}}}}"#
        );
        assert!(matches!(
            device_error_from_body(404, not_found),
            ApiError::NotFound(_)
        ));
        assert!(matches!(
            device_error_from_body(502, "<html>"),
            ApiError::ServerError { status: 502, .. }
        ));
    }
}
