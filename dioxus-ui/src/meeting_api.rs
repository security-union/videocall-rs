// SPDX-License-Identifier: MIT OR Apache-2.0

//! Meeting API client facade for the dioxus-ui.

use crate::constants::meeting_api_client;
pub use videocall_meeting_client::ApiError as JoinError;
pub use videocall_meeting_types::responses::CreateMeetingResponse;
pub use videocall_meeting_types::responses::MeetingGuestInfoResponse;
pub use videocall_meeting_types::responses::MeetingInfoResponse as MeetingInfo;
pub use videocall_meeting_types::responses::ParticipantStatusResponse as JoinMeetingResponse;

fn client() -> Result<videocall_meeting_client::MeetingApiClient, JoinError> {
    meeting_api_client().map_err(JoinError::Config)
}

pub async fn join_meeting(
    meeting_id: &str,
    display_name: Option<&str>,
) -> Result<JoinMeetingResponse, JoinError> {
    log::info!("Joining meeting via API: {meeting_id} (display_name: {display_name:?})");
    let result = client()?.join_meeting(meeting_id, display_name).await?;
    log::info!(
        "Join response: status={}, is_host={}",
        result.status,
        result.is_host
    );
    Ok(result)
}

pub async fn get_meeting_info(meeting_id: &str) -> Result<MeetingInfo, JoinError> {
    client()?.get_meeting(meeting_id).await
}

pub async fn check_status(meeting_id: &str) -> Result<JoinMeetingResponse, JoinError> {
    client()?.get_status(meeting_id).await
}

/// Check status for a guest participant using their observer JWT as a Bearer
/// token. Calls `GET /api/v1/meetings/{meeting_id}/guest-status`.
pub async fn check_guest_status(
    meeting_id: &str,
    observer_token: &str,
) -> Result<JoinMeetingResponse, JoinError> {
    let base_url = crate::constants::meeting_api_base_url().map_err(JoinError::Config)?;
    videocall_meeting_client::MeetingApiClient::new(
        &base_url,
        videocall_meeting_client::AuthMode::Bearer(observer_token.to_string()),
    )
    .get_guest_status(meeting_id)
    .await
}

/// Fetch the participant's current status using the appropriate auth mode.
pub async fn fetch_participant_status(
    meeting_id: &str,
    observer_token: &str,
    is_guest: bool,
) -> Result<JoinMeetingResponse, JoinError> {
    if is_guest {
        check_guest_status(meeting_id, observer_token).await
    } else {
        check_status(meeting_id).await
    }
}

fn make_guest_client(token: &str) -> Result<videocall_meeting_client::MeetingApiClient, JoinError> {
    let base_url = crate::constants::meeting_api_base_url().map_err(JoinError::Config)?;
    Ok(videocall_meeting_client::MeetingApiClient::new(
        &base_url,
        videocall_meeting_client::AuthMode::Bearer(token.to_string()),
    ))
}

pub async fn refresh_room_token(meeting_id: &str) -> Result<String, JoinError> {
    client()?.refresh_room_token(meeting_id).await
}

pub async fn update_meeting(
    meeting_id: &str,
    waiting_room_enabled: Option<bool>,
    admitted_can_admit: Option<bool>,
    allow_guests: Option<bool>,
) -> Result<MeetingInfo, JoinError> {
    let req = videocall_meeting_types::requests::UpdateMeetingRequest {
        waiting_room_enabled,
        admitted_can_admit,
        allow_guests,
    };
    client()?.update_meeting(meeting_id, &req).await
}

pub async fn end_meeting(meeting_id: &str) -> Result<MeetingInfo, JoinError> {
    log::info!("Ending meeting via API: {meeting_id}");
    client()?.end_meeting(meeting_id).await
}

pub async fn get_meeting_guest_info(
    meeting_id: &str,
) -> Result<MeetingGuestInfoResponse, JoinError> {
    client()?.get_meeting_guest_info(meeting_id).await
}

pub async fn delete_meeting(meeting_id: &str) -> Result<(), JoinError> {
    log::info!("Deleting meeting via API: {meeting_id}");
    client()?.delete_meeting(meeting_id).await?;
    Ok(())
}

pub async fn leave_meeting(meeting_id: &str) -> Result<(), JoinError> {
    log::info!("Leaving meeting via API: {meeting_id}");
    match client()?.leave_meeting(meeting_id).await {
        Ok(_) => {
            log::info!("Successfully left meeting {meeting_id}");
            Ok(())
        }
        Err(JoinError::NotFound(_)) => {
            log::warn!("Not in meeting {meeting_id} when trying to leave");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

pub async fn update_display_name(
    meeting_id: &str,
    display_name: &str,
) -> Result<JoinMeetingResponse, JoinError> {
    log::info!("Updating display name via API: {meeting_id} (display_name: {display_name})");
    client()?
        .update_display_name(meeting_id, display_name)
        .await
}

pub async fn create_meeting(
    meeting_id: Option<&str>,
    allow_guests: bool,
) -> Result<CreateMeetingResponse, JoinError> {
    let req = videocall_meeting_types::requests::CreateMeetingRequest {
        meeting_id: meeting_id.map(|s| s.to_string()),
        attendees: vec![],
        password: None,
        waiting_room_enabled: Some(true),
        admitted_can_admit: Some(false),
        allow_guests: Some(allow_guests),
    };
    client()?.create_meeting(&req).await
}

pub async fn join_meeting_as_guest(
    meeting_id: &str,
    display_name: &str,
) -> Result<JoinMeetingResponse, JoinError> {
    log::info!("Joining meeting as guest via API: {meeting_id} (display_name: {display_name})");
    let stored_id = crate::auth::get_guest_session_id();
    let base_url = crate::constants::meeting_api_base_url().map_err(JoinError::Config)?;
    let result = videocall_meeting_client::MeetingApiClient::new(
        &base_url,
        videocall_meeting_client::AuthMode::Cookie,
    )
    .join_meeting_as_guest(meeting_id, display_name, stored_id.as_deref())
    .await?;

    crate::auth::store_guest_session_id(&result.user_id);
    log::info!(
        "Guest join response: status={}, is_host={}, user_id={}",
        result.status,
        result.is_host,
        result.user_id,
    );
    Ok(result)
}

/// Leave a meeting as a guest, authenticated via the observer JWT that was
/// issued at join time. Calls `POST /api/v1/meetings/{meeting_id}/leave-guest`.
pub async fn leave_meeting_as_guest(
    meeting_id: &str,
    observer_token: &str,
) -> Result<(), JoinError> {
    log::info!("Guest leaving meeting via API: {meeting_id}");
    match make_guest_client(observer_token)?
        .leave_meeting_as_guest(meeting_id)
        .await
    {
        Ok(_) => {
            log::info!("Guest successfully left meeting {meeting_id}");
            Ok(())
        }
        Err(JoinError::NotFound(_)) => {
            log::warn!("Guest row not found for meeting {meeting_id} when trying to leave");
            Ok(())
        }
        Err(e) => Err(e),
    }
}
