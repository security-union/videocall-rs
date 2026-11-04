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
 */

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use videocall_types::validation::is_valid_meeting_id;

use crate::db::meetings as db_meetings;

pub const CAPTIONS_TTL_SECS: u64 = 120;

pub const SERVER_REQUESTER: &str = "meeting-api";

pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(3);

/// Most stale rows one [`reconcile`] pass revokes.
const RECONCILE_BATCH: i64 = 100;

/// Publishes a server revoke makes before giving up.
const PUBLISH_ATTEMPTS: u32 = 3;

const PUBLISH_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Why the server revoked captions on its own; route-originated payloads carry none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevokeReason {
    Ended,
    Deleted,
    Idle,
    Departure,
    NewInstance,
    Reconcile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptionsControlPayload {
    pub room_id: String,
    pub enabled: bool,
    pub revoke: bool,
    pub requested_by: String,
    pub ttl_secs: u64,
    pub generation: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RevokeReason>,
}

/// `None`, with a warning, when `room_id` is not a valid meeting ID.
pub fn captions_control_subject(room_id: &str) -> Option<String> {
    if is_valid_meeting_id(room_id) {
        Some(format!("captions.control.{room_id}"))
    } else {
        tracing::warn!("Not publishing captions control: invalid room id {room_id:?}");
        None
    }
}

/// Publish, then flush, within [`PUBLISH_TIMEOUT`]; an invalid room id publishes nothing.
pub async fn publish_captions_control(
    nats: &async_nats::Client,
    payload: &CaptionsControlPayload,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(subject) = captions_control_subject(&payload.room_id) else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(payload)?;
    tokio::time::timeout(PUBLISH_TIMEOUT, async {
        nats.publish(subject, bytes.into()).await?;
        nats.flush().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await
    .map_err(|_| "captions control publish timed out")?
}

pub fn control_payload(
    room_id: String,
    requested_by: String,
    enabled: bool,
    generation: i64,
) -> CaptionsControlPayload {
    CaptionsControlPayload {
        room_id,
        enabled,
        revoke: !enabled,
        requested_by,
        ttl_secs: if enabled { CAPTIONS_TTL_SECS } else { 0 },
        generation,
        reason: None,
    }
}

/// Publishes a committed control change, retrying [`PUBLISH_ATTEMPTS`] times in all.
pub async fn publish_with_retry(nats: async_nats::Client, payload: CaptionsControlPayload) {
    let (generation, action) = (
        payload.generation,
        if payload.revoke { "revoke" } else { "lease" },
    );
    for attempt in 1..=PUBLISH_ATTEMPTS {
        match publish_captions_control(&nats, &payload).await {
            Ok(()) => return,
            Err(e) if attempt < PUBLISH_ATTEMPTS => {
                tracing::warn!(
                    meeting_id = %payload.room_id,
                    generation,
                    attempt,
                    "NATS publish of captions {action} failed, retrying: {e}"
                );
                tokio::time::sleep(PUBLISH_RETRY_DELAY).await;
            }
            Err(e) => tracing::error!(
                meeting_id = %payload.room_id,
                generation,
                "NATS publish of captions {action} failed after {PUBLISH_ATTEMPTS} attempts; \
                 the bridge lease lapses within {CAPTIONS_TTL_SECS}s: {e}"
            ),
        }
    }
}

/// Publishes a committed server revoke with [`publish_with_retry`].
async fn publish_server_revoke(
    nats: Option<async_nats::Client>,
    room_id: String,
    generation: i64,
    reason: RevokeReason,
    actor: Option<String>,
) {
    tracing::info!(
        meeting_id = %room_id,
        requested_by = SERVER_REQUESTER,
        action = "revoke",
        ?reason,
        actor,
        generation,
        "captions control"
    );
    let Some(nats) = nats else { return };
    let payload = CaptionsControlPayload {
        reason: Some(reason),
        ..control_payload(room_id, SERVER_REQUESTER.to_string(), false, generation)
    };
    publish_with_retry(nats, payload).await
}

pub async fn revoke_if_inactive(
    db: &PgPool,
    nats: Option<&async_nats::Client>,
    meeting_id: i32,
    reason: RevokeReason,
    actor: Option<&str>,
) {
    match db_meetings::revoke_captions_if_inactive(db, meeting_id).await {
        Ok(Some((room_id, generation))) => {
            let actor = actor.map(str::to_owned);
            tokio::spawn(publish_server_revoke(
                nats.cloned(),
                room_id,
                generation,
                reason,
                actor,
            ));
        }
        Ok(None) => {}
        Err(e) => tracing::error!("Failed to revoke captions of meeting {meeting_id}: {e}"),
    }
}

pub fn spawn_revoke_if_inactive(
    db: PgPool,
    nats: Option<async_nats::Client>,
    meeting_id: i32,
    reason: RevokeReason,
    actor: Option<String>,
) {
    tokio::spawn(async move {
        revoke_if_inactive(&db, nats.as_ref(), meeting_id, reason, actor.as_deref()).await
    });
}

/// Revoke captions still on in idle, ended or deleted meetings: a transition
/// whose revoke never ran.
pub async fn reconcile(db: &PgPool, nats: Option<&async_nats::Client>) {
    match db_meetings::revoke_stale_captions(db, RECONCILE_BATCH).await {
        Ok(rows) => {
            for (room_id, generation) in rows {
                let reason = RevokeReason::Reconcile;
                tokio::spawn(publish_server_revoke(
                    nats.cloned(),
                    room_id,
                    generation,
                    reason,
                    None,
                ));
            }
        }
        Err(e) => tracing::error!("Failed to reconcile stale captions: {e}"),
    }
}

pub fn spawn_new_instance_revoke(
    nats: Option<async_nats::Client>,
    room_id: &str,
    activation: &db_meetings::Activation,
) {
    if let Some(generation) = activation.captions_generation() {
        let room_id = room_id.to_string();
        let reason = RevokeReason::NewInstance;
        tokio::spawn(publish_server_revoke(
            nats, room_id, generation, reason, None,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_and_revoke_payloads_carry_generation() {
        let lease = control_payload("r".into(), "h".into(), true, 7);
        assert_eq!(
            serde_json::to_value(&lease).unwrap(),
            serde_json::json!({
                "room_id": "r", "enabled": true, "revoke": false,
                "requested_by": "h", "ttl_secs": CAPTIONS_TTL_SECS, "generation": 7,
            })
        );
        let revoke = CaptionsControlPayload {
            reason: Some(RevokeReason::NewInstance),
            ..control_payload("r".into(), SERVER_REQUESTER.into(), false, 8)
        };
        assert_eq!(
            serde_json::to_value(&revoke).unwrap(),
            serde_json::json!({
                "room_id": "r", "enabled": false, "revoke": true,
                "requested_by": "meeting-api", "ttl_secs": 0, "generation": 8,
                "reason": "new_instance",
            })
        );
    }

    #[test]
    fn control_subject_is_per_valid_room() {
        assert_eq!(
            captions_control_subject("r").as_deref(),
            Some("captions.control.r")
        );
        assert_eq!(captions_control_subject("a.>"), None);
        assert_eq!(captions_control_subject(""), None);
    }
}
