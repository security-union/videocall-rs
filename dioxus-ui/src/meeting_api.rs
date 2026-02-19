// SPDX-License-Identifier: MIT OR Apache-2.0

//! Meeting API client facade for the dioxus-ui.

use crate::constants::meeting_api_client;
pub use videocall_meeting_client::ApiError as JoinError;
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

pub async fn refresh_room_token(meeting_id: &str) -> Result<String, JoinError> {
    client()?.refresh_room_token(meeting_id).await
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
