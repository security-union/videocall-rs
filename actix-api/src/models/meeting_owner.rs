use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::get_connection_query;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeetingOwner {
    pub id: i32,
    pub meeting_id: String,
    pub user_id: String,
    pub delegated_by: Option<String>,
    pub delegated_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MeetingOwner {
    pub fn is_owner(meeting_id: &str, user_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
        let mut conn = get_connection_query()?;

        let query = conn.query(
            "SELECT COUNT(*) as count FROM meeting_owners WHERE meeting_id = $1 AND user_id = $2 AND is_active = true",
            &[&meeting_id, &user_id]
        )?;

        if query.is_empty() {
            return Ok(false);
        }

        let count: i64 = query[0].get("count");

        Ok(count > 0)
    }

    pub fn create(
        meeting_id: &str,
        user_id: &str,
        delegated_by: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut conn = get_connection_query()?;

        let query = conn.query_one(
            "INSERT INTO meeting_owners (meeting_id, user_id, delegated_by, delegated_at, is_active) 
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (meeting_id, user_id) DO UPDATE
                SET is_active = true, updated_at = NOW()
                RETURNING id, meeting_id, user_id, delegated_by, delegated_at, is_active, created_at, updated_at    
            ",
            &[&meeting_id, &user_id, &delegated_by, &Utc::now()]
        )?;

        Ok(MeetingOwner {
            id: query.get("id"),
            meeting_id: query.get("meeting_id"),
            user_id: query.get("user_id"),
            delegated_by: query.get("delegated_by"),
            delegated_at: query.get("delegated_at"),
            is_active: query.get("is_active"),
            created_at: query.get("created_at"),
            updated_at: query.get("updated_at"),
        })
    }

    pub fn get_owners(meeting_id: &str) -> Result<Vec<Self>, Box<dyn std::error::Error>> {
        let mut conn = get_connection_query()?;

        let query = conn.query(
            "SELECT * FROM meeting_owners WHERE meeting_id = $1 AND is_active = true ORDER BY created_at ASC",
            &[&meeting_id]
        )?;

        Ok(query
            .iter()
            .map(|row| MeetingOwner {
                id: row.get("id"),
                meeting_id: row.get("meeting_id"),
                user_id: row.get("user_id"),
                delegated_by: row.get("delegated_by"),
                delegated_at: row.get("delegated_at"),
                is_active: row.get("is_active"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect())
    }
}
