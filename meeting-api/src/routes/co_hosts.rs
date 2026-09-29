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

//! Co-host management: grant/revoke are owner-only; list also allows a live co-host or present host.

use axum::{extract::State, Json};
use videocall_meeting_types::{
    requests::{GrantCoHostRequest, RevokeCoHostRequest},
    responses::{APIResponse, CoHostEntry, ListCoHostsResponse},
    GUEST_USER_ID_PREFIX,
};

use crate::auth::AuthUser;
use crate::db::co_hosts::{self as db_co_hosts, GrantOutcome, RevokeOutcome};
use crate::db::meetings::{self as db_meetings, MeetingRow};
use crate::error::AppError;
use crate::nats_events;
use crate::routes::valid_meeting_id::ValidMeetingId;
use crate::search;
use crate::state::AppState;

/// Maximum co-host entries per meeting.
pub const MAX_CO_HOSTS: usize = 100;

const MAX_USER_ID_LEN: usize = 254;

/// Trims and lower-cases `raw` for storage/comparison, so co-host matching is
/// case-insensitive regardless of how the owner typed the target or how the
/// target's own identity provider cases their `sub`/email.
fn validate_target(raw: &str, owner: &str) -> Result<String, AppError> {
    let target = raw.trim().to_lowercase();
    if target.is_empty() {
        return Err(AppError::bad_request("user_id must not be empty"));
    }
    if target.len() > MAX_USER_ID_LEN {
        return Err(AppError::bad_request("user_id too long"));
    }
    if target == owner.trim().to_lowercase() {
        return Err(AppError::bad_request("the meeting owner is always a host"));
    }
    if target.starts_with(GUEST_USER_ID_PREFIX) {
        return Err(AppError::bad_request(
            "a guest participant cannot be a co-host",
        ));
    }
    Ok(target)
}

/// Validate and dedupe the `co_hosts` list of a create request.
pub(crate) fn normalize_create_list(raw: &[String], owner: &str) -> Result<Vec<String>, AppError> {
    if raw.len() > MAX_CO_HOSTS {
        return Err(AppError::bad_request(format!(
            "Too many co-hosts: {} provided, maximum is {MAX_CO_HOSTS}",
            raw.len()
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for entry in raw {
        let target = validate_target(entry, owner)?;
        if !out.iter().any(|existing| existing == &target) {
            out.push(target);
        }
    }
    Ok(out)
}

async fn require_owner(
    state: &AppState,
    meeting_id: &str,
    user_id: &str,
) -> Result<MeetingRow, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(meeting_id))?;
    if meeting.creator_id.as_deref() != Some(user_id) {
        return Err(AppError::not_owner());
    }
    Ok(meeting)
}

async fn list_response(
    state: &AppState,
    meeting_pk: i32,
    healthy: bool,
) -> Result<Json<APIResponse<ListCoHostsResponse>>, AppError> {
    let co_hosts = db_co_hosts::list(&state.db, meeting_pk, healthy)
        .await?
        .into_iter()
        .map(|row| CoHostEntry {
            user_id: row.user_id,
            persistent: row.persistent,
            is_present_host: row.is_present_host,
            display_name: row.display_name,
            designated: row.designated,
            suspended: row.suspended,
        })
        .collect();
    Ok(Json(APIResponse::ok(ListCoHostsResponse { co_hosts })))
}

/// `GET /api/v1/meetings/{meeting_id}/co-hosts` — owner, live co-host, or present host; else `403 NOT_HOST`.
pub async fn list_co_hosts(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    ValidMeetingId(meeting_id): ValidMeetingId,
) -> Result<Json<APIResponse<ListCoHostsResponse>>, AppError> {
    let meeting = db_meetings::get_by_room_id(&state.db, &meeting_id)
        .await?
        .ok_or_else(|| AppError::meeting_not_found(&meeting_id))?;
    let healthy = state.presence_healthy().await?;
    if !crate::routes::meetings::can_edit_options(&state, &meeting, &user_id, healthy).await? {
        return Err(AppError::not_host());
    }
    list_response(&state, meeting.id, healthy).await
}

/// `POST /api/v1/meetings/{meeting_id}/co-hosts` — owner only; returns the updated list.
pub async fn grant_co_host(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    ValidMeetingId(meeting_id): ValidMeetingId,
    Json(body): Json<GrantCoHostRequest>,
) -> Result<Json<APIResponse<ListCoHostsResponse>>, AppError> {
    let meeting = require_owner(&state, &meeting_id, &user_id).await?;
    let target = validate_target(&body.user_id, &user_id)?;
    let healthy = state.presence_healthy().await?;

    let outcome = db_co_hosts::grant(
        &state.db,
        meeting.id,
        &target,
        body.persist,
        &user_id,
        MAX_CO_HOSTS as i64,
        healthy,
    )
    .await?;
    match outcome {
        GrantOutcome::Granted { promoted: false } => {}
        GrantOutcome::Granted { promoted: true } => {
            let announced_id = db_co_hosts::canonical_user_id(&state.db, meeting.id, &target)
                .await?
                .unwrap_or_else(|| body.user_id.trim().to_string());
            nats_events::announce_host_change(
                state.nats.as_ref(),
                &meeting_id,
                &announced_id,
                &user_id,
                true,
            )
            .await;
            search::spawn_repush(&state, meeting.id, meeting_id.clone());
        }
        GrantOutcome::LimitReached => {
            return Err(AppError::bad_request(format!(
                "a meeting can have at most {MAX_CO_HOSTS} co-hosts"
            )));
        }
        GrantOutcome::NotActive => {
            return Err(AppError::bad_request(
                "an instance-only co-host requires an active meeting; set persist to save it for future instances",
            ));
        }
    }

    list_response(&state, meeting.id, healthy).await
}

/// `POST /api/v1/meetings/{meeting_id}/co-hosts/revoke` — owner only; `409 LAST_PRESENT_HOST` guards the last host.
pub async fn revoke_co_host(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    ValidMeetingId(meeting_id): ValidMeetingId,
    Json(body): Json<RevokeCoHostRequest>,
) -> Result<Json<APIResponse<ListCoHostsResponse>>, AppError> {
    let meeting = require_owner(&state, &meeting_id, &user_id).await?;
    let target = validate_target(&body.user_id, &user_id)?;
    let healthy = state.presence_healthy().await?;

    match db_co_hosts::revoke(&state.db, meeting.id, &target, healthy).await? {
        RevokeOutcome::LastPresentHost => return Err(AppError::last_present_host()),
        RevokeOutcome::NotFound => return Err(AppError::co_host_not_found(&target)),
        RevokeOutcome::Revoked { demoted: false } => {}
        RevokeOutcome::Revoked { demoted: true } => {
            let announced_id = db_co_hosts::canonical_user_id(&state.db, meeting.id, &target)
                .await?
                .unwrap_or_else(|| body.user_id.trim().to_string());
            nats_events::announce_host_change(
                state.nats.as_ref(),
                &meeting_id,
                &announced_id,
                &user_id,
                false,
            )
            .await;
            search::spawn_repush(&state, meeting.id, meeting_id.clone());
        }
    }

    list_response(&state, meeting.id, healthy).await
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "owner@example.com";

    #[test]
    fn validate_target_trims_and_rejects_owner_guest_empty_and_long() {
        assert_eq!(
            validate_target("  a@example.com ", OWNER).unwrap(),
            "a@example.com"
        );
        assert!(validate_target("   ", OWNER).is_err());
        assert!(validate_target(OWNER, OWNER).is_err());
        assert!(validate_target(&format!(" {OWNER}"), OWNER).is_err());
        assert!(validate_target("guest:1234", OWNER).is_err());
        assert!(validate_target(&"a".repeat(MAX_USER_ID_LEN + 1), OWNER).is_err());
        assert!(validate_target(&"a".repeat(MAX_USER_ID_LEN), OWNER).is_ok());
    }

    #[test]
    fn normalize_create_list_dedupes_and_caps() {
        let raw = vec![
            "a@example.com".to_string(),
            " a@example.com".to_string(),
            "b@example.com".to_string(),
        ];
        assert_eq!(
            normalize_create_list(&raw, OWNER).unwrap(),
            vec!["a@example.com".to_string(), "b@example.com".to_string()]
        );
        let too_many: Vec<String> = (0..=MAX_CO_HOSTS)
            .map(|i| format!("u{i}@example.com"))
            .collect();
        assert!(normalize_create_list(&too_many, OWNER).is_err());
        assert!(normalize_create_list(&[OWNER.to_string()], OWNER).is_err());
    }
}
