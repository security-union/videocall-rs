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

//! Per-user avatar API types and limits (issue #3125).

use serde::{Deserialize, Serialize};

/// Largest request body accepted by `PUT /api/v1/user/avatar`.
pub const MAX_AVATAR_UPLOAD_BYTES: usize = 262_144;

/// Largest re-encoded image the server stores.
pub const MAX_AVATAR_STORED_BYTES: usize = 65_536;

/// Side of the square image the server stores (smaller inputs are not upscaled).
pub const AVATAR_OUTPUT_DIMENSION: u32 = 256;

/// Smallest accepted width or height of an uploaded image.
pub const MIN_AVATAR_DIMENSION: u32 = 16;

/// Largest accepted width or height of an uploaded image.
pub const MAX_AVATAR_INPUT_DIMENSION: u32 = 1024;

/// `Content-Type` values accepted by `PUT /api/v1/user/avatar`.
pub const ALLOWED_AVATAR_UPLOAD_CONTENT_TYPES: [&str; 3] =
    ["image/png", "image/jpeg", "image/webp"];

/// Largest file a UI should take from its file picker before resizing it client-side.
pub const AVATAR_CLIENT_MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Body of `PUT /api/v1/user/avatar/gravatar`: a Gravatar e-mail or a Gravatar avatar link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetGravatarRequest {
    pub gravatar: String,
}

/// Where the stored avatar came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AvatarSource {
    Upload,
    Gravatar,
}

/// Metadata of the caller's stored avatar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvatarInfo {
    pub source: AvatarSource,
    /// `https://gravatar.com/avatar/{hash}` when `source` is `gravatar`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gravatar_url: Option<String>,
    pub content_type: String,
    pub etag: String,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// Response of `GET /api/v1/user/avatar`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAvatarResponse {
    pub avatar: Option<AvatarInfo>,
    pub gravatar_enabled: bool,
    /// The caller's user id; empty from a server that predates this field.
    #[serde(default)]
    pub user_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatar_source_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&AvatarSource::Upload).unwrap(),
            "\"upload\""
        );
        assert_eq!(
            serde_json::to_string(&AvatarSource::Gravatar).unwrap(),
            "\"gravatar\""
        );
    }

    #[test]
    fn upload_info_omits_gravatar_url() {
        let info = AvatarInfo {
            source: AvatarSource::Upload,
            gravatar_url: None,
            content_type: "image/jpeg".to_string(),
            etag: "0123456789abcdef0123456789abcdef".to_string(),
            updated_at: 1_700_000_000_000,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "source": "upload",
                "content_type": "image/jpeg",
                "etag": "0123456789abcdef0123456789abcdef",
                "updated_at": 1_700_000_000_000_i64,
            })
        );
        assert_eq!(serde_json::from_value::<AvatarInfo>(json).unwrap(), info);
    }

    #[test]
    fn no_avatar_round_trips_as_null() {
        let response = UserAvatarResponse {
            avatar: None,
            gravatar_enabled: false,
            user_id: "alice@example.com".to_string(),
        };
        let text = serde_json::to_string(&response).unwrap();
        assert_eq!(
            text,
            r#"{"avatar":null,"gravatar_enabled":false,"user_id":"alice@example.com"}"#
        );
        assert_eq!(
            serde_json::from_str::<UserAvatarResponse>(&text).unwrap(),
            response
        );
    }

    #[test]
    fn a_response_without_user_id_reads_as_unknown() {
        let response: UserAvatarResponse =
            serde_json::from_str(r#"{"avatar":null,"gravatar_enabled":true}"#).unwrap();
        assert_eq!(response.user_id, "");
        assert!(response.gravatar_enabled);
    }
}
