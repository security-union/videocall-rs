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

//! Avatar endpoints (issue #3125).

use reqwest::header::CONTENT_TYPE;
use videocall_meeting_types::avatar::{AvatarInfo, SetGravatarRequest, UserAvatarResponse};
use videocall_meeting_types::{APIError, APIResponse};

use crate::error::ApiError;
use crate::{parse_api_response, MeetingApiClient};

/// An avatar image as served, with the response `Content-Type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvatarImage {
    pub bytes: Vec<u8>,
    pub content_type: String,
}

impl MeetingApiClient {
    /// `GET /api/v1/user/avatar`: the caller's avatar metadata.
    pub async fn get_my_avatar(&self) -> Result<UserAvatarResponse, ApiError> {
        let response = self.get("/api/v1/user/avatar").send().await?;
        parse_api_response(response).await
    }

    /// `PUT /api/v1/user/avatar` with raw image bytes of type `content_type`.
    ///
    /// With `expected_user_id`, a different signed-in user fails with
    /// [`ApiError::IdentityMismatch`] instead of being written.
    pub async fn upload_avatar(
        &self,
        bytes: Vec<u8>,
        content_type: &str,
        expected_user_id: Option<&str>,
    ) -> Result<AvatarInfo, ApiError> {
        let response = with_expected_user(self.put("/api/v1/user/avatar"), expected_user_id)
            .header(CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await?;
        parse_api_response(response).await
    }

    /// `PUT /api/v1/user/avatar/gravatar` with a Gravatar e-mail or avatar link.
    ///
    /// `expected_user_id` behaves as in [`Self::upload_avatar`].
    pub async fn set_gravatar_avatar(
        &self,
        gravatar: &str,
        expected_user_id: Option<&str>,
    ) -> Result<AvatarInfo, ApiError> {
        let body = SetGravatarRequest {
            gravatar: gravatar.to_string(),
        };
        let response =
            with_expected_user(self.put("/api/v1/user/avatar/gravatar"), expected_user_id)
                .json(&body)
                .send()
                .await?;
        parse_api_response(response).await
    }

    /// `DELETE /api/v1/user/avatar`.
    ///
    /// `expected_user_id` behaves as in [`Self::upload_avatar`].
    pub async fn delete_avatar(&self, expected_user_id: Option<&str>) -> Result<(), ApiError> {
        let response = with_expected_user(self.delete("/api/v1/user/avatar"), expected_user_id)
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(error_from(response).await)
    }

    /// `GET /api/v1/user/avatar/image`; `None` when no avatar is set.
    pub async fn get_my_avatar_image(&self) -> Result<Option<AvatarImage>, ApiError> {
        let response = self.get("/api/v1/user/avatar/image").send().await?;
        parse_image_response(response).await
    }

    /// `GET /api/v1/meetings/{meeting_id}/avatar?user_id=`; the client must hold the room token.
    pub async fn get_meeting_avatar_image(
        &self,
        meeting_id: &str,
        user_id: &str,
    ) -> Result<Option<AvatarImage>, ApiError> {
        let response = self
            .get(&format!("/api/v1/meetings/{meeting_id}/avatar"))
            .query(&[("user_id", user_id)])
            .send()
            .await?;
        parse_image_response(response).await
    }
}

fn with_expected_user(
    request: reqwest::RequestBuilder,
    expected_user_id: Option<&str>,
) -> reqwest::RequestBuilder {
    match expected_user_id {
        Some(user_id) => request.query(&[("expected_user_id", user_id)]),
        None => request,
    }
}

async fn parse_image_response(
    response: reqwest::Response,
) -> Result<Option<AvatarImage>, ApiError> {
    match response.status().as_u16() {
        200 => {
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let bytes = response.bytes().await?.to_vec();
            Ok(Some(AvatarImage {
                bytes,
                content_type,
            }))
        }
        404 => {
            let text = response.text().await.unwrap_or_default();
            let no_avatar = serde_json::from_str::<APIResponse<APIError>>(&text)
                .is_ok_and(|body| body.result.code == "AVATAR_NOT_FOUND");
            if no_avatar {
                Ok(None)
            } else {
                Err(ApiError::NotFound(text))
            }
        }
        _ => Err(error_from(response).await),
    }
}

async fn error_from(response: reqwest::Response) -> ApiError {
    let status = response.status().as_u16();
    match parse_api_response::<serde_json::Value>(response).await {
        Err(error) => error,
        Ok(body) => ApiError::ServerError {
            status,
            body: body.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AuthMode;
    use videocall_meeting_types::avatar::AvatarSource;
    use wiremock::matchers::{
        body_bytes, body_json, header, method, path, query_param, query_param_is_missing,
    };
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn info() -> serde_json::Value {
        serde_json::json!({
            "success": true,
            "result": {
                "source": "upload",
                "content_type": "image/jpeg",
                "etag": "0123456789abcdef0123456789abcdef",
                "updated_at": 1_700_000_000_000_i64,
            }
        })
    }

    fn error_body(code: &str) -> serde_json::Value {
        serde_json::json!({ "success": false, "result": { "code": code, "message": "m" } })
    }

    #[tokio::test]
    async fn upload_sends_the_exact_bytes_and_type() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/user/avatar"))
            .and(header("content-type", "image/webp"))
            .and(header("authorization", "Bearer session-token"))
            .and(body_bytes(vec![1u8, 2, 3, 255]))
            .and(query_param_is_missing("expected_user_id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(info()))
            .expect(1)
            .mount(&server)
            .await;
        let client = MeetingApiClient::new(&server.uri(), AuthMode::Bearer("session-token".into()));
        let info = client
            .upload_avatar(vec![1, 2, 3, 255], "image/webp", None)
            .await
            .unwrap();
        assert_eq!(info.source, AvatarSource::Upload);
        assert_eq!(info.gravatar_url, None);
    }

    #[tokio::test]
    async fn gravatar_and_metadata_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/api/v1/user/avatar/gravatar"))
            .and(body_json(
                serde_json::json!({ "gravatar": "me@example.com" }),
            ))
            .and(query_param_is_missing("expected_user_id"))
            .respond_with(ResponseTemplate::new(200).set_body_json(info()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/user/avatar"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "success": true,
                "result": {
                    "avatar": null,
                    "gravatar_enabled": false,
                    "user_id": "me@example.com"
                }
            })))
            .mount(&server)
            .await;
        let client = MeetingApiClient::new(&server.uri(), AuthMode::Cookie);
        client
            .set_gravatar_avatar("me@example.com", None)
            .await
            .unwrap();
        let mine = client.get_my_avatar().await.unwrap();
        assert_eq!(mine.avatar, None);
        assert!(!mine.gravatar_enabled);
        assert_eq!(mine.user_id, "me@example.com");
    }

    #[tokio::test]
    async fn writes_send_the_expected_user_id_as_an_encoded_query() {
        let expected = "a/b+c@x %?#&";
        let server = MockServer::start().await;
        for (verb, route) in [
            ("PUT", "/api/v1/user/avatar"),
            ("PUT", "/api/v1/user/avatar/gravatar"),
            ("DELETE", "/api/v1/user/avatar"),
        ] {
            Mock::given(method(verb))
                .and(path(route))
                .and(query_param("expected_user_id", expected))
                .respond_with(if verb == "DELETE" {
                    ResponseTemplate::new(204)
                } else {
                    ResponseTemplate::new(200).set_body_json(info())
                })
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = MeetingApiClient::new(&server.uri(), AuthMode::Cookie);
        client
            .upload_avatar(vec![1], "image/png", Some(expected))
            .await
            .unwrap();
        client
            .set_gravatar_avatar("me@example.com", Some(expected))
            .await
            .unwrap();
        client.delete_avatar(Some(expected)).await.unwrap();
    }

    #[tokio::test]
    async fn only_an_identity_mismatch_409_maps_to_identity_mismatch() {
        let server = MockServer::start().await;
        Mock::given(query_param("expected_user_id", "a"))
            .respond_with(
                ResponseTemplate::new(409).set_body_json(error_body("AVATAR_IDENTITY_MISMATCH")),
            )
            .mount(&server)
            .await;
        Mock::given(query_param("expected_user_id", "b"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
                "success": false,
                "result": { "code": "MEETING_EXISTS", "message": "AVATAR_IDENTITY_MISMATCH" }
            })))
            .mount(&server)
            .await;
        let client = MeetingApiClient::new(&server.uri(), AuthMode::Cookie);
        assert!(matches!(
            client.upload_avatar(vec![1], "image/png", Some("a")).await,
            Err(ApiError::IdentityMismatch)
        ));
        assert!(matches!(
            client
                .set_gravatar_avatar("me@example.com", Some("a"))
                .await,
            Err(ApiError::IdentityMismatch)
        ));
        assert!(matches!(
            client.delete_avatar(Some("a")).await,
            Err(ApiError::IdentityMismatch)
        ));
        assert!(matches!(
            client.delete_avatar(Some("b")).await,
            Err(ApiError::ServerError { status: 409, .. })
        ));
    }

    #[tokio::test]
    async fn meeting_image_sends_the_user_id_as_an_encoded_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/meetings/m1/avatar"))
            .and(query_param("user_id", "a/b+c@x %?#&"))
            .and(header("authorization", "Bearer room-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/jpeg")
                    .set_body_bytes(vec![0xff, 0xd8, 0xff]),
            )
            .expect(1)
            .mount(&server)
            .await;
        let client = MeetingApiClient::new(&server.uri(), AuthMode::Bearer("room-token".into()));
        let image = client
            .get_meeting_avatar_image("m1", "a/b+c@x %?#&")
            .await
            .unwrap()
            .expect("image");
        assert_eq!(image.bytes, vec![0xff, 0xd8, 0xff]);
        assert_eq!(image.content_type, "image/jpeg");
    }

    #[tokio::test]
    async fn only_avatar_not_found_means_no_avatar() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("user_id", "none"))
            .respond_with(ResponseTemplate::new(404).set_body_json(error_body("AVATAR_NOT_FOUND")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("user_id", "gone"))
            .respond_with(ResponseTemplate::new(404).set_body_json(error_body("NOT_IN_MEETING")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/user/avatar/image"))
            .respond_with(ResponseTemplate::new(404).set_body_json(error_body("AVATAR_NOT_FOUND")))
            .mount(&server)
            .await;
        let client = MeetingApiClient::new(&server.uri(), AuthMode::Bearer("t".into()));
        assert_eq!(
            client.get_meeting_avatar_image("m1", "none").await.unwrap(),
            None
        );
        assert!(matches!(
            client.get_meeting_avatar_image("m1", "gone").await,
            Err(ApiError::NotFound(body)) if body.contains("NOT_IN_MEETING")
        ));
        assert_eq!(client.get_my_avatar_image().await.unwrap(), None);
    }

    #[tokio::test]
    async fn delete_and_rate_limits_map_to_typed_errors() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(query_param_is_missing("expected_user_id"))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(
                ResponseTemplate::new(429).set_body_json(error_body("RATE_LIMIT_EXCEEDED")),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(429).set_body_json(error_body("RATE_LIMIT_EXCEEDED")),
            )
            .mount(&server)
            .await;
        let client = MeetingApiClient::new(&server.uri(), AuthMode::Cookie);
        client.delete_avatar(None).await.unwrap();
        assert!(matches!(
            client.delete_avatar(None).await,
            Err(ApiError::RateLimitExceeded)
        ));
        assert!(matches!(
            client.get_meeting_avatar_image("m1", "u").await,
            Err(ApiError::RateLimitExceeded)
        ));
    }
}
