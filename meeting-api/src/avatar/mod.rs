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

//! Per-user avatars (issue #3125): ingest limits, re-encoding and the Gravatar source.

pub mod gravatar;
pub mod image;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use tokio::sync::Semaphore;
use url::Url;
use videocall_meeting_types::avatar::{MAX_AVATAR_INPUT_DIMENSION, MIN_AVATAR_DIMENSION};

use self::gravatar::{GravatarClient, GravatarHash};
use self::image::{AvatarImageError, ImageKind};
use crate::error::AppError;
use crate::rate_limit::KeyedRateLimiter;

const WRITES_PER_WINDOW: u32 = 10;
const READS_PER_WINDOW: u32 = 300;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const MAX_CONCURRENT_ENCODES: usize = 2;

/// Why an avatar operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvatarError {
    Image(AvatarImageError),
    Busy,
    GravatarDisabled,
    GravatarNotFound,
    GravatarTimeout,
    GravatarUnavailable,
    GravatarTooLarge,
    Internal,
}

impl AvatarError {
    /// Stable reason code for logs.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Image(error) => error.reason(),
            Self::Busy => "busy",
            Self::GravatarDisabled => "gravatar_disabled",
            Self::GravatarNotFound => "gravatar_not_found",
            Self::GravatarTimeout => "gravatar_timeout",
            Self::GravatarUnavailable => "gravatar_unavailable",
            Self::GravatarTooLarge => "gravatar_too_large",
            Self::Internal => "internal",
        }
    }
}

impl From<AvatarError> for AppError {
    fn from(error: AvatarError) -> Self {
        match error {
            AvatarError::Image(AvatarImageError::TypeMismatch) => AppError::unsupported_media_type(),
            AvatarError::Image(AvatarImageError::UnknownFormat | AvatarImageError::Corrupt) => {
                AppError::invalid_image("The file is not a valid PNG, JPEG or WebP image.")
            }
            AvatarError::Image(AvatarImageError::TooLarge) => AppError::invalid_image(&format!(
                "The image must be at most {MAX_AVATAR_INPUT_DIMENSION}×{MAX_AVATAR_INPUT_DIMENSION} pixels."
            )),
            AvatarError::Image(AvatarImageError::TooSmall) => AppError::invalid_image(&format!(
                "The image must be at least {MIN_AVATAR_DIMENSION}×{MIN_AVATAR_DIMENSION} pixels."
            )),
            AvatarError::Image(AvatarImageError::EncodedTooLarge) => {
                AppError::invalid_image("The image is too detailed to store. Try another image.")
            }
            AvatarError::GravatarTooLarge => {
                AppError::invalid_image("The Gravatar image is too large.")
            }
            AvatarError::Busy => AppError::service_busy(),
            AvatarError::GravatarDisabled => AppError::feature_not_found(),
            AvatarError::GravatarNotFound => AppError::gravatar_not_found(),
            AvatarError::GravatarTimeout => AppError::gravatar_timeout(),
            AvatarError::GravatarUnavailable => AppError::gravatar_unavailable(),
            AvatarError::Internal => AppError::internal("avatar processing failed"),
        }
    }
}

/// Avatar limits and outbound clients shared by every request on this replica.
pub struct AvatarService {
    write_limiter: KeyedRateLimiter,
    read_limiter: KeyedRateLimiter,
    encode_permits: Arc<Semaphore>,
    gravatar: Option<GravatarClient>,
}

impl AvatarService {
    /// `gravatar_base: None` disables the Gravatar source.
    pub fn new(gravatar_base: Option<Url>) -> Self {
        Self {
            write_limiter: KeyedRateLimiter::new(WRITES_PER_WINDOW, RATE_WINDOW),
            read_limiter: KeyedRateLimiter::new(READS_PER_WINDOW, RATE_WINDOW),
            encode_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_ENCODES)),
            gravatar: gravatar_base.map(GravatarClient::new),
        }
    }

    /// Whether `PUT /api/v1/user/avatar/gravatar` is served.
    pub fn gravatar_enabled(&self) -> bool {
        self.gravatar.is_some()
    }

    /// Consumes one slot of `user_id`'s shared write budget.
    pub fn allow_write(&self, user_id: &str) -> bool {
        self.write_limiter.allow(user_id)
    }

    /// Consumes one slot of `viewer`'s read budget in `meeting_id`.
    pub fn allow_read(&self, meeting_id: &str, viewer: &str) -> bool {
        self.read_limiter
            .allow(&format!("{meeting_id}\u{0}{viewer}"))
    }

    /// Re-encodes `bytes` off the async runtime, shedding work beyond the concurrency bound.
    pub async fn reencode(
        &self,
        bytes: Bytes,
        declared: Option<ImageKind>,
    ) -> Result<Vec<u8>, AvatarError> {
        let permit = Arc::clone(&self.encode_permits)
            .try_acquire_owned()
            .map_err(|_| AvatarError::Busy)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            self::image::process_avatar(&bytes, declared)
        })
        .await
        .map_err(|_| AvatarError::Internal)?
        .map_err(AvatarError::Image)
    }

    /// Fetches the Gravatar for `hash` and re-encodes it.
    pub async fn import_gravatar(&self, hash: &GravatarHash) -> Result<Vec<u8>, AvatarError> {
        let gravatar = self
            .gravatar
            .as_ref()
            .ok_or(AvatarError::GravatarDisabled)?;
        let bytes = gravatar.fetch(hash).await?;
        self.reencode(bytes.into(), None).await
    }
}

impl Default for AvatarService {
    fn default() -> Self {
        Self::new(Some(gravatar::default_base()))
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    const PNG: &[u8] = include_bytes!("../../tests/fixtures/avatars/ok-64.png");

    #[tokio::test]
    async fn reencoding_is_shed_when_every_permit_is_held() {
        let service = AvatarService::new(None);
        let held = Arc::clone(&service.encode_permits)
            .try_acquire_many_owned(MAX_CONCURRENT_ENCODES as u32)
            .unwrap();
        assert_eq!(
            service
                .reencode(Bytes::from_static(PNG), Some(ImageKind::Png))
                .await,
            Err(AvatarError::Busy)
        );
        drop(held);
        assert!(service
            .reencode(Bytes::from_static(PNG), Some(ImageKind::Png))
            .await
            .is_ok());
    }

    #[test]
    fn writes_share_one_budget_per_user() {
        let service = AvatarService::default();
        for _ in 0..WRITES_PER_WINDOW {
            assert!(service.allow_write("alice@example.com"));
        }
        assert!(!service.allow_write("alice@example.com"));
        assert!(service.allow_write("bob@example.com"));
    }

    #[test]
    fn reads_are_budgeted_per_meeting_and_viewer() {
        let service = AvatarService::default();
        for _ in 0..READS_PER_WINDOW {
            assert!(service.allow_read("room-a", "alice"));
        }
        assert!(!service.allow_read("room-a", "alice"));
        assert!(service.allow_read("room-b", "alice"));
        assert!(service.allow_read("room-a", "bob"));
    }

    #[tokio::test]
    async fn a_disabled_gravatar_source_never_fetches() {
        let service = AvatarService::new(None);
        assert!(!service.gravatar_enabled());
        let hash = gravatar::parse_gravatar_input("someone@example.com").unwrap();
        assert_eq!(
            service.import_gravatar(&hash).await,
            Err(AvatarError::GravatarDisabled)
        );
        assert!(AvatarService::default().gravatar_enabled());
    }

    #[test]
    fn errors_map_to_the_documented_statuses_and_codes() {
        let cases = [
            (
                AvatarError::Image(AvatarImageError::TypeMismatch),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "UNSUPPORTED_MEDIA_TYPE",
            ),
            (
                AvatarError::Image(AvatarImageError::UnknownFormat),
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_IMAGE",
            ),
            (
                AvatarError::Image(AvatarImageError::TooLarge),
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_IMAGE",
            ),
            (
                AvatarError::Image(AvatarImageError::EncodedTooLarge),
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_IMAGE",
            ),
            (
                AvatarError::GravatarTooLarge,
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_IMAGE",
            ),
            (
                AvatarError::Busy,
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_BUSY",
            ),
            (
                AvatarError::GravatarDisabled,
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
            ),
            (
                AvatarError::GravatarNotFound,
                StatusCode::UNPROCESSABLE_ENTITY,
                "GRAVATAR_NOT_FOUND",
            ),
            (
                AvatarError::GravatarTimeout,
                StatusCode::GATEWAY_TIMEOUT,
                "GRAVATAR_UNAVAILABLE",
            ),
            (
                AvatarError::GravatarUnavailable,
                StatusCode::BAD_GATEWAY,
                "GRAVATAR_UNAVAILABLE",
            ),
        ];
        for (error, status, code) in cases {
            let app = AppError::from(error);
            assert_eq!(
                (app.status, app.body.code.as_str()),
                (status, code),
                "{error:?}"
            );
        }
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
    }
}
