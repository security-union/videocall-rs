use crate::models::meeting::Meeting;
use crate::models::meeting::Meeting as DbMeeting;
use actix_web::{web, HttpResponse};
use chrono::Utc;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

        // Convert timestamp to DateTime
        let started_at = chrono::DateTime::from_timestamp_millis(timestamp_ms as i64)
            .ok_or("Invalid timestamp")?;

        let creator = creator_id.as_deref().unwrap_or("unknown");

        let result = DbMeeting::create(&room_id, started_at, Some(creator.to_string())).await;

        // Call the async-synchronous create
        match result {
            Ok(_) => {
                info!("Meeting {} saved to database", room_id);
                Ok(())
            }
            Err(e) => {
                info!("Meeting {} saved to database", room_id);
                Err(e)
            }
        }
    }

    /// Load the meeting state from the database
    pub async fn load_from_db(&self) -> bool {
        let room_id = self.room_id.clone();
        let start_time = self.start_time.clone();

        //   let result = tokio::task::spawn_blocking(move || DbMeeting::get_by_room_id(&room_id)).await;

        match DbMeeting::get_by_room_id(&room_id).await {
            Ok(Some(meeting)) => {
                let timestamp_ms = meeting.start_time_unix_ms() as u64;
                // info!("Loaded meeting {} with start time {}", room_id, timestamp_ms);
                start_time.store(timestamp_ms, Ordering::SeqCst);
                true
            }
            Ok(None) => {
                //info!("No existing meeting for {}", room_id);
                false
            }
            Err(e) => {
                error!("Failed to load meeting: {}", e);
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

#[derive(Debug, Clone, Default)]
pub struct MeetingManager {
    // meetings: Arc<RwLock<HashMap<String, Arc<MeetingState>>>>,
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

        match self.get_by_room_id(room_id).await? {
            Some(m) => Ok(m.start_time_unix_ms() as u64),
            None => {
                let now = Utc::now();
                match Meeting::create(room_id, now, Some(creator_id.to_string())).await {
                    Ok(_) => Ok(now.timestamp_millis() as u64),
                    Err(e) => {
                        error!("Failed to create meeting: {}", e);
                        Err(e)
                    }
                }
            }
        }
    }

    pub async fn get_by_room_id(
        &self,
        room_id: &str,
    ) -> Result<Option<Meeting>, Box<dyn std::error::Error + Send + Sync>> {
        DbMeeting::get_by_room_id(room_id).await
    }

    /// Get or create a meeting
    pub async fn get_or_create_meeting(
        &self,
        room_id: &str,
        creator_id: Option<String>,
    ) -> Arc<MeetingState> {
        let now = Utc::now();
        let start_time = Arc::new(AtomicU64::new(now.timestamp_millis() as u64));

        match Meeting::get_by_room_id(room_id).await {
            Ok(Some(meeting)) => {
                Arc::new(MeetingState {
                    room_id: meeting.room_id.clone(),
                    creator_id: meeting.creator_id,
                    start_time,
                })
            }
            Ok(None) => {
                if let Ok(meeting) = Meeting::create(room_id, now, creator_id.clone()).await {
                    Arc::new(MeetingState {
                        room_id: meeting.room_id.clone(),
                        creator_id: meeting.creator_id,
                        start_time,
                    })
                } else {
                    Arc::new(MeetingState {
                        room_id: room_id.to_string(),
                        creator_id,
                        start_time,
                    })
                }
            }
            Err(error) => {
                error!("Error fetching meeting from database: {}", error);
                Arc::new(MeetingState {
                    room_id: room_id.to_string(),
                    creator_id,
                    start_time,
                })
            }
        }
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
                Err(Box::new(std::io::Error::other(error_msg))
                    as Box<dyn std::error::Error + Send + Sync>)
            }
        }
    }

    pub async fn get_meeting_start_time(
        &self,
        room_id: &str,
    ) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>> {
        let room_id = room_id.to_string();

        tokio::task::spawn_blocking(move || Meeting::get_meeting_start_time(&room_id))
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
    }

    pub async fn get_meeting_info(&self, room_id: &str) -> Result<HttpResponse, actix_web::Error> {
        match Meeting::get_by_room_id(room_id).await {
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
        match Meeting::get_by_room_id(room_id).await {
            Ok(Some(meeting)) => {
                if let Some(creator_id) = &meeting.creator_id {
                    creator_id == user_id
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}
