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
    routing::{delete, get, post},
    Router,
};

use crate::state::AppState;

/// Build the full application router with all meeting API routes.
pub fn router() -> Router<AppState> {
    Router::new()
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
        // Participant actions
        .route(
            "/api/v1/meetings/{meeting_id}/join",
            post(participants::join_meeting),
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
