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

//! Axum router configuration for the Meeting Backend API.

pub mod meetings;
pub mod oauth;
pub mod participants;
pub mod waiting_room;

use axum::{
    extract::State,
    routing::{delete, get, patch, post, put},
    Router,
};

use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// Build version metadata returned by all service `/version` endpoints.
#[derive(Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    pub service: String,
    pub version: String,
    pub git_sha: String,
    pub git_branch: String,
    pub build_timestamp: String,
}

fn own_build_info() -> BuildInfo {
    BuildInfo {
        service: "meeting-api".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        git_sha: env!("GIT_SHA").into(),
        git_branch: env!("GIT_BRANCH").into(),
        build_timestamp: env!("BUILD_TIMESTAMP").into(),
    }
}

/// Handler that returns build version info for the meeting-api service.
pub async fn version() -> axum::Json<BuildInfo> {
    axum::Json(own_build_info())
}

/// Aggregated version info from meeting-api and all configured peer services.
///
/// Fetches `/version` from each URL in `SERVICE_VERSION_URLS` (with a short
/// timeout) and returns them alongside this service's own build info.
/// Peer responses are deserialized into [`BuildInfo`]; unexpected payloads
/// are silently dropped.
pub async fn versions(State(state): State<AppState>) -> axum::Json<serde_json::Value> {
    let mut components = vec![own_build_info()];

    let fetches: Vec<_> = state
        .service_version_urls
        .iter()
        .map(|url| {
            let client = state.http_client.clone();
            let url = url.clone();
            async move {
                match client.get(&url).send().await {
                    Ok(resp) => resp.json::<BuildInfo>().await.ok(),
                    Err(_) => None,
                }
            }
        })
        .collect();

    let results = futures::future::join_all(fetches).await;
    for result in results.into_iter().flatten() {
        components.push(result);
    }

    axum::Json(serde_json::json!({ "components": components }))
}

/// Build the full application router with all meeting API routes.
pub fn router() -> Router<AppState> {
    Router::new()
        // Version info
        .route("/version", get(version))
        .route("/api/v1/versions", get(versions))
        // OAuth / session
        .route("/login", get(oauth::login))
        .route("/login/callback", get(oauth::callback))
        .route("/session", get(oauth::check_session))
        .route("/profile", get(oauth::get_profile))
        .route("/logout", get(oauth::logout))
        // Meeting CRUD
        .route("/api/v1/meetings", get(meetings::list_meetings))
        .route("/api/v1/meetings", post(meetings::create_meeting))
        .route("/api/v1/meetings/{meeting_id}", get(meetings::get_meeting))
        .route(
            "/api/v1/meetings/{meeting_id}",
            delete(meetings::delete_meeting),
        )
        .route(
            "/api/v1/meetings/{meeting_id}",
            patch(meetings::update_meeting),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/end",
            post(meetings::end_meeting_handler),
        )
        // Participant actions
        .route(
            "/api/v1/meetings/{meeting_id}/join",
            post(participants::join_meeting),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/join_guest",
            post(participants::join_meeting_as_guest),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/leave",
            post(participants::leave_meeting),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/status",
            get(participants::get_my_status),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/display-name",
            put(participants::update_display_name),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/participants",
            get(participants::get_participants),
        )
        // Waiting room
        .route(
            "/api/v1/meetings/{meeting_id}/waiting",
            get(waiting_room::get_waiting_room),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/admit",
            post(waiting_room::admit_participant),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/admit-all",
            post(waiting_room::admit_all),
        )
        .route(
            "/api/v1/meetings/{meeting_id}/reject",
            post(waiting_room::reject_participant),
        )
}
