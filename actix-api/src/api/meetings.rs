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

use actix_web::{web, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::{error, info};

use crate::constants::VALID_ID_PATTERN;
use crate::models::meeting::{CreateMeetingError, Meeting};
use crate::models::meeting_participant::{MeetingParticipant, ParticipantError};

const MAX_ATTENDEES: usize = 100;

#[derive(Debug, Deserialize)]
pub struct CreateMeetingRequest {
    pub meeting_id: Option<String>,
    #[serde(default)]
    pub attendees: Vec<String>,
    pub password: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateMeetingResponse {
    pub meeting_id: String,
    pub host: String,
    pub created_timestamp: i64,
    pub state: String,
    pub attendees: Vec<String>,
    pub has_password: bool,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

/// Response for participant status
#[derive(Debug, Serialize)]
pub struct ParticipantStatusResponse {
    pub email: String,
    pub status: String,
    pub is_host: bool,
    pub joined_at: i64,
    pub admitted_at: Option<i64>,
}

impl From<MeetingParticipant> for ParticipantStatusResponse {
    fn from(p: MeetingParticipant) -> Self {
        Self {
            email: p.email,
            status: p.status,
            is_host: p.is_host,
            joined_at: p.joined_at.timestamp(),
            admitted_at: p.admitted_at.map(|t| t.timestamp()),
        }
    }
}

/// Response for waiting room list
#[derive(Debug, Serialize)]
pub struct WaitingRoomResponse {
    pub meeting_id: String,
    pub waiting: Vec<ParticipantStatusResponse>,
}

/// Request to admit/reject a participant
#[derive(Debug, Deserialize)]
pub struct AdmitRequest {
    pub email: String,
}

/// Request body for joining a meeting
#[derive(Debug, Deserialize)]
pub struct JoinMeetingRequest {
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Response for meeting info (for attendees)
#[derive(Debug, Serialize)]
pub struct MeetingInfoResponse {
    pub meeting_id: String,
    pub state: String,
    pub host: String,
    pub host_display_name: Option<String>,
    pub has_password: bool,
    pub your_status: Option<ParticipantStatusResponse>,
}

/// Response for listing meetings
#[derive(Debug, Serialize)]
pub struct ListMeetingsResponse {
    pub meetings: Vec<MeetingSummary>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Summary of a meeting for listing
#[derive(Debug, Serialize)]
pub struct MeetingSummary {
    pub meeting_id: String,
    pub host: Option<String>,
    pub state: String,
    pub has_password: bool,
    pub created_at: i64,
    pub participant_count: i64,
}

/// Query parameters for listing meetings
#[derive(Debug, Deserialize)]
pub struct ListMeetingsQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
}

fn default_limit() -> i64 {
    20
}

impl ApiError {
    pub fn unauthorized() -> Self {
        Self {
            code: "UNAUTHORIZED".to_string(),
            message: "Authentication required. Please provide email cookie.".to_string(),
        }
    }

    pub fn invalid_meeting_id(detail: &str) -> Self {
        Self {
            code: "INVALID_MEETING_ID".to_string(),
            message: format!("Invalid meeting ID: {}", detail),
        }
    }

    pub fn too_many_attendees(count: usize) -> Self {
        Self {
            code: "TOO_MANY_ATTENDEES".to_string(),
            message: format!(
                "Too many attendees: {} provided, maximum is {}",
                count, MAX_ATTENDEES
            ),
        }
    }

    pub fn meeting_exists(meeting_id: &str) -> Self {
        Self {
            code: "MEETING_EXISTS".to_string(),
            message: format!("Meeting with ID '{}' already exists", meeting_id),
        }
    }

    pub fn meeting_not_found(meeting_id: &str) -> Self {
        Self {
            code: "MEETING_NOT_FOUND".to_string(),
            message: format!("Meeting '{}' not found", meeting_id),
        }
    }

    pub fn meeting_not_active(meeting_id: &str) -> Self {
        Self {
            code: "MEETING_NOT_ACTIVE".to_string(),
            message: format!("Meeting '{}' is not active. Host must join first.", meeting_id),
        }
    }

    pub fn not_host() -> Self {
        Self {
            code: "NOT_HOST".to_string(),
            message: "Only the meeting host can perform this action".to_string(),
        }
    }

    pub fn participant_not_found(email: &str) -> Self {
        Self {
            code: "PARTICIPANT_NOT_FOUND".to_string(),
            message: format!("Participant '{}' not found in waiting room", email),
        }
    }

    pub fn internal_error(detail: &str) -> Self {
        Self {
            code: "INTERNAL_ERROR".to_string(),
            message: format!("Internal server error: {}", detail),
        }
    }
}

/// Extract email from request cookies for authentication
fn get_email_from_cookies(req: &HttpRequest) -> Option<String> {
    req.cookie("email")
        .map(|c| c.value().to_string())
        .filter(|e| !e.is_empty())
}

/// Validate meeting ID format
fn validate_meeting_id(meeting_id: &str) -> Result<(), String> {
    if meeting_id.is_empty() {
        return Err("Meeting ID cannot be empty".to_string());
    }
    if meeting_id.len() > 255 {
        return Err("Meeting ID cannot exceed 255 characters".to_string());
    }
    let re = regex::Regex::new(VALID_ID_PATTERN).unwrap();
    if !re.is_match(meeting_id) {
        return Err(format!(
            "Meeting ID must match pattern: {}",
            VALID_ID_PATTERN
        ));
    }
    Ok(())
}

/// Generate a random meeting ID
fn generate_meeting_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let id: String = (0..12)
        .map(|_| {
            let idx = rng.gen_range(0..36);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'a' + idx - 10) as char
            }
        })
        .collect();
    id
}

/// POST /api/v1/meetings - Create a new meeting
pub async fn create_meeting(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateMeetingRequest>,
) -> HttpResponse {
    // 1. Authenticate via email cookie
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            info!("Unauthorized meeting creation attempt - no email cookie");
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    info!("Creating meeting for user: {}", email);

    // 2. Validate meeting_id if provided, or generate one
    let meeting_id = match &body.meeting_id {
        Some(id) => {
            let clean_id = id.replace(' ', "_");
            if let Err(e) = validate_meeting_id(&clean_id) {
                return HttpResponse::BadRequest().json(ApiError::invalid_meeting_id(&e));
            }
            clean_id
        }
        None => generate_meeting_id(),
    };

    // 3. Validate attendees count
    if body.attendees.len() > MAX_ATTENDEES {
        return HttpResponse::BadRequest().json(ApiError::too_many_attendees(body.attendees.len()));
    }

    // 4. Create meeting in database
    let result = Meeting::create_meeting_api(
        pool.get_ref(),
        &meeting_id,
        &email,
        &body.attendees,
        body.password.as_deref(),
    )
    .await;

    match result {
        Ok(meeting) => {
            info!("Meeting '{}' created by {}", meeting_id, email);
            HttpResponse::Created().json(CreateMeetingResponse {
                meeting_id: meeting.room_id,
                host: meeting.creator_id.unwrap_or_default(),
                created_timestamp: meeting.created_at.timestamp(),
                state: meeting.state.unwrap_or_else(|| "idle".to_string()),
                attendees: meeting
                    .attendees
                    .map(|a| serde_json::from_value(a).unwrap_or_default())
                    .unwrap_or_default(),
                has_password: meeting.password_hash.is_some(),
            })
        }
        Err(CreateMeetingError::MeetingExists) => {
            info!("Meeting '{}' already exists", meeting_id);
            HttpResponse::Conflict().json(ApiError::meeting_exists(&meeting_id))
        }
        Err(CreateMeetingError::DatabaseError(e)) => {
            error!("Database error creating meeting: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(CreateMeetingError::HashError(e)) => {
            error!("Password hashing error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Hashing error"))
        }
    }
}

/// GET /api/v1/meetings/{meeting_id} - Get meeting info
pub async fn get_meeting(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    // Get meeting
    let meeting = match Meeting::get_by_room_id_async(pool.get_ref(), &meeting_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id));
        }
        Err(e) => {
            error!("Database error: {}", e);
            return HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"));
        }
    };

    // Get participant status
    let your_status = match MeetingParticipant::get_status(pool.get_ref(), &meeting_id, &email).await {
        Ok(status) => status.map(ParticipantStatusResponse::from),
        Err(e) => {
            error!("Error getting participant status: {}", e);
            None
        }
    };

    // Look up the host's display name from their participant record (based on creator_id/email)
    // This ensures the host is identified by their user ID, not just a stored display name
    let host_display_name = if let Some(ref creator_id) = meeting.creator_id {
        match MeetingParticipant::get_display_name_by_email(pool.get_ref(), meeting.id, creator_id).await {
            Ok(name) => name,
            Err(_) => meeting.host_display_name.clone(), // Fallback to stored value
        }
    } else {
        meeting.host_display_name.clone()
    };

    HttpResponse::Ok().json(MeetingInfoResponse {
        meeting_id: meeting.room_id,
        state: meeting.state.unwrap_or_else(|| "idle".to_string()),
        host: meeting.creator_id.unwrap_or_default(),
        host_display_name,
        has_password: meeting.password_hash.is_some(),
        your_status,
    })
}

/// POST /api/v1/meetings/{meeting_id}/join - Request to join meeting (enters wait room)
pub async fn join_meeting(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
    body: Option<web::Json<JoinMeetingRequest>>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    // Extract display_name from body if provided
    let display_name = body.and_then(|b| b.display_name.clone());

    info!("User '{}' (display: {:?}) requesting to join meeting '{}'", email, display_name, meeting_id);

    // Request to join
    match MeetingParticipant::request_join(pool.get_ref(), &meeting_id, &email, display_name.as_deref()).await {
        Ok(participant) => {
            HttpResponse::Ok().json(ParticipantStatusResponse::from(participant))
        }
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::MeetingNotActive) => {
            HttpResponse::BadRequest().json(ApiError::meeting_not_active(&meeting_id))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error joining meeting: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// GET /api/v1/meetings/{meeting_id}/waiting - Get waiting room participants (host only)
pub async fn get_waiting_room(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    // Get waiting participants
    match MeetingParticipant::get_waiting(pool.get_ref(), &meeting_id, &email).await {
        Ok(participants) => {
            let waiting: Vec<ParticipantStatusResponse> = participants
                .into_iter()
                .map(ParticipantStatusResponse::from)
                .collect();
            HttpResponse::Ok().json(WaitingRoomResponse {
                meeting_id,
                waiting,
            })
        }
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => {
            HttpResponse::Forbidden().json(ApiError::not_host())
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error getting waiting room: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// POST /api/v1/meetings/{meeting_id}/admit - Admit participant (host only)
pub async fn admit_participant(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
    body: web::Json<AdmitRequest>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let host_email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    info!(
        "Host '{}' admitting '{}' to meeting '{}'",
        host_email, body.email, meeting_id
    );

    // Admit participant
    match MeetingParticipant::admit(pool.get_ref(), &meeting_id, &host_email, &body.email).await {
        Ok(participant) => {
            HttpResponse::Ok().json(ParticipantStatusResponse::from(participant))
        }
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => {
            HttpResponse::Forbidden().json(ApiError::not_host())
        }
        Err(ParticipantError::NotFound) => {
            HttpResponse::NotFound().json(ApiError::participant_not_found(&body.email))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error admitting participant: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// Response for admit all
#[derive(Debug, Serialize)]
pub struct AdmitAllResponse {
    pub admitted_count: usize,
    pub admitted: Vec<ParticipantStatusResponse>,
}

/// POST /api/v1/meetings/{meeting_id}/admit-all - Admit all waiting participants (host only)
pub async fn admit_all_participants(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let host_email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    info!(
        "Host '{}' admitting all waiting participants to meeting '{}'",
        host_email, meeting_id
    );

    // Admit all participants
    match MeetingParticipant::admit_all(pool.get_ref(), &meeting_id, &host_email).await {
        Ok(participants) => {
            let admitted: Vec<ParticipantStatusResponse> = participants
                .into_iter()
                .map(ParticipantStatusResponse::from)
                .collect();
            HttpResponse::Ok().json(AdmitAllResponse {
                admitted_count: admitted.len(),
                admitted,
            })
        }
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => {
            HttpResponse::Forbidden().json(ApiError::not_host())
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error admitting all participants: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// POST /api/v1/meetings/{meeting_id}/reject - Reject participant (host only)
pub async fn reject_participant(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
    body: web::Json<AdmitRequest>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let host_email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    info!(
        "Host '{}' rejecting '{}' from meeting '{}'",
        host_email, body.email, meeting_id
    );

    // Reject participant
    match MeetingParticipant::reject(pool.get_ref(), &meeting_id, &host_email, &body.email).await {
        Ok(participant) => {
            HttpResponse::Ok().json(ParticipantStatusResponse::from(participant))
        }
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => {
            HttpResponse::Forbidden().json(ApiError::not_host())
        }
        Err(ParticipantError::NotFound) => {
            HttpResponse::NotFound().json(ApiError::participant_not_found(&body.email))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error rejecting participant: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// GET /api/v1/meetings/{meeting_id}/status - Get my status in meeting
pub async fn get_my_status(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    // Get status
    match MeetingParticipant::get_status(pool.get_ref(), &meeting_id, &email).await {
        Ok(Some(participant)) => {
            HttpResponse::Ok().json(ParticipantStatusResponse::from(participant))
        }
        Ok(None) => {
            HttpResponse::NotFound().json(ApiError {
                code: "NOT_IN_MEETING".to_string(),
                message: "You have not requested to join this meeting".to_string(),
            })
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error getting status: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// POST /api/v1/meetings/{meeting_id}/leave - Leave meeting
pub async fn leave_meeting(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    info!("User '{}' leaving meeting '{}'", email, meeting_id);

    // Leave meeting
    match MeetingParticipant::leave(pool.get_ref(), &meeting_id, &email).await {
        Ok(Some(participant)) => {
            HttpResponse::Ok().json(ParticipantStatusResponse::from(participant))
        }
        Ok(None) => {
            HttpResponse::NotFound().json(ApiError {
                code: "NOT_IN_MEETING".to_string(),
                message: "You are not in this meeting".to_string(),
            })
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error leaving meeting: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// GET /api/v1/meetings/{meeting_id}/participants - Get all admitted participants
pub async fn get_participants(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let _email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    // Get admitted participants
    match MeetingParticipant::get_admitted(pool.get_ref(), &meeting_id).await {
        Ok(participants) => {
            let admitted: Vec<ParticipantStatusResponse> = participants
                .into_iter()
                .map(ParticipantStatusResponse::from)
                .collect();
            HttpResponse::Ok().json(admitted)
        }
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error getting participants: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error(&e.to_string()))
        }
    }
}

/// DELETE /api/v1/meetings/{meeting_id} - Delete a meeting (owner only)
pub async fn delete_meeting(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    path: web::Path<String>,
) -> HttpResponse {
    let meeting_id = path.into_inner();

    // Authenticate
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    info!("User '{}' attempting to delete meeting '{}'", email, meeting_id);

    // Get meeting and verify ownership
    let meeting = match Meeting::get_by_room_id_async(pool.get_ref(), &meeting_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return HttpResponse::NotFound().json(ApiError::meeting_not_found(&meeting_id));
        }
        Err(e) => {
            error!("Database error: {}", e);
            return HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"));
        }
    };

    // Verify the user is the owner
    if meeting.creator_id.as_ref() != Some(&email) {
        return HttpResponse::Forbidden().json(ApiError {
            code: "NOT_OWNER".to_string(),
            message: "Only the meeting owner can delete this meeting".to_string(),
        });
    }

    // Soft delete the meeting (set deleted_at)
    match sqlx::query(
        "UPDATE meetings SET deleted_at = NOW(), ended_at = COALESCE(ended_at, NOW()) WHERE room_id = $1"
    )
    .bind(&meeting_id)
    .execute(pool.get_ref())
    .await
    {
        Ok(_) => {
            info!("Meeting '{}' deleted by owner '{}'", meeting_id, email);
            HttpResponse::Ok().json(serde_json::json!({
                "success": true,
                "message": format!("Meeting '{}' has been deleted", meeting_id)
            }))
        }
        Err(e) => {
            error!("Database error deleting meeting: {}", e);
            HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"))
        }
    }
}

/// GET /api/v1/meetings - List active meetings owned by the authenticated user
pub async fn list_meetings(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    query: web::Query<ListMeetingsQuery>,
) -> HttpResponse {
    // Authenticate
    let email = match get_email_from_cookies(&req) {
        Some(email) => email,
        None => {
            return HttpResponse::Unauthorized().json(ApiError::unauthorized());
        }
    };

    // Clamp limit to reasonable bounds
    let limit = query.limit.clamp(1, 100);
    let offset = query.offset.max(0);

    // Get meetings owned by the authenticated user
    let meetings = match Meeting::list_by_owner_async(pool.get_ref(), &email, limit, offset).await {
        Ok(m) => m,
        Err(e) => {
            error!("Database error listing meetings: {}", e);
            return HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"));
        }
    };

    // Get total count for this owner
    let total = match Meeting::count_by_owner_async(pool.get_ref(), &email).await {
        Ok(c) => c,
        Err(e) => {
            error!("Database error counting meetings: {}", e);
            return HttpResponse::InternalServerError().json(ApiError::internal_error("Database error"));
        }
    };

    // Get participant counts for each meeting
    let mut summaries = Vec::with_capacity(meetings.len());
    for meeting in meetings {
        let participant_count = MeetingParticipant::count_admitted(pool.get_ref(), &meeting.room_id)
            .await
            .unwrap_or_default();

        summaries.push(MeetingSummary {
            meeting_id: meeting.room_id,
            host: meeting.creator_id,
            state: meeting.state.unwrap_or_else(|| "idle".to_string()),
            has_password: meeting.password_hash.is_some(),
            created_at: meeting.created_at.timestamp(),
            participant_count,
        });
    }

    HttpResponse::Ok().json(ListMeetingsResponse {
        meetings: summaries,
        total,
        limit,
        offset,
    })
}
