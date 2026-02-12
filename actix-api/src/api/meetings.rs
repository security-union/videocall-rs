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
use sqlx::PgPool;
use tracing::{error, info};

use crate::constants::VALID_ID_PATTERN;
use crate::models::meeting::{CreateMeetingError, Meeting};
use crate::models::meeting_participant::{MeetingParticipant, ParticipantError};

// Import shared types from videocall-meeting-types crate
pub use videocall_meeting_types::error::APIError;
pub use videocall_meeting_types::requests::{
    AdmitRequest, CreateMeetingRequest, JoinMeetingRequest, ListMeetingsQuery,
};
pub use videocall_meeting_types::responses::{
    AdmitAllResponse, CreateMeetingResponse, ListMeetingsResponse, MeetingInfoResponse,
    MeetingSummary, ParticipantStatusResponse, WaitingRoomResponse,
};

const MAX_ATTENDEES: usize = 100;

/// Convert MeetingParticipant to ParticipantStatusResponse
impl From<MeetingParticipant> for ParticipantStatusResponse {
    fn from(p: MeetingParticipant) -> Self {
        Self {
            email: p.email,
            display_name: p.display_name,
            status: p.status,
            is_host: p.is_host,
            joined_at: p.joined_at.timestamp(),
            admitted_at: p.admitted_at.map(|t| t.timestamp()),
            room_token: None, // Token is set separately when needed
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
        return Err(format!("Meeting ID must match pattern: {VALID_ID_PATTERN}"));
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    info!("Creating meeting for user: {}", email);

    // 2. Validate meeting_id if provided, or generate one
    let meeting_id = match &body.meeting_id {
        Some(id) => {
            let clean_id = id.replace(' ', "_");
            if let Err(e) = validate_meeting_id(&clean_id) {
                return HttpResponse::BadRequest().json(APIError::invalid_meeting_id(&e));
            }
            clean_id
        }
        None => generate_meeting_id(),
    };

    // 3. Validate attendees count
    if body.attendees.len() > MAX_ATTENDEES {
        return HttpResponse::BadRequest().json(APIError::too_many_attendees(body.attendees.len(), MAX_ATTENDEES));
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
                created_at: meeting.created_at.timestamp(),
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
            HttpResponse::Conflict().json(APIError::meeting_exists(&meeting_id))
        }
        Err(CreateMeetingError::DatabaseError(e)) => {
            error!("Database error creating meeting: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(CreateMeetingError::HashError(e)) => {
            error!("Password hashing error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Hashing error"))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    // Get meeting
    let meeting = match Meeting::get_by_room_id_async(pool.get_ref(), &meeting_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id));
        }
        Err(e) => {
            error!("Database error: {}", e);
            return HttpResponse::InternalServerError()
                .json(APIError::internal_error("Database error"));
        }
    };

    // Get participant status
    let your_status =
        match MeetingParticipant::get_status(pool.get_ref(), &meeting_id, &email).await {
            Ok(status) => status.map(ParticipantStatusResponse::from),
            Err(e) => {
                error!("Error getting participant status: {}", e);
                None
            }
        };

    // Look up the host's display name from their participant record (based on creator_id/email)
    // This ensures the host is identified by their user ID, not just a stored display name
    let host_display_name = if let Some(ref creator_id) = meeting.creator_id {
        match MeetingParticipant::get_display_name_by_email(pool.get_ref(), meeting.id, creator_id)
            .await
        {
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    // Extract display_name from body if provided
    let display_name = body.and_then(|b| b.display_name.clone());

    info!(
        "User '{}' (display: {:?}) requesting to join meeting '{}'",
        email, display_name, meeting_id
    );

    // Request to join
    match MeetingParticipant::request_join(
        pool.get_ref(),
        &meeting_id,
        &email,
        display_name.as_deref(),
    )
    .await
    {
        Ok(participant) => HttpResponse::Ok().json(ParticipantStatusResponse::from(participant)),
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::MeetingNotActive) => {
            HttpResponse::BadRequest().json(APIError::meeting_not_active(&meeting_id))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error joining meeting: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
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
            HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => HttpResponse::Forbidden().json(APIError::not_host()),
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error getting waiting room: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    info!(
        "Host '{}' admitting '{}' to meeting '{}'",
        host_email, body.email, meeting_id
    );

    // Admit participant
    match MeetingParticipant::admit(pool.get_ref(), &meeting_id, &host_email, &body.email).await {
        Ok(participant) => HttpResponse::Ok().json(ParticipantStatusResponse::from(participant)),
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => HttpResponse::Forbidden().json(APIError::not_host()),
        Err(ParticipantError::NotFound) => {
            HttpResponse::NotFound().json(APIError::participant_not_found(&body.email))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error admitting participant: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
        }
    }
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
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
            HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => HttpResponse::Forbidden().json(APIError::not_host()),
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error admitting all participants: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    info!(
        "Host '{}' rejecting '{}' from meeting '{}'",
        host_email, body.email, meeting_id
    );

    // Reject participant
    match MeetingParticipant::reject(pool.get_ref(), &meeting_id, &host_email, &body.email).await {
        Ok(participant) => HttpResponse::Ok().json(ParticipantStatusResponse::from(participant)),
        Err(ParticipantError::MeetingNotFound) => {
            HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::NotHost) => HttpResponse::Forbidden().json(APIError::not_host()),
        Err(ParticipantError::NotFound) => {
            HttpResponse::NotFound().json(APIError::participant_not_found(&body.email))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error rejecting participant: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    // Get status
    match MeetingParticipant::get_status(pool.get_ref(), &meeting_id, &email).await {
        Ok(Some(participant)) => {
            HttpResponse::Ok().json(ParticipantStatusResponse::from(participant))
        }
        Ok(None) => HttpResponse::NotFound().json(APIError::not_in_meeting()),
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error getting status: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    info!("User '{}' leaving meeting '{}'", email, meeting_id);

    // Leave meeting
    match MeetingParticipant::leave(pool.get_ref(), &meeting_id, &email).await {
        Ok(Some(participant)) => {
            HttpResponse::Ok().json(ParticipantStatusResponse::from(participant))
        }
        Ok(None) => HttpResponse::NotFound().json(APIError::not_in_meeting()),
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error leaving meeting: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
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
            HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id))
        }
        Err(ParticipantError::DatabaseError(e)) => {
            error!("Database error: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
        }
        Err(e) => {
            error!("Error getting participants: {}", e);
            HttpResponse::InternalServerError().json(APIError::internal_error(&e.to_string()))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
        }
    };

    info!(
        "User '{}' attempting to delete meeting '{}'",
        email, meeting_id
    );

    // Get meeting and verify ownership
    let meeting = match Meeting::get_by_room_id_async(pool.get_ref(), &meeting_id).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            return HttpResponse::NotFound().json(APIError::meeting_not_found(&meeting_id));
        }
        Err(e) => {
            error!("Database error: {}", e);
            return HttpResponse::InternalServerError()
                .json(APIError::internal_error("Database error"));
        }
    };

    // Verify the user is the owner
    if meeting.creator_id.as_ref() != Some(&email) {
        return HttpResponse::Forbidden().json(APIError::not_owner());
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
            HttpResponse::InternalServerError().json(APIError::internal_error("Database error"))
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
            return HttpResponse::Unauthorized().json(APIError::unauthorized());
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
            return HttpResponse::InternalServerError()
                .json(APIError::internal_error("Database error"));
        }
    };

    // Get total count for this owner
    let total = match Meeting::count_by_owner_async(pool.get_ref(), &email).await {
        Ok(c) => c,
        Err(e) => {
            error!("Database error counting meetings: {}", e);
            return HttpResponse::InternalServerError()
                .json(APIError::internal_error("Database error"));
        }
    };

    // Get participant counts for each meeting
    let mut summaries = Vec::with_capacity(meetings.len());
    for meeting in meetings {
        let participant_count =
            MeetingParticipant::count_admitted(pool.get_ref(), &meeting.room_id)
                .await
                .unwrap_or_default();

        let waiting_count = MeetingParticipant::count_waiting(pool.get_ref(), &meeting.room_id)
            .await
            .unwrap_or_default();

        summaries.push(MeetingSummary {
            meeting_id: meeting.room_id,
            host: meeting.creator_id,
            state: meeting.state.unwrap_or_else(|| "idle".to_string()),
            has_password: meeting.password_hash.is_some(),
            created_at: meeting.created_at.timestamp(),
            participant_count,
            started_at: meeting.started_at.timestamp_millis(),
            ended_at: meeting.ended_at.map(|t| t.timestamp_millis()),
            waiting_count,
        });
    }

    HttpResponse::Ok().json(ListMeetingsResponse {
        meetings: summaries,
        total,
        limit,
        offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{cookie::Cookie, test, App};
    use serial_test::serial;

    /// Get a test database pool from DATABASE_URL
    async fn get_test_pool() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    /// Clean up test data - deletes meeting and related records
    async fn cleanup_test_data(pool: &PgPool, room_id: &str) {
        // Delete participants first (FK constraint)
        let _ = sqlx::query(
            "DELETE FROM meeting_participants WHERE meeting_id IN (SELECT id FROM meetings WHERE room_id = $1)",
        )
        .bind(room_id)
        .execute(pool)
        .await;

        // Delete meeting
        let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
    }

    /// Create a test app with all meeting routes
    async fn create_test_app(
        pool: PgPool,
    ) -> impl actix_web::dev::Service<
        actix_http::Request,
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
    > {
        test::init_service(
            App::new()
                .app_data(web::Data::new(pool))
                .route("/api/v1/meetings", web::post().to(create_meeting))
                .route("/api/v1/meetings", web::get().to(list_meetings))
                .route("/api/v1/meetings/{meeting_id}", web::get().to(get_meeting))
                .route(
                    "/api/v1/meetings/{meeting_id}",
                    web::delete().to(delete_meeting),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/join",
                    web::post().to(join_meeting),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/waiting",
                    web::get().to(get_waiting_room),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/admit",
                    web::post().to(admit_participant),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/admit-all",
                    web::post().to(admit_all_participants),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/reject",
                    web::post().to(reject_participant),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/status",
                    web::get().to(get_my_status),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/leave",
                    web::post().to(leave_meeting),
                )
                .route(
                    "/api/v1/meetings/{meeting_id}/participants",
                    web::get().to(get_participants),
                ),
        )
        .await
    }

    //  CREATE MEETING TESTS

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_success() {
        let pool = get_test_pool().await;
        let room_id = "test-create-meeting-success";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({
                "meeting_id": room_id,
                "attendees": ["user1@example.com", "user2@example.com"],
                "password": "secret123"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201, "Expected 201 Created");

        let body: CreateMeetingResponse = test::read_body_json(resp).await;
        assert_eq!(body.meeting_id, room_id);
        assert_eq!(body.host, "host@example.com");
        assert!(body.has_password);
        assert_eq!(body.attendees.len(), 2);

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_generates_id() {
        let pool = get_test_pool().await;
        let app = create_test_app(pool.clone()).await;

        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({
                "attendees": []
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201, "Expected 201 Created");

        let body: CreateMeetingResponse = test::read_body_json(resp).await;
        assert!(
            !body.meeting_id.is_empty(),
            "Meeting ID should be generated"
        );
        assert_eq!(body.meeting_id.len(), 12, "Generated ID should be 12 chars");

        // Cleanup
        cleanup_test_data(&pool, &body.meeting_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_unauthorized() {
        let pool = get_test_pool().await;
        let app = create_test_app(pool.clone()).await;

        // No email cookie
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .set_json(serde_json::json!({
                "meeting_id": "unauthorized-meeting"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401, "Expected 401 Unauthorized");

        let body: APIError = test::read_body_json(resp).await;
        assert_eq!(body.code, "UNAUTHORIZED");
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_duplicate_id() {
        let pool = get_test_pool().await;
        let room_id = "test-duplicate-meeting";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create first meeting
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({
                "meeting_id": room_id
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        // Try to create duplicate
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({
                "meeting_id": room_id
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 409, "Expected 409 Conflict");

        let body: APIError = test::read_body_json(resp).await;
        assert_eq!(body.code, "MEETING_EXISTS");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_too_many_attendees() {
        let pool = get_test_pool().await;
        let app = create_test_app(pool.clone()).await;

        // Create 101 attendees (over the limit)
        let attendees: Vec<String> = (0..101).map(|i| format!("user{i}@example.com")).collect();

        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({
                "meeting_id": "too-many-attendees",
                "attendees": attendees
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400, "Expected 400 Bad Request");

        let body: APIError = test::read_body_json(resp).await;
        assert_eq!(body.code, "TOO_MANY_ATTENDEES");
    }

    //  GET MEETING TESTS

    #[tokio::test]
    #[serial]
    async fn test_get_meeting_success() {
        let pool = get_test_pool().await;
        let room_id = "test-get-meeting";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create meeting first
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({
                "meeting_id": room_id,
                "password": "secret"
            }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Get meeting
        let req = test::TestRequest::get()
            .uri(&format!("/api/v1/meetings/{room_id}"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: MeetingInfoResponse = test::read_body_json(resp).await;
        assert_eq!(body.meeting_id, room_id);
        assert_eq!(body.host, "host@example.com");
        assert!(body.has_password);

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_get_meeting_not_found() {
        let pool = get_test_pool().await;
        let app = create_test_app(pool.clone()).await;

        let req = test::TestRequest::get()
            .uri("/api/v1/meetings/nonexistent-meeting")
            .cookie(Cookie::new("email", "user@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404, "Expected 404 Not Found");

        let body: APIError = test::read_body_json(resp).await;
        assert_eq!(body.code, "MEETING_NOT_FOUND");
    }

    #[tokio::test]
    #[serial]
    async fn test_get_meeting_unauthorized() {
        let pool = get_test_pool().await;
        let app = create_test_app(pool.clone()).await;

        let req = test::TestRequest::get()
            .uri("/api/v1/meetings/some-meeting")
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401, "Expected 401 Unauthorized");
    }

    #[tokio::test]
    #[serial]
    async fn test_join_meeting_host_activates() {
        let pool = get_test_pool().await;
        let room_id = "test-host-join";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create meeting
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Host joins (should activate meeting)
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "display_name": "Host User" }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: ParticipantStatusResponse = test::read_body_json(resp).await;
        assert_eq!(body.status, "admitted");
        assert!(body.is_host);

        // Verify meeting is now active
        let req = test::TestRequest::get()
            .uri(&format!("/api/v1/meetings/{room_id}"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        let body: MeetingInfoResponse = test::read_body_json(resp).await;
        assert_eq!(body.state, "active");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_join_meeting_attendee_waits() {
        let pool = get_test_pool().await;
        let room_id = "test-attendee-wait";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create and activate meeting (host joins)
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Attendee joins (should be in waiting room)
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .set_json(serde_json::json!({ "display_name": "Attendee" }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: ParticipantStatusResponse = test::read_body_json(resp).await;
        assert_eq!(body.status, "waiting");
        assert!(!body.is_host);

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_join_meeting_not_active() {
        let pool = get_test_pool().await;
        let room_id = "test-join-not-active";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create meeting but don't have host join
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Non-host tries to join inactive meeting
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400, "Expected 400 Bad Request");

        let body: APIError = test::read_body_json(resp).await;
        assert_eq!(body.code, "MEETING_NOT_ACTIVE");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_get_waiting_room_success() {
        let pool = get_test_pool().await;
        let room_id = "test-waiting-room";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create and activate meeting
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Add attendee to waiting room
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Host gets waiting room
        let req = test::TestRequest::get()
            .uri(&format!("/api/v1/meetings/{room_id}/waiting"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: WaitingRoomResponse = test::read_body_json(resp).await;
        assert_eq!(body.meeting_id, room_id);
        assert_eq!(body.waiting.len(), 1);
        assert_eq!(body.waiting[0].email, "attendee@example.com");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_admit_participant_success() {
        let pool = get_test_pool().await;
        let room_id = "test-admit-participant";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Setup: Create meeting, host joins, attendee waits
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Host admits attendee
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/admit"))
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "email": "attendee@example.com" }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: ParticipantStatusResponse = test::read_body_json(resp).await;
        assert_eq!(body.status, "admitted");
        assert!(body.admitted_at.is_some());

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_admit_participant_not_found() {
        let pool = get_test_pool().await;
        let room_id = "test-admit-not-found";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Setup: Create meeting, host joins
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Try to admit non-existent participant
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/admit"))
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "email": "nonexistent@example.com" }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404, "Expected 404 Not Found");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_reject_participant_success() {
        let pool = get_test_pool().await;
        let room_id = "test-reject-participant";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Setup: Create meeting, host joins, attendee waits
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Host rejects attendee
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/reject"))
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "email": "attendee@example.com" }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: ParticipantStatusResponse = test::read_body_json(resp).await;
        assert_eq!(body.status, "rejected");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_admit_all_participants() {
        let pool = get_test_pool().await;
        let room_id = "test-admit-all";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Setup: Create meeting, host joins, multiple attendees wait
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Add multiple attendees
        for i in 1..=3 {
            let req = test::TestRequest::post()
                .uri(&format!("/api/v1/meetings/{room_id}/join"))
                .cookie(Cookie::new("email", format!("attendee{i}@example.com")))
                .to_request();
            let _ = test::call_service(&app, req).await;
        }

        // Host admits all
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/admit-all"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: AdmitAllResponse = test::read_body_json(resp).await;
        assert_eq!(body.admitted_count, 3);
        assert_eq!(body.admitted.len(), 3);

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_leave_meeting_success() {
        let pool = get_test_pool().await;
        let room_id = "test-leave-meeting";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Setup: Create meeting, host joins, attendee joins and is admitted
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/admit"))
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "email": "attendee@example.com" }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Attendee leaves
        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/leave"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: ParticipantStatusResponse = test::read_body_json(resp).await;
        assert_eq!(body.status, "left");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_list_meetings_success() {
        let pool = get_test_pool().await;
        let room_id = "test-list-meetings";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create a meeting
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // List meetings
        let req = test::TestRequest::get()
            .uri("/api/v1/meetings?limit=10&offset=0")
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: ListMeetingsResponse = test::read_body_json(resp).await;
        assert!(body.meetings.iter().any(|m| m.meeting_id == room_id));

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_delete_meeting_success() {
        let pool = get_test_pool().await;
        let room_id = "test-delete-meeting";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create a meeting
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Owner deletes meeting
        let req = test::TestRequest::delete()
            .uri(&format!("/api/v1/meetings/{room_id}"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        // Verify meeting is deleted (soft delete - will return 404)
        let req = test::TestRequest::get()
            .uri(&format!("/api/v1/meetings/{room_id}"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404, "Expected 404 after deletion");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_delete_meeting_not_owner() {
        let pool = get_test_pool().await;
        let room_id = "test-delete-not-owner";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Create a meeting as host
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Non-owner tries to delete
        let req = test::TestRequest::delete()
            .uri(&format!("/api/v1/meetings/{room_id}"))
            .cookie(Cookie::new("email", "other@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 403, "Expected 403 Forbidden");

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_get_participants_success() {
        let pool = get_test_pool().await;
        let room_id = "test-get-participants";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Setup: Create meeting, host joins, attendee joins and is admitted
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "attendee@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/admit"))
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "email": "attendee@example.com" }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Get participants
        let req = test::TestRequest::get()
            .uri(&format!("/api/v1/meetings/{room_id}/participants"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: Vec<ParticipantStatusResponse> = test::read_body_json(resp).await;
        assert_eq!(body.len(), 2); // Host + admitted attendee

        cleanup_test_data(&pool, room_id).await;
    }

    #[tokio::test]
    #[serial]
    async fn test_get_my_status_success() {
        let pool = get_test_pool().await;
        let room_id = "test-get-my-status";
        cleanup_test_data(&pool, room_id).await;

        let app = create_test_app(pool.clone()).await;

        // Setup: Create meeting, host joins
        let req = test::TestRequest::post()
            .uri("/api/v1/meetings")
            .cookie(Cookie::new("email", "host@example.com"))
            .set_json(serde_json::json!({ "meeting_id": room_id }))
            .to_request();
        let _ = test::call_service(&app, req).await;

        let req = test::TestRequest::post()
            .uri(&format!("/api/v1/meetings/{room_id}/join"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();
        let _ = test::call_service(&app, req).await;

        // Get my status
        let req = test::TestRequest::get()
            .uri(&format!("/api/v1/meetings/{room_id}/status"))
            .cookie(Cookie::new("email", "host@example.com"))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200, "Expected 200 OK");

        let body: ParticipantStatusResponse = test::read_body_json(resp).await;
        assert_eq!(body.email, "host@example.com");
        assert!(body.is_host);
        assert_eq!(body.status, "admitted");

        cleanup_test_data(&pool, room_id).await;
    }
}
