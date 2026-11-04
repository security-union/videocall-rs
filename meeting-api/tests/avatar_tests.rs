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

//! Per-user avatar routes (issue #3125) against a real database.

mod test_helpers;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{header, HeaderMap, Request, StatusCode};
use futures::StreamExt;
use meeting_api::avatar::image::etag_for;
use meeting_api::avatar::AvatarService;
use meeting_api::db::participants::{current_room_ids, MAX_CURRENT_ROOMS};
use meeting_api::state::AppState;
use meeting_api::token::{generate_observer_token, generate_room_token};
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::avatar::{
    AvatarInfo, AvatarSource, UserAvatarResponse, MAX_AVATAR_UPLOAD_BYTES,
};
use videocall_meeting_types::{APIError, APIResponse};
use videocall_types::protos::meeting_packet::{meeting_packet::MeetingEventType, MeetingPacket};
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PNG: &[u8] = include_bytes!("fixtures/avatars/ok-64.png");
const JPEG: &[u8] = include_bytes!("fixtures/avatars/ok-64.jpg");
const WEBP: &[u8] = include_bytes!("fixtures/avatars/ok-64.webp");
const WEBP_LOSSLESS: &[u8] = include_bytes!("fixtures/avatars/ok-64-lossless.webp");
const GIF: &[u8] = include_bytes!("fixtures/avatars/ok-64.gif");
const APNG: &[u8] = include_bytes!("fixtures/avatars/anim.png");
const ANIMATED_WEBP: &[u8] = include_bytes!("fixtures/avatars/anim.webp");
const BIG: &[u8] = include_bytes!("fixtures/avatars/big-1100.png");
const TINY: &[u8] = include_bytes!("fixtures/avatars/tiny-8.png");
const BOMB: &[u8] = include_bytes!("fixtures/avatars/bomb-20000.png");
const EXIF_ROTATED: &[u8] = include_bytes!("fixtures/avatars/exif-rot6.jpg");

const H64: &str = "16c18d336f0b250f0e2d907452ceb9658a74ecdae8bc94864c23122a72cc27a5";

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{}: {e}: {:?}", self.status, self.body))
    }

    fn code(&self) -> String {
        self.json::<APIResponse<APIError>>().result.code
    }

    fn info(&self) -> AvatarInfo {
        assert_eq!(self.status, StatusCode::OK, "{:?}", self.body);
        self.json::<APIResponse<AvatarInfo>>().result
    }

    fn header(&self, name: header::HeaderName) -> &str {
        self.headers
            .get(&name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .to_str()
            .unwrap()
    }
}

async fn setup(users: &[&str]) -> AppState {
    let state = build_state(get_test_pool().await, None, None);
    forget(&state.db, users).await;
    state
}

async fn forget(pool: &PgPool, users: &[&str]) {
    sqlx::query("DELETE FROM user_avatars WHERE user_id = ANY($1)")
        .bind(users.iter().map(|u| u.to_string()).collect::<Vec<_>>())
        .execute(pool)
        .await
        .unwrap();
}

async fn send(state: &AppState, request: Request<Body>) -> Reply {
    let response = build_app_from_state(state.clone())
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body,
    }
}

async fn upload(state: &AppState, user: &str, bytes: &[u8], content_type: Option<&str>) -> Reply {
    upload_at(state, "/api/v1/user/avatar", user, bytes, content_type).await
}

async fn upload_at(
    state: &AppState,
    uri: &str,
    user: &str,
    bytes: &[u8],
    content_type: Option<&str>,
) -> Reply {
    let mut request = request_with_cookie("PUT", uri, user);
    if let Some(content_type) = content_type {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    send(state, request.body(Body::from(bytes.to_vec())).unwrap()).await
}

async fn as_user(state: &AppState, method: &str, uri: &str, user: &str) -> Reply {
    send(
        state,
        request_with_cookie(method, uri, user)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn own_image(state: &AppState, user: &str, if_none_match: Option<&str>) -> Reply {
    let mut request = request_with_cookie("GET", "/api/v1/user/avatar/image", user);
    if let Some(tag) = if_none_match {
        request = request.header(header::IF_NONE_MATCH, tag);
    }
    send(state, request.body(Body::empty()).unwrap()).await
}

async fn set_gravatar(state: &AppState, user: &str, gravatar: &str) -> Reply {
    set_gravatar_at(state, "/api/v1/user/avatar/gravatar", user, gravatar).await
}

async fn set_gravatar_at(state: &AppState, uri: &str, user: &str, gravatar: &str) -> Reply {
    send(
        state,
        request_with_cookie("PUT", uri, user)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({ "gravatar": gravatar }).to_string(),
            ))
            .unwrap(),
    )
    .await
}

fn with_gravatar(state: &mut AppState, server: &MockServer) {
    state.avatars = Arc::new(AvatarService::new(Some(server.uri().parse().unwrap())));
}

fn room_token(user: &str, room: &str, is_guest: bool) -> String {
    generate_room_token(
        TEST_JWT_SECRET,
        600,
        user,
        room,
        false,
        user,
        true,
        is_guest,
    )
    .unwrap()
}

async fn meeting_image(state: &AppState, room: &str, token: Option<&str>, query: &str) -> Reply {
    let mut request = Request::builder()
        .method("GET")
        .uri(format!("/api/v1/meetings/{room}/avatar{query}"));
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    send(state, request.body(Body::empty()).unwrap()).await
}

fn user_query(user: &str) -> String {
    format!(
        "?user_id={}",
        url::form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>()
    )
}

fn expecting(user: &str) -> String {
    format!(
        "?expected_user_id={}",
        url::form_urlencoded::byte_serialize(user.as_bytes()).collect::<String>()
    )
}

fn my_avatar(reply: &Reply) -> UserAvatarResponse {
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    reply.json::<APIResponse<UserAvatarResponse>>().result
}

async fn peer_image(state: &AppState, room: &str, viewer: &str, target: &str) -> Reply {
    let token = room_token(viewer, room, viewer.starts_with("guest:"));
    meeting_image(state, room, Some(&token), &user_query(target)).await
}

async fn new_meeting(pool: &PgPool, room: &str, host: &str) -> i32 {
    cleanup_test_data(pool, room).await;
    meeting_api::db::meetings::create(pool, room, host, None, &serde_json::json!([]))
        .await
        .unwrap()
        .id
}

/// Seconds relative to the meeting's `started_at`; `None` leaves `admitted_at` NULL.
async fn member(pool: &PgPool, meeting: i32, user: &str, status: &str, admitted: Option<f64>) {
    sqlx::query(
        "INSERT INTO meeting_participants (meeting_id, user_id, status, is_guest, admitted_at) \
         VALUES ($1, $2, $3, $4, \
                 (SELECT started_at FROM meetings WHERE id = $1) + make_interval(secs => $5))",
    )
    .bind(meeting)
    .bind(user)
    .bind(status)
    .bind(user.starts_with("guest:"))
    .bind(admitted)
    .execute(pool)
    .await
    .unwrap();
}

const NOW: Option<f64> = Some(1.0);
const AT_START: Option<f64> = Some(0.0);
const PRIOR_INSTANCE: Option<f64> = Some(-3600.0);

async fn alter_meeting(pool: &PgPool, meeting: i32, change: &str) {
    sqlx::query(&format!("UPDATE meetings SET {change} WHERE id = $1"))
        .bind(meeting)
        .execute(pool)
        .await
        .unwrap();
}

fn assert_image_headers(reply: &Reply, etag: &str) {
    assert_eq!(reply.header(header::CONTENT_TYPE), "image/jpeg");
    assert_eq!(reply.header(header::ETAG), format!("\"{etag}\""));
    assert_eq!(reply.header(header::CACHE_CONTROL), "private, no-cache");
    assert_eq!(reply.header(header::VARY), "Authorization, Cookie");
    assert_eq!(reply.header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(
        reply.header(header::CONTENT_SECURITY_POLICY),
        "default-src 'none'; sandbox"
    );
    assert_eq!(
        reply.header(header::HeaderName::from_static(
            "cross-origin-resource-policy"
        )),
        "same-site"
    );
}

fn assert_error_headers(reply: &Reply) {
    assert_eq!(reply.header(header::CACHE_CONTROL), "no-store");
    assert_eq!(reply.header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert_eq!(
        reply.header(header::CONTENT_SECURITY_POLICY),
        "default-src 'none'; sandbox"
    );
    assert_eq!(
        reply.header(header::HeaderName::from_static(
            "cross-origin-resource-policy"
        )),
        "same-site"
    );
}

fn assert_no_avatar(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{:?}", reply.body);
    assert_eq!(reply.code(), "AVATAR_NOT_FOUND");
    assert_error_headers(reply);
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0xd8, 0xff])
}

#[tokio::test]
#[serial]
async fn an_upload_is_reencoded_and_served_back_to_its_owner() {
    let user = "avatar-owner@example.com";
    let state = setup(&[user]).await;

    let info = upload(&state, user, PNG, Some("image/png")).await.info();
    assert_eq!(info.source, AvatarSource::Upload);
    assert_eq!(info.content_type, "image/jpeg");
    assert_eq!(info.gravatar_url, None);
    assert_eq!(info.etag.len(), 32);

    let mine = as_user(&state, "GET", "/api/v1/user/avatar", user).await;
    let mine = mine.json::<APIResponse<UserAvatarResponse>>().result;
    assert_eq!(mine.avatar, Some(info.clone()));
    assert!(mine.gravatar_enabled);

    let image = own_image(&state, user, None).await;
    assert_eq!(image.status, StatusCode::OK);
    assert_image_headers(&image, &info.etag);
    assert!(is_jpeg(&image.body));
    assert_ne!(&image.body[..], PNG);
    assert_eq!(etag_for(&image.body), info.etag);

    for validator in [
        format!("\"{}\"", info.etag),
        format!("W/\"{}\"", info.etag),
        format!("\"stale\", \"{}\"", info.etag),
        "*".to_string(),
    ] {
        let revalidated = own_image(&state, user, Some(&validator)).await;
        assert_eq!(revalidated.status, StatusCode::NOT_MODIFIED, "{validator}");
        assert!(revalidated.body.is_empty());
        assert_image_headers(&revalidated, &info.etag);
    }
    let stale = own_image(&state, user, Some("\"0123456789abcdef0123456789abcdef\"")).await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(stale.body, image.body);

    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn every_accepted_format_uploads() {
    let user = "avatar-formats@example.com";
    let state = setup(&[user]).await;
    for (bytes, content_type) in [
        (JPEG, "image/jpeg"),
        (WEBP, "image/webp"),
        (WEBP_LOSSLESS, "image/webp"),
        (PNG, "image/png; charset=binary"),
        (PNG, "IMAGE/PNG"),
    ] {
        let info = upload(&state, user, bytes, Some(content_type)).await.info();
        assert_eq!(info.content_type, "image/jpeg", "{content_type}");
        assert!(is_jpeg(&own_image(&state, user, None).await.body));
    }
    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn writes_need_a_signed_in_user() {
    let state = setup(&[]).await;
    let anonymous = Request::builder()
        .method("PUT")
        .uri("/api/v1/user/avatar")
        .header(header::CONTENT_TYPE, "image/png")
        .body(Body::from(PNG))
        .unwrap();
    assert_eq!(
        send(&state, anonymous).await.status,
        StatusCode::UNAUTHORIZED
    );

    let guest = upload(&state, "guest:6f1d2c3b", PNG, Some("image/png")).await;
    assert_eq!(guest.status, StatusCode::UNAUTHORIZED);

    for method in ["PUT", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri("/api/v1/user/avatar")
            .header(
                header::AUTHORIZATION,
                format!(
                    "Bearer {}",
                    room_token("avatar-x@example.com", "avatar-room", false)
                ),
            )
            .header(header::CONTENT_TYPE, "image/png")
            .body(Body::from(PNG))
            .unwrap();
        assert_eq!(
            send(&state, request).await.status,
            StatusCode::UNAUTHORIZED,
            "a room token is not a user session ({method})"
        );
    }
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_avatars WHERE user_id LIKE 'guest:%' OR user_id = 'avatar-x@example.com'")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

#[tokio::test]
#[serial]
async fn an_overlong_user_id_is_refused_before_the_database() {
    let state = setup(&[]).await;
    let user = format!("{}@example.com", "a".repeat(250));
    for reply in [
        upload(&state, &user, PNG, Some("image/png")).await,
        as_user(&state, "DELETE", "/api/v1/user/avatar", &user).await,
    ] {
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{:?}", reply.body);
        assert_eq!(reply.code(), "INVALID_INPUT");
    }
}

#[tokio::test]
#[serial]
async fn the_declared_type_must_be_an_accepted_image_type_matching_the_bytes() {
    let user = "avatar-types@example.com";
    let state = setup(&[user]).await;
    for (bytes, content_type) in [
        (PNG, Some("image/gif")),
        (PNG, None),
        (PNG, Some("multipart/form-data; boundary=x")),
        (PNG, Some("text/plain")),
        (PNG, Some("application/x-www-form-urlencoded")),
        (PNG, Some("image/svg+xml")),
        (JPEG, Some("image/png")),
        (WEBP, Some("image/jpeg")),
        (GIF, Some("image/png")),
    ] {
        let reply = upload(&state, user, bytes, content_type).await;
        assert_eq!(
            reply.status,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "{content_type:?}"
        );
        assert_eq!(reply.code(), "UNSUPPORTED_MEDIA_TYPE");
    }
    assert_no_avatar(&own_image(&state, user, None).await);
}

#[tokio::test]
#[serial]
async fn bytes_that_are_not_a_usable_image_are_422() {
    let user = "avatar-invalid@example.com";
    let state = setup(&[user]).await;
    for bytes in [
        &b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script></svg>"[..],
        b"<!DOCTYPE html><html><script>alert(1)</script></html>",
        &PNG[..PNG.len() / 2],
        BIG,
        TINY,
        BOMB,
    ] {
        let reply = upload(&state, user, bytes, Some("image/png")).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.code(), "INVALID_IMAGE");
    }
    assert_no_avatar(&own_image(&state, user, None).await);
}

#[tokio::test]
#[serial]
async fn an_oversize_body_is_a_413_envelope() {
    let user = "avatar-oversize@example.com";
    let state = setup(&[user]).await;
    let mut oversize = PNG.to_vec();
    oversize.resize(MAX_AVATAR_UPLOAD_BYTES + 1, 0);
    let reply = upload(&state, user, &oversize, Some("image/png")).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.code(), "PAYLOAD_TOO_LARGE");

    oversize.truncate(MAX_AVATAR_UPLOAD_BYTES);
    let at_limit = upload(&state, user, &oversize, Some("image/png")).await;
    assert_eq!(
        at_limit.status,
        StatusCode::OK,
        "trailing bytes after IEND are dropped by the re-encode: {:?}",
        at_limit.body
    );
    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn animation_metadata_and_trailing_payloads_do_not_survive() {
    let user = "avatar-sanitized@example.com";
    let state = setup(&[user]).await;
    let mut polyglot = PNG.to_vec();
    polyglot.extend_from_slice(b"<script>alert(document.cookie)</script>");
    for (bytes, content_type) in [
        (APNG, "image/png"),
        (ANIMATED_WEBP, "image/webp"),
        (&polyglot[..], "image/png"),
        (EXIF_ROTATED, "image/jpeg"),
    ] {
        upload(&state, user, bytes, Some(content_type)).await.info();
        let stored = own_image(&state, user, None).await.body;
        assert!(is_jpeg(&stored));
        assert!(!stored.windows(7).any(|w| w == b"<script"));
        assert!(!stored.windows(4).any(|w| w == b"Exif"));
        let decoded = image::load_from_memory_with_format(&stored, image::ImageFormat::Jpeg)
            .expect("a single still JPEG");
        assert!(decoded.width() == decoded.height() && decoded.width() <= 256);
    }
    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn a_new_upload_replaces_the_old_one() {
    let user = "avatar-replace@example.com";
    let state = setup(&[user]).await;
    let first = upload(&state, user, PNG, Some("image/png")).await.info();
    let second = upload(&state, user, WEBP, Some("image/webp")).await.info();
    assert_ne!(first.etag, second.etag);
    assert!(second.updated_at >= first.updated_at);
    let image = own_image(&state, user, None).await;
    assert_eq!(etag_for(&image.body), second.etag);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_avatars WHERE user_id = $1")
        .bind(user)
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert_eq!(count, 1);
    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn delete_is_idempotent_and_hides_the_avatar_everywhere() {
    let (host, attendee) = ("avatar-del-host@example.com", "avatar-del-att@example.com");
    let room = "avatar-delete-room";
    let state = setup(&[host, attendee]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    member(&state.db, meeting, attendee, "admitted", NOW).await;
    upload(&state, attendee, PNG, Some("image/png"))
        .await
        .info();
    assert_eq!(
        peer_image(&state, room, host, attendee).await.status,
        StatusCode::OK
    );

    for _ in 0..2 {
        let deleted = as_user(&state, "DELETE", "/api/v1/user/avatar", attendee).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    }
    let mine = as_user(&state, "GET", "/api/v1/user/avatar", attendee).await;
    assert_eq!(
        mine.json::<APIResponse<UserAvatarResponse>>().result.avatar,
        None
    );
    assert_no_avatar(&own_image(&state, attendee, None).await);
    assert_no_avatar(&peer_image(&state, room, host, attendee).await);
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn uploads_gravatar_saves_and_deletes_share_one_budget_per_user() {
    let (alice, bob) = ("avatar-budget-a@example.com", "avatar-budget-b@example.com");
    let state = setup(&[alice, bob]).await;
    for _ in 0..10 {
        let deleted = as_user(&state, "DELETE", "/api/v1/user/avatar", alice).await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    }
    let upload_reply = upload(&state, alice, PNG, Some("image/png")).await;
    assert_eq!(upload_reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(upload_reply.code(), "RATE_LIMIT_EXCEEDED");
    let gravatar = set_gravatar(&state, alice, "alice@example.com").await;
    assert_eq!(gravatar.status, StatusCode::TOO_MANY_REQUESTS);
    let delete = as_user(&state, "DELETE", "/api/v1/user/avatar", alice).await;
    assert_eq!(delete.status, StatusCode::TOO_MANY_REQUESTS);
    assert_no_avatar(&own_image(&state, alice, None).await);

    upload(&state, bob, PNG, Some("image/png")).await.info();
    forget(&state.db, &[alice, bob]).await;
}

#[tokio::test]
#[serial]
async fn a_write_expecting_another_user_is_409_before_any_write_or_budget() {
    let (tab_owner, signed_in) = ("avatar-id-owner@example.com", "avatar-id-other@example.com");
    let state = setup(&[tab_owner, signed_in]).await;
    let owner_before = upload(&state, tab_owner, PNG, Some("image/png"))
        .await
        .info();
    let signed_in_before = upload(&state, signed_in, JPEG, Some("image/jpeg"))
        .await
        .info();
    let query = expecting(tab_owner);
    let avatar_uri = format!("/api/v1/user/avatar{query}");
    let gravatar_uri = format!("/api/v1/user/avatar/gravatar{query}");

    for _ in 0..11 {
        for reply in [
            upload_at(&state, &avatar_uri, signed_in, WEBP, Some("image/webp")).await,
            set_gravatar_at(&state, &gravatar_uri, signed_in, "x@example.com").await,
            as_user(&state, "DELETE", &avatar_uri, signed_in).await,
        ] {
            assert_eq!(reply.status, StatusCode::CONFLICT, "{:?}", reply.body);
            assert_eq!(reply.code(), "AVATAR_IDENTITY_MISMATCH");
        }
    }

    for (user, before) in [(tab_owner, &owner_before), (signed_in, &signed_in_before)] {
        let mine = my_avatar(&as_user(&state, "GET", "/api/v1/user/avatar", user).await);
        assert_eq!(mine.user_id, user);
        assert_eq!(mine.avatar.as_ref(), Some(before), "{user}");
        assert_eq!(
            etag_for(&own_image(&state, user, None).await.body),
            before.etag
        );
    }
    upload_at(
        &state,
        &format!("/api/v1/user/avatar{}", expecting(signed_in)),
        signed_in,
        WEBP,
        Some("image/webp"),
    )
    .await
    .info();
    forget(&state.db, &[tab_owner, signed_in]).await;
}

#[tokio::test]
#[serial]
async fn a_write_expecting_the_signed_in_user_goes_through() {
    let user = "avatar-id-self@example.com";
    let mut state = setup(&[user]).await;
    let server = gravatar_stub(H64, ResponseTemplate::new(200).set_body_bytes(PNG), 1).await;
    with_gravatar(&mut state, &server);
    let query = expecting(user);

    let uploaded = upload_at(
        &state,
        &format!("/api/v1/user/avatar{query}"),
        user,
        PNG,
        Some("image/png"),
    )
    .await
    .info();
    let mine = my_avatar(&as_user(&state, "GET", "/api/v1/user/avatar", user).await);
    assert_eq!(mine.user_id, user);
    assert_eq!(mine.avatar, Some(uploaded));

    let gravatar = set_gravatar_at(
        &state,
        &format!("/api/v1/user/avatar/gravatar{query}"),
        user,
        "myemail@example.com",
    )
    .await
    .info();
    assert_eq!(gravatar.source, AvatarSource::Gravatar);

    let deleted = as_user(
        &state,
        "DELETE",
        &format!("/api/v1/user/avatar{query}"),
        user,
    )
    .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_no_avatar(&own_image(&state, user, None).await);
}

#[tokio::test]
#[serial]
async fn co_participants_and_guests_can_read_a_members_avatar() {
    let (host, attendee) = (
        "avatar-read-host@example.com",
        "avatar-read-att@example.com",
    );
    let guest = "guest:0b9e1f7a-avatar";
    let room = "avatar-read-room";
    let state = setup(&[host, attendee]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    member(&state.db, meeting, attendee, "admitted", NOW).await;
    member(&state.db, meeting, guest, "admitted", NOW).await;
    let info = upload(&state, attendee, PNG, Some("image/png"))
        .await
        .info();
    let own = own_image(&state, attendee, None).await;

    for viewer in [host, guest, attendee] {
        let reply = peer_image(&state, room, viewer, attendee).await;
        assert_eq!(reply.status, StatusCode::OK, "{viewer}: {:?}", reply.body);
        assert_image_headers(&reply, &info.etag);
        assert_eq!(reply.body, own.body);
    }

    let token = room_token(host, room, false);
    let revalidated = send(
        &state,
        Request::builder()
            .uri(format!(
                "/api/v1/meetings/{room}/avatar{}",
                user_query(attendee)
            ))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::IF_NONE_MATCH, format!("\"{}\"", info.etag))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(revalidated.status, StatusCode::NOT_MODIFIED);
    assert_image_headers(&revalidated, &info.etag);

    forget(&state.db, &[host, attendee]).await;
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn a_room_token_only_reads_its_own_meeting() {
    let (host, attendee) = ("avatar-x-host@example.com", "avatar-x-att@example.com");
    let state = setup(&[host, attendee]).await;
    for room in ["avatar-room-a", "avatar-room-b"] {
        let meeting = new_meeting(&state.db, room, host).await;
        member(&state.db, meeting, host, "admitted", NOW).await;
        member(&state.db, meeting, attendee, "admitted", NOW).await;
    }
    upload(&state, attendee, PNG, Some("image/png"))
        .await
        .info();
    let token_for_a = room_token(host, "avatar-room-a", false);
    let reply = meeting_image(
        &state,
        "avatar-room-b",
        Some(&token_for_a),
        &user_query(attendee),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "FORBIDDEN");
    assert_error_headers(&reply);

    forget(&state.db, &[host, attendee]).await;
    cleanup_test_data(&state.db, "avatar-room-a").await;
    cleanup_test_data(&state.db, "avatar-room-b").await;
}

#[tokio::test]
#[serial]
async fn readers_must_be_in_the_current_instance() {
    let (host, target) = ("avatar-v-host@example.com", "avatar-v-target@example.com");
    let room = "avatar-viewer-room";
    let state = setup(&[host, target]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    member(&state.db, meeting, target, "admitted", NOW).await;
    upload(&state, target, PNG, Some("image/png")).await.info();

    let viewers = [
        ("avatar-v-kicked@example.com", Some(("kicked", NOW))),
        ("avatar-v-prior@example.com", Some(("left", PRIOR_INSTANCE))),
        ("avatar-v-waiting@example.com", Some(("waiting", None))),
        ("avatar-v-rejected@example.com", Some(("rejected", None))),
        ("avatar-v-stranger@example.com", None),
    ];
    for (viewer, row) in viewers {
        if let Some((status, admitted)) = row {
            member(&state.db, meeting, viewer, status, admitted).await;
        }
        let reply = peer_image(&state, room, viewer, target).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{viewer}");
        assert_eq!(reply.code(), "NOT_IN_MEETING", "{viewer}");
    }

    let leaver = "avatar-v-leaver@example.com";
    member(&state.db, meeting, leaver, "left", NOW).await;
    assert_eq!(
        peer_image(&state, room, leaver, target).await.status,
        StatusCode::OK
    );

    forget(&state.db, &[host, target]).await;
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn targets_outside_the_current_instance_look_exactly_like_no_avatar() {
    let host = "avatar-t-host@example.com";
    let room = "avatar-target-room";
    let targets = [
        ("avatar-t-kicked@example.com", Some(("kicked", NOW))),
        ("avatar-t-prior@example.com", Some(("left", PRIOR_INSTANCE))),
        ("avatar-t-waiting@example.com", Some(("waiting", None))),
        ("avatar-t-rejected@example.com", Some(("rejected", None))),
        ("avatar-t-outsider@example.com", None),
    ];
    let bare = "avatar-t-bare@example.com";
    let mut everyone: Vec<&str> = targets.iter().map(|(user, _)| *user).collect();
    everyone.extend([host, bare]);
    let state = setup(&everyone).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    member(&state.db, meeting, bare, "admitted", NOW).await;

    let baseline = peer_image(&state, room, host, bare).await;
    assert_no_avatar(&baseline);
    for (target, row) in targets {
        if let Some((status, admitted)) = row {
            member(&state.db, meeting, target, status, admitted).await;
        }
        upload(&state, target, PNG, Some("image/png")).await.info();
        let reply = peer_image(&state, room, host, target).await;
        assert_no_avatar(&reply);
        assert_eq!(reply.body, baseline.body, "{target}");
    }

    let leaver = "avatar-t-leaver@example.com";
    forget(&state.db, &[leaver]).await;
    member(&state.db, meeting, leaver, "left", NOW).await;
    upload(&state, leaver, PNG, Some("image/png")).await.info();
    assert_eq!(
        peer_image(&state, room, host, leaver).await.status,
        StatusCode::OK
    );

    everyone.push(leaver);
    forget(&state.db, &everyone).await;
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn observer_tokens_and_missing_tokens_are_401() {
    let host = "avatar-o-host@example.com";
    let room = "avatar-observer-room";
    let state = setup(&[host]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    upload(&state, host, PNG, Some("image/png")).await.info();

    let observer =
        generate_observer_token(TEST_JWT_SECRET, "guest:obs", room, "Obs", true).unwrap();
    for token in [None, Some(observer.as_str()), Some("not-a-jwt")] {
        let reply = meeting_image(&state, room, token, &user_query(host)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{token:?}");
    }
    let session = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room}/avatar{}", user_query(host)),
        host,
    )
    .body(Body::empty())
    .unwrap();
    assert_eq!(send(&state, session).await.status, StatusCode::UNAUTHORIZED);

    forget(&state.db, &[host]).await;
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn user_ids_are_matched_byte_for_byte() {
    let host = "avatar-id-host@example.com";
    let odd = "avatar+tag|x/y:z%2F..@example.com";
    let room = "avatar-id-room";
    let state = setup(&[host, odd]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    member(&state.db, meeting, odd, "admitted", NOW).await;
    upload(&state, odd, PNG, Some("image/png")).await.info();

    assert_eq!(
        peer_image(&state, room, host, odd).await.status,
        StatusCode::OK
    );
    for near_miss in [
        "avatar tag|x/y:z%2F..@example.com",
        "avatar+tag|x/y:z/..@example.com",
        "AVATAR+tag|x/y:z%2F..@example.com",
        " avatar+tag|x/y:z%2F..@example.com",
    ] {
        assert_no_avatar(&peer_image(&state, room, host, near_miss).await);
    }

    forget(&state.db, &[host, odd]).await;
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn malformed_user_ids_are_404_not_500() {
    let host = "avatar-bad-host@example.com";
    let room = "avatar-bad-room";
    let state = setup(&[host]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    let token = room_token(host, room, false);
    let long = format!("?user_id={}", "a".repeat(256));
    for query in [
        "",
        "?user_id=",
        "?user_id=%00",
        "?user_id=a%00b",
        "?user_id=a%0Ab",
        "?user_id=%FF",
        "?user_id=a&user_id=b",
        long.as_str(),
    ] {
        let reply = meeting_image(&state, room, Some(&token), query).await;
        assert_no_avatar(&reply);
    }
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn ended_or_unknown_meetings_serve_no_avatars() {
    let host = "avatar-end-host@example.com";
    let room = "avatar-ended-room";
    let state = setup(&[host]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    upload(&state, host, PNG, Some("image/png")).await.info();
    sqlx::query("UPDATE meetings SET state = 'ended' WHERE id = $1")
        .bind(meeting)
        .execute(&state.db)
        .await
        .unwrap();
    for target_room in [room, "avatar-no-such-room"] {
        let token = room_token(host, target_room, false);
        let reply = meeting_image(&state, target_room, Some(&token), &user_query(host)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.code(), "MEETING_NOT_FOUND");
    }
    let invalid = meeting_image(
        &state,
        "bad%20room",
        Some(&room_token(host, "bad room", false)),
        &user_query(host),
    )
    .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.code(), "INVALID_MEETING_ID");

    forget(&state.db, &[host]).await;
    cleanup_test_data(&state.db, room).await;
}

#[tokio::test]
#[serial]
async fn meeting_reads_are_budgeted_per_viewer() {
    let (host, attendee) = ("avatar-rl-host@example.com", "avatar-rl-att@example.com");
    let room = "avatar-read-limit-room";
    let state = setup(&[host, attendee]).await;
    let meeting = new_meeting(&state.db, room, host).await;
    member(&state.db, meeting, host, "admitted", NOW).await;
    member(&state.db, meeting, attendee, "admitted", NOW).await;
    upload(&state, host, PNG, Some("image/png")).await.info();

    let mut last = StatusCode::OK;
    for _ in 0..300 {
        last = peer_image(&state, room, attendee, host).await.status;
        if last != StatusCode::OK {
            break;
        }
    }
    assert_eq!(last, StatusCode::OK, "300 reads per window are allowed");
    let limited = peer_image(&state, room, attendee, host).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.code(), "RATE_LIMIT_EXCEEDED");
    assert_eq!(
        peer_image(&state, room, host, host).await.status,
        StatusCode::OK
    );

    forget(&state.db, &[host, attendee]).await;
    cleanup_test_data(&state.db, room).await;
}

async fn gravatar_stub(hash: &str, response: ResponseTemplate, expected_calls: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/avatar/{hash}")))
        .and(query_param("s", "256"))
        .and(query_param("d", "404"))
        .and(query_param("r", "g"))
        .respond_with(response)
        .expect(expected_calls)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
#[serial]
async fn a_gravatar_link_is_fetched_once_and_stored_as_a_copy() {
    let user = "avatar-g-link@example.com";
    let mut state = setup(&[user]).await;
    let server = gravatar_stub(H64, ResponseTemplate::new(200).set_body_bytes(PNG), 1).await;
    with_gravatar(&mut state, &server);

    let pasted = format!(
        "https://www.gravatar.com/avatar/{}.png?s=80&d=https://evil.example",
        H64.to_uppercase()
    );
    let info = set_gravatar(&state, user, &pasted).await.info();
    assert_eq!(info.source, AvatarSource::Gravatar);
    assert_eq!(
        info.gravatar_url.as_deref(),
        Some(format!("https://gravatar.com/avatar/{H64}").as_str())
    );
    assert_eq!(info.content_type, "image/jpeg");

    let mine = as_user(&state, "GET", "/api/v1/user/avatar", user).await;
    assert_eq!(
        mine.json::<APIResponse<UserAvatarResponse>>().result.avatar,
        Some(info.clone())
    );
    let image = own_image(&state, user, None).await;
    assert!(is_jpeg(&image.body));
    assert_eq!(etag_for(&image.body), info.etag);
    let stored_hash: Option<String> =
        sqlx::query_scalar("SELECT gravatar_hash FROM user_avatars WHERE user_id = $1")
            .bind(user)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert_eq!(stored_hash.as_deref(), Some(H64));

    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn a_gravatar_email_is_hashed_before_the_fetch() {
    let user = "avatar-g-email@example.com";
    let mut state = setup(&[user]).await;
    let server = gravatar_stub(H64, ResponseTemplate::new(200).set_body_bytes(JPEG), 1).await;
    with_gravatar(&mut state, &server);
    let info = set_gravatar(&state, user, " MyEmail@Example.com ")
        .await
        .info();
    assert_eq!(
        info.gravatar_url.as_deref(),
        Some(format!("https://gravatar.com/avatar/{H64}").as_str())
    );
    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn gravatar_failures_map_to_documented_errors_and_keep_the_previous_avatar() {
    let user = "avatar-g-fail@example.com";
    let mut state = setup(&[user]).await;
    let previous = upload(&state, user, PNG, Some("image/png")).await.info();

    let redirect_target = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
        .expect(0)
        .mount(&redirect_target)
        .await;
    let cases = [
        (
            ResponseTemplate::new(404),
            StatusCode::UNPROCESSABLE_ENTITY,
            "GRAVATAR_NOT_FOUND",
        ),
        (
            ResponseTemplate::new(500),
            StatusCode::BAD_GATEWAY,
            "GRAVATAR_UNAVAILABLE",
        ),
        (
            ResponseTemplate::new(302).insert_header(
                "location",
                format!("{}/avatar/{H64}", redirect_target.uri()),
            ),
            StatusCode::BAD_GATEWAY,
            "GRAVATAR_UNAVAILABLE",
        ),
        (
            ResponseTemplate::new(200).set_body_raw("<html>hi</html>", "text/html"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_IMAGE",
        ),
        (
            ResponseTemplate::new(200).set_body_bytes(GIF),
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_IMAGE",
        ),
        (
            ResponseTemplate::new(200).set_body_bytes(vec![0u8; MAX_AVATAR_UPLOAD_BYTES + 1]),
            StatusCode::UNPROCESSABLE_ENTITY,
            "INVALID_IMAGE",
        ),
    ];
    for (response, status, code) in cases {
        let server = gravatar_stub(H64, response, 1).await;
        with_gravatar(&mut state, &server);
        let reply = set_gravatar(&state, user, &format!("gravatar.com/avatar/{H64}")).await;
        assert_eq!((reply.status, reply.code().as_str()), (status, code));
        server.verify().await;
    }
    let image = own_image(&state, user, None).await;
    assert_eq!(
        etag_for(&image.body),
        previous.etag,
        "a failed import must not replace the avatar"
    );
    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn invalid_gravatar_input_never_reaches_the_network() {
    let user = "avatar-g-bad@example.com";
    let mut state = setup(&[user]).await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
        .expect(0)
        .mount(&server)
        .await;
    with_gravatar(&mut state, &server);
    for input in [
        format!("https://evilgravatar.com/avatar/{H64}"),
        format!("https://gravatar.com.evil.io/avatar/{H64}"),
        format!("https://127.0.0.1/avatar/{H64}"),
        "https://gravatar.com/my-profile".to_string(),
        "not a gravatar".to_string(),
        String::new(),
    ] {
        let reply = set_gravatar(&state, user, &input).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{input}");
        assert_eq!(reply.code(), "INVALID_GRAVATAR", "{input}");
    }
    for (content_type, body) in [
        ("application/json", "{\"gravatar\":"),
        ("application/json", "{\"url\":\"x\"}"),
        ("text/plain", "{\"gravatar\":\"me@example.com\"}"),
    ] {
        let reply = send(
            &state,
            request_with_cookie("PUT", "/api/v1/user/avatar/gravatar", user)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reply.code(), "INVALID_INPUT", "{body}");
    }
    assert_no_avatar(&own_image(&state, user, None).await);
}

#[tokio::test]
#[serial]
async fn the_gravatar_kill_switch_hides_and_refuses_the_source() {
    let user = "avatar-g-off@example.com";
    let mut state = setup(&[user]).await;
    state.avatars = Arc::new(AvatarService::new(None));
    let mine = as_user(&state, "GET", "/api/v1/user/avatar", user).await;
    assert!(
        !mine
            .json::<APIResponse<UserAvatarResponse>>()
            .result
            .gravatar_enabled
    );
    let reply = set_gravatar(&state, user, "me@example.com").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.code(), "NOT_FOUND");
    upload(&state, user, PNG, Some("image/png")).await.info();
    forget(&state.db, &[user]).await;
}

#[tokio::test]
#[serial]
async fn the_table_rejects_rows_the_api_never_writes() {
    let pool = get_test_pool().await;
    let user = "avatar-db@example.com";
    forget(&pool, &[user, "guest:avatar-db"]).await;
    let insert = |user: &'static str,
                  source: &'static str,
                  image: Vec<u8>,
                  hash: Option<&'static str>,
                  content_type: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO user_avatars (user_id, source, content_type, image, gravatar_hash, etag) \
                 VALUES ($1, $2, $3, $4, $5, 'e')",
            )
            .bind(user)
            .bind(source)
            .bind(content_type)
            .bind(image)
            .bind(hash)
            .execute(&pool)
            .await
        }
    };
    let ok_hash = Some("0123456789abcdef0123456789abcdef");
    let rejected = [
        (
            insert(user, "bogus", vec![1], None, "image/jpeg").await,
            "user_avatars_source_check",
        ),
        (
            insert(user, "gravatar", vec![1], None, "image/jpeg").await,
            "chk_user_avatars_gravatar_hash",
        ),
        (
            insert(user, "upload", vec![1], ok_hash, "image/jpeg").await,
            "chk_user_avatars_gravatar_hash",
        ),
        (
            insert(
                user,
                "gravatar",
                vec![1],
                Some("0123456789ABCDEF0123456789ABCDEF"),
                "image/jpeg",
            )
            .await,
            "user_avatars_gravatar_hash_check",
        ),
        (
            insert(user, "upload", vec![1], None, "image/gif").await,
            "user_avatars_content_type_check",
        ),
        (
            insert(user, "upload", vec![], None, "image/jpeg").await,
            "user_avatars_image_check",
        ),
        (
            insert(user, "upload", vec![0; 65_537], None, "image/jpeg").await,
            "user_avatars_image_check",
        ),
        (
            insert("guest:avatar-db", "upload", vec![1], None, "image/jpeg").await,
            "user_avatars_user_id_check",
        ),
    ];
    for (i, (result, constraint)) in rejected.iter().enumerate() {
        let error = result
            .as_ref()
            .err()
            .and_then(|error| error.as_database_error())
            .unwrap_or_else(|| panic!("case {i} must violate {constraint}: {result:?}"));
        assert_eq!(error.code().as_deref(), Some("23514"), "case {i}: {error}");
        assert_eq!(error.constraint(), Some(*constraint), "case {i}: {error}");
    }
    assert!(insert(user, "upload", vec![0; 65_536], None, "image/jpeg")
        .await
        .is_ok());
    assert!(insert(
        "avatar-db2@example.com",
        "gravatar",
        vec![1],
        ok_hash,
        "image/jpeg"
    )
    .await
    .is_ok());
    forget(&pool, &[user, "avatar-db2@example.com"]).await;
}

#[tokio::test]
#[serial]
async fn the_rooms_told_of_an_avatar_change_are_the_rooms_that_can_read_it() {
    let (host, user) = ("avatar-ev-host@example.com", "avatar-ev-user@example.com");
    let state = setup(&[host, user]).await;
    let cases = [
        ("avatar-ev-admitted", Some(("admitted", NOW)), None, true),
        (
            "avatar-ev-active",
            Some(("admitted", NOW)),
            Some("state = 'active'"),
            true,
        ),
        ("avatar-ev-left", Some(("left", NOW)), None, true),
        (
            "avatar-ev-left-at-start",
            Some(("left", AT_START)),
            None,
            true,
        ),
        ("avatar-ev-waiting", Some(("waiting", None)), None, false),
        ("avatar-ev-rejected", Some(("rejected", None)), None, false),
        ("avatar-ev-kicked", Some(("kicked", NOW)), None, false),
        (
            "avatar-ev-prior",
            Some(("left", PRIOR_INSTANCE)),
            None,
            false,
        ),
        (
            "avatar-ev-ended",
            Some(("admitted", NOW)),
            Some("state = 'ended'"),
            false,
        ),
        (
            "avatar-ev-deleted",
            Some(("admitted", NOW)),
            Some("deleted_at = NOW()"),
            false,
        ),
        ("avatar-ev-elsewhere", None, None, false),
    ];
    for (room, row, change, _) in cases {
        let meeting = new_meeting(&state.db, room, host).await;
        member(&state.db, meeting, host, "admitted", NOW).await;
        if let Some((status, admitted)) = row {
            member(&state.db, meeting, user, status, admitted).await;
        }
        if let Some(change) = change {
            alter_meeting(&state.db, meeting, change).await;
        }
    }
    upload(&state, user, PNG, Some("image/png")).await.info();

    let mut rooms = current_room_ids(&state.db, user).await.unwrap();
    rooms.sort();
    assert_eq!(
        rooms,
        [
            "avatar-ev-active",
            "avatar-ev-admitted",
            "avatar-ev-left",
            "avatar-ev-left-at-start"
        ]
    );
    for (room, _, _, included) in cases {
        let readable = peer_image(&state, room, host, user).await.status == StatusCode::OK;
        assert_eq!(readable, included, "{room}");
    }

    forget(&state.db, &[host, user]).await;
    for (room, ..) in cases {
        cleanup_test_data(&state.db, room).await;
    }
}

#[tokio::test]
#[serial]
async fn the_room_lookup_is_bounded_and_keeps_admitted_then_recent_rooms() {
    let pool = get_test_pool().await;
    let (host, user) = ("avatar-cap-host@example.com", "avatar-cap-user@example.com");
    let rooms: Vec<String> = (0..=MAX_CURRENT_ROOMS)
        .map(|i| format!("avatar-cap-{i}"))
        .collect();
    let admitted_room = rooms.len() - 1;
    for (i, room) in rooms.iter().enumerate() {
        let meeting = new_meeting(&pool, room, host).await;
        let (status, secs) = if i == admitted_room {
            ("admitted", 1.0)
        } else {
            ("left", 10.0 + i as f64)
        };
        member(&pool, meeting, user, status, Some(secs)).await;
    }

    let mut found = current_room_ids(&pool, user).await.unwrap();
    found.sort();
    let mut expected = rooms[1..].to_vec();
    expected.sort();
    assert_eq!(found, expected);

    for room in &rooms {
        cleanup_test_data(&pool, room).await;
    }
}

async fn avatar_events(
    sub: &mut async_nats::Subscriber,
    user: &str,
    window: Duration,
) -> BTreeMap<String, usize> {
    let deadline = tokio::time::Instant::now() + window;
    let mut seen = BTreeMap::new();
    while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, sub.next()).await {
        let Ok(wrapper) = <PacketWrapper as protobuf::Message>::parse_from_bytes(&msg.payload)
        else {
            continue;
        };
        let Ok(packet) = <MeetingPacket as protobuf::Message>::parse_from_bytes(&wrapper.data)
        else {
            continue;
        };
        if packet.event_type == MeetingEventType::PARTICIPANT_AVATAR_CHANGED.into()
            && packet.target_user_id == user.as_bytes()
        {
            assert_eq!(
                msg.subject.to_string(),
                format!("room.{}.system", packet.room_id)
            );
            *seen.entry(packet.room_id).or_default() += 1;
        }
    }
    seen
}

#[tokio::test]
#[serial]
async fn avatar_writes_announce_the_change_once_per_current_room() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping avatar change publish test");
        return;
    };
    let (host, user) = ("avatar-pub-host@example.com", "avatar-pub-user@example.com");
    let rooms = [
        ("avatar-pub-admitted", "admitted", NOW, None),
        (
            "avatar-pub-active",
            "admitted",
            NOW,
            Some("state = 'active'"),
        ),
        ("avatar-pub-left", "left", NOW, None),
        ("avatar-pub-left-at-start", "left", AT_START, None),
        ("avatar-pub-waiting", "waiting", None, None),
    ];
    let mut state = setup(&[user]).await;
    state.nats = Some(nats.clone());
    for (room, status, admitted, change) in rooms {
        let meeting = new_meeting(&state.db, room, host).await;
        member(&state.db, meeting, user, status, admitted).await;
        if let Some(change) = change {
            alter_meeting(&state.db, meeting, change).await;
        }
    }
    let server = gravatar_stub(H64, ResponseTemplate::new(200).set_body_bytes(PNG), 1).await;
    with_gravatar(&mut state, &server);
    let mut sub = nats.subscribe("room.*.system").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let window = Duration::from_secs(1);
    let once = BTreeMap::from([
        ("avatar-pub-active".to_string(), 1),
        ("avatar-pub-admitted".to_string(), 1),
        ("avatar-pub-left".to_string(), 1),
        ("avatar-pub-left-at-start".to_string(), 1),
    ]);

    upload(&state, user, PNG, Some("image/png")).await.info();
    assert_eq!(avatar_events(&mut sub, user, window).await, once, "upload");
    set_gravatar(&state, user, "myemail@example.com")
        .await
        .info();
    assert_eq!(
        avatar_events(&mut sub, user, window).await,
        once,
        "gravatar"
    );
    let deleted = as_user(&state, "DELETE", "/api/v1/user/avatar", user).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(avatar_events(&mut sub, user, window).await, once, "delete");

    let elsewhere = expecting("someone-else@example.com");
    let mismatch = format!("/api/v1/user/avatar{elsewhere}");
    let refused = [
        as_user(&state, "DELETE", "/api/v1/user/avatar", user).await,
        upload_at(&state, &mismatch, user, PNG, Some("image/png")).await,
        set_gravatar_at(
            &state,
            &format!("/api/v1/user/avatar/gravatar{elsewhere}"),
            user,
            "myemail@example.com",
        )
        .await,
        as_user(&state, "DELETE", &mismatch, user).await,
        set_gravatar(&state, user, "not a gravatar").await,
    ];
    let statuses: Vec<_> = refused.iter().map(|reply| reply.status).collect();
    assert_eq!(
        statuses,
        [
            StatusCode::NO_CONTENT,
            StatusCode::CONFLICT,
            StatusCode::CONFLICT,
            StatusCode::CONFLICT,
            StatusCode::BAD_REQUEST,
        ]
    );
    let mut exhausted = false;
    for _ in 0..100 {
        let reply = as_user(&state, "DELETE", "/api/v1/user/avatar", user).await;
        if reply.status == StatusCode::TOO_MANY_REQUESTS {
            exhausted = true;
            break;
        }
    }
    assert!(exhausted, "the write budget must run out");
    let limited = upload(&state, user, PNG, Some("image/png")).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    state.avatars = Arc::new(AvatarService::new(None));
    let disabled = set_gravatar(&state, user, "myemail@example.com").await;
    assert_eq!(disabled.status, StatusCode::NOT_FOUND);
    assert!(
        avatar_events(&mut sub, user, window).await.is_empty(),
        "a no-op delete or a refused write must announce nothing"
    );

    forget(&state.db, &[user]).await;
    for (room, ..) in rooms {
        cleanup_test_data(&state.db, room).await;
    }
}

#[tokio::test]
#[serial]
async fn the_cors_preflight_allows_a_raw_image_upload() {
    let pool = get_test_pool().await;
    let request = Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/user/avatar")
        .header(header::ORIGIN, TEST_ORIGIN)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PUT")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
        .body(Body::empty())
        .unwrap();
    let response = build_app_with_cors(pool).oneshot(request).await.unwrap();
    let headers = response.headers();
    assert_eq!(
        headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        TEST_ORIGIN
    );
    let methods = headers
        .get(header::ACCESS_CONTROL_ALLOW_METHODS)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        methods.contains("PUT") && methods.contains("DELETE"),
        "{methods}"
    );
    let allowed = headers
        .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        allowed.to_ascii_lowercase().contains("content-type"),
        "{allowed}"
    );
}
