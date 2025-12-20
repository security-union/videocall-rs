use crate::models::meeting::Meeting;
use crate::models::meeting::Meeting as DbMeeting;
use actix_web::{web, HttpResponse};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info};

#[derive(Debug, Clone)]
pub struct MeetingState {
    room_id: String,
    start_time: Arc<AtomicU64>,
    creator_id: Option<String>,
}

impl MeetingState {
    pub fn new(room_id: String, creator_id: Option<String>) -> Self {
        Self {
            room_id,
            start_time: Arc::new(AtomicU64::new(0)),
            creator_id,
        }
    }

    /// Get the start time of the meeting
    pub fn get_start_time(&self) -> u64 {
        self.start_time.load(Ordering::SeqCst)
    }

    /// Set the start time of the meeting
    pub fn set_start_time(&self, timestamp: u64) -> bool {
        self.start_time
            .compare_exchange(0, timestamp, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
    }

    pub async fn save_to_db(
        &self,
        timestamp_ms: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let room_id = self.room_id.clone();
        let creator_id = self.creator_id.clone();

        tokio::task::spawn_blocking(move || {
            // Convert timestamp to DateTime
            let started_at = chrono::DateTime::from_timestamp_millis(timestamp_ms as i64)
                .ok_or("Invalid timestamp")?;

            let creator = creator_id.as_deref().unwrap_or("unknown");

            // Call the SYNCHRONOUS get_or_create, NOT create
            DbMeeting::create(&room_id, started_at, Some(creator.to_string()))?;
            info!("Meeting {} saved to database", room_id);
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await
        .map_err(|e| format!("Spawn blocking failed: {}", e))?
    }

    /// Load the meeting state from the database
    pub async fn load_from_db(&self) -> bool {
        let room_id = self.room_id.clone();
        let start_time = self.start_time.clone();

        let result = tokio::task::spawn_blocking(move || DbMeeting::get_by_room_id(&room_id)).await;

        match result {
            Ok(Ok(Some(meeting))) => {
                let timestamp_ms = meeting.start_time_unix_ms() as u64;
                // info!("Loaded meeting {} with start time {}", room_id, timestamp_ms);
                start_time.store(timestamp_ms, Ordering::SeqCst);
                true
            }
            Ok(Ok(None)) => {
                //info!("No existing meeting for {}", room_id);
                false
            }
            Ok(Err(e)) => {
                error!("Failed to load meeting: {}", e);
                false
            }
            Err(e) => {
                error!("Spawn blocking failed: {}", e);
                false
            }
        }
    }

    pub fn end_meeting(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let room_id = self.room_id.clone();
        DbMeeting::end_meeting(&room_id)
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()))?;
        Ok(())
    }

    pub fn is_creator(&self, user_id: &str) -> bool {
        self.creator_id == Some(user_id.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct MeetingManager {
    meetings: Arc<RwLock<HashMap<String, Arc<MeetingState>>>>,
}

impl Default for MeetingManager {
    fn default() -> Self {
        Self {
            meetings: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

#[derive(Serialize)]
struct MeetingInfo {
    room_id: String,
    started_at: String,
    ended_at: Option<String>,
    duration_ms: i64,
    is_active: bool,
}

impl MeetingManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize meeting when first participant joins
    pub async fn start_meeting(
        &self,
        room_id: &str,
        creator_id: &str,
    ) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
        info!(" start_meeting called for room: {}", room_id);

        let meeting = self
            .get_or_create_meeting(room_id, Some(creator_id.to_string()))
            .await;

        let current_start = meeting.get_start_time();
        if current_start != 0 {
            info!("Meeting {} already started at {}", room_id, current_start);
            return Ok(current_start);
        }

        let now_ms = Utc::now().timestamp_millis() as u64;
        info!(" Setting start time {} for room {}", now_ms, room_id);

        if meeting.set_start_time(now_ms) {
            info!(" Meeting {} started at {}", room_id, now_ms);

            let meeting_clone = meeting.clone();
            tokio::spawn(async move {
                info!(" Attempting to save meeting to database");
                if let Err(e) = meeting_clone.save_to_db(now_ms).await {
                    error!("Failed to save meeting: {}", e);
                } else {
                    info!(" Meeting saved to database successfully");
                }
            });

            Ok(now_ms)
        } else {
            Ok(meeting.get_start_time())
        }
    }

    /// Get or create a meeting
    pub async fn get_or_create_meeting(
        &self,
        room_id: &str,
        creator_id: Option<String>,
    ) -> Arc<MeetingState> {
        {
            let meetings = self.meetings.read().await;
            if let Some(meeting) = meetings.get(room_id) {
                return meeting.clone();
            }
        }

        let mut meetings = self.meetings.write().await;

        if let Some(meetings) = meetings.get(room_id) {
            return meetings.clone();
        }

        let state = Arc::new(MeetingState::new(room_id.to_string(), creator_id));

        state.load_from_db().await;
        meetings.insert(room_id.to_string(), state.clone());
        state
    }

    /// End the meeting and save to the database
    pub async fn end_meeting(
        &self,
        room_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        error!("MeetingManager::end_meeting called for room: {}", room_id);

        let room_id_clone = room_id.to_string();

        match tokio::task::spawn_blocking(move || {
            error!("Inside spawn_blocking, calling DbMeeting::end_meeting");
            DbMeeting::end_meeting(&room_id_clone)
        })
        .await
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                error!("Error ending meeting: {}", e);
                Err(e)
            }
            Err(join_err) => {
                let error_msg = join_err.to_string();
                error!("Join error in end_meeting: {}", error_msg);
                Err(
                    Box::new(std::io::Error::new(std::io::ErrorKind::Other, error_msg))
                        as Box<dyn std::error::Error + Send + Sync>,
                )
            }
        }
    }

    pub async fn get_meeting_start_time(&self, room_id: &str) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>>  {
        let room_id = room_id.to_string();

        tokio::task::spawn_blocking(move || Meeting::get_meeting_start_time(&room_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
    }

    pub async fn get_meeting_info(&self, room_id: &str) -> Result<HttpResponse, actix_web::Error> {
        let room_id_clone = room_id.to_string();

        let meeting = tokio::task::spawn_blocking(move || Meeting::get_by_room_id(&room_id_clone))
            .await
            .map_err(|e| actix_web::error::ErrorInternalServerError(e))?;

        match meeting {
            Ok(Some(meeting)) => {
                let meeting_info = MeetingInfo {
                    room_id: meeting.room_id.clone(),
                    started_at: meeting.started_at.to_rfc3339(),
                    ended_at: meeting.ended_at.map(|dt| dt.to_rfc3339()),
                    duration_ms: meeting.current_durtion_ms(),
                    is_active: meeting.is_active(),
                };

                Ok(HttpResponse::Ok().json(meeting_info))
            }
            Ok(None) => Ok(HttpResponse::NotFound().json(serde_json::json!("error: Not found"))),
            Err(e) => Err(actix_web::error::ErrorInternalServerError(e)),
        }
    }


    pub async fn get_meeting_info_route(
        room_id: web::Path<String>,
        meeting_manager: web::Data<MeetingManager>,
    ) -> Result<HttpResponse, actix_web::Error> {
        meeting_manager.get_meeting_info(&room_id).await
    }

    pub async fn is_creator(&self, room_id: &str, user_id: &str) -> bool {
        let meetings = self.meetings.read().await;
        meetings
            .get(room_id)
            .map(|m| m.is_creator(user_id))
            .unwrap_or(false)
    }

    pub fn configure(&self, cfg: &mut web::ServiceConfig) {
        let manager = Arc::new(self.clone());
        cfg.app_data(web::Data::new(manager)).service(
            web::resource("/api/meeting/{room_id}/info")
                .route(web::get().to(Self::get_meeting_info_route)),
        );
    }
}
