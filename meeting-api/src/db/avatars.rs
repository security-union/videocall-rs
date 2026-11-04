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

//! `user_avatars` table queries.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use videocall_meeting_types::avatar::AvatarSource;

/// Avatar metadata; never carries the image.
#[derive(Debug, sqlx::FromRow)]
pub struct AvatarMetaRow {
    pub source: String,
    pub gravatar_hash: Option<String>,
    pub content_type: String,
    pub etag: String,
    pub updated_at: DateTime<Utc>,
}

impl AvatarMetaRow {
    /// The stored `source` as the API enum.
    pub fn source(&self) -> AvatarSource {
        if self.source == "gravatar" {
            AvatarSource::Gravatar
        } else {
            AvatarSource::Upload
        }
    }
}

/// An image lookup; `image` is `None` when the caller's validator matched.
#[derive(Debug, sqlx::FromRow)]
pub struct AvatarImageRow {
    pub content_type: String,
    pub etag: String,
    pub image: Option<Vec<u8>>,
}

/// A new or replacement avatar.
pub struct NewAvatar<'a> {
    pub source: AvatarSource,
    pub content_type: &'a str,
    pub image: &'a [u8],
    pub gravatar_hash: Option<&'a str>,
    pub etag: &'a str,
}

/// Parsed `If-None-Match`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IfNoneMatch {
    pub any: bool,
    pub tags: Vec<String>,
}

fn source_str(source: AvatarSource) -> &'static str {
    match source {
        AvatarSource::Upload => "upload",
        AvatarSource::Gravatar => "gravatar",
    }
}

/// Insert or replace `user_id`'s avatar.
pub async fn upsert(
    pool: &PgPool,
    user_id: &str,
    avatar: &NewAvatar<'_>,
) -> Result<AvatarMetaRow, sqlx::Error> {
    sqlx::query_as::<_, AvatarMetaRow>(
        "INSERT INTO user_avatars (user_id, source, content_type, image, gravatar_hash, etag) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (user_id) DO UPDATE SET source = EXCLUDED.source, \
             content_type = EXCLUDED.content_type, image = EXCLUDED.image, \
             gravatar_hash = EXCLUDED.gravatar_hash, etag = EXCLUDED.etag \
         RETURNING source, gravatar_hash, content_type, etag, updated_at",
    )
    .bind(user_id)
    .bind(source_str(avatar.source))
    .bind(avatar.content_type)
    .bind(avatar.image)
    .bind(avatar.gravatar_hash)
    .bind(avatar.etag)
    .fetch_one(pool)
    .await
}

/// `user_id`'s avatar metadata.
pub async fn get_meta(pool: &PgPool, user_id: &str) -> Result<Option<AvatarMetaRow>, sqlx::Error> {
    sqlx::query_as::<_, AvatarMetaRow>(
        "SELECT source, gravatar_hash, content_type, etag, updated_at \
         FROM user_avatars WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// `user_id`'s image, omitted when `if_none_match` matches the stored etag.
pub async fn get_image(
    pool: &PgPool,
    user_id: &str,
    if_none_match: &IfNoneMatch,
) -> Result<Option<AvatarImageRow>, sqlx::Error> {
    sqlx::query_as::<_, AvatarImageRow>(
        "SELECT content_type, etag, \
                CASE WHEN $2 OR etag = ANY($3) THEN NULL ELSE image END AS image \
         FROM user_avatars WHERE user_id = $1",
    )
    .bind(user_id)
    .bind(if_none_match.any)
    .bind(&if_none_match.tags)
    .fetch_optional(pool)
    .await
}

/// Remove `user_id`'s avatar; `true` when a row was deleted.
pub async fn delete(pool: &PgPool, user_id: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query("DELETE FROM user_avatars WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
