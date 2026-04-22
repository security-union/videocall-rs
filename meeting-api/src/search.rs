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

//! Push-based indexing of meeting documents to the external SearchV2 middleware.
//!
//! Videocall meetings are modelled inside SearchV2 as the **CC** product's
//! `cc-meetings` content source — the same content source and shape produced
//! by the built-in pull-based `VideocallCrawlerDriver` in the
//! `opensearch-middleware` repo.  This module is the push-based counterpart:
//! the meeting-api pushes a document whenever a meeting is created, updated,
//! ended, or when its participant roster changes, so search stays near-real-time.
//!
//! **Endpoints** (all PUT/DELETE-against the content-push API):
//!
//! ```text
//! PUT    {base}/contentsources/cs-cc-meetings/documents/cc-meetings:{room_id}
//! DELETE {base}/contentsources/cs-cc-meetings/documents/cc-meetings:{room_id}
//! ```
//!
//! Every outbound request carries:
//!
//! * `Authorization: Bearer <SEARCH_API_TOKEN>` — a middleware admin token
//!   (`pushadmin` / `searchadmin` role).
//! * `X-App-Type: CC` — scopes the request to the CC product so the middleware
//!   picks the right ACL / mapping registry.
//!
//! **Optional service.**  When [`SearchConfig`] is `None` (either env var
//! unset) every function in this module becomes a silent no-op — search push
//! is best-effort and never blocks the API response.  This mirrors the
//! `Option<&async_nats::Client>` pattern used by [`crate::nats_events`].
//!
//! **ACLs.**  The CC query filter (`buildCcFilter` on the middleware side)
//! matches on `acls` entries in two forms — `user:<principal>` and the bare
//! `<principal>`.  Because the videocall schema only stores a single
//! `user_id` string (no separate email column), we emit both forms for every
//! principal so whichever form SearchV2 associates with the caller matches.

use crate::config::SearchConfig;
use crate::db::meetings::MeetingRow;
use serde_json::json;
use sqlx::PgPool;

/// SearchV2 content source that holds CC / videocall meeting documents.
/// Shared with the built-in `VideocallCrawlerDriver` — both pull and push
/// paths write to the same index, which keeps the two interchangeable.
const CONTENT_SOURCE_ID: &str = "cs-cc-meetings";
/// Document type tag emitted on every indexed meeting.
const DOC_TYPE: &str = "cc-meetings";
/// Application scope header.  Tells the middleware's `app-context` middleware
/// to resolve this request under the CC product's ACL / filter registry.
const APP_TYPE: &str = "CC";

/// Subset of a `meeting_participants` row relevant to search indexing.
///
/// We only need identity (for ACLs and the top-level `participants` array)
/// plus display metadata for the `documentObject`.  See
/// [`crate::db::participants::list_for_search`] for the producer.
#[derive(Debug, Clone)]
pub struct ParticipantAcl {
    pub user_id: String,
    pub display_name: Option<String>,
    pub is_host: bool,
    pub status: String,
    pub joined_at: Option<chrono::DateTime<chrono::Utc>>,
    pub admitted_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Load the participant roster for a SearchV2 push, logging DB errors and
/// returning an empty Vec on failure so callers stay fire-and-forget.
///
/// Pushing a doc with an incomplete ACL list is better than blocking an API
/// response on an OpenSearch indexer quirk — the next participant mutation
/// will re-push the full list anyway.
pub async fn load_participants(pool: &PgPool, meeting_id: i32) -> Vec<ParticipantAcl> {
    match crate::db::participants::list_for_search(pool, meeting_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                "Failed to load participants for SearchV2 push (meeting_id={meeting_id}): {e}"
            );
            Vec::new()
        }
    }
}

/// Spawn a background task that pushes the latest meeting document to
/// SearchV2.  The task:
///
/// * returns immediately if [`SearchConfig`] is `None` (SearchV2 disabled);
/// * re-fetches the meeting row from Postgres so the push always reflects
///   the freshest state (handles the case where a concurrent write landed
///   between the caller's DB write and the push);
/// * loads the participant roster via [`load_participants`];
/// * calls [`push_meeting`] with the result.
///
/// This is the idiomatic entry point used by every route handler that
/// mutates a meeting or its participant set.  Takes the room_id by value so
/// it can be moved into the `'static` task body.
pub fn spawn_repush(state: &crate::state::AppState, meeting_id: i32, room_id: String) {
    // Skip the spawn entirely when SearchV2 isn't configured — otherwise we'd
    // do a wasted meeting re-fetch + participant-roster query on every
    // participant mutation (join/leave/admit/etc.) just to throw the result
    // away at the end.
    if state.search.is_none() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let meeting = match crate::db::meetings::get_by_room_id(&state.db, &room_id).await {
            Ok(Some(m)) => m,
            // Meeting hard-deleted between the write and the spawn — nothing
            // to push.  The matching `delete_meeting_doc` call handles the
            // index cleanup.
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(
                    "SearchV2 re-push aborted: failed to re-fetch meeting {room_id}: {e}"
                );
                return;
            }
        };
        let participants = load_participants(&state.db, meeting_id).await;
        push_meeting(
            state.search.as_ref(),
            &state.http_client,
            &meeting,
            &participants,
        )
        .await;
    });
}

/// Push a meeting document to SearchV2 after create / update / end / participant-change.
///
/// Fire-and-forget: failures are logged at WARN and never block the API
/// response.  When `cfg` is `None`, the call returns immediately and nothing
/// is sent over the wire.
pub async fn push_meeting(
    cfg: Option<&SearchConfig>,
    http: &reqwest::Client,
    meeting: &MeetingRow,
    participants: &[ParticipantAcl],
) {
    let Some(cfg) = cfg else {
        return;
    };

    let doc_id = format!("{DOC_TYPE}:{}", meeting.room_id);
    let url = format!(
        "{}/contentsources/{}/documents/{}",
        cfg.base_url.trim_end_matches('/'),
        CONTENT_SOURCE_ID,
        doc_id,
    );

    let body = build_meeting_body(meeting, participants);

    let result = http
        .put(&url)
        .bearer_auth(&cfg.token)
        .header("X-App-Type", APP_TYPE)
        .json(&body)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!("SearchV2 push OK for meeting {}", meeting.room_id);
        }
        Ok(resp) => {
            let status = resp.status();
            // Pull up to 256 chars of the body so the reason (often a
            // machine-readable JSON error from the middleware) is visible in
            // logs without risking an unbounded allocation on a rogue server.
            let body_preview = response_body_preview(resp).await;
            tracing::warn!(
                "SearchV2 push failed for meeting {}: HTTP {} body={}",
                meeting.room_id,
                status,
                body_preview
            );
        }
        Err(e) => {
            tracing::warn!("SearchV2 push error for meeting {}: {}", meeting.room_id, e);
        }
    }
}

/// Delete a meeting document from SearchV2 after hard deletion.
///
/// Same no-op semantics as [`push_meeting`] when `cfg` is `None`.  Accepts
/// HTTP 2xx or 204 as success; anything else is logged at WARN.
pub async fn delete_meeting_doc(cfg: Option<&SearchConfig>, http: &reqwest::Client, room_id: &str) {
    let Some(cfg) = cfg else {
        return;
    };

    let doc_id = format!("{DOC_TYPE}:{room_id}");
    let url = format!(
        "{}/contentsources/{}/documents/{}",
        cfg.base_url.trim_end_matches('/'),
        CONTENT_SOURCE_ID,
        doc_id,
    );

    let result = http
        .delete(&url)
        .bearer_auth(&cfg.token)
        .header("X-App-Type", APP_TYPE)
        .send()
        .await;

    match result {
        Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 204 => {
            tracing::debug!("SearchV2 delete OK for meeting {}", room_id);
        }
        Ok(resp) => {
            let status = resp.status();
            let body_preview = response_body_preview(resp).await;
            tracing::warn!(
                "SearchV2 delete failed for meeting {}: HTTP {} body={}",
                room_id,
                status,
                body_preview
            );
        }
        Err(e) => {
            tracing::warn!("SearchV2 delete error for meeting {}: {}", room_id, e);
        }
    }
}

// ---------------------------------------------------------------------------
// Response diagnostics
// ---------------------------------------------------------------------------

/// Maximum number of bytes of a failed-response body that we pull into logs.
///
/// Large enough to cover any realistic middleware error payload (usually a
/// short JSON document with a `message` field) but small enough that a rogue
/// or misbehaving server cannot exhaust meeting-api memory via a very long
/// error response.
const RESPONSE_BODY_PREVIEW_BYTES: usize = 256;

/// Consume a failed `reqwest::Response` and return a short, log-safe preview
/// of its body — truncated to [`RESPONSE_BODY_PREVIEW_BYTES`] and rendered as
/// a lossy UTF-8 string so binary or malformed payloads can't break log
/// formatters.
///
/// Returns the placeholder `"<body read error: …>"` when the body cannot be
/// read (e.g. connection reset mid-response).
async fn response_body_preview(resp: reqwest::Response) -> String {
    match resp.bytes().await {
        Ok(bytes) => {
            let slice = &bytes[..bytes.len().min(RESPONSE_BODY_PREVIEW_BYTES)];
            let preview = String::from_utf8_lossy(slice).to_string();
            if bytes.len() > RESPONSE_BODY_PREVIEW_BYTES {
                format!("{preview}… (truncated at {RESPONSE_BODY_PREVIEW_BYTES} bytes)")
            } else {
                preview
            }
        }
        Err(e) => format!("<body read error: {e}>"),
    }
}

// ---------------------------------------------------------------------------
// Body builder — split out so unit tests can exercise the shape without
// involving an HTTP mock.
// ---------------------------------------------------------------------------

/// Build the JSON body shipped to the content-push API.
///
/// The shape mirrors
/// [`videocall-crawler.driver.ts::transformMeeting`](https://github.com/... )
/// in `opensearch-middleware`, which is the canonical producer for
/// `cc-meetings` documents.  We aim for interchangeable output: a doc pushed
/// here should be indistinguishable from a doc crawled by the middleware's
/// pull driver.
fn build_meeting_body(meeting: &MeetingRow, participants: &[ParticipantAcl]) -> serde_json::Value {
    let created_ms = meeting.created_at.timestamp_millis();
    let updated_ms = meeting.updated_at.timestamp_millis();
    let creator = meeting.creator_id.as_deref().unwrap_or("unknown");
    let state_str = meeting.state.as_deref().unwrap_or("idle");
    let host_display_name = meeting.host_display_name.as_deref().unwrap_or("");
    let started_at_ms = meeting.started_at.timestamp_millis();
    let ended_at_ms = meeting.ended_at.map(|t| t.timestamp_millis());
    let duration_minutes = meeting
        .ended_at
        .map(|end| (end - meeting.started_at).num_minutes());

    // Principals — creator first, then every participant row we were handed.
    // De-dup while preserving order so the creator stays at the top.
    let mut seen = std::collections::HashSet::new();
    let mut ordered_principals: Vec<String> = Vec::with_capacity(participants.len() + 1);
    if seen.insert(creator.to_string()) {
        ordered_principals.push(creator.to_string());
    }
    for p in participants {
        if seen.insert(p.user_id.clone()) {
            ordered_principals.push(p.user_id.clone());
        }
    }

    // Emit both forms (`user:<id>` and bare `<id>`) so the CC query filter
    // matches regardless of whether the middleware recognises the caller by
    // the prefixed or bare principal.  No `"public"` — per PR review, only
    // users listed here should see the meeting via SearchV2.
    let acls: Vec<String> = ordered_principals
        .iter()
        .flat_map(|p| [format!("user:{p}"), p.clone()])
        .collect();

    // Top-level participant arrays — used by the CC filter's
    // `participants` / `documentObject.participants.keyword` clauses.
    let participant_ids: Vec<&str> = participants.iter().map(|p| p.user_id.as_str()).collect();
    let participant_names: Vec<String> = participants
        .iter()
        .map(|p| p.display_name.clone().unwrap_or_else(|| p.user_id.clone()))
        .collect();

    // Rich participant objects for the documentObject nested field.
    let participant_objs: Vec<serde_json::Value> = participants
        .iter()
        .map(|p| {
            json!({
                "email": p.user_id,
                "displayName": p.display_name.clone().unwrap_or_else(|| p.user_id.clone()),
                "isHost": p.is_host,
                "status": p.status,
                "joinedAt": p.joined_at.map(|t| t.timestamp_millis()),
                "admittedAt": p.admitted_at.map(|t| t.timestamp_millis()),
            })
        })
        .collect();

    let title = if host_display_name.is_empty() {
        meeting.room_id.clone()
    } else {
        host_display_name.to_string()
    };

    json!({
        "id": format!("{DOC_TYPE}:{}", meeting.room_id),
        "type": DOC_TYPE,
        "appType": APP_TYPE,
        "created": created_ms,
        "updated": updated_ms,

        "acls": acls,
        "owner": creator,

        "title": title,
        "description": format!("Meeting {} ({})", meeting.room_id, state_str),
        "text": "",
        "tags": ["meeting", "videocall", state_str],

        // Crawler-aligned top-level fields — what the CC filter targets.
        "meetingId": meeting.room_id,
        "organizer": creator,
        "organizerName": host_display_name,
        "participants": participant_ids,
        "participantNames": participant_names,
        "participantCount": participants.len(),
        "startTime": meeting.started_at.to_rfc3339(),
        "endTime": meeting.ended_at.map(|t| t.to_rfc3339()),
        "durationMinutes": duration_minutes,

        // Raw meeting details preserved under documentObject so the CC filter's
        // documentObject.participants clause also matches and deep-link
        // builders have everything they need.
        "documentObject": {
            "meetingId": meeting.room_id,
            "roomId": meeting.room_id,
            "host": creator,
            "state": state_str,
            "hostDisplayName": host_display_name,
            "creator_id": creator,
            "startedAt": started_at_ms,
            "endedAt": ended_at_ms,
            "durationMinutes": duration_minutes,
            "participantCount": participants.len(),
            "participants": participant_objs,
            "attendees": meeting.attendees,
            "hasPassword": meeting.password_hash.is_some(),
            "waitingRoomEnabled": meeting.waiting_room_enabled,
            "admittedCanAdmit": meeting.admitted_can_admit,
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample_meeting() -> MeetingRow {
        let ts = chrono::Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap();
        MeetingRow {
            id: 42,
            room_id: "standup".to_string(),
            started_at: ts,
            ended_at: Some(ts + chrono::Duration::minutes(30)),
            created_at: ts,
            updated_at: ts,
            deleted_at: None,
            creator_id: Some("alice@example.com".to_string()),
            password_hash: None,
            state: Some("ended".to_string()),
            attendees: Some(serde_json::json!(["alice@example.com"])),
            host_display_name: Some("Alice".to_string()),
            waiting_room_enabled: true,
            admitted_can_admit: false,
            end_on_host_leave: true,
            allow_guests: false,
        }
    }

    fn sample_participant(user_id: &str, is_host: bool) -> ParticipantAcl {
        ParticipantAcl {
            user_id: user_id.to_string(),
            display_name: Some(user_id.split('@').next().unwrap_or("").to_string()),
            is_host,
            status: "admitted".to_string(),
            joined_at: None,
            admitted_at: None,
        }
    }

    #[test]
    fn body_shape_matches_cc_meetings_canonical() {
        let meeting = sample_meeting();
        let participants = vec![
            sample_participant("alice@example.com", true),
            sample_participant("bob@example.com", false),
        ];
        let body = build_meeting_body(&meeting, &participants);

        assert_eq!(body["id"], "cc-meetings:standup");
        assert_eq!(body["type"], "cc-meetings");
        assert_eq!(body["appType"], "CC");
        assert_eq!(body["owner"], "alice@example.com");
        assert_eq!(body["meetingId"], "standup");
        assert_eq!(body["organizer"], "alice@example.com");
        assert_eq!(body["organizerName"], "Alice");
        assert_eq!(body["participantCount"], 2);
        assert_eq!(body["durationMinutes"], 30);
        assert!(body["startTime"].is_string());
        assert!(body["endTime"].is_string());

        let participants_field = body["participants"].as_array().unwrap();
        assert_eq!(participants_field.len(), 2);
        assert_eq!(participants_field[0], "alice@example.com");

        let tags = body["tags"].as_array().unwrap();
        assert!(tags.iter().any(|t| t == "meeting"));
        assert!(tags.iter().any(|t| t == "videocall"));
    }

    #[test]
    fn acls_contain_both_prefixed_and_bare_principals_and_no_public() {
        let meeting = sample_meeting();
        let participants = vec![
            sample_participant("alice@example.com", true),
            sample_participant("bob@example.com", false),
        ];
        let body = build_meeting_body(&meeting, &participants);

        let acls: Vec<String> = body["acls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();

        assert!(acls.contains(&"user:alice@example.com".to_string()));
        assert!(acls.contains(&"alice@example.com".to_string()));
        assert!(acls.contains(&"user:bob@example.com".to_string()));
        assert!(acls.contains(&"bob@example.com".to_string()));
        // Per review: no public/anonymous — scope is strict per-user.
        assert!(!acls.iter().any(|a| a == "public"));
        assert!(!acls.iter().any(|a| a == "anonymous"));
    }

    #[test]
    fn acls_dedupe_when_creator_also_appears_in_participants() {
        let meeting = sample_meeting();
        // Creator appears explicitly in the participant list as the host.
        let participants = vec![sample_participant("alice@example.com", true)];
        let body = build_meeting_body(&meeting, &participants);

        let acls: Vec<String> = body["acls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();

        // Each principal should contribute exactly two ACL entries
        // (`user:<id>` + bare `<id>`), not four.
        let alice_prefixed = acls
            .iter()
            .filter(|a| a == &"user:alice@example.com")
            .count();
        let alice_bare = acls.iter().filter(|a| a == &"alice@example.com").count();
        assert_eq!(alice_prefixed, 1);
        assert_eq!(alice_bare, 1);
    }

    #[test]
    fn document_object_mirrors_crawler_fields() {
        let meeting = sample_meeting();
        let participants = vec![sample_participant("alice@example.com", true)];
        let body = build_meeting_body(&meeting, &participants);

        let doc = &body["documentObject"];
        assert_eq!(doc["roomId"], "standup");
        assert_eq!(doc["host"], "alice@example.com");
        assert_eq!(doc["creator_id"], "alice@example.com");
        assert_eq!(doc["hostDisplayName"], "Alice");
        assert_eq!(doc["state"], "ended");
        assert_eq!(doc["hasPassword"], false);
        assert_eq!(doc["waitingRoomEnabled"], true);
        assert_eq!(doc["admittedCanAdmit"], false);
        assert!(doc["startedAt"].is_number());
        assert!(doc["endedAt"].is_number());

        let ps = doc["participants"].as_array().unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0]["email"], "alice@example.com");
        assert_eq!(ps[0]["isHost"], true);
        assert_eq!(ps[0]["status"], "admitted");
    }

    #[tokio::test]
    async fn push_meeting_none_is_noop() {
        // No panic, no HTTP call when config is None — just returns.
        let http = reqwest::Client::new();
        let meeting = sample_meeting();
        push_meeting(None, &http, &meeting, &[]).await;
    }

    #[tokio::test]
    async fn delete_meeting_doc_none_is_noop() {
        let http = reqwest::Client::new();
        delete_meeting_doc(None, &http, "standup").await;
    }

    #[tokio::test]
    async fn push_meeting_builds_url_with_document_prefix_and_app_type_header() {
        let mock = wiremock::MockServer::start().await;

        use wiremock::matchers::{header, header_exists, method, path};
        use wiremock::{Mock, ResponseTemplate};

        Mock::given(method("PUT"))
            .and(path(
                "/contentsources/cs-cc-meetings/documents/cc-meetings:standup",
            ))
            .and(header("x-app-type", "CC"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = SearchConfig {
            base_url: mock.uri(),
            token: "test-admin-token".to_string(),
        };
        let http = reqwest::Client::new();
        let meeting = sample_meeting();
        let participants = vec![sample_participant("alice@example.com", true)];

        push_meeting(Some(&cfg), &http, &meeting, &participants).await;

        // `expect(1)` above asserts the mock was called exactly once.
        // Dropping the mock server at scope end verifies all expectations.
        drop(mock);
    }

    #[tokio::test]
    async fn delete_meeting_doc_uses_correct_url() {
        let mock = wiremock::MockServer::start().await;

        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, ResponseTemplate};

        Mock::given(method("DELETE"))
            .and(path(
                "/contentsources/cs-cc-meetings/documents/cc-meetings:standup",
            ))
            .and(header("x-app-type", "CC"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&mock)
            .await;

        let cfg = SearchConfig {
            base_url: mock.uri(),
            token: "test-admin-token".to_string(),
        };
        let http = reqwest::Client::new();
        delete_meeting_doc(Some(&cfg), &http, "standup").await;

        drop(mock);
    }
}
