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

//! Per-user avatar routes (issue #3125).

use axum::{
    body::Bytes,
    extract::{
        rejection::{BytesRejection, JsonRejection, QueryRejection},
        Query, State,
    },
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use videocall_meeting_types::avatar::{
    AvatarInfo, AvatarSource, SetGravatarRequest, UserAvatarResponse, MAX_AVATAR_UPLOAD_BYTES,
};
use videocall_meeting_types::{APIError, APIResponse};

use crate::auth::{AuthUser, RoomMember};
use crate::avatar::gravatar::{self, GravatarHash};
use crate::avatar::image::{etag_for, ImageKind, AVATAR_CONTENT_TYPE};
use crate::avatar::{AvatarError, AvatarService};
use crate::db::avatars::{
    self as db_avatars, AvatarImageRow, AvatarMetaRow, IfNoneMatch, NewAvatar,
};
use crate::db::{meetings as db_meetings, participants as db_participants};
use crate::error::AppError;
use crate::nats_events;
use crate::routes::valid_meeting_id::ValidMeetingId;
use crate::state::AppState;

/// Body limit of `PUT /api/v1/user/avatar/gravatar`.
pub const MAX_GRAVATAR_BODY: usize = 4096;

const MAX_USER_ID_BYTES: usize = 255;
const MAX_VALIDATORS: usize = 16;

/// Query of `GET /api/v1/meetings/{meeting_id}/avatar`.
#[derive(Debug, Deserialize)]
pub struct MeetingAvatarQuery {
    #[serde(default)]
    pub user_id: String,
}

/// Query of the own-avatar write routes.
#[derive(Debug, Deserialize)]
pub struct AvatarWriteQuery {
    /// When present, the write is refused unless it equals the caller's user id.
    pub expected_user_id: Option<String>,
}

/// `GET /api/v1/user/avatar`
pub async fn get_my_avatar(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
) -> Result<Json<APIResponse<UserAvatarResponse>>, AppError> {
    let avatar = db_avatars::get_meta(&state.db, &user_id)
        .await?
        .map(avatar_info);
    Ok(Json(APIResponse::ok(UserAvatarResponse {
        avatar,
        gravatar_enabled: state.avatars.gravatar_enabled(),
        user_id,
    })))
}

/// `PUT /api/v1/user/avatar[?expected_user_id=]` with a raw PNG, JPEG or WebP body.
pub async fn upload_avatar(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    query: Result<Query<AvatarWriteQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<APIResponse<AvatarInfo>>, AppError> {
    ensure_expected_user(&user_id, query)?;
    ensure_writable(&state.avatars, &user_id)?;
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            AppError::payload_too_large(MAX_AVATAR_UPLOAD_BYTES)
        } else {
            AppError::invalid_input("Could not read the request body")
        }
    })?;
    if body.len() > MAX_AVATAR_UPLOAD_BYTES {
        return Err(AppError::payload_too_large(MAX_AVATAR_UPLOAD_BYTES));
    }
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(ImageKind::from_content_type)
        .ok_or_else(|| {
            tracing::warn!(%user_id, reason = "content_type", "avatar upload rejected");
            AppError::unsupported_media_type()
        })?;
    let image = state
        .avatars
        .reencode(body, Some(declared))
        .await
        .map_err(|error| rejected(&user_id, "avatar upload rejected", error))?;
    store(&state, &user_id, &image, None).await
}

/// `PUT /api/v1/user/avatar/gravatar[?expected_user_id=]` with `{"gravatar": "<e-mail or avatar link>"}`.
pub async fn set_gravatar_avatar(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    query: Result<Query<AvatarWriteQuery>, QueryRejection>,
    body: Result<Json<SetGravatarRequest>, JsonRejection>,
) -> Result<Json<APIResponse<AvatarInfo>>, AppError> {
    if !state.avatars.gravatar_enabled() {
        return Err(AppError::feature_not_found());
    }
    ensure_expected_user(&user_id, query)?;
    ensure_writable(&state.avatars, &user_id)?;
    let Json(request) =
        body.map_err(|_| AppError::invalid_input("Expected a JSON body with a `gravatar` string"))?;
    let hash = gravatar::parse_gravatar_input(&request.gravatar).map_err(|error| {
        tracing::warn!(%user_id, reason = error.reason(), "gravatar input rejected");
        AppError::invalid_gravatar()
    })?;
    let image = state
        .avatars
        .import_gravatar(&hash)
        .await
        .map_err(|error| rejected(&user_id, "gravatar import failed", error))?;
    store(&state, &user_id, &image, Some(&hash)).await
}

/// `DELETE /api/v1/user/avatar[?expected_user_id=]`; idempotent.
pub async fn delete_avatar(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    query: Result<Query<AvatarWriteQuery>, QueryRejection>,
) -> Result<StatusCode, AppError> {
    ensure_expected_user(&user_id, query)?;
    ensure_writable(&state.avatars, &user_id)?;
    if db_avatars::delete(&state.db, &user_id).await? {
        announce_change(&state, &user_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/v1/user/avatar/image`: the caller's own image.
pub async fn get_my_avatar_image(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    headers: HeaderMap,
) -> Response {
    let lookup = db_avatars::get_image(&state.db, &user_id, &parse_if_none_match(&headers)).await;
    guard_errors(match lookup {
        Ok(Some(row)) => image_response(row),
        Ok(None) => Err(AppError::avatar_not_found()),
        Err(error) => Err(error.into()),
    })
}

/// `GET /api/v1/meetings/{meeting_id}/avatar?user_id=`: a co-participant's image.
pub async fn get_meeting_avatar_image(
    State(state): State<AppState>,
    ValidMeetingId(meeting_id): ValidMeetingId,
    RoomMember {
        user_id: viewer,
        meeting_id: token_meeting_id,
        ..
    }: RoomMember,
    query: Result<Query<MeetingAvatarQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let target = query.map(|Query(q)| q.user_id).unwrap_or_default();
    guard_errors(
        meeting_avatar(
            &state,
            &meeting_id,
            &token_meeting_id,
            &viewer,
            &target,
            &parse_if_none_match(&headers),
        )
        .await,
    )
}

async fn meeting_avatar(
    state: &AppState,
    meeting_id: &str,
    token_meeting_id: &str,
    viewer: &str,
    target: &str,
    if_none_match: &IfNoneMatch,
) -> Result<Response, AppError> {
    if token_meeting_id != meeting_id {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            APIError {
                code: "FORBIDDEN".to_string(),
                message: "Room token is not valid for this meeting".to_string(),
                engineering_error: None,
            },
        ));
    }
    if !state.avatars.allow_read(meeting_id, viewer) {
        return Err(AppError::avatar_rate_limited());
    }
    let target_safe = is_lookup_safe(target);
    let lookup: &[&str] = if target_safe && target != viewer {
        &[viewer, target]
    } else {
        &[viewer]
    };
    let room = db_participants::get_room_participants(&state.db, meeting_id, lookup)
        .await?
        .filter(|room| room.state.as_deref() != Some(db_meetings::STATE_ENDED))
        .ok_or_else(|| AppError::meeting_not_found(meeting_id))?;
    let row_of = |user: &str| room.rows.iter().find(|row| row.user_id == user);
    let member = |row: &&db_participants::ParticipantRow| {
        db_participants::in_current_instance(row, room.started_at)
    };
    row_of(viewer)
        .filter(member)
        .ok_or_else(AppError::not_in_meeting)?;
    if !target_safe {
        return Err(AppError::avatar_not_found());
    }
    if target != viewer {
        row_of(target)
            .filter(member)
            .ok_or_else(AppError::avatar_not_found)?;
    }
    let row = db_avatars::get_image(&state.db, target, if_none_match)
        .await?
        .ok_or_else(AppError::avatar_not_found)?;
    image_response(row)
}

fn ensure_expected_user(
    user_id: &str,
    query: Result<Query<AvatarWriteQuery>, QueryRejection>,
) -> Result<(), AppError> {
    let Query(AvatarWriteQuery { expected_user_id }) =
        query.map_err(|_| AppError::invalid_input("Malformed query string"))?;
    let Some(expected) = expected_user_id else {
        return Ok(());
    };
    if expected.len() > MAX_USER_ID_BYTES {
        return Err(AppError::invalid_input("expected_user_id is too long"));
    }
    if expected != user_id {
        tracing::debug!(%user_id, reason = "identity_mismatch", "avatar write rejected");
        return Err(AppError::avatar_identity_mismatch());
    }
    Ok(())
}

fn ensure_writable(avatars: &AvatarService, user_id: &str) -> Result<(), AppError> {
    if user_id.len() > MAX_USER_ID_BYTES {
        return Err(AppError::invalid_input("user id is too long"));
    }
    if !avatars.allow_write(user_id) {
        tracing::debug!(%user_id, reason = "rate_limited", "avatar write rejected");
        return Err(AppError::avatar_rate_limited());
    }
    Ok(())
}

fn rejected(user_id: &str, what: &'static str, error: AvatarError) -> AppError {
    tracing::warn!(%user_id, reason = error.reason(), "{what}");
    error.into()
}

async fn store(
    state: &AppState,
    user_id: &str,
    image: &[u8],
    gravatar_hash: Option<&GravatarHash>,
) -> Result<Json<APIResponse<AvatarInfo>>, AppError> {
    let source = if gravatar_hash.is_some() {
        AvatarSource::Gravatar
    } else {
        AvatarSource::Upload
    };
    let etag = etag_for(image);
    let row = db_avatars::upsert(
        &state.db,
        user_id,
        &NewAvatar {
            source,
            content_type: AVATAR_CONTENT_TYPE,
            image,
            gravatar_hash: gravatar_hash.map(GravatarHash::as_str),
            etag: &etag,
        },
    )
    .await?;
    announce_change(state, user_id).await;
    Ok(Json(APIResponse::ok(avatar_info(row))))
}

async fn announce_change(state: &AppState, user_id: &str) {
    if state.nats.is_none() {
        return;
    }
    let rooms = match db_participants::current_room_ids(&state.db, user_id).await {
        Ok(rooms) => rooms,
        Err(error) => {
            tracing::warn!(%user_id, %error, "avatar change not announced");
            return;
        }
    };
    for room_id in rooms {
        nats_events::publish_participant_avatar_changed(state.nats.as_ref(), &room_id, user_id)
            .await;
    }
}

fn avatar_info(row: AvatarMetaRow) -> AvatarInfo {
    AvatarInfo {
        source: row.source(),
        gravatar_url: row.gravatar_hash.as_deref().map(gravatar::public_url),
        content_type: row.content_type,
        etag: row.etag,
        updated_at: row.updated_at.timestamp_millis(),
    }
}

fn is_lookup_safe(user_id: &str) -> bool {
    !user_id.is_empty()
        && user_id.len() <= MAX_USER_ID_BYTES
        && !user_id.chars().any(char::is_control)
}

/// Parses every `If-None-Match` header: weak and strong tags compare equal, `*` matches anything.
pub fn parse_if_none_match(headers: &HeaderMap) -> IfNoneMatch {
    let mut parsed = IfNoneMatch::default();
    let values = headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .take(MAX_VALIDATORS);
    for tag in values {
        if tag == "*" {
            parsed.any = true;
        } else {
            let tag = tag.strip_prefix("W/").unwrap_or(tag);
            parsed.tags.push(tag.trim_matches('"').to_string());
        }
    }
    parsed
}

fn image_response(row: AvatarImageRow) -> Result<Response, AppError> {
    let header_value = |value: &str| {
        HeaderValue::from_str(value).map_err(|_| AppError::internal("unencodable avatar header"))
    };
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, header_value(&row.content_type)?);
    headers.insert(header::ETAG, header_value(&format!("\"{}\"", row.etag))?);
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    headers.insert(
        header::VARY,
        HeaderValue::from_static("Authorization, Cookie"),
    );
    insert_guard_headers(&mut headers);
    Ok(match row.image {
        Some(image) => (StatusCode::OK, headers, image).into_response(),
        None => (StatusCode::NOT_MODIFIED, headers).into_response(),
    })
}

fn insert_guard_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-site"),
    );
}

fn guard_errors(result: Result<Response, AppError>) -> Response {
    result.unwrap_or_else(|error| {
        let mut response = error.into_response();
        let headers = response.headers_mut();
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        insert_guard_headers(headers);
        response
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_inm(values: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn if_none_match_accepts_strong_weak_lists_and_wildcard() {
        assert_eq!(
            parse_if_none_match(&HeaderMap::new()),
            IfNoneMatch::default()
        );
        assert_eq!(parse_if_none_match(&with_inm(&["\"a\""])).tags, vec!["a"]);
        assert_eq!(parse_if_none_match(&with_inm(&["W/\"a\""])).tags, vec!["a"]);
        assert_eq!(
            parse_if_none_match(&with_inm(&["\"x\", W/\"a\"", "\"b\""])).tags,
            vec!["x", "a", "b"]
        );
        let any = parse_if_none_match(&with_inm(&["*"]));
        assert!(any.any && any.tags.is_empty());
    }

    #[test]
    fn if_none_match_is_bounded() {
        let many = vec!["\"t\""; 100].join(",");
        assert_eq!(
            parse_if_none_match(&with_inm(&[&many])).tags.len(),
            MAX_VALIDATORS
        );
    }

    #[test]
    fn lookup_ids_must_be_non_empty_bounded_and_printable() {
        assert!(is_lookup_safe("alice+tag@example.com"));
        assert!(is_lookup_safe("a/b|c:d%2F"));
        assert!(is_lookup_safe(&"x".repeat(MAX_USER_ID_BYTES)));
        for rejected in [
            String::new(),
            "x".repeat(MAX_USER_ID_BYTES + 1),
            "a\0b".to_string(),
            "a\nb".to_string(),
        ] {
            assert!(!is_lookup_safe(&rejected), "{rejected:?}");
        }
    }

    fn expected(query: &str) -> Result<(), AppError> {
        let uri: axum::http::Uri = format!("/api/v1/user/avatar{query}").parse().unwrap();
        ensure_expected_user("me@example.com", Query::try_from_uri(&uri))
    }

    #[test]
    #[tracing_test::traced_test]
    fn expected_user_id_is_compare_only_and_optional() {
        assert!(expected("").is_ok());
        assert!(expected("?other=1").is_ok());
        assert!(expected("?expected_user_id=me%40example.com").is_ok());
        for mismatch in [
            "?expected_user_id=someone-else%40example.com",
            "?expected_user_id=",
            "?expected_user_id=ME%40example.com",
        ] {
            let refused = expected(mismatch).expect_err(mismatch);
            assert_eq!(refused.status, StatusCode::CONFLICT, "{mismatch}");
            assert_eq!(refused.body.code, "AVATAR_IDENTITY_MISMATCH", "{mismatch}");
        }
        for invalid in [
            format!("?expected_user_id={}", "x".repeat(MAX_USER_ID_BYTES + 1)),
            "?expected_user_id=a&expected_user_id=b".to_string(),
        ] {
            let refused = expected(&invalid).expect_err(&invalid);
            assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{invalid}");
            assert_eq!(refused.body.code, "INVALID_INPUT", "{invalid}");
        }
        assert!(!logs_contain("someone-else"));
        logs_assert(|lines: &[&str]| {
            let rejected: Vec<_> = lines
                .iter()
                .filter(|l| l.contains("avatar write rejected"))
                .collect();
            match rejected.len() {
                3 if rejected
                    .iter()
                    .all(|l| l.contains("DEBUG") && l.contains("reason=\"identity_mismatch\"")) =>
                {
                    Ok(())
                }
                _ => Err(format!("{rejected:?}")),
            }
        });
    }

    #[test]
    #[tracing_test::traced_test]
    fn a_rate_limited_write_is_logged_at_debug() {
        let avatars = AvatarService::new(None);
        let refused = (0..100)
            .find_map(|_| ensure_writable(&avatars, "log-level@example.com").err())
            .expect("the write budget must run out");
        assert_eq!(refused.status, StatusCode::TOO_MANY_REQUESTS);
        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|l| l.contains("avatar write rejected"))
                .collect::<Vec<_>>()
                .as_slice()
            {
                [line] if line.contains("DEBUG") && line.contains("reason=\"rate_limited\"") => {
                    Ok(())
                }
                other => Err(format!("{other:?}")),
            }
        });
    }
}
