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

//! Meeting API handlers for creating and managing meetings.
//!
//! This module implements the Create Meeting API as per the requirements:
//! - Host and attendees must be authenticated IDs with the identity provider
//! - Meetings are identified by a unique ID
//! - Meeting metadata is stored at create request time (not at start time)

use actix_web::{error, post, web, Error, HttpRequest, HttpResponse};
use bcrypt::{hash, DEFAULT_COST};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::{error as log_error, info};
use uuid::Uuid;
use videocall_types::FeatureFlags;

use crate::constants::VALID_ID_PATTERN;
use crate::models::meeting::Meeting;
use crate::models::meeting_attendee::{MeetingAttendee, MAX_ATTENDEES};
use crate::models::meeting_owner::MeetingOwner;

/// Request body for creating a new meeting
#[derive(Debug, Deserialize)]
pub struct CreateMeetingRequest {
    /// Optional meeting ID. If not provided, the system generates one.
    #[serde(rename = "meetingId")]
    pub meeting_id: Option<String>,

    /// Optional list of attendee IDs (up to 100)
    pub attendees: Option<Vec<String>>,

    /// Optional meeting password
    pub password: Option<String>,
}

/// Meeting metadata schema as per requirements
#[derive(Debug, Serialize, Deserialize)]
pub struct MeetingMetadata {
    /// Host user ID
    pub host: String,

    /// Creation timestamp (epoch time UTC)
    #[serde(rename = "createdTimestamp")]
    pub created_timestamp: i64,

    /// Meeting state: not_started, active, or ended
    pub state: String,

    /// Optional list of attendee IDs
    pub attendees: Vec<String>,

    /// Whether meeting has a password (don't expose the actual hash)
    #[serde(rename = "hasPassword")]
    pub has_password: bool,
}

/// Successful response for creating a meeting
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateMeetingResponse {
    /// The meeting ID (either provided or system-generated)
    #[serde(rename = "meetingId")]
    pub meeting_id: String,

    /// Meeting metadata
    pub metadata: MeetingMetadata,
}

/// Error response for create meeting API
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateMeetingError {
    pub error: String,
    pub code: String,
}

impl CreateMeetingError {
    pub fn authentication_error() -> Self {
        Self {
            error: "Authentication required".to_string(),
            code: "AUTH_REQUIRED".to_string(),
        }
    }

    pub fn meeting_exists(meeting_id: &str) -> Self {
        Self {
            error: format!("Meeting already exists: {meeting_id}"),
            code: "MEETING_EXISTS".to_string(),
        }
    }

    pub fn invalid_meeting_id() -> Self {
        Self {
            error: "Invalid meeting ID format. Only alphanumeric characters, underscores, and hyphens are allowed.".to_string(),
            code: "INVALID_MEETING_ID".to_string(),
        }
    }

    pub fn too_many_attendees() -> Self {
        Self {
            error: format!("Too many attendees. Maximum allowed: {MAX_ATTENDEES}"),
            code: "TOO_MANY_ATTENDEES".to_string(),
        }
    }

    pub fn invalid_attendee_id(attendee: &str) -> Self {
        Self {
            error: format!("Invalid attendee ID format: {attendee}"),
            code: "INVALID_ATTENDEE_ID".to_string(),
        }
    }

    pub fn internal_error(msg: &str) -> Self {
        Self {
            error: format!("Internal server error: {msg}"),
            code: "INTERNAL_ERROR".to_string(),
        }
    }

    pub fn feature_disabled() -> Self {
        Self {
            error: "Meeting management feature is not enabled".to_string(),
            code: "FEATURE_DISABLED".to_string(),
        }
    }
}

/// Generate a unique meeting ID
fn generate_meeting_id() -> String {
    Uuid::new_v4().to_string()
}

/// Validate a meeting ID against the allowed pattern
fn is_valid_id(id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    let re = Regex::new(VALID_ID_PATTERN).unwrap();
    re.is_match(id)
}

/// Hash a password using bcrypt
fn hash_password(password: &str) -> Result<String, bcrypt::BcryptError> {
    hash(password, DEFAULT_COST)
}

/// Create a new meeting
///
/// POST /api/meetings
///
/// Request body:
/// ```json
/// {
///   "meetingId": "optional-custom-id",
///   "attendees": ["user1", "user2"],
///   "password": "optional-password"
/// }
/// ```
///
/// Response:
/// ```json
/// {
///   "meetingId": "meeting-id",
///   "metadata": {
///     "host": "host-user-id",
///     "createdTimestamp": 1234567890,
///     "state": "not_started",
///     "attendees": ["user1", "user2"],
///     "hasPassword": true
///   }
/// }
/// ```
#[post("/api/meetings")]
pub async fn create_meeting(
    req: HttpRequest,
    pool: web::Data<PgPool>,
    body: web::Json<CreateMeetingRequest>,
) -> Result<HttpResponse, Error> {
    // Check if meeting management feature is enabled
    if !FeatureFlags::meeting_management_enabled() {
        return Ok(HttpResponse::ServiceUnavailable().json(CreateMeetingError::feature_disabled()));
    }

    // Authenticate the host
    let host_id = req
        .cookie("email")
        .map(|c| c.value().to_string())
        .ok_or_else(|| {
            log_error!("Create meeting: No session cookie found");
            error::ErrorUnauthorized(
                serde_json::to_string(&CreateMeetingError::authentication_error()).unwrap(),
            )
        })?;

    // Clean the host ID (replace spaces with underscores)
    let host_id = host_id.replace(' ', "_");

    // Validate the host ID format
    if !is_valid_id(&host_id) {
        log_error!("Create meeting: Invalid host ID format: {}", host_id);
        return Ok(HttpResponse::BadRequest().json(CreateMeetingError::invalid_meeting_id()));
    }

    info!("Create meeting request from host: {}", host_id);

    // Determine meeting ID (provided or generated)
    let meeting_id = match &body.meeting_id {
        Some(id) => {
            let clean_id = id.replace(' ', "_");
            // Validate the provided meeting ID
            if !is_valid_id(&clean_id) {
                log_error!("Create meeting: Invalid meeting ID format: {}", id);
                return Ok(
                    HttpResponse::BadRequest().json(CreateMeetingError::invalid_meeting_id())
                );
            }
            clean_id.to_string()
        }
        None => generate_meeting_id(),
    };

    // Check if meeting ID already exists
    match Meeting::exists_async(&pool, &meeting_id).await {
        Ok(true) => {
            log_error!("Create meeting: Meeting already exists: {}", meeting_id);
            return Ok(
                HttpResponse::Conflict().json(CreateMeetingError::meeting_exists(&meeting_id))
            );
        }
        Ok(false) => {}
        Err(e) => {
            log_error!("Create meeting: Database error checking existence: {}", e);
            return Ok(HttpResponse::InternalServerError().json(
                CreateMeetingError::internal_error("Failed to check meeting existence"),
            ));
        }
    }

    // Validate attendees list
    let attendees = body.attendees.clone().unwrap_or_default();

    if attendees.len() > MAX_ATTENDEES {
        log_error!(
            "Create meeting: Too many attendees: {} > {}",
            attendees.len(),
            MAX_ATTENDEES
        );
        return Ok(HttpResponse::BadRequest().json(CreateMeetingError::too_many_attendees()));
    }

    // Validate each attendee ID
    let cleaned_attendees: Vec<String> = attendees.iter().map(|a| a.replace(' ', "_")).collect();

    for attendee in &cleaned_attendees {
        if !is_valid_id(attendee) {
            log_error!("Create meeting: Invalid attendee ID: {}", attendee);
            return Ok(
                HttpResponse::BadRequest().json(CreateMeetingError::invalid_attendee_id(attendee))
            );
        }
    }

    // Hash password if provided
    let password_hash = match &body.password {
        Some(pw) if !pw.is_empty() => match hash_password(pw) {
            Ok(hashed) => Some(hashed),
            Err(e) => {
                log_error!("Create meeting: Failed to hash password: {}", e);
                return Ok(HttpResponse::InternalServerError().json(
                    CreateMeetingError::internal_error("Failed to process password"),
                ));
            }
        },
        _ => None,
    };

    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            log_error!("Create meeting: Failed to start transaction: {}", e);
            return Ok(HttpResponse::InternalServerError().json(
                CreateMeetingError::internal_error("Failed to start transaction"),
            ));
        }
    };

    // Create the meeting in the database
    let meeting_result =
        Meeting::create_meeting_api(&mut *tx, &meeting_id, &host_id, password_hash.as_deref())
            .await;
    let meeting = match meeting_result {
        Ok(m) => m,
        Err(e) => {
            log_error!("Create meeting: Failed to create meeting: {}", e);
            let _ = tx.rollback().await;
            return Ok(HttpResponse::InternalServerError().json(
                CreateMeetingError::internal_error("Failed to create meeting"),
            ));
        }
    };

    // Create the meeting owner record
    let owner_result = MeetingOwner::create(&mut *tx, &meeting_id, &host_id, None).await;
    if let Err(e) = owner_result {
        log_error!("Create meeting: Failed to create meeting owner: {}", e);
        let _ = tx.rollback().await;
        return Ok(
            HttpResponse::InternalServerError().json(CreateMeetingError::internal_error(
                "Failed to create meeting owner",
            )),
        );
    }

    // Add attendees to the meeting
    if !cleaned_attendees.is_empty() {
        if let Err(e) =
            MeetingAttendee::add_attendees(&mut *tx, &meeting_id, &cleaned_attendees).await
        {
            log_error!("Create meeting: Failed to add attendees: {}", e);
            let _ = tx.rollback().await;
            return Ok(HttpResponse::InternalServerError().json(
                CreateMeetingError::internal_error("Failed to add attendees"),
            ));
        }
    }

    if let Err(e) = tx.commit().await {
        log_error!("Create meeting: Failed to commit transaction: {}", e);
        return Ok(
            HttpResponse::InternalServerError().json(CreateMeetingError::internal_error(
                "Failed to commit transaction",
            )),
        );
    }

    // Build the response
    let response = CreateMeetingResponse {
        meeting_id: meeting.room_id.clone(),
        metadata: MeetingMetadata {
            host: host_id.to_owned(),
            created_timestamp: meeting.created_at.timestamp(),
            state: meeting
                .meeting_status
                .unwrap_or_else(|| "not_started".to_string()),
            attendees: cleaned_attendees,
            has_password: password_hash.is_some(),
        },
    };

    info!(
        "Meeting created successfully: {} by host {}",
        meeting.room_id, response.metadata.host
    );

    Ok(HttpResponse::Created().json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test, web, App};
    use serial_test::serial;
    use sqlx::PgPool;

    async fn get_test_pool() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    async fn cleanup_test_meeting(pool: &PgPool, meeting_id: &str) {
        let _ = sqlx::query("DELETE FROM meeting_attendees WHERE meeting_id = $1")
            .bind(meeting_id)
            .execute(pool)
            .await;

        let _ = sqlx::query("DELETE FROM meeting_owners WHERE meeting_id = $1")
            .bind(meeting_id)
            .execute(pool)
            .await;

        let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
            .bind(meeting_id)
            .execute(pool)
            .await;
    }

    fn setup_meeting_mgmt_enabled() {
        FeatureFlags::set_meeting_management_override(true);
    }

    fn teardown_meeting_mgmt() {
        FeatureFlags::clear_meeting_management_override();
    }

    #[tokio::test]
    async fn test_is_valid_id_valid_alphanumeric() {
        assert!(is_valid_id("validid123"));
        assert!(is_valid_id("ValidID123"));
        assert!(is_valid_id("valid_id_123"));
        assert!(is_valid_id("valid-id-123"));
        assert!(is_valid_id("valid_id-123"));
    }

    #[tokio::test]
    async fn test_is_valid_id_invalid_characters() {
        assert!(!is_valid_id("invalid id")); // space
        assert!(!is_valid_id("invalid@id")); // @
        assert!(!is_valid_id("invalid!id")); // !
        assert!(!is_valid_id("invalid#id")); // #
        assert!(!is_valid_id("")); // empty
    }

    #[tokio::test]
    async fn test_generate_meeting_id_is_uuid() {
        let id = generate_meeting_id();
        // UUID v4 format: 8-4-4-4-12 hex chars
        assert_eq!(id.len(), 36);
        assert!(id.contains('-'));
    }

    #[tokio::test]
    async fn test_hash_password_produces_bcrypt_hash() {
        let password = "test_password";
        let hash = hash_password(password).unwrap();

        assert!(hash.starts_with("$2"));
        assert!(hash.len() > 50);
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_success_with_custom_id() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;
        let meeting_id = "test-meeting-custom-id";

        cleanup_test_meeting(&pool, meeting_id).await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": meeting_id,
                "attendees": ["user1", "user2"],
                "password": "secret123"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        let body: CreateMeetingResponse = test::read_body_json(resp).await;
        assert_eq!(body.meeting_id, meeting_id);
        assert_eq!(body.metadata.host, "host_user");
        assert_eq!(body.metadata.state, "not_started");
        assert_eq!(body.metadata.attendees.len(), 2);
        assert!(body.metadata.has_password);

        cleanup_test_meeting(&pool, meeting_id).await;
        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_success_with_generated_id() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({}))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        let body: CreateMeetingResponse = test::read_body_json(resp).await;
        assert_eq!(body.meeting_id.len(), 36);
        assert_eq!(body.meeting_id.len(), 36);
        assert!(!body.metadata.has_password);
        assert!(body.metadata.attendees.is_empty());

        cleanup_test_meeting(&pool, &body.meeting_id).await;
        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_without_password() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;
        let meeting_id = "test-meeting-no-password";

        cleanup_test_meeting(&pool, meeting_id).await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": meeting_id
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        let body: CreateMeetingResponse = test::read_body_json(resp).await;
        assert!(!body.metadata.has_password);

        cleanup_test_meeting(&pool, meeting_id).await;
        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_unauthorized_no_cookie() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .set_json(serde_json::json!({
                "meetingId": "test-meeting"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 401);

        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_duplicate_id_returns_conflict() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;
        let meeting_id = "test-meeting-duplicate";

        cleanup_test_meeting(&pool, meeting_id).await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req1 = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": meeting_id
            }))
            .to_request();

        let resp1 = test::call_service(&app, req1).await;
        assert_eq!(resp1.status(), 201);

        let req2 = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": meeting_id
            }))
            .to_request();

        let resp2 = test::call_service(&app, req2).await;
        assert_eq!(resp2.status(), 409);

        let body: CreateMeetingError = test::read_body_json(resp2).await;
        assert_eq!(body.code, "MEETING_EXISTS");

        cleanup_test_meeting(&pool, meeting_id).await;
        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_invalid_meeting_id() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": "invalid@meeting#id!"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);

        let body: CreateMeetingError = test::read_body_json(resp).await;
        assert_eq!(body.code, "INVALID_MEETING_ID");

        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_invalid_attendee_id() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": "valid-meeting-id",
                "attendees": ["valid_user", "invalid@user!"]
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);

        let body: CreateMeetingError = test::read_body_json(resp).await;
        assert_eq!(body.code, "INVALID_ATTENDEE_ID");

        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_feature_disabled() {
        teardown_meeting_mgmt();
        FeatureFlags::set_meeting_management_override(false);

        let pool = get_test_pool().await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": "test-meeting"
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 503);

        let body: CreateMeetingError = test::read_body_json(resp).await;
        assert_eq!(body.code, "FEATURE_DISABLED");

        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_spaces_converted_to_underscores() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;
        let meeting_id = "test_meeting_spaces";

        cleanup_test_meeting(&pool, meeting_id).await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        // Meeting ID with spaces should be converted to underscores
        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": "test meeting spaces",
                "attendees": ["user one", "user two"]
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        let body: CreateMeetingResponse = test::read_body_json(resp).await;
        assert_eq!(body.meeting_id, "test_meeting_spaces");
        assert!(body.metadata.attendees.contains(&"user_one".to_string()));
        assert!(body.metadata.attendees.contains(&"user_two".to_string()));

        cleanup_test_meeting(&pool, meeting_id).await;
        teardown_meeting_mgmt();
    }

    #[tokio::test]
    #[serial]
    async fn test_create_meeting_metadata_state_is_not_started() {
        setup_meeting_mgmt_enabled();
        let pool = get_test_pool().await;
        let meeting_id = "test-meeting-state-check";

        cleanup_test_meeting(&pool, meeting_id).await;

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(pool.clone()))
                .service(create_meeting),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/api/meetings")
            .cookie(actix_web::cookie::Cookie::new("email", "host_user"))
            .set_json(serde_json::json!({
                "meetingId": meeting_id
            }))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 201);

        let body: CreateMeetingResponse = test::read_body_json(resp).await;
        assert_eq!(body.metadata.state, "not_started");

        cleanup_test_meeting(&pool, meeting_id).await;
        teardown_meeting_mgmt();
    }
}
