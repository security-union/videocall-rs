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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Meeting API client facade for the yew-ui.
//!
//! Thin wrappers around [`videocall_meeting_client::MeetingApiClient`] that
//! create a client from the browser runtime config on each call.

use crate::constants::meeting_api_client;
pub use videocall_meeting_client::ApiError as JoinError;
pub use videocall_meeting_types::responses::MeetingInfoResponse as MeetingInfo;
pub use videocall_meeting_types::responses::ParticipantStatusResponse as JoinMeetingResponse;

fn client() -> Result<videocall_meeting_client::MeetingApiClient, JoinError> {
    meeting_api_client().map_err(JoinError::Config)
}

/// Join a meeting via the API.
/// Returns the participant status which indicates if they are admitted, waiting, etc.
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

/// Get meeting info including host email.
pub async fn get_meeting_info(meeting_id: &str) -> Result<MeetingInfo, JoinError> {
    client()?.get_meeting(meeting_id).await
}

/// Check participant status in a meeting.
pub async fn check_status(meeting_id: &str) -> Result<JoinMeetingResponse, JoinError> {
    client()?.get_status(meeting_id).await
}

/// Fetch a fresh room access token from the meeting API.
///
/// Calls `GET /api/v1/meetings/{id}/status` and extracts the `room_token`
/// from the response. Returns an error if the participant is no longer
/// admitted or the session has expired.
pub async fn refresh_room_token(meeting_id: &str) -> Result<String, JoinError> {
    client()?.refresh_room_token(meeting_id).await
}

/// Leave a meeting - updates participant status to 'left' in database.
pub async fn leave_meeting(meeting_id: &str) -> Result<(), JoinError> {
    log::info!("Leaving meeting via API: {meeting_id}");
    match client()?.leave_meeting(meeting_id).await {
        Ok(_) => {
            log::info!("Successfully left meeting {meeting_id}");
            Ok(())
        }
        Err(JoinError::NotFound(_)) => {
            // Not in meeting is fine - just means we weren't tracked
            log::warn!("Not in meeting {meeting_id} when trying to leave");
            Ok(())
        }
        Err(e) => Err(e),
    }
}
