use crate::models::meeting::Meeting as DbMeeting;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use chrono::Utc;
use tokio::sync::RwLock;
use tracing::{error, info};

#[derive(Debug, Clone)]
pub struct MeetingState {
    room_id: String,
    start_time: Arc<AtomicU64>,
}

impl MeetingState {
    pub fn new(room_id: String) -> Self {
        Self {
            room_id,
            start_time: Arc::new(AtomicU64::new(0)),
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

    /// Load the meeting state from the database
    pub async fn load_from_db(&self) -> bool {
        let room_id = self.room_id.clone();
        let start_time = self.start_time.clone();

        let result = tokio::task::spawn_blocking(move || {
            DbMeeting::get_by_room_id(&room_id)
        }).await; 
        
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

    pub async fn end_meeting(&self) -> Result<(), Box<dyn std::error::Error>> {
        let room_id = self.room_id.clone();
        
        DbMeeting::end_meeting(&room_id).await?;
        Ok(())
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

impl MeetingManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Initialize meeting when first participant joins
    pub async fn start_meeting(&self, room_id: &str) -> Result<u64, Box<dyn std::error::Error>> {
        let meeting = self.get_or_create_meeting(room_id).await; 

        let current_start = meeting.get_start_time();
        if current_start != 0 {
            return Ok(current_start)
        }

        let now_ms = Utc::now().timestamp_millis() as u64; 

        if meeting.set_start_time(now_ms) {
            let room_id_clone = room_id.to_string();
            let meeting_clone = meeting.clone();

            tokio::task::spawn_blocking(async move || {
                if let Err(e) = meeting_clone.end_meeting().await {
                    error!("Failed to persist meeting {}: {}", room_id_clone, e);
                }
            });

            Ok(now_ms)
        }
        else {
            Ok(meeting.get_start_time())
        }
    }

    /// Get or create a meeting
    pub async fn get_or_create_meeting(&self, room_id: &str) -> Arc<MeetingState> {
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

        let state = Arc::new(MeetingState::new(room_id.to_string()));

        state.load_from_db().await;
        meetings.insert(room_id.to_string(), state.clone());
        state
    }

    /// End the meeting and save to the database
    pub async fn end_meeting(&self, room_id: &str) -> Result<(), Box<dyn std::error::Error>> {
        let meeting = self.get_or_create_meeting(room_id).await;

        let meeting_clone = meeting.clone();
        tokio::task::spawn_blocking(async move || {
            if let Err(e) = meeting_clone.end_meeting().await {
                error!("Failed to end meeting in database: {}", e);
            }
        });

        Ok(())
    }

    /// Remove meeting from memory (but keeps it in the database)
    pub async fn cleanup_meeting(&self, room_id: &str) {
        let mut meetings = self.meetings.write().await;
        if meetings.remove(room_id).is_some() {
            info!("Meeting {} removed from memory", room_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_meeting_state() {
        let state = MeetingState::new("test-room".to_string());
        assert_eq!(state.get_start_time(), 0);

        // First set should succeed
        assert!(state.set_start_time(1000));
        assert_eq!(state.get_start_time(), 1000);

        // Second set should fail
        assert!(!state.set_start_time(2000));
        assert_eq!(state.get_start_time(), 1000);
    }

    #[tokio::test]
    async fn test_meeting_manager() {
        let manager = MeetingManager::default();
        let room_id = "test-room";

        // Get or create a meeting
        let meeting1 = manager.get_or_create_meeting(room_id).await;
        assert_eq!(meeting1.get_start_time(), 0);

        // Set start time
        assert!(meeting1.set_start_time(1000));

        // Get the same meeting again
        let meeting2 = manager.get_or_create_meeting(room_id).await;
        assert_eq!(meeting2.get_start_time(), 1000);

        // Cleanup
        manager.cleanup_meeting(room_id).await;
    }
}
